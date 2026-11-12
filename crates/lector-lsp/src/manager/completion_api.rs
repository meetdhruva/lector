//! Completion, completion resolve and signature help requests.
//!
//! Lifecycle (all three): the request is tied to the document version synced
//! when it is made; an answer for another version is dropped
//! ([`RequestOutcome::Stale`]); a newer request of the same kind for the same
//! document cancels the one in flight (`$/cancelRequest`); a [`CancelToken`]
//! cancels it on demand. Cancelled requests resolve without waiting for the
//! server.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use futures::channel::oneshot;
use futures::future::Either;
use parking_lot::Mutex;
use serde_json::Value;

use super::{LspManager, Slot};
use crate::completion::{
    completion_params, parse_completion, parse_resolved, CompletionItem, CompletionRequest,
    CompletionResponse, ResolvedCompletion,
};
use crate::error::{LspError, Result};
use crate::request::{CancelToken, RequestOutcome};
use crate::signature::{parse_signature_help, signature_params, SignatureHelp, SignatureRequest};
use crate::types::{DocumentState, ServerCapabilities};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Completion,
    Resolve,
    Signature,
}

struct Flight {
    seq: u64,
    token: CancelToken,
}

/// In-flight requests per (document, kind) and the snippets setting.
#[derive(Default)]
pub(super) struct CompletionState {
    snippets: AtomicBool,
    next_seq: AtomicU64,
    inflight: Mutex<std::collections::HashMap<(PathBuf, Kind), Flight>>,
}

impl CompletionState {
    /// Whether servers started from now on are told that snippets work.
    pub(super) fn snippets(&self) -> bool {
        self.snippets.load(Ordering::SeqCst)
    }
}

fn doc_state(slot: &Slot, path: &Path) -> Option<DocumentState> {
    slot.inner.lock().docs.get(path).map(|d| DocumentState {
        version: d.version,
        tag: d.tag,
    })
}

/// Everything a request future needs, captured when the request is made.
struct Prepared {
    slot: Arc<Slot>,
    path: PathBuf,
    uri: String,
    doc: DocumentState,
    token: CancelToken,
    seq: u64,
}

impl LspManager {
    /// Tells servers started from now on whether the client supports snippet
    /// syntax in completion items (`editor.completion.snippets`, default off).
    /// Servers already running keep what they were told at `initialize`;
    /// restart them for a change to take effect. Call before opening documents.
    pub fn set_completion_snippets(&self, enabled: bool) {
        self.inner
            .events
            .completion
            .snippets
            .store(enabled, Ordering::SeqCst);
    }

    /// Registers a request of `kind` for `path`, cancelling the previous one of
    /// that kind. `None` when no server has the document.
    fn prepare(&self, path: &Path, kind: Kind, cancel: Option<&CancelToken>) -> Option<Prepared> {
        let (slot, uri) = self.target(path)?;
        let doc = doc_state(&slot, path)?;
        let token = CancelToken::new();
        if let Some(user) = cancel {
            let own = token.clone();
            user.on_cancel(move || own.cancel());
        }
        let state = &self.inner.events.completion;
        let seq = state.next_seq.fetch_add(1, Ordering::SeqCst);
        let previous = state.inflight.lock().insert(
            (path.to_path_buf(), kind),
            Flight {
                seq,
                token: token.clone(),
            },
        );
        if let Some(previous) = previous {
            previous.token.cancel();
        }
        Some(Prepared {
            slot,
            path: path.to_path_buf(),
            uri,
            doc,
            token,
            seq,
        })
    }

    /// `textDocument/completion`. The document must have been synced
    /// (`open_document_tagged`) with the text the position refers to.
    ///
    /// Resolves to [`RequestOutcome::Done`] with the items, or `Cancelled`
    /// (superseded by a newer completion request of this document, or `cancel`
    /// fired), `Stale` (the document changed meanwhile; ask again if still
    /// wanted) or `Unavailable` (no server, not running, no completion).
    pub fn completion(
        &self,
        path: &Path,
        request: CompletionRequest,
        cancel: Option<&CancelToken>,
    ) -> impl Future<Output = Result<RequestOutcome<CompletionResponse>>> + Send + 'static {
        let prepared = self.prepare(path, Kind::Completion, cancel);
        async move {
            let Some(p) = prepared else {
                return Ok(RequestOutcome::Unavailable);
            };
            let params = completion_params(&p.uri, &request);
            let (slot, doc) = (p.slot.clone(), p.doc);
            match run_request(p, "textDocument/completion", params, |c| {
                c.completion.is_some()
            })
            .await?
            {
                RequestOutcome::Done(v) => {
                    let caps = slot
                        .inner
                        .lock()
                        .caps
                        .clone()
                        .and_then(|c| c.completion)
                        .unwrap_or_default();
                    let (items, is_incomplete) = parse_completion(v, &caps.all_commit_characters)?;
                    Ok(RequestOutcome::Done(CompletionResponse {
                        items,
                        is_incomplete,
                        request_pos: request.pos,
                        doc_version: doc.version,
                        tag: doc.tag,
                        resolve_supported: caps.resolve,
                    }))
                }
                other => Ok(other.map_empty()),
            }
        }
    }

    /// `completionItem/resolve`: documentation, detail and additional edits of
    /// `item`. `Unavailable` when the server cannot resolve or the item
    /// already was. A newer resolve of this document cancels the one in flight
    /// (moving the selection quickly); the answer is `Stale` when the document
    /// changed meanwhile, because additional edits refer to a document version.
    pub fn resolve_completion(
        &self,
        path: &Path,
        item: &CompletionItem,
        cancel: Option<&CancelToken>,
    ) -> impl Future<Output = Result<RequestOutcome<ResolvedCompletion>>> + Send + 'static {
        let already = item.resolved;
        let raw = item.raw.clone();
        let prepared = self.prepare(path, Kind::Resolve, cancel);
        async move {
            let Some(p) = prepared else {
                return Ok(RequestOutcome::Unavailable);
            };
            if already {
                return Ok(RequestOutcome::Unavailable);
            }
            let doc = p.doc;
            match run_request(p, "completionItem/resolve", raw, |c| {
                c.completion.as_ref().is_some_and(|c| c.resolve)
            })
            .await?
            {
                RequestOutcome::Done(v) => {
                    let mut resolved = parse_resolved(&v);
                    resolved.doc_version = doc.version;
                    resolved.tag = doc.tag;
                    Ok(RequestOutcome::Done(resolved))
                }
                other => Ok(other.map_empty()),
            }
        }
    }

    /// `textDocument/signatureHelp`. `Done(None)`: the server has no signature
    /// there (close the popup). Same lifecycle as [`LspManager::completion`].
    pub fn signature_help(
        &self,
        path: &Path,
        request: SignatureRequest,
        cancel: Option<&CancelToken>,
    ) -> impl Future<Output = Result<RequestOutcome<Option<SignatureHelp>>>> + Send + 'static {
        let prepared = self.prepare(path, Kind::Signature, cancel);
        async move {
            let Some(p) = prepared else {
                return Ok(RequestOutcome::Unavailable);
            };
            let params = signature_params(&p.uri, &request);
            match run_request(p, "textDocument/signatureHelp", params, |c| {
                c.signature_help.is_some()
            })
            .await?
            {
                RequestOutcome::Done(v) => Ok(RequestOutcome::Done(parse_signature_help(v))),
                other => Ok(other.map_empty()),
            }
        }
    }
}

impl RequestOutcome<Value> {
    /// The non-`Done` outcomes carried over to another payload type.
    fn map_empty<U>(self) -> RequestOutcome<U> {
        match self {
            RequestOutcome::Done(_) | RequestOutcome::Unavailable => RequestOutcome::Unavailable,
            RequestOutcome::Cancelled => RequestOutcome::Cancelled,
            RequestOutcome::Stale => RequestOutcome::Stale,
        }
    }
}

/// Sends one request and applies the lifecycle rules; resolves to the raw result.
async fn run_request(
    p: Prepared,
    method: &'static str,
    params: Value,
    supported: impl Fn(&ServerCapabilities) -> bool,
) -> Result<RequestOutcome<Value>> {
    let key = (p.path.clone(), kind_of(method));
    let slot = p.slot.clone();
    let seq = p.seq;
    let outcome = run_inner(&p, method, params, supported).await;
    let mut inflight = slot.events.completion.inflight.lock();
    if inflight.get(&key).is_some_and(|f| f.seq == seq) {
        inflight.remove(&key);
    }
    outcome
}

fn kind_of(method: &str) -> Kind {
    match method {
        "completionItem/resolve" => Kind::Resolve,
        "textDocument/signatureHelp" => Kind::Signature,
        _ => Kind::Completion,
    }
}

async fn run_inner(
    p: &Prepared,
    method: &'static str,
    params: Value,
    supported: impl Fn(&ServerCapabilities) -> bool,
) -> Result<RequestOutcome<Value>> {
    let changed = |slot: &Slot| doc_state(slot, &p.path).map(|s| s.version) != Some(p.doc.version);

    // Wait for the server, giving up as soon as the request is cancelled.
    let (tx, rx) = oneshot::channel::<()>();
    p.token.on_cancel(move || {
        let _ = tx.send(());
    });
    let ready = Box::pin(p.slot.clone().wait_ready());
    let client = match futures::future::select(ready, rx).await {
        Either::Left((Some(client), _)) => client,
        Either::Left((None, _)) => return Ok(RequestOutcome::Unavailable),
        Either::Right((Ok(()), _)) => return Ok(RequestOutcome::Cancelled),
        Either::Right((Err(_), ready)) => match ready.await {
            Some(client) => client,
            None => return Ok(RequestOutcome::Unavailable),
        },
    };
    if p.token.is_cancelled() {
        return Ok(RequestOutcome::Cancelled);
    }
    let caps = p.slot.inner.lock().caps.clone();
    if !caps.as_ref().is_some_and(supported) {
        return Ok(RequestOutcome::Unavailable);
    }
    if changed(&p.slot) {
        return Ok(RequestOutcome::Stale);
    }

    let (id, response) = client.request_cancellable(method, params);
    {
        let client = client.clone();
        p.token.on_cancel(move || client.cancel_request(id));
    }
    let result = response.await;
    match result {
        Err(LspError::Cancelled) => Ok(RequestOutcome::Cancelled),
        Err(LspError::Server { code: -32800, .. }) => Ok(RequestOutcome::Cancelled),
        Err(LspError::Server { code: -32801, .. }) => Ok(RequestOutcome::Stale),
        Err(_) if p.token.is_cancelled() => Ok(RequestOutcome::Cancelled),
        Err(e) => Err(e),
        Ok(_) if p.token.is_cancelled() => Ok(RequestOutcome::Cancelled),
        Ok(_) if changed(&p.slot) => Ok(RequestOutcome::Stale),
        Ok(v) => Ok(RequestOutcome::Done(v)),
    }
}

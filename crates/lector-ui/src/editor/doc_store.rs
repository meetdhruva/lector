//! The GPUI side of the headless document store: which
//! [`lector_app::documents::DocumentStore`] the app's tabs use, and the
//! executor it runs on.
//!
//! In the app the store is the `AppCore`'s (so commands, agents and every
//! window share documents) and runs on the shared scheduler. Under GPUI's
//! test dispatcher it runs on the test executor instead, so
//! `run_until_parked` and fake clocks drive reparses, queries and debounces
//! deterministically.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, BackgroundExecutor, Global};
use lector_app::documents::{DocExecutor, DocJob, DocumentStore};
use lector_app::scheduler::{CancelToken, JobKey, Priority};
use parking_lot::Mutex;

/// The store used by this app's document models.
#[derive(Clone)]
struct GlobalStore(DocumentStore);

impl Global for GlobalStore {}

/// The document store of the app (created on first use).
pub fn document_store(cx: &mut App) -> DocumentStore {
    if let Some(store) = cx.try_global::<GlobalStore>() {
        return store.0.clone();
    }
    let store = new_store(cx);
    cx.set_global(GlobalStore(store.clone()));
    store
}

/// Installs `store` (e.g. one shared with a headless core) before any
/// document is opened.
pub fn set_document_store(store: DocumentStore, cx: &mut App) {
    cx.set_global(GlobalStore(store));
}

fn new_store(cx: &mut App) -> DocumentStore {
    #[cfg(any(test, feature = "test-support"))]
    if cx.background_executor().dispatcher.as_test().is_some() {
        return DocumentStore::new(Arc::new(GpuiExecutor::new(
            cx.background_executor().clone(),
        )));
    }
    let _ = &cx;
    let core = lector_app::AppCore::global();
    (*DocumentStore::for_core(&core)).clone()
}

/// A [`DocExecutor`] on GPUI's background executor (keys and debounces
/// implemented with cancel tokens; timers use the executor's clock).
pub struct GpuiExecutor {
    bg: BackgroundExecutor,
    keys: Arc<Mutex<HashMap<String, CancelToken>>>,
}

impl GpuiExecutor {
    /// An executor spawning on `bg`.
    pub fn new(bg: BackgroundExecutor) -> Self {
        GpuiExecutor {
            bg,
            keys: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn token(&self, key: Option<&JobKey>) -> CancelToken {
        let token = CancelToken::new();
        if let Some(key) = key {
            if let Some(old) = self.keys.lock().insert(key.0.clone(), token.clone()) {
                old.cancel();
            }
        }
        token
    }
}

impl DocExecutor for GpuiExecutor {
    fn spawn(&self, _prio: Priority, key: Option<JobKey>, job: DocJob) {
        let token = self.token(key.as_ref());
        self.bg
            .spawn(async move {
                if !token.is_cancelled() {
                    job(&token);
                }
            })
            .detach();
    }

    fn debounce(&self, key: JobKey, delay: Duration, _prio: Priority, job: DocJob) {
        let token = self.token(Some(&key));
        let bg = self.bg.clone();
        self.bg
            .spawn(async move {
                if !delay.is_zero() {
                    bg.timer(delay).await;
                }
                if !token.is_cancelled() {
                    job(&token);
                }
            })
            .detach();
    }
}

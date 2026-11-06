//! [`AgentManager`]: the sessions of a process (an [`AppCore`] extension), shared by the
//! GUI, commands, the CLI and MCP; plus the agent [`ProvenanceProvider`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use lector_app::provenance::{
    LineProvenance, LineQuery, ProvenanceKind, ProvenanceProvider, PRIORITY_AGENT,
};
use lector_app::AppCore;
use lector_diff::remap::LineMap;
use lector_diff::{diff_lines, DiffOptions};
use lector_workspace::Project;
use parking_lot::Mutex;

use crate::config::AgentConfig;
use crate::error::AgentError;
use crate::model::{Turn, TurnId};
use crate::session::{AgentSession, SessionId, SessionOptions};

type Callback = Arc<dyn Fn() + Send + Sync>;
/// Changed lines of a turn's "after" text of one file, and that text.
type TurnLines = Arc<(Vec<u32>, String)>;

/// All agent sessions of the process.
#[derive(Default)]
pub struct AgentManager {
    sessions: Mutex<Vec<AgentSession>>,
    subscribers: Mutex<Vec<(u64, Callback)>>,
    next_sub: AtomicU64,
    provider: AtomicBool,
    /// (session, turn, path) → changed lines of the turn's "after" text, and that text.
    provenance_cache: Mutex<HashMap<(SessionId, TurnId, String), TurnLines>>,
}

/// Unsubscribes on drop.
pub struct ManagerSubscription {
    id: u64,
    manager: Weak<AgentManager>,
}

impl Drop for ManagerSubscription {
    fn drop(&mut self) {
        if let Some(m) = self.manager.upgrade() {
            m.subscribers.lock().retain(|(id, _)| *id != self.id);
        }
    }
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

impl AgentManager {
    /// The core's manager (created on first use; registers the provenance provider).
    pub fn of(core: &AppCore) -> Arc<AgentManager> {
        let manager = core.extension_or_init(|| Arc::new(AgentManager::default()));
        if !manager.provider.swap(true, Ordering::AcqRel) {
            core.provenance().register(
                PRIORITY_AGENT,
                Arc::new(AgentProvenance {
                    manager: Arc::downgrade(&manager),
                }),
            );
        }
        manager
    }

    /// Every session, oldest first.
    pub fn sessions(&self) -> Vec<AgentSession> {
        self.sessions.lock().clone()
    }

    /// The newest session of the project at `root`.
    pub fn session_for(&self, root: &Path) -> Option<AgentSession> {
        let root = canonical(root);
        self.sessions
            .lock()
            .iter()
            .rev()
            .find(|s| canonical(s.project().root()) == root)
            .cloned()
    }

    /// A session by id.
    pub fn get(&self, id: SessionId) -> Option<AgentSession> {
        self.sessions.lock().iter().find(|s| s.id() == id).cloned()
    }

    /// Starts an agent in `project` (blocking) and makes it the project's current session.
    /// The project's previous session is stopped.
    pub fn start(
        &self,
        core: &AppCore,
        project: &Project,
        cfg: &AgentConfig,
    ) -> Result<AgentSession, AgentError> {
        let options = SessionOptions::from_settings(&lector_config::load_settings().0.agent);
        self.start_with(core, project, cfg, options)
    }

    /// [`AgentManager::start`] with explicit options.
    pub fn start_with(
        &self,
        core: &AppCore,
        project: &Project,
        cfg: &AgentConfig,
        options: SessionOptions,
    ) -> Result<AgentSession, AgentError> {
        let session = AgentSession::start_with(core, project, cfg, options)?;
        if let Some(previous) = self.session_for(project.root()) {
            previous.stop();
        }
        self.sessions.lock().push(session.clone());
        self.notify();
        Ok(session)
    }

    /// Stops every session (on quit).
    pub fn stop_all(&self) {
        for s in self.sessions() {
            s.stop();
        }
    }

    /// Calls `cb` when sessions are added.
    pub fn subscribe(
        self: &Arc<Self>,
        cb: impl Fn() + Send + Sync + 'static,
    ) -> ManagerSubscription {
        let id = self.next_sub.fetch_add(1, Ordering::Relaxed);
        self.subscribers.lock().push((id, Arc::new(cb)));
        ManagerSubscription {
            id,
            manager: Arc::downgrade(self),
        }
    }

    fn notify(&self) {
        let subs: Vec<Callback> = self
            .subscribers
            .lock()
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        for cb in subs {
            cb();
        }
    }

    /// Lines of `path` (repository-relative) in the current working tree that `turn`
    /// changed and that are still as the turn left them.
    pub fn turn_lines_now(&self, session: &AgentSession, turn: &Turn, path: &str) -> Vec<u32> {
        let key = (session.id(), turn.id, path.to_string());
        let cached = self.provenance_cache.lock().get(&key).cloned();
        let entry = match cached {
            Some(e) => e,
            None => {
                let Some(e) = changed_after_lines(session, turn, path) else {
                    return Vec::new();
                };
                let e = Arc::new(e);
                self.provenance_cache.lock().insert(key, e.clone());
                e
            }
        };
        let (lines, after_text) = (&entry.0, &entry.1);
        let Some(workdir) = session.project().repo().map(|r| r.workdir().to_path_buf()) else {
            return Vec::new();
        };
        let current = crate::context::current_text(
            &session_core(session),
            session.project(),
            &workdir.join(path).to_string_lossy(),
        )
        .map(|(_, t)| t)
        .unwrap_or_default();
        let diff = diff_lines(after_text, &current, &DiffOptions::default());
        let map = LineMap::new(after_text, &current, &diff);
        let mut out: Vec<u32> = lines
            .iter()
            .filter_map(|l| match map.map(*l) {
                lector_diff::remap::Mapped::Exact(n) | lector_diff::remap::Mapped::Moved(n) => {
                    Some(n)
                }
                _ => None,
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

fn session_core(session: &AgentSession) -> AppCore {
    session.core().clone()
}

/// Added or modified lines of the turn's "after" version of `path` (and that text).
fn changed_after_lines(
    session: &AgentSession,
    turn: &Turn,
    path: &str,
) -> Option<(Vec<u32>, String)> {
    let shadow = session.shadow_repo()?;
    let before = turn.before.as_ref()?;
    let after = turn.after.as_ref()?;
    let old = shadow
        .file_at(before, path)
        .ok()
        .flatten()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let new = String::from_utf8_lossy(&shadow.file_at(after, path).ok()??).into_owned();
    let diff = diff_lines(&old, &new, &DiffOptions::default());
    let lines = diff
        .hunks
        .iter()
        .flat_map(|h| h.new.clone())
        .collect::<Vec<u32>>();
    Some((lines, new))
}

/// "Why is this line like this?" for lines an agent turn wrote.
struct AgentProvenance {
    manager: Weak<AgentManager>,
}

impl ProvenanceProvider for AgentProvenance {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn explain(&self, q: &LineQuery) -> Option<LineProvenance> {
        if q.rev.is_some() {
            return None;
        }
        let manager = self.manager.upgrade()?;
        let repo = canonical(&q.repo);
        for session in manager.sessions().iter().rev() {
            let Some(workdir) = session.project().repo().map(|r| canonical(r.workdir())) else {
                continue;
            };
            if workdir != repo {
                continue;
            }
            for turn in session.turns().iter().rev() {
                if turn.undone
                    || turn.after.is_none()
                    || !turn.changed_files.iter().any(|c| c.path == q.path)
                {
                    continue;
                }
                if manager
                    .turn_lines_now(session, turn, &q.path)
                    .contains(&q.line)
                {
                    return Some(LineProvenance {
                        kind: ProvenanceKind::AgentTurn,
                        title: format!(
                            "{} turn {}: {}",
                            session.config().name,
                            turn.id,
                            turn.short_prompt()
                        ),
                        detail: format!(
                            "Prompt:\n{}\n\n{} file{} changed in this turn.",
                            turn.prompt,
                            turn.changed_files.len(),
                            if turn.changed_files.len() == 1 {
                                ""
                            } else {
                                "s"
                            }
                        ),
                        id: format!("{}:{}", session.id().0, turn.id.0),
                    });
                }
            }
        }
        None
    }
}

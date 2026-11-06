//! Plain data of an agent session: turns, messages, tool calls, plans, permissions.
//! Everything here is `Clone` and serializable (commands return it as JSON).

use std::time::{SystemTime, UNIX_EPOCH};

use lector_git::{ChangeKind, ShadowSnapshot};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identifies a turn within a session (1, 2, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TurnId(pub u64);

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifies a pending permission request within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PermissionRequestId(pub u64);

/// Where a turn is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum TurnState {
    /// Taking the "before" checkpoint.
    Starting,
    /// The agent is working.
    Running,
    /// Finished normally (`stop_reason` as the agent reported it: `end_turn`, `max_tokens`...).
    Completed(String),
    /// Cancelled by the user.
    Cancelled,
    /// The agent failed or crashed; the message says why.
    Failed(String),
}

impl TurnState {
    /// Whether the turn is over.
    pub fn is_finished(&self) -> bool {
        !matches!(self, TurnState::Starting | TurnState::Running)
    }

    /// Short label ("running", "completed", ...).
    pub fn label(&self) -> &'static str {
        match self {
            TurnState::Starting => "starting",
            TurnState::Running => "running",
            TurnState::Completed(_) => "completed",
            TurnState::Cancelled => "cancelled",
            TurnState::Failed(_) => "failed",
        }
    }
}

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// The agent's answer (Markdown).
    Agent,
    /// The agent's reasoning.
    Thought,
    /// A user message echoed by the agent.
    User,
}

/// One message (chunks of the same kind in a row are merged).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    /// Who.
    pub kind: MessageKind,
    /// Markdown text.
    pub text: String,
}

/// Tool call status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Not started (waiting for input or permission).
    #[default]
    Pending,
    /// Running.
    InProgress,
    /// Done.
    Completed,
    /// Failed.
    Failed,
}

impl ToolStatus {
    fn parse(s: &str) -> Option<ToolStatus> {
        Some(match s {
            "pending" => ToolStatus::Pending,
            "in_progress" => ToolStatus::InProgress,
            "completed" => ToolStatus::Completed,
            "failed" => ToolStatus::Failed,
            _ => return None,
        })
    }
}

/// A file location a tool call touches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLocation {
    /// Absolute path (as the agent sent it).
    pub path: String,
    /// 0-based line, if given.
    pub line: Option<u32>,
}

/// A proposed or applied edit shown with a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDiff {
    /// Absolute path.
    pub path: String,
    /// Old text (`None` for a new file).
    pub old_text: Option<String>,
    /// New text.
    pub new_text: String,
}

/// A tool call of the agent (read, edit, run...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolCall {
    /// The agent's id for it.
    pub id: String,
    /// Human title ("Edit src/main.rs").
    pub title: String,
    /// ACP kind: `read`, `edit`, `delete`, `move`, `search`, `execute`, `think`, `fetch`, `other`.
    pub kind: String,
    /// Status.
    pub status: ToolStatus,
    /// Files it touches.
    pub locations: Vec<ToolLocation>,
    /// Diffs it carries.
    pub diffs: Vec<ToolDiff>,
    /// Text output (content blocks, joined).
    pub output: String,
}

/// Plan entry status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// To do.
    #[default]
    Pending,
    /// Being worked on.
    InProgress,
    /// Done.
    Completed,
}

/// One entry of the agent's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    /// What.
    pub content: String,
    /// `high` / `medium` / `low`.
    pub priority: String,
    /// Status.
    pub status: PlanStatus,
}

/// The user's answer to a permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionChoice {
    /// Allow this once.
    AllowOnce,
    /// Allow this kind of tool call for the rest of the session.
    AllowAlways,
    /// Reject.
    Reject,
}

/// An option the agent offers for a permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    /// The agent's option id.
    pub id: String,
    /// Label.
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

/// A request of the agent to run a tool call, shown to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Id within the session.
    pub id: PermissionRequestId,
    /// The turn it belongs to.
    pub turn: Option<TurnId>,
    /// The tool call's id.
    pub tool_call_id: String,
    /// What the agent wants to do.
    pub title: String,
    /// Tool kind (`edit`, `execute`...).
    pub kind: String,
    /// The options offered.
    pub options: Vec<PermissionOption>,
    /// How it was answered (`None` while pending; also `None` if the turn was cancelled).
    pub answer: Option<PermissionChoice>,
    /// Whether it is still waiting for the user.
    pub pending: bool,
    /// Answered automatically ("always" earlier in the session, or `auto_allow_reads`).
    pub automatic: bool,
}

/// One element of a turn's timeline, in arrival order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "index", rename_all = "snake_case")]
pub enum TurnItem {
    /// `Turn::messages[i]`.
    Message(usize),
    /// `Turn::tool_calls[i]`.
    ToolCall(usize),
    /// `Turn::permissions[i]`.
    Permission(usize),
}

/// A file the turn changed (from its checkpoints).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// Repository-relative path.
    pub path: String,
    /// What happened.
    pub kind: ChangeKind,
}

/// Context attached to a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptContext {
    /// A whole file (sent with its current, possibly unsaved, text).
    File {
        /// Absolute or project-relative path.
        path: String,
    },
    /// Lines of a file (0-based, `[start, end)`).
    Range {
        /// Absolute or project-relative path.
        path: String,
        /// First line.
        start: u32,
        /// Line after the last.
        end: u32,
    },
    /// A unified diff (current changes, a hunk...).
    Diff {
        /// Label ("Working tree changes").
        label: String,
        /// The diff text.
        diff: String,
    },
    /// Review comments / feedback.
    Comments {
        /// Already formatted feedback (file:line anchors and text).
        text: String,
    },
}

impl PromptContext {
    /// Short label for chips and the transcript ("@src/main.rs:10-20").
    pub fn label(&self) -> String {
        match self {
            PromptContext::File { path } => format!("@{path}"),
            PromptContext::Range { path, start, end } => {
                format!("@{path}:{}-{}", start + 1, end.max(&(start + 1)))
            }
            PromptContext::Diff { label, .. } => label.clone(),
            PromptContext::Comments { .. } => "Review feedback".into(),
        }
    }
}

/// One prompt and everything the agent did for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// Id (1, 2, ... within the session).
    pub id: TurnId,
    /// The prompt text.
    pub prompt: String,
    /// Labels of the attached context.
    pub context: Vec<String>,
    /// Working-tree checkpoint taken before the prompt (`None`: checkpoints off, no
    /// repository, or the snapshot failed).
    pub before: Option<ShadowSnapshot>,
    /// Checkpoint taken after the turn finished.
    pub after: Option<ShadowSnapshot>,
    /// Messages.
    pub messages: Vec<AgentMessage>,
    /// Tool calls.
    pub tool_calls: Vec<ToolCall>,
    /// Permission requests of this turn.
    pub permissions: Vec<PermissionRequest>,
    /// The latest plan.
    pub plan: Vec<PlanEntry>,
    /// Arrival order of messages / tool calls / permissions.
    pub timeline: Vec<TurnItem>,
    /// State.
    pub state: TurnState,
    /// Files changed between the checkpoints.
    pub changed_files: Vec<ChangedFile>,
    /// Project-relative paths the agent wrote through Lector (`fs/write_text_file`).
    pub written: Vec<String>,
    /// Journal entry carrying the turn's undo.
    pub journal_id: Option<u64>,
    /// Whether "Undo turn" ran (and was not redone).
    pub undone: bool,
    /// Unix seconds.
    pub started: i64,
    /// Unix seconds.
    pub finished: Option<i64>,
}

impl Turn {
    pub(crate) fn new(id: TurnId, prompt: String, context: Vec<String>) -> Turn {
        Turn {
            id,
            prompt,
            context,
            before: None,
            after: None,
            messages: Vec::new(),
            tool_calls: Vec::new(),
            permissions: Vec::new(),
            plan: Vec::new(),
            timeline: Vec::new(),
            state: TurnState::Starting,
            changed_files: Vec::new(),
            written: Vec::new(),
            journal_id: None,
            undone: false,
            started: now_secs(),
            finished: None,
        }
    }

    /// The turn's changes as a reviewable changeset (both checkpoints present).
    pub fn changeset(&self) -> Option<lector_git::Changeset> {
        Some(lector_git::Changeset::Snapshots {
            from: self.before.as_ref()?.commit,
            to: self.after.as_ref()?.commit,
        })
    }

    /// The first line of the prompt, shortened.
    pub fn short_prompt(&self) -> String {
        let first = self.prompt.lines().next().unwrap_or("").trim();
        let mut s: String = first.chars().take(60).collect();
        if first.chars().count() > 60 {
            s.push('…');
        }
        s
    }

    /// Applies one `session/update` payload.
    pub(crate) fn apply_update(&mut self, update: &Value) {
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "agent_message_chunk" => self.push_chunk(MessageKind::Agent, update),
            "agent_thought_chunk" => self.push_chunk(MessageKind::Thought, update),
            "user_message_chunk" => self.push_chunk(MessageKind::User, update),
            "tool_call" => {
                let id = str_field(update, "toolCallId");
                let ix = match self.tool_calls.iter().position(|t| t.id == id) {
                    Some(ix) => ix,
                    None => {
                        self.tool_calls.push(ToolCall {
                            id: id.clone(),
                            ..Default::default()
                        });
                        self.timeline
                            .push(TurnItem::ToolCall(self.tool_calls.len() - 1));
                        self.tool_calls.len() - 1
                    }
                };
                merge_tool_call(&mut self.tool_calls[ix], update, true);
            }
            "tool_call_update" => {
                let id = str_field(update, "toolCallId");
                match self.tool_calls.iter().position(|t| t.id == id) {
                    Some(ix) => merge_tool_call(&mut self.tool_calls[ix], update, false),
                    None => {
                        let mut call = ToolCall {
                            id,
                            ..Default::default()
                        };
                        merge_tool_call(&mut call, update, true);
                        self.tool_calls.push(call);
                        self.timeline
                            .push(TurnItem::ToolCall(self.tool_calls.len() - 1));
                    }
                }
            }
            "plan" => {
                self.plan = update
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|entries| {
                        entries
                            .iter()
                            .map(|e| PlanEntry {
                                content: str_field(e, "content"),
                                priority: e
                                    .get("priority")
                                    .and_then(Value::as_str)
                                    .unwrap_or("medium")
                                    .to_string(),
                                status: match e.get("status").and_then(Value::as_str) {
                                    Some("in_progress") => PlanStatus::InProgress,
                                    Some("completed") => PlanStatus::Completed,
                                    _ => PlanStatus::Pending,
                                },
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            }
            other => tracing::trace!("ignored session update `{other}`"),
        }
    }

    fn push_chunk(&mut self, kind: MessageKind, update: &Value) {
        let text = content_text(update.get("content").unwrap_or(&Value::Null));
        if text.is_empty() {
            return;
        }
        if let Some(TurnItem::Message(ix)) = self.timeline.last() {
            if self.messages[*ix].kind == kind {
                self.messages[*ix].text.push_str(&text);
                return;
            }
        }
        self.messages.push(AgentMessage { kind, text });
        self.timeline
            .push(TurnItem::Message(self.messages.len() - 1));
    }

    /// All agent messages joined (for CLI/MCP answers).
    pub fn answer(&self) -> String {
        self.messages
            .iter()
            .filter(|m| m.kind == MessageKind::Agent)
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The text of an ACP content block (text, or a link/resource label).
pub(crate) fn content_text(block: &Value) -> String {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => str_field(block, "text"),
        Some("resource_link") => format!(
            "[{}]({})",
            str_field(block, "name"),
            str_field(block, "uri")
        ),
        Some("resource") => block
            .get("resource")
            .and_then(|r| r.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Some("image") => "[image]".into(),
        Some("audio") => "[audio]".into(),
        _ => String::new(),
    }
}

fn merge_tool_call(call: &mut ToolCall, v: &Value, initial: bool) {
    if let Some(t) = v.get("title").and_then(Value::as_str) {
        call.title = t.to_string();
    }
    if let Some(k) = v.get("kind").and_then(Value::as_str) {
        call.kind = k.to_string();
    } else if initial && call.kind.is_empty() {
        call.kind = "other".into();
    }
    if let Some(s) = v
        .get("status")
        .and_then(Value::as_str)
        .and_then(ToolStatus::parse)
    {
        call.status = s;
    }
    if let Some(locs) = v.get("locations").and_then(Value::as_array) {
        call.locations = locs
            .iter()
            .filter_map(|l| {
                Some(ToolLocation {
                    path: l.get("path")?.as_str()?.to_string(),
                    // ACP lines are 1-based.
                    line: l
                        .get("line")
                        .and_then(Value::as_u64)
                        .map(|n| n.saturating_sub(1) as u32),
                })
            })
            .collect();
    }
    if let Some(content) = v.get("content").and_then(Value::as_array) {
        // `content` replaces the previous content on updates.
        call.diffs.clear();
        call.output.clear();
        for item in content {
            match item.get("type").and_then(Value::as_str) {
                Some("diff") => call.diffs.push(ToolDiff {
                    path: str_field(item, "path"),
                    old_text: item
                        .get("oldText")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    new_text: str_field(item, "newText"),
                }),
                Some("content") => {
                    let text = content_text(item.get("content").unwrap_or(&Value::Null));
                    if !text.is_empty() {
                        if !call.output.is_empty() {
                            call.output.push('\n');
                        }
                        call.output.push_str(&text);
                    }
                }
                Some("terminal") => {
                    if !call.output.is_empty() {
                        call.output.push('\n');
                    }
                    call.output.push_str("[terminal output]");
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn updates_build_a_timeline() {
        let mut t = Turn::new(TurnId(1), "fix it".into(), vec![]);
        t.apply_update(&json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}}));
        t.apply_update(&json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "I will "}}));
        t.apply_update(&json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "edit."}}));
        t.apply_update(&json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Edit a.rs", "kind": "edit", "status": "pending", "locations": [{"path": "/p/a.rs", "line": 3}]}));
        t.apply_update(&json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed", "content": [{"type": "diff", "path": "/p/a.rs", "oldText": "a", "newText": "b"}]}));
        t.apply_update(&json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done."}}));
        t.apply_update(&json!({"sessionUpdate": "plan", "entries": [{"content": "x", "priority": "high", "status": "completed"}]}));
        t.apply_update(&json!({"sessionUpdate": "something_new", "x": 1}));
        assert_eq!(t.messages.len(), 3);
        assert_eq!(t.messages[1].text, "I will edit.");
        assert_eq!(
            t.timeline,
            vec![
                TurnItem::Message(0),
                TurnItem::Message(1),
                TurnItem::ToolCall(0),
                TurnItem::Message(2)
            ]
        );
        let call = &t.tool_calls[0];
        assert_eq!(call.status, ToolStatus::Completed);
        assert_eq!(call.locations[0].line, Some(2));
        assert_eq!(call.diffs[0].new_text, "b");
        assert_eq!(t.plan[0].status, PlanStatus::Completed);
        assert_eq!(t.answer(), "I will edit.\n\nDone.");
    }
}

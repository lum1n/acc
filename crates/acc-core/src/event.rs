//! The normalized event stream every driver produces.
//!
//! Native details that don't fit the common model ride along in `extensions`
//! rather than being dropped, so a UI can light up harness-specific features.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness::Harness;
use crate::state::{BlockKind, SessionState};

fn permission_kind() -> BlockKind {
    BlockKind::Permission
}

/// Identifies an item (message, tool call, file edit, command) within a session.
pub type ItemId = String;

/// One thing that happened in a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionStarted {
        harness: Harness,
        /// The harness's own session/thread id, used for resume and takeover.
        native_id: String,
        model: Option<String>,
    },
    StateChanged(SessionState),
    ItemStarted {
        id: ItemId,
        kind: ItemKind,
        /// The subagent run this item belongs to (its [`ItemKind::Subagent`]
        /// item in the same transcript), when it isn't the main conversation's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<ItemId>,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        extensions: Value,
    },
    ItemDelta {
        id: ItemId,
        text: String,
    },
    /// An item changed (status, content, diff) without finishing yet.
    ItemUpdated {
        id: ItemId,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        extensions: Value,
    },
    ItemCompleted {
        id: ItemId,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        extensions: Value,
    },
    /// The harness is waiting for an approval. Answer with the request id.
    PermissionRequested {
        request_id: String,
        title: String,
        options: Vec<PermissionOption>,
        /// A question to answer (options are suggested answers, and any
        /// text is accepted as the option id) rather than an approval.
        #[serde(default = "permission_kind")]
        kind: BlockKind,
        /// The tool item this request is about, when the harness says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<ItemId>,
        /// The tool input or request parameters, for previews.
        #[serde(default, skip_serializing_if = "Value::is_null")]
        input: Value,
    },
    /// A pending request was answered (possibly by another client) or withdrawn.
    PermissionResolved {
        request_id: String,
    },
    Usage(Usage),
    /// Quota or rate-limit state, only where the harness reports it itself.
    Quota(Quota),
    /// The command palette changed; read it again with `Driver::commands`.
    CommandsChanged,
    /// Options or their current values changed; read them with `Driver::options`.
    OptionsChanged,
    /// Informational text from the harness: warnings, command output, notices.
    Notice {
        message: String,
    },
    /// What a TUI would show in a dialog (MCP servers, status, rules),
    /// with the actions it offers. Answers a palette entry; a newer panel
    /// with the same `id` replaces the old one.
    Panel(Panel),
    /// The last event of a turn, on every harness: the state the turn ended
    /// in (done, error, idle, waiting) has already been reported.
    TurnCompleted,
    Exited {
        code: Option<i32>,
    },
    /// A frame the driver didn't understand. Kept for debugging and forward-compat.
    Unrecognized {
        raw: String,
    },
    /// A harness-native update with no common shape yet (plans, session
    /// titles, extension notifications), passed through for UIs.
    Native {
        kind: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        payload: Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    UserMessage,
    AgentMessage,
    Reasoning,
    ToolCall,
    CommandExecution,
    FileChange,
    /// A tool call that runs another agent (Claude's `Agent`, Codex's
    /// collab tools, opencode's `task`, pi's `subagent`). Items of the child
    /// conversation carry this item's id as their `parent`.
    Subagent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
    /// What answering with this option does, when the harness says (ACP's
    /// option kinds, mapped from each harness's own decisions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<PermissionOptionKind>,
}

impl PermissionOption {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind: None,
        }
    }

    pub fn with_kind(mut self, kind: PermissionOptionKind) -> Self {
        self.kind = Some(kind);
        self
    }
}

/// What a permission option does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// Token, cost and context usage as reported by the harness itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub context_used: Option<u64>,
    pub context_window: Option<u64>,
}

/// A harness-reported quota snapshot. ACC never fetches quota itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    /// Account the harness reports the quota for, so sessions on one plan share a bar.
    pub account: Option<String>,
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
}

/// One rate-limit window, such as Claude's five-hour or Codex's primary window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// The harness's own name for the window (`five_hour`, `primary`, ...).
    pub id: String,
    /// 0–100.
    pub used_percent: Option<f64>,
    pub window_minutes: Option<u64>,
    /// Seconds since the Unix epoch.
    pub resets_at: Option<u64>,
    /// Harness status such as `allowed`, `allowed_warning` or `rejected`.
    pub status: Option<String>,
}

/// A dialog's content in a shape any client can draw: sections of rows,
/// each row with optional actions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Panel {
    /// The palette entry it answers, e.g. `mcp`.
    pub id: String,
    pub title: String,
    pub sections: Vec<PanelSection>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub rows: Vec<PanelRow>,
    /// Actions for the section as a whole, such as "Add rule".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<PanelAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelRow {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<PanelAction>,
}

/// A button: runs `Command::Invoke { name: command, args }`. With `prompt`,
/// the client first asks for text and appends it to `args` after a space.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelAction {
    pub label: String,
    pub command: String,
    pub args: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub destructive: bool,
}

impl PanelAction {
    pub fn new(label: impl Into<String>, command: impl Into<String>, args: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            command: command.into(),
            args: args.into(),
            ..Self::default()
        }
    }
}

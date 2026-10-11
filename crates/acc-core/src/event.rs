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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
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

//! The normalized event stream every driver produces.
//!
//! Native details that don't fit the common model ride along in `extensions`
//! rather than being dropped, so a UI can light up harness-specific features.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness::Harness;
use crate::state::SessionState;

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
    },
    Usage(Usage),
    /// The command palette changed; read it again with `Driver::commands`.
    CommandsChanged,
    /// Options or their allowed values changed; read them with `Driver::options`.
    OptionsChanged,
    /// A harness-native update with no common shape yet (plans, notices,
    /// session titles, extension notifications), passed through for UIs.
    Native {
        kind: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        payload: Value,
    },
    TurnCompleted,
    Exited {
        code: Option<i32>,
    },
    /// A frame the driver didn't understand. Kept for debugging and forward-compat.
    Unrecognized {
        raw: String,
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

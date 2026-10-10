//! Canonical commands and harness options.
//!
//! See `research/commands-and-options.md`: some harnesses expand `/name args`
//! themselves, others need ACC to translate a command into a protocol call.
//! Options are never hard-coded; drivers fill them from the harness at runtime.

use serde::{Deserialize, Serialize};

/// Commands ACC offers on every harness, whatever the harness supports headlessly.
///
/// `Invoke` runs a palette entry from `Driver::commands` by name with free-form
/// arguments; `Refresh` asks the harness for fresh usage, quota, commands and options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Prompt { text: String },
    Interrupt,
    Compact { hint: Option<String> },
    New,
    Fork,
    Rename { name: String },
    Review,
    Shell { command: String },
    SetOption { id: OptionId, value: String },
    AnswerPermission { request_id: String, option_id: String },
    Invoke { name: String, args: String },
    Refresh,
}

/// How a slash command reaches the harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "exec", rename_all = "snake_case")]
pub enum Exec {
    /// Forward `/name args` as user text; the harness expands it.
    Text,
    /// Call a protocol method, e.g. `thread/compact/start`.
    Rpc { method: String },
    /// HTTP endpoint (opencode).
    Endpoint { method: String, path: String },
    /// Codex typed skill input item.
    SkillItem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandSource {
    Builtin,
    Custom,
    Skill,
    Mcp,
    Plugin,
    Acc,
}

/// One entry in a session's command palette.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEntry {
    pub name: String,
    pub description: String,
    pub arg_hint: Option<String>,
    pub source: CommandSource,
    pub exec: Exec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionId {
    Model,
    Effort,
    Mode,
    Permission,
}

/// When a changed option takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Apply {
    Live,
    NextTurn,
    /// Needs a restart, which ACC does transparently via native resume.
    Restart,
}

/// One configurable option and its allowed values, as reported by the harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionDef {
    pub id: OptionId,
    pub values: Vec<String>,
    pub current: Option<String>,
    pub apply: Apply,
}

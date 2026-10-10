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
    /// Display names for `values`, in the same order, where the harness gives
    /// them (Copilot's modes are URLs, named `Agent`, `Plan`, ...). Empty when
    /// the values are their own names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    pub current: Option<String>,
    pub apply: Apply,
}

impl OptionDef {
    /// The display name of `value`: its label when the harness named it.
    /// Otherwise an empty parameter list is dropped (Cursor reports its
    /// automatic model as `default[]`).
    pub fn label<'a>(&'a self, value: &'a str) -> &'a str {
        self.values
            .iter()
            .position(|v| v == value)
            .and_then(|i| self.labels.get(i))
            .map_or_else(|| value.strip_suffix("[]").unwrap_or(value), String::as_str)
    }

    /// The display name of the current value.
    pub fn current_label(&self) -> Option<&str> {
        self.current.as_deref().map(|c| self.label(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_name_values() {
        let mut def = OptionDef {
            id: OptionId::Mode,
            values: vec!["https://x#agent".into(), "https://x#plan".into()],
            labels: vec!["Agent".into(), "Plan".into()],
            current: Some("https://x#plan".into()),
            apply: Apply::Live,
        };
        assert_eq!(def.current_label(), Some("Plan"));
        assert_eq!(def.label("other"), "other");
        def.labels.clear();
        def.current = Some("default[]".into());
        assert_eq!(def.current_label(), Some("default"));
        assert_eq!(def.label("gpt-5[effort=high]"), "gpt-5[effort=high]");
    }
}

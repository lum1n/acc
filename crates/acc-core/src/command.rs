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
    /// Only the harness's own TUI can run it (a full-screen dialog with no
    /// headless equivalent). Clients open the TUI, e.g. with
    /// `multiplexer::tui::launch`, instead of invoking it.
    Terminal,
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
    /// What to ask for before invoking it.
    #[serde(default, skip_serializing_if = "CommandInput::is_none")]
    pub input: CommandInput,
}

/// What a palette entry needs from the user. The answer is passed as the
/// `args` of [`Command::Invoke`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandInput {
    /// Free text as `arg_hint` describes it, or nothing.
    #[default]
    None,
    /// A value of the option `id`, chosen from `Driver::options`. Invoking
    /// with a value sets it; without one shows what is set.
    Option { id: OptionId },
    /// One of these values; `required` when it can't run without one.
    Choice { choices: Vec<Choice>, required: bool },
}

impl CommandInput {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// One value a palette entry or panel action accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Choice {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            value,
            description: None,
        }
    }
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

impl CommandEntry {
    /// One of ACC's own entries.
    pub fn acc(name: &str, description: &str, arg_hint: Option<&str>, exec: Exec) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            arg_hint: arg_hint.map(str::to_owned),
            source: CommandSource::Acc,
            exec,
            input: CommandInput::None,
        }
    }

    pub fn rpc(name: &str, description: &str, arg_hint: Option<&str>, method: &str) -> Self {
        Self::acc(
            name,
            description,
            arg_hint,
            Exec::Rpc {
                method: method.into(),
            },
        )
    }

    pub fn with_input(mut self, input: CommandInput) -> Self {
        self.input = input;
        self
    }
}

/// Lets `name` pick option `id` (`/model` opens a model picker): marks the
/// harness's own entry of that name, or adds `fallback` when it has none.
/// Does nothing when the harness doesn't report the option.
pub fn offer_option(
    entries: &mut Vec<CommandEntry>,
    options: &[OptionDef],
    id: OptionId,
    fallback: CommandEntry,
) {
    if !options.iter().any(|o| o.id == id) {
        return;
    }
    let input = CommandInput::Option { id };
    match entries.iter_mut().find(|e| e.name == fallback.name) {
        Some(e) => e.input = input,
        None => entries.push(fallback.with_input(input)),
    }
}

/// Adds `entry` unless the harness already has one of that name.
pub fn offer(entries: &mut Vec<CommandEntry>, entry: CommandEntry) {
    if !entries.iter().any(|e| e.name == entry.name) {
        entries.push(entry);
    }
}

/// Splits `args` into its first word and the rest, both trimmed.
pub fn split_verb(args: &str) -> (&str, &str) {
    let args = args.trim();
    match args.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (args, ""),
    }
}

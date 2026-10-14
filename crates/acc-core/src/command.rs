//! Canonical commands and harness options.
//!
//! See `research/commands-and-options.md`: some harnesses expand `/name args`
//! themselves, others need ACC to translate a command into a protocol call.
//! Options are never hard-coded; drivers fill them from the harness at runtime.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::driver::DriverError;

/// Commands ACC offers on every harness, whatever the harness supports headlessly.
///
/// `Invoke` runs a palette entry from `Driver::commands` by name with free-form
/// arguments; `Refresh` asks the harness for fresh usage, quota, commands and options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// A user turn: text, images, or both.
    Prompt {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    Interrupt,
    Compact {
        hint: Option<String>,
    },
    New,
    Fork,
    Rename {
        name: String,
    },
    Review,
    Shell {
        command: String,
    },
    SetOption {
        id: OptionId,
        value: String,
    },
    /// `option_id` may be [`crate::question::encode`]d answers to every question.
    AnswerPermission {
        request_id: String,
        option_id: String,
    },
    Invoke {
        name: String,
        args: String,
    },
    Refresh,
}

impl Command {
    /// A text-only prompt.
    pub fn prompt(text: impl Into<String>) -> Self {
        Self::Prompt {
            text: text.into(),
            attachments: Vec::new(),
        }
    }
}

/// An image sent with a prompt, inline. Every harness takes images this way
/// (Anthropic content blocks, Codex data URLs, ACP and pi image blocks), so
/// nothing has to be copied to the host first. Base64 so it rides in JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub media_type: String,
    /// Standard base64 with padding.
    pub data: String,
}

impl Attachment {
    /// Largest image, decoded. Claude Code downsamples anything bigger itself;
    /// the others would carry it whole.
    pub const MAX_BYTES: usize = 5 * 1024 * 1024;
    /// Largest prompt, decoded. Harnesses echo the prompt into the session
    /// log, where a line over `wire::MAX_FRAME` (16 MiB) is dropped; this
    /// keeps the base64 of one prompt well under that.
    pub const MAX_PROMPT_BYTES: usize = 10 * 1024 * 1024;

    pub fn from_bytes(media_type: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            media_type: media_type.into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    /// The decoded image, empty when `data` isn't valid base64.
    pub fn bytes(&self) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.data)
            .unwrap_or_default()
    }

    /// Decoded size, from the base64 length alone.
    pub fn raw_len(&self) -> usize {
        let data = self.data.trim_end_matches('=');
        data.len() * 3 / 4
    }

    /// `data:<media_type>;base64,<data>`, as Codex takes images.
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

/// Rejects attachments no harness, or no session log, can carry.
pub fn validate_attachments(attachments: &[Attachment]) -> Result<(), DriverError> {
    let reject = |why: String| Err(DriverError::AttachmentRejected(why));
    let mut total = 0;
    for a in attachments {
        if !a.media_type.starts_with("image/") {
            return reject(format!("{} is not an image type", a.media_type));
        }
        if a.data.is_empty() {
            return reject("empty image".into());
        }
        if base64::engine::general_purpose::STANDARD.decode(&a.data).is_err() {
            return reject("image data is not valid base64".into());
        }
        let len = a.raw_len();
        if len > Attachment::MAX_BYTES {
            return reject(format!(
                "image is {} MiB, the limit is {} MiB",
                len >> 20,
                Attachment::MAX_BYTES >> 20
            ));
        }
        total += len;
    }
    if total > Attachment::MAX_PROMPT_BYTES {
        return reject(format!(
            "images total {} MiB, the limit per prompt is {} MiB",
            total >> 20,
            Attachment::MAX_PROMPT_BYTES >> 20
        ));
    }
    Ok(())
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn png(bytes: usize) -> Attachment {
        Attachment::from_bytes("image/png", &vec![0x89; bytes])
    }

    #[test]
    fn prompt_json_without_attachments_is_unchanged() {
        let legacy = r#"{"type":"prompt","text":"hi"}"#;
        let cmd: Command = serde_json::from_str(legacy).unwrap();
        assert_eq!(cmd, Command::prompt("hi"));
        assert_eq!(serde_json::to_string(&cmd).unwrap(), legacy);

        let with_image = Command::Prompt {
            text: "what is this".into(),
            attachments: vec![Attachment::from_bytes("image/png", b"PNG")],
        };
        let json = serde_json::to_string(&with_image).unwrap();
        assert!(json.contains(r#""attachments":[{"media_type":"image/png","data":"UE5H"}]"#));
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), with_image);
    }

    #[test]
    fn attachment_helpers() {
        let a = Attachment::from_bytes("image/jpeg", &[1, 2, 3, 4, 5]);
        assert_eq!(a.raw_len(), 5);
        assert_eq!(a.bytes(), [1, 2, 3, 4, 5]);
        assert_eq!(a.data_url(), "data:image/jpeg;base64,AQIDBAU=");
        assert_eq!(png(3000).raw_len(), 3000);
    }

    #[test]
    fn validation_rejects_what_no_harness_or_log_can_carry() {
        let rejected = |list: &[Attachment]| match validate_attachments(list) {
            Err(DriverError::AttachmentRejected(why)) => why,
            other => panic!("accepted: {other:?}"),
        };
        assert!(validate_attachments(&[]).is_ok());
        assert!(validate_attachments(&[png(10), png(Attachment::MAX_BYTES)]).is_ok());
        assert!(rejected(&[Attachment::from_bytes("text/plain", b"x")]).contains("not an image"));
        assert!(
            rejected(&[Attachment {
                media_type: "image/png".into(),
                data: "not base64!".into(),
            }])
            .contains("base64")
        );
        assert!(
            rejected(&[Attachment {
                media_type: "image/png".into(),
                data: String::new(),
            }])
            .contains("empty")
        );
        assert_eq!(
            rejected(&[png(Attachment::MAX_BYTES + 1)]),
            "image is 5 MiB, the limit is 5 MiB"
        );
        let four = 4 * 1024 * 1024;
        assert_eq!(
            rejected(&[png(four), png(four), png(four)]),
            "images total 12 MiB, the limit per prompt is 10 MiB"
        );
    }

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

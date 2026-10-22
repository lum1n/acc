//! Swift/Kotlin mirrors of the core's model types, and conversions both ways.
//!
//! The core types carry serde attributes and `serde_json::Value` payloads that
//! UniFFI can't express, so the boundary gets its own plain records and enums.
//! Items carry the typed `AccItemDetail`; free-form JSON (`native`,
//! `payload`) crosses as JSON strings.

use acc_core::command as core_cmd;
use acc_core::event as core_ev;
use acc_core::harness as core_h;
use acc_core::state::{self as core_st, BlockKind, RunState};
use serde_json::Value;

use crate::acc_types::{AccItemDetail, AccItemStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum Harness {
    Claude,
    Codex,
    Cursor,
    Copilot,
    Pi,
    Opencode,
}

impl From<core_h::Harness> for Harness {
    fn from(h: core_h::Harness) -> Self {
        match h {
            core_h::Harness::Claude => Self::Claude,
            core_h::Harness::Codex => Self::Codex,
            core_h::Harness::Cursor => Self::Cursor,
            core_h::Harness::Copilot => Self::Copilot,
            core_h::Harness::Pi => Self::Pi,
            core_h::Harness::Opencode => Self::Opencode,
        }
    }
}

impl From<Harness> for core_h::Harness {
    fn from(h: Harness) -> Self {
        match h {
            Harness::Claude => Self::Claude,
            Harness::Codex => Self::Codex,
            Harness::Cursor => Self::Cursor,
            Harness::Copilot => Self::Copilot,
            Harness::Pi => Self::Pi,
            Harness::Opencode => Self::Opencode,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Protocol {
    ClaudeStreamJson,
    CodexAppServer,
    Acp,
    PiRpc,
}

impl From<core_h::Protocol> for Protocol {
    fn from(p: core_h::Protocol) -> Self {
        match p {
            core_h::Protocol::ClaudeStreamJson => Self::ClaudeStreamJson,
            core_h::Protocol::CodexAppServer => Self::CodexAppServer,
            core_h::Protocol::Acp => Self::Acp,
            core_h::Protocol::PiRpc => Self::PiRpc,
        }
    }
}

impl From<Protocol> for core_h::Protocol {
    fn from(p: Protocol) -> Self {
        match p {
            Protocol::ClaudeStreamJson => Self::ClaudeStreamJson,
            Protocol::CodexAppServer => Self::CodexAppServer,
            Protocol::Acp => Self::Acp,
            Protocol::PiRpc => Self::PiRpc,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// How to start a harness headless on the host.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<EnvVar>,
    pub protocol: Protocol,
}

impl From<&core_h::LaunchSpec> for LaunchSpec {
    fn from(s: &core_h::LaunchSpec) -> Self {
        Self {
            program: s.program.clone(),
            args: s.args.clone(),
            env: s
                .env
                .iter()
                .map(|(name, value)| EnvVar {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            protocol: s.protocol.into(),
        }
    }
}

impl From<LaunchSpec> for core_h::LaunchSpec {
    fn from(s: LaunchSpec) -> Self {
        Self {
            program: s.program,
            args: s.args,
            env: s.env.into_iter().map(|v| (v.name, v.value)).collect(),
            protocol: s.protocol.into(),
        }
    }
}

/// A run state in OSC 7501's vocabulary. `Clear` only comes from OSC 7501
/// (`state=clear`: the program withdrew its status).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ProgramState {
    Idle,
    Working,
    BlockedPermission,
    BlockedQuestion,
    BlockedAuth,
    Done,
    /// The turn ended with a background subagent still running, whose report
    /// will start another turn on its own.
    Waiting,
    Error,
    Exited,
    Clear,
}

/// Splits a core state into the flat enum plus its progress percentage.
pub fn from_run_state(s: Option<&RunState>) -> (ProgramState, Option<u8>) {
    match s {
        None => (ProgramState::Clear, None),
        Some(RunState::Idle) => (ProgramState::Idle, None),
        Some(RunState::Working { progress }) => (ProgramState::Working, *progress),
        Some(RunState::Blocked {
            kind: BlockKind::Permission,
        }) => (ProgramState::BlockedPermission, None),
        Some(RunState::Blocked {
            kind: BlockKind::Question,
        }) => (ProgramState::BlockedQuestion, None),
        Some(RunState::Blocked {
            kind: BlockKind::Auth,
        }) => (ProgramState::BlockedAuth, None),
        Some(RunState::Done) => (ProgramState::Done, None),
        Some(RunState::Waiting) => (ProgramState::Waiting, None),
        Some(RunState::Error) => (ProgramState::Error, None),
        Some(RunState::Exited) => (ProgramState::Exited, None),
    }
}

/// The inverse of [`from_run_state`]; `None` for `Clear`.
pub fn to_run_state(state: ProgramState, progress: Option<u8>) -> Option<RunState> {
    Some(match state {
        ProgramState::Idle => RunState::Idle,
        ProgramState::Working => RunState::Working { progress },
        ProgramState::BlockedPermission => RunState::Blocked {
            kind: BlockKind::Permission,
        },
        ProgramState::BlockedQuestion => RunState::Blocked {
            kind: BlockKind::Question,
        },
        ProgramState::BlockedAuth => RunState::Blocked {
            kind: BlockKind::Auth,
        },
        ProgramState::Done => RunState::Done,
        ProgramState::Waiting => RunState::Waiting,
        ProgramState::Error => RunState::Error,
        ProgramState::Exited => RunState::Exited,
        ProgramState::Clear => return None,
    })
}

/// Where a state observation came from, lowest fidelity first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum StateSource {
    Heuristic,
    ScreenMatch,
    Transcript,
    Osc7501,
    Native,
}

impl From<core_st::StateSource> for StateSource {
    fn from(s: core_st::StateSource) -> Self {
        match s {
            core_st::StateSource::Heuristic => Self::Heuristic,
            core_st::StateSource::ScreenMatch => Self::ScreenMatch,
            core_st::StateSource::Transcript => Self::Transcript,
            core_st::StateSource::Osc7501 => Self::Osc7501,
            core_st::StateSource::Native => Self::Native,
        }
    }
}

impl From<StateSource> for core_st::StateSource {
    fn from(s: StateSource) -> Self {
        match s {
            StateSource::Heuristic => Self::Heuristic,
            StateSource::ScreenMatch => Self::ScreenMatch,
            StateSource::Transcript => Self::Transcript,
            StateSource::Osc7501 => Self::Osc7501,
            StateSource::Native => Self::Native,
        }
    }
}

/// The latest known state of a session.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SessionState {
    pub state: ProgramState,
    /// 0–100 while working, when the harness reports it.
    #[uniffi(default = None)]
    pub progress: Option<u8>,
    #[uniffi(default = None)]
    pub message: Option<String>,
    pub source: StateSource,
    /// Milliseconds since the Unix epoch.
    pub observed_at_ms: u64,
}

impl From<&core_st::SessionState> for SessionState {
    fn from(s: &core_st::SessionState) -> Self {
        let (state, progress) = from_run_state(Some(&s.state));
        Self {
            state,
            progress,
            message: s.message.clone(),
            source: s.source.into(),
            observed_at_ms: s.observed_at_ms,
        }
    }
}

impl SessionState {
    /// `None` for `Clear`, which the core has no state for.
    pub fn into_core(self) -> Option<core_st::SessionState> {
        Some(core_st::SessionState {
            state: to_run_state(self.state, self.progress)?,
            message: self.message,
            source: self.source.into(),
            observed_at_ms: self.observed_at_ms,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ItemKind {
    UserMessage,
    AgentMessage,
    Reasoning,
    ToolCall,
    CommandExecution,
    FileChange,
    /// A tool call that runs another agent; its conversation's items name
    /// it as `parent`.
    Subagent,
}

impl From<core_ev::ItemKind> for ItemKind {
    fn from(k: core_ev::ItemKind) -> Self {
        match k {
            core_ev::ItemKind::UserMessage => Self::UserMessage,
            core_ev::ItemKind::AgentMessage => Self::AgentMessage,
            core_ev::ItemKind::Reasoning => Self::Reasoning,
            core_ev::ItemKind::ToolCall => Self::ToolCall,
            core_ev::ItemKind::CommandExecution => Self::CommandExecution,
            core_ev::ItemKind::FileChange => Self::FileChange,
            core_ev::ItemKind::Subagent => Self::Subagent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
    /// What it does, when the harness says.
    pub kind: Option<crate::acc_types::AccPermissionOptionKind>,
}

/// Token, cost and context usage as reported by the harness itself.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub context_used: Option<u64>,
    pub context_window: Option<u64>,
}

impl From<core_ev::Usage> for Usage {
    fn from(u: core_ev::Usage) -> Self {
        Self {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_write_tokens: u.cache_write_tokens,
            cost_usd: u.cost_usd,
            context_used: u.context_used,
            context_window: u.context_window,
        }
    }
}

/// A harness-reported quota snapshot.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct Quota {
    pub account: Option<String>,
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct QuotaWindow {
    pub id: String,
    pub used_percent: Option<f64>,
    pub window_minutes: Option<u64>,
    /// Seconds since the Unix epoch.
    pub resets_at: Option<u64>,
    pub status: Option<String>,
}

impl From<core_ev::Quota> for Quota {
    fn from(q: core_ev::Quota) -> Self {
        Self {
            account: q.account,
            plan: q.plan,
            windows: q
                .windows
                .into_iter()
                .map(|w| QuotaWindow {
                    id: w.id,
                    used_percent: w.used_percent,
                    window_minutes: w.window_minutes,
                    resets_at: w.resets_at,
                    status: w.status,
                })
                .collect(),
        }
    }
}

/// One thing that happened in a session. Mirrors `acc_core::Event`; the
/// usage and quota variants are renamed so Kotlin's nested classes don't
/// shadow the `Usage` and `Quota` records.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum Event {
    SessionStarted {
        harness: Harness,
        native_id: String,
        model: Option<String>,
    },
    StateChanged {
        state: SessionState,
    },
    /// An item began; `detail` is what it is so far, the same shape on
    /// every harness.
    ItemStarted {
        id: String,
        kind: ItemKind,
        /// The subagent item this one ran inside, if any.
        parent: Option<String>,
        detail: AccItemDetail,
        /// The harness's own JSON for this frame; for debugging only.
        native_json: Option<String>,
    },
    ItemDelta {
        id: String,
        text: String,
    },
    /// `detail` replaces the item's previous detail whole.
    ItemUpdated {
        id: String,
        detail: AccItemDetail,
        native_json: Option<String>,
    },
    /// `detail` replaces the item's previous detail whole.
    ItemCompleted {
        id: String,
        status: AccItemStatus,
        detail: AccItemDetail,
        native_json: Option<String>,
    },
    PermissionRequested {
        request_id: String,
        title: String,
        options: Vec<PermissionOption>,
        /// `Question`: any text is accepted as the option id.
        kind: RequestKind,
        /// The tool item the request is about, when the harness says.
        item_id: Option<String>,
        /// The tool input or request parameters as JSON, for previews.
        input_json: Option<String>,
    },
    PermissionResolved {
        request_id: String,
    },
    UsageUpdated {
        usage: Usage,
    },
    QuotaUpdated {
        quota: Quota,
    },
    /// The harness named or renamed the session; `SessionSpec::title` now
    /// holds it.
    TitleChanged {
        title: String,
    },
    CommandsChanged,
    OptionsChanged,
    Notice {
        message: String,
    },
    TurnCompleted,
    Exited {
        code: Option<i32>,
    },
    Unrecognized {
        raw: String,
    },
    Native {
        kind: String,
        payload_json: Option<String>,
    },
    /// A dialog a TUI would show (MCP servers, status, rules), with the
    /// actions it offers, as `acc_core::event::Panel` JSON.
    Panel {
        panel_json: String,
    },
}

fn json(v: &Value) -> Option<String> {
    (!v.is_null()).then(|| v.to_string())
}

impl From<core_ev::Event> for Event {
    fn from(e: core_ev::Event) -> Self {
        use core_ev::Event as E;
        match e {
            E::SessionStarted {
                harness,
                native_id,
                model,
            } => Self::SessionStarted {
                harness: harness.into(),
                native_id,
                model,
            },
            E::StateChanged(s) => Self::StateChanged { state: (&s).into() },
            E::ItemStarted {
                id,
                kind,
                parent,
                detail,
                native,
            } => Self::ItemStarted {
                id,
                kind: kind.into(),
                parent,
                detail: detail.into(),
                native_json: json(&native),
            },
            E::ItemDelta { id, text } => Self::ItemDelta { id, text },
            E::ItemUpdated { id, detail, native } => Self::ItemUpdated {
                id,
                detail: detail.into(),
                native_json: json(&native),
            },
            E::ItemCompleted {
                id,
                status,
                detail,
                native,
            } => Self::ItemCompleted {
                id,
                status: status.into(),
                detail: detail.into(),
                native_json: json(&native),
            },
            E::PermissionRequested {
                request_id,
                title,
                options,
                kind,
                item_id,
                input,
            } => Self::PermissionRequested {
                request_id,
                title,
                kind: kind.into(),
                item_id,
                input_json: json(&input),
                options: options
                    .into_iter()
                    .map(|o| PermissionOption {
                        id: o.id,
                        label: o.label,
                        kind: o.kind.map(Into::into),
                    })
                    .collect(),
            },
            E::PermissionResolved { request_id } => Self::PermissionResolved { request_id },
            E::Usage(u) => Self::UsageUpdated { usage: u.into() },
            E::Quota(q) => Self::QuotaUpdated { quota: q.into() },
            E::TitleChanged { title } => Self::TitleChanged { title },
            E::CommandsChanged => Self::CommandsChanged,
            E::OptionsChanged => Self::OptionsChanged,
            E::Notice { message } => Self::Notice { message },
            E::TurnCompleted => Self::TurnCompleted,
            E::Exited { code } => Self::Exited { code },
            E::Unrecognized { raw } => Self::Unrecognized { raw },
            E::Native { kind, payload } => Self::Native {
                kind,
                payload_json: json(&payload),
            },
            E::Panel(panel) => Self::Panel {
                panel_json: serde_json::to_string(&panel).unwrap_or_default(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum OptionId {
    Model,
    Effort,
    Mode,
    Permission,
}

impl From<core_cmd::OptionId> for OptionId {
    fn from(o: core_cmd::OptionId) -> Self {
        match o {
            core_cmd::OptionId::Model => Self::Model,
            core_cmd::OptionId::Effort => Self::Effort,
            core_cmd::OptionId::Mode => Self::Mode,
            core_cmd::OptionId::Permission => Self::Permission,
        }
    }
}

impl From<OptionId> for core_cmd::OptionId {
    fn from(o: OptionId) -> Self {
        match o {
            OptionId::Model => Self::Model,
            OptionId::Effort => Self::Effort,
            OptionId::Mode => Self::Mode,
            OptionId::Permission => Self::Permission,
        }
    }
}

/// An image sent with a prompt: `image/png`, `image/jpeg`, `image/gif` or
/// `image/webp`, at most 5 MiB (10 MiB per prompt).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Attachment {
    pub media_type: String,
    pub data: Vec<u8>,
}

/// What a permission request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RequestKind {
    Permission,
    Question,
    Auth,
}

impl From<BlockKind> for RequestKind {
    fn from(k: BlockKind) -> Self {
        match k {
            BlockKind::Permission => Self::Permission,
            BlockKind::Question => Self::Question,
            BlockKind::Auth => Self::Auth,
        }
    }
}

/// Commands ACC offers on every harness. Mirrors `acc_core::command::Command`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum Command {
    /// Text, plus images sent with it (see [`Attachment`]).
    Prompt {
        text: String,
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
    AnswerPermission {
        request_id: String,
        option_id: String,
    },
    /// Runs a palette entry from `Session::commands` by name.
    Invoke {
        name: String,
        args: String,
    },
    Refresh,
    /// Continue in another harness; only a detached `AccSession` can
    /// (it answers with a relaunch).
    Handoff {
        harness: Harness,
        prompt: Option<String>,
    },
}

impl From<Command> for core_cmd::Command {
    fn from(c: Command) -> Self {
        match c {
            Command::Prompt { text, attachments } => Self::Prompt {
                text,
                attachments: attachments
                    .iter()
                    .map(|a| core_cmd::Attachment::from_bytes(&a.media_type, &a.data))
                    .collect(),
            },
            Command::Interrupt => Self::Interrupt,
            Command::Compact { hint } => Self::Compact { hint },
            Command::New => Self::New,
            Command::Fork => Self::Fork,
            Command::Rename { name } => Self::Rename { name },
            Command::Review => Self::Review,
            Command::Shell { command } => Self::Shell { command },
            Command::SetOption { id, value } => Self::SetOption { id: id.into(), value },
            Command::AnswerPermission {
                request_id,
                option_id,
            } => Self::AnswerPermission {
                request_id,
                option_id,
            },
            Command::Invoke { name, args } => Self::Invoke { name, args },
            Command::Refresh => Self::Refresh,
            Command::Handoff { harness, prompt } => Self::Handoff {
                harness: harness.into(),
                prompt,
            },
        }
    }
}

/// How a slash command reaches the harness.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum Exec {
    Text,
    Rpc {
        method: String,
    },
    Endpoint {
        method: String,
        path: String,
    },
    SkillItem,
    /// Nothing on the wire runs it; open the harness's TUI (`tui_command`).
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CommandSource {
    Builtin,
    Custom,
    Skill,
    Mcp,
    Plugin,
    Acc,
}

/// One entry in a session's command palette.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CommandEntry {
    pub name: String,
    pub description: String,
    pub arg_hint: Option<String>,
    pub source: CommandSource,
    pub exec: Exec,
}

impl From<&core_cmd::CommandEntry> for CommandEntry {
    fn from(c: &core_cmd::CommandEntry) -> Self {
        Self {
            name: c.name.clone(),
            description: c.description.clone(),
            arg_hint: c.arg_hint.clone(),
            source: match c.source {
                core_cmd::CommandSource::Builtin => CommandSource::Builtin,
                core_cmd::CommandSource::Custom => CommandSource::Custom,
                core_cmd::CommandSource::Skill => CommandSource::Skill,
                core_cmd::CommandSource::Mcp => CommandSource::Mcp,
                core_cmd::CommandSource::Plugin => CommandSource::Plugin,
                core_cmd::CommandSource::Acc => CommandSource::Acc,
            },
            exec: match &c.exec {
                core_cmd::Exec::Text => Exec::Text,
                core_cmd::Exec::Rpc { method } => Exec::Rpc {
                    method: method.clone(),
                },
                core_cmd::Exec::Endpoint { method, path } => Exec::Endpoint {
                    method: method.clone(),
                    path: path.clone(),
                },
                core_cmd::Exec::SkillItem => Exec::SkillItem,
                core_cmd::Exec::Terminal => Exec::Terminal,
            },
        }
    }
}

/// When a changed option takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Apply {
    Live,
    NextTurn,
    Restart,
}

/// One configurable option and its allowed values.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OptionDef {
    pub id: OptionId,
    pub values: Vec<String>,
    pub current: Option<String>,
    pub apply: Apply,
}

impl From<&core_cmd::OptionDef> for OptionDef {
    fn from(o: &core_cmd::OptionDef) -> Self {
        Self {
            id: o.id.into(),
            values: o.values.clone(),
            current: o.current.clone(),
            apply: match o.apply {
                core_cmd::Apply::Live => Apply::Live,
                core_cmd::Apply::NextTurn => Apply::NextTurn,
                core_cmd::Apply::Restart => Apply::Restart,
            },
        }
    }
}

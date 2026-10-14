//! FFI mirrors of acc-core types, with conversions.
//!
//! Harness-native JSON (`extensions`, tool input) crosses as JSON strings, so
//! UIs can light up harness-specific features without the bindings knowing
//! every shape.

use acc_core::command::{
    Apply, Attachment, Choice, Command, CommandEntry, CommandInput, CommandSource, Exec as CmdExec,
    OptionDef, OptionId,
};
use acc_core::detached::{FeedEntry, FeedPath, Listing, Status};
use acc_core::event::{
    Event, ItemKind, Panel, PanelAction, PanelRow, PanelSection, PermissionOption, Quota, QuotaWindow, Usage,
};
use acc_core::harness::{Availability, Compat, Launcher};
use acc_core::item::{DiffLine, FileChangeKind, FileDiff, Hunk, ItemDetail, LineTag};
use acc_core::session::{LaunchRequest, Origin, Outbound, Permission, SessionSpec, Update};
use acc_core::state::{BlockKind, RunState, SessionState, StateSource};
use acc_core::transcript::{Item, ItemChange, ItemStatus};
use acc_core::{Exec, Harness};
use serde_json::Value;

fn json(v: &Value) -> String {
    if v.is_null() { String::new() } else { v.to_string() }
}

macro_rules! mirror_enum {
    ($(#[$m:meta])* $ffi:ident <=> $core:ty { $($v:ident),* $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
        pub enum $ffi { $($v),* }
        impl From<$core> for $ffi {
            fn from(v: $core) -> Self { match v { $(<$core>::$v => $ffi::$v),* } }
        }
        impl From<$ffi> for $core {
            fn from(v: $ffi) -> Self { match v { $($ffi::$v => <$core>::$v),* } }
        }
    };
}

mirror_enum!(
    /// A supported agent harness.
    AccHarness <=> Harness { Claude, Codex, Cursor, Copilot, Pi, Opencode }
);
mirror_enum!(AccBlockKind <=> BlockKind { Permission, Question, Auth });
mirror_enum!(AccStateSource <=> StateSource { Heuristic, ScreenMatch, Transcript, Osc7501, Native });
mirror_enum!(AccItemKind <=> ItemKind { UserMessage, AgentMessage, Reasoning, ToolCall, CommandExecution, FileChange, Subagent });
mirror_enum!(AccItemStatus <=> ItemStatus { Running, Completed, Failed });
mirror_enum!(AccOptionId <=> OptionId { Model, Effort, Mode, Permission });
mirror_enum!(AccApply <=> Apply { Live, NextTurn, Restart });
mirror_enum!(AccCommandSource <=> CommandSource { Builtin, Custom, Skill, Mcp, Plugin, Acc });
mirror_enum!(AccFileChangeKind <=> FileChangeKind { Add, Delete, Update, Rename });
mirror_enum!(AccLineTag <=> LineTag { Context, Add, Remove });
mirror_enum!(
    /// A harness version against the range ACC was tested with.
    AccCompat <=> Compat { Tested, Newer, Older, Unknown }
);

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccRunState {
    Idle,
    Working { progress: Option<u8> },
    Blocked { kind: AccBlockKind },
    Done,
    Waiting,
    Error,
    Exited,
}

impl From<RunState> for AccRunState {
    fn from(s: RunState) -> Self {
        match s {
            RunState::Idle => Self::Idle,
            RunState::Working { progress } => Self::Working { progress },
            RunState::Blocked { kind } => Self::Blocked { kind: kind.into() },
            RunState::Done => Self::Done,
            RunState::Waiting => Self::Waiting,
            RunState::Error => Self::Error,
            RunState::Exited => Self::Exited,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccSessionState {
    pub state: AccRunState,
    pub message: Option<String>,
    pub source: AccStateSource,
    pub observed_at_ms: u64,
}

impl From<SessionState> for AccSessionState {
    fn from(s: SessionState) -> Self {
        Self {
            state: s.state.into(),
            message: s.message,
            source: s.source.into(),
            observed_at_ms: s.observed_at_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccPermissionOption {
    pub id: String,
    pub label: String,
}

impl From<PermissionOption> for AccPermissionOption {
    fn from(o: PermissionOption) -> Self {
        Self {
            id: o.id,
            label: o.label,
        }
    }
}

/// A request the harness is waiting on the user for.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccPermission {
    pub request_id: String,
    pub title: String,
    pub options: Vec<AccPermissionOption>,
    /// `Question`: options are suggestions and any text is a valid answer.
    pub kind: AccBlockKind,
    /// The tool item this request is about.
    pub item_id: Option<String>,
    /// Tool input or request parameters as JSON (empty when none).
    pub input_json: String,
}

impl From<Permission> for AccPermission {
    fn from(p: Permission) -> Self {
        Self {
            request_id: p.request_id,
            title: p.title,
            options: p.options.into_iter().map(Into::into).collect(),
            kind: p.kind.into(),
            item_id: p.item_id,
            input_json: json(&p.input),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, uniffi::Record)]
pub struct AccUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub context_used: Option<u64>,
    pub context_window: Option<u64>,
}

impl From<Usage> for AccUsage {
    fn from(u: Usage) -> Self {
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

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct AccQuotaWindow {
    pub id: String,
    pub used_percent: Option<f64>,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<u64>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct AccQuota {
    pub account: Option<String>,
    pub plan: Option<String>,
    pub windows: Vec<AccQuotaWindow>,
}

impl From<Quota> for AccQuota {
    fn from(q: Quota) -> Self {
        Self {
            account: q.account,
            plan: q.plan,
            windows: q
                .windows
                .into_iter()
                .map(|w: QuotaWindow| AccQuotaWindow {
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

/// One thing that happened in a session.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum AccEvent {
    SessionStarted {
        harness: AccHarness,
        native_id: String,
        model: Option<String>,
    },
    StateChanged {
        state: AccSessionState,
    },
    ItemStarted {
        id: String,
        kind: AccItemKind,
        /// The `Subagent` item this one runs inside, for child conversations.
        parent: Option<String>,
        extensions_json: String,
    },
    ItemDelta {
        id: String,
        text: String,
    },
    ItemUpdated {
        id: String,
        extensions_json: String,
    },
    ItemCompleted {
        id: String,
        extensions_json: String,
    },
    PermissionRequested {
        permission: AccPermission,
    },
    PermissionResolved {
        request_id: String,
    },
    Usage {
        usage: AccUsage,
    },
    Quota {
        quota: AccQuota,
    },
    CommandsChanged,
    OptionsChanged,
    Notice {
        message: String,
    },
    /// A dialog to draw; replaces an open panel with the same id.
    Panel {
        panel: AccPanel,
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
        payload_json: String,
    },
}

impl From<Event> for AccEvent {
    fn from(e: Event) -> Self {
        match e {
            Event::SessionStarted {
                harness,
                native_id,
                model,
            } => Self::SessionStarted {
                harness: harness.into(),
                native_id,
                model,
            },
            Event::StateChanged(s) => Self::StateChanged { state: s.into() },
            Event::ItemStarted {
                id,
                kind,
                parent,
                extensions,
            } => Self::ItemStarted {
                id,
                kind: kind.into(),
                parent,
                extensions_json: json(&extensions),
            },
            Event::ItemDelta { id, text } => Self::ItemDelta { id, text },
            Event::ItemUpdated { id, extensions } => Self::ItemUpdated {
                id,
                extensions_json: json(&extensions),
            },
            Event::ItemCompleted { id, extensions } => Self::ItemCompleted {
                id,
                extensions_json: json(&extensions),
            },
            Event::PermissionRequested {
                request_id,
                title,
                options,
                kind,
                item_id,
                input,
            } => Self::PermissionRequested {
                permission: Permission {
                    request_id,
                    title,
                    options,
                    kind,
                    item_id,
                    input,
                }
                .into(),
            },
            Event::PermissionResolved { request_id } => Self::PermissionResolved { request_id },
            Event::Usage(u) => Self::Usage { usage: u.into() },
            Event::Quota(q) => Self::Quota { quota: q.into() },
            Event::CommandsChanged => Self::CommandsChanged,
            Event::OptionsChanged => Self::OptionsChanged,
            Event::Notice { message } => Self::Notice { message },
            Event::Panel(panel) => Self::Panel { panel: panel.into() },
            Event::TurnCompleted => Self::TurnCompleted,
            Event::Exited { code } => Self::Exited { code },
            Event::Unrecognized { raw } => Self::Unrecognized { raw },
            Event::Native { kind, payload } => Self::Native {
                kind,
                payload_json: json(&payload),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccItemChange {
    Started { id: String },
    Delta { id: String, text: String },
    Updated { id: String },
    Completed { id: String },
}

impl From<ItemChange> for AccItemChange {
    fn from(c: ItemChange) -> Self {
        match c {
            ItemChange::Started { id } => Self::Started { id },
            ItemChange::Delta { id, text } => Self::Delta { id, text },
            ItemChange::Updated { id } => Self::Updated { id },
            ItemChange::Completed { id } => Self::Completed { id },
        }
    }
}

/// One event and what it changed in the transcript.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct AccUpdate {
    pub event: AccEvent,
    pub item: Option<AccItemChange>,
    /// History being replayed while attaching: don't notify about it.
    pub replay: bool,
}

impl From<Update> for AccUpdate {
    fn from(u: Update) -> Self {
        Self {
            event: u.event.into(),
            item: u.item.map(Into::into),
            replay: u.replay,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccDiffLine {
    pub tag: AccLineTag,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccHunk {
    /// 1-based; 0 when the position in the file is unknown.
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<AccDiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccFileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub kind: AccFileChangeKind,
    pub hunks: Vec<AccHunk>,
    pub added: u32,
    pub removed: u32,
}

impl From<FileDiff> for AccFileDiff {
    fn from(f: FileDiff) -> Self {
        Self {
            path: f.path,
            old_path: f.old_path,
            kind: f.kind.into(),
            hunks: f
                .hunks
                .into_iter()
                .map(|h: Hunk| AccHunk {
                    old_start: h.old_start,
                    old_lines: h.old_lines,
                    new_start: h.new_start,
                    new_lines: h.new_lines,
                    lines: h
                        .lines
                        .into_iter()
                        .map(|l: DiffLine| AccDiffLine {
                            tag: l.tag.into(),
                            text: l.text,
                        })
                        .collect(),
                })
                .collect(),
            added: f.added,
            removed: f.removed,
        }
    }
}

/// What an item is, in a harness-independent shape.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccItemDetail {
    /// A message or reasoning; the item's text is the content.
    Text,
    Tool {
        name: String,
        title: String,
        input_json: String,
        output: Option<String>,
    },
    Command {
        command: String,
        cwd: Option<String>,
        output: String,
        exit_code: Option<i32>,
    },
    FileChange {
        files: Vec<AccFileDiff>,
    },
    /// A run of another agent. Its child conversation is `children(id)`.
    Subagent {
        /// Agent type or name (Claude `subagent_type`, opencode agent, Codex nickname).
        agent: Option<String>,
        description: String,
        prompt: Option<String>,
        /// The harness's id for the child conversation.
        child: Option<String>,
        /// The harness's own status word (`running`, `completed`, ...).
        status: Option<String>,
        /// The final report, or the latest summary while it runs.
        output: Option<String>,
        model: Option<String>,
    },
}

impl From<ItemDetail> for AccItemDetail {
    fn from(d: ItemDetail) -> Self {
        match d {
            ItemDetail::Text => Self::Text,
            ItemDetail::Subagent {
                agent,
                description,
                prompt,
                child,
                status,
                output,
                model,
            } => Self::Subagent {
                agent,
                description,
                prompt,
                child,
                status,
                output,
                model,
            },
            ItemDetail::Tool {
                name,
                title,
                input,
                output,
            } => Self::Tool {
                name,
                title,
                input_json: json(&input),
                output,
            },
            ItemDetail::Command {
                command,
                cwd,
                output,
                exit_code,
            } => Self::Command {
                command,
                cwd,
                output,
                exit_code,
            },
            ItemDetail::FileChange { files } => Self::FileChange {
                files: files.into_iter().map(Into::into).collect(),
            },
        }
    }
}

/// One message, reasoning block, tool call, command or file change.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccItem {
    pub id: String,
    pub kind: AccItemKind,
    pub status: AccItemStatus,
    pub text: String,
    pub detail: AccItemDetail,
    /// The merged harness-native extensions as JSON.
    pub ext_json: String,
    pub parent: Option<String>,
    pub seq: u64,
}

pub fn item(i: &Item, h: Harness) -> AccItem {
    AccItem {
        id: i.id.clone(),
        kind: i.kind.into(),
        status: i.status.into(),
        text: i.text.clone(),
        detail: i.detail(h).into(),
        ext_json: json(&i.ext),
        parent: i.parent.clone(),
        seq: i.seq,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccOptionDef {
    pub id: AccOptionId,
    pub values: Vec<String>,
    pub current: Option<String>,
    pub apply: AccApply,
}

impl From<&OptionDef> for AccOptionDef {
    fn from(o: &OptionDef) -> Self {
        Self {
            id: o.id.into(),
            values: o.values.clone(),
            current: o.current.clone(),
            apply: o.apply.into(),
        }
    }
}

/// One entry in a session's command palette.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccCommandEntry {
    pub name: String,
    pub description: String,
    pub arg_hint: Option<String>,
    pub source: AccCommandSource,
    /// How it runs: `text`, `rpc`, `endpoint`, `skill` or `terminal`
    /// (only the harness's own TUI can run it).
    pub exec: String,
    /// What to ask for first; the answer is `Invoke`'s `args`.
    pub input: AccCommandInput,
}

/// What a palette entry needs from the user.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccCommandInput {
    /// Free text as `arg_hint` describes it, or nothing.
    None,
    /// A value of this option, picked from `options()`.
    Option { id: AccOptionId },
    /// One of these values.
    Choice { choices: Vec<AccChoice>, required: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccChoice {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

impl From<&Choice> for AccChoice {
    fn from(c: &Choice) -> Self {
        Self {
            value: c.value.clone(),
            label: c.label.clone(),
            description: c.description.clone(),
        }
    }
}

impl From<&CommandInput> for AccCommandInput {
    fn from(i: &CommandInput) -> Self {
        match i {
            CommandInput::None => Self::None,
            CommandInput::Option { id } => Self::Option { id: (*id).into() },
            CommandInput::Choice { choices, required } => Self::Choice {
                choices: choices.iter().map(Into::into).collect(),
                required: *required,
            },
        }
    }
}

/// A dialog's content: sections of rows, each with optional actions.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccPanel {
    /// The palette entry it answers, e.g. `mcp`.
    pub id: String,
    pub title: String,
    pub sections: Vec<AccPanelSection>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccPanelSection {
    pub title: Option<String>,
    pub rows: Vec<AccPanelRow>,
    pub actions: Vec<AccPanelAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccPanelRow {
    pub label: String,
    pub value: Option<String>,
    pub detail: Option<String>,
    pub actions: Vec<AccPanelAction>,
}

/// A button: send `AccCommand::Invoke { name: command, args }`. With
/// `prompt`, ask for text first and append it to `args` after a space.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccPanelAction {
    pub label: String,
    pub command: String,
    pub args: String,
    pub prompt: Option<String>,
    pub destructive: bool,
}

impl From<PanelAction> for AccPanelAction {
    fn from(a: PanelAction) -> Self {
        Self {
            label: a.label,
            command: a.command,
            args: a.args,
            prompt: a.prompt,
            destructive: a.destructive,
        }
    }
}

impl From<PanelRow> for AccPanelRow {
    fn from(r: PanelRow) -> Self {
        Self {
            label: r.label,
            value: r.value,
            detail: r.detail,
            actions: r.actions.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<PanelSection> for AccPanelSection {
    fn from(s: PanelSection) -> Self {
        Self {
            title: s.title,
            rows: s.rows.into_iter().map(Into::into).collect(),
            actions: s.actions.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<Panel> for AccPanel {
    fn from(p: Panel) -> Self {
        Self {
            id: p.id,
            title: p.title,
            sections: p.sections.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<&CommandEntry> for AccCommandEntry {
    fn from(c: &CommandEntry) -> Self {
        Self {
            name: c.name.clone(),
            description: c.description.clone(),
            arg_hint: c.arg_hint.clone(),
            source: c.source.into(),
            exec: match c.exec {
                CmdExec::Text => "text",
                CmdExec::Rpc { .. } => "rpc",
                CmdExec::Endpoint { .. } => "endpoint",
                CmdExec::SkillItem => "skill",
                CmdExec::Terminal => "terminal",
            }
            .to_owned(),
            input: (&c.input).into(),
        }
    }
}

/// An image sent with a prompt: raw bytes and their type (`image/png`,
/// `image/jpeg`, `image/gif`, `image/webp`). At most 5 MiB each and 10 MiB per
/// prompt, else `AccError::AttachmentRejected`; resize larger photos first.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccAttachment {
    pub media_type: String,
    pub data: Vec<u8>,
}

impl From<AccAttachment> for Attachment {
    fn from(a: AccAttachment) -> Self {
        Attachment::from_bytes(a.media_type, &a.data)
    }
}

impl From<Attachment> for AccAttachment {
    fn from(a: Attachment) -> Self {
        Self {
            data: a.bytes(),
            media_type: a.media_type,
        }
    }
}

/// Commands ACC offers on every harness.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccCommand {
    Prompt {
        text: String,
        /// Images sent with the text (see [`AccAttachment`]).
        attachments: Vec<AccAttachment>,
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
        id: AccOptionId,
        value: String,
    },
    AnswerPermission {
        request_id: String,
        option_id: String,
    },
    /// Runs a palette entry by name.
    Invoke {
        name: String,
        args: String,
    },
    Refresh,
}

impl From<AccCommand> for Command {
    fn from(c: AccCommand) -> Self {
        match c {
            AccCommand::Prompt { text, attachments } => Command::Prompt {
                text,
                attachments: attachments.into_iter().map(Into::into).collect(),
            },
            AccCommand::Interrupt => Command::Interrupt,
            AccCommand::Compact { hint } => Command::Compact { hint },
            AccCommand::New => Command::New,
            AccCommand::Fork => Command::Fork,
            AccCommand::Rename { name } => Command::Rename { name },
            AccCommand::Review => Command::Review,
            AccCommand::Shell { command } => Command::Shell { command },
            AccCommand::SetOption { id, value } => Command::SetOption { id: id.into(), value },
            AccCommand::AnswerPermission {
                request_id,
                option_id,
            } => Command::AnswerPermission {
                request_id,
                option_id,
            },
            AccCommand::Invoke { name, args } => Command::Invoke { name, args },
            AccCommand::Refresh => Command::Refresh,
        }
    }
}

/// A POSIX `sh` script to run on the host over an SSH exec channel, with
/// bytes to write to its stdin before closing it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccExec {
    pub script: String,
    pub stdin: Vec<u8>,
}

impl From<Exec> for AccExec {
    fn from(e: Exec) -> Self {
        Self {
            script: e.script,
            stdin: e.stdin,
        }
    }
}

impl AccExec {
    pub fn script(script: String) -> Self {
        Self {
            script,
            stdin: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccOrigin {
    Started,
    TakenOver {
        pane: Option<String>,
        transcript: Option<String>,
    },
}

impl From<Origin> for AccOrigin {
    fn from(o: Origin) -> Self {
        match o {
            Origin::Started => Self::Started,
            Origin::TakenOver { pane, transcript } => Self::TakenOver { pane, transcript },
        }
    }
}

impl From<AccOrigin> for Origin {
    fn from(o: AccOrigin) -> Self {
        match o {
            AccOrigin::Started => Self::Started,
            AccOrigin::TakenOver { pane, transcript } => Self::TakenOver { pane, transcript },
        }
    }
}

/// How a TUI the user started was launched: the wrapper's argv up to the
/// harness (a `bwrap` sandbox, `env`, ...), the harness executable as the
/// wrapper ran it, and the user's flags to keep.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccLauncher {
    pub prefix: Vec<String>,
    pub program: Option<String>,
    pub args: Vec<String>,
}

impl From<Launcher> for AccLauncher {
    fn from(l: Launcher) -> Self {
        Self {
            prefix: l.prefix,
            program: l.program,
            args: l.args,
        }
    }
}

impl From<AccLauncher> for Launcher {
    fn from(l: AccLauncher) -> Self {
        Self {
            prefix: l.prefix,
            program: l.program,
            args: l.args,
        }
    }
}

/// What to start.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccLaunchRequest {
    pub harness: AccHarness,
    /// Absolute for Cursor, Copilot and opencode (see `resolve_dir_exec`).
    pub cwd: String,
    pub model: Option<String>,
    /// Claude permission mode, or Codex permission profile.
    pub permission: Option<String>,
    pub resume: Option<String>,
    pub fork: bool,
    pub title: Option<String>,
    pub prompt: Option<String>,
    /// Images sent with `prompt`.
    #[uniffi(default = [])]
    pub attachments: Vec<AccAttachment>,
    pub parent: Option<String>,
    pub replaces_parent: bool,
    pub origin: AccOrigin,
    /// Start inside a taken-over TUI's wrapper (sandbox) with its flags.
    #[uniffi(default = None)]
    pub launcher: Option<AccLauncher>,
}

impl From<AccLaunchRequest> for LaunchRequest {
    fn from(r: AccLaunchRequest) -> Self {
        Self {
            harness: Some(r.harness.into()),
            cwd: r.cwd,
            model: r.model,
            permission: r.permission,
            resume: r.resume,
            fork: r.fork,
            title: r.title,
            prompt: r.prompt,
            attachments: r.attachments.into_iter().map(Into::into).collect(),
            parent: r.parent,
            replaces_parent: r.replaces_parent,
            origin: r.origin.into(),
            launcher: r.launcher.map(Into::into),
        }
    }
}

impl From<LaunchRequest> for AccLaunchRequest {
    fn from(r: LaunchRequest) -> Self {
        Self {
            harness: r.harness.unwrap_or(Harness::Claude).into(),
            cwd: r.cwd,
            model: r.model,
            permission: r.permission,
            resume: r.resume,
            fork: r.fork,
            title: r.title,
            prompt: r.prompt,
            attachments: r.attachments.into_iter().map(Into::into).collect(),
            parent: r.parent,
            replaces_parent: r.replaces_parent,
            origin: r.origin.into(),
            launcher: r.launcher.map(Into::into),
        }
    }
}

/// A session's `spec.json`: what any client needs to attach.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccSessionSpec {
    pub id: String,
    pub harness: AccHarness,
    pub cwd: String,
    pub model: Option<String>,
    pub permission: Option<String>,
    pub native_id: Option<String>,
    pub title: Option<String>,
    pub created_ms: u64,
    pub parent: Option<String>,
    pub origin: AccOrigin,
    /// Set when the harness runs in a taken-over TUI's wrapper (sandbox).
    pub launcher: Option<AccLauncher>,
    /// The full spec, to persist and pass back to `AccSession.attach`.
    pub json: String,
}

impl From<&SessionSpec> for AccSessionSpec {
    fn from(s: &SessionSpec) -> Self {
        Self {
            id: s.id.clone(),
            harness: s.harness.into(),
            cwd: s.cwd.clone(),
            model: s.model.clone(),
            permission: s.permission.clone(),
            native_id: s.native_id.clone(),
            title: s.title.clone(),
            created_ms: s.created_ms,
            parent: s.parent.clone(),
            origin: s.origin.clone().into(),
            launcher: s.launcher.clone().map(Into::into),
            json: s.to_json(),
        }
    }
}

// Built once per action; boxing a launch request isn't worth the churn.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccOutbound {
    /// Write these bytes with the session's `input_exec` (may be empty).
    Send { bytes: Vec<u8> },
    /// Launch this instead; stop this session first if `replaces_parent`.
    Relaunch { request: AccLaunchRequest },
}

impl From<Outbound> for AccOutbound {
    fn from(o: Outbound) -> Self {
        match o {
            Outbound::Send(bytes) => Self::Send { bytes },
            Outbound::Relaunch(r) => Self::Relaunch { request: r.into() },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccStatus {
    Alive { pid: u32, size: u64 },
    Exited { code: Option<i32>, size: u64 },
    Missing,
}

impl From<Status> for AccStatus {
    fn from(s: Status) -> Self {
        match s {
            Status::Alive { pid, size } => Self::Alive { pid, size },
            Status::Exited { code, size } => Self::Exited { code, size },
            Status::Missing => Self::Missing,
        }
    }
}

/// A detached session found on a host.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccListing {
    pub id: String,
    pub status: AccStatus,
    /// Last log write, seconds since the Unix epoch.
    pub modified: Option<u64>,
    pub spec: Option<AccSessionSpec>,
}

impl From<Listing> for AccListing {
    fn from(l: Listing) -> Self {
        Self {
            spec: l
                .spec_json
                .as_deref()
                .and_then(SessionSpec::from_json)
                .map(|s| (&s).into()),
            id: l.id,
            status: l.status.into(),
            modified: l.modified,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccFeedPath {
    SessionLog {
        id: String,
    },
    SessionExit {
        id: String,
    },
    /// Any file, such as a harness transcript (`~/` is expanded).
    File {
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccFeedEntry {
    pub key: String,
    pub path: AccFeedPath,
    pub offset: u64,
}

impl From<AccFeedEntry> for FeedEntry {
    fn from(e: AccFeedEntry) -> Self {
        Self {
            key: e.key,
            path: match e.path {
                AccFeedPath::SessionLog { id } => FeedPath::SessionLog(id),
                AccFeedPath::SessionExit { id } => FeedPath::SessionExit(id),
                AccFeedPath::File { path } => FeedPath::File(path),
            },
            offset: e.offset,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccFeedLine {
    pub key: String,
    /// The line's bytes, newline included.
    pub bytes: Vec<u8>,
    /// File offset just past this line.
    pub offset: u64,
}

/// A harness installed on a host.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccAvailability {
    pub harness: AccHarness,
    pub path: String,
    pub version: Option<String>,
    pub compat: AccCompat,
}

impl From<Availability> for AccAvailability {
    fn from(a: Availability) -> Self {
        Self {
            harness: a.harness.into(),
            path: a.path,
            version: a.version,
            compat: a.compat.into(),
        }
    }
}

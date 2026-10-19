//! Claude Code driver.
//!
//! Speaks the stream-json control protocol of
//! `claude -p --input-format stream-json --output-format stream-json`, the
//! same one the Claude Agent SDK uses. Every stdout line is one JSON message:
//! conversation frames (`system`, `assistant`, `user`, `stream_event`,
//! `result`, `rate_limit_event`) and `control_request` / `control_response`
//! envelopes going both ways. Checked against Claude Code 2.1.296 and the
//! Agent SDK 0.3.296 type definitions; the fixtures under
//! `tests/fixtures/claude` are recorded from that version.
//!
//! What maps where:
//! - State: `system/session_state_changed` (enabled by
//!   `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1` in the launch spec) is the
//!   authoritative idle/running/requires_action signal; `result` marks done or
//!   error. Without it (older CLIs) state is derived from prompts and results.
//! - Permissions: `can_use_tool` control requests, which the CLI only sends
//!   with `--permission-prompt-tool stdio`. `AskUserQuestion` arrives the same
//!   way and is answered by allowing it with the chosen answer filled in.
//! - Commands: the palette comes from the `initialize` reply and
//!   `system/commands_changed`; slash commands run by sending `/name args` as
//!   a user message. Model, effort, permission mode and rename use dedicated
//!   control requests. `/plan`, which Claude only runs in its TUI and never
//!   lists headless, is `set_permission_mode` plus `get_plan`.
//! - Usage: `result` (tokens, cost), `get_context_usage` (context fill) and
//!   `rate_limit_event` (subscription windows).

use std::collections::{HashMap, VecDeque};

use serde_json::{Value, json};

use crate::command::{
    Apply, Attachment, Choice, Command, CommandEntry, CommandInput, CommandSource, Exec, OptionDef, OptionId,
    offer_option, split_verb, validate_attachments,
};
use crate::driver::{Driver, DriverError};
use crate::event::{
    Event, ItemKind, Panel, PanelAction, PanelRow, PanelSection, PermissionOption, PermissionOptionKind,
    Quota, QuotaWindow, Usage,
};
use crate::harness::{Harness, LaunchSpec};
use crate::item::ItemFeed;
use crate::ndjson::{Frame, LineFramer};
use crate::state::{BlockKind, RunState, SessionState, StateSource};
use crate::wire::{self, f64_at, owned, str_at, u64_at};

/// Permission modes Claude accepts at runtime. `bypassPermissions` only works
/// when the process was started with a bypass flag, so it is offered only
/// when it is already the current mode.
const PERMISSION_MODES: [&str; 5] = ["default", "acceptEdits", "plan", "auto", "dontAsk"];

/// Commands only Claude's TUI runs, as 2.1.296 reports them in
/// `system/init`. Assumed until the first `init` gives the real list.
const TERMINAL_COMMANDS: [&str; 4] = ["doctor", "color", "focus", "reload-plugins"];

/// Dialogs ACC draws itself from a control request: palette name,
/// description (when Claude doesn't list the command) and request subtype.
const PANELS: [(&str, &str, &str); 5] = [
    (
        "mcp",
        "MCP servers: status, reconnect, enable or disable",
        "mcp_status",
    ),
    (
        "status",
        "Version, session, account and environment",
        "get_status",
    ),
    (
        "permissions",
        "Allow, ask and deny rules",
        "list_permission_rules",
    ),
    ("memory", "Memory files and auto-memory", "get_memory_dialog"),
    ("hooks", "Configured hooks", "get_hooks_listing"),
];

/// `/plan` as Claude's TUI describes it (2.1.280: `[open|<description>]`,
/// `requires: { ink }`). Headless it is neither listed nor accepted as text,
/// so ACC runs it itself: `set_permission_mode` into plan mode, the
/// description as the first prompt, and `get_plan` to show the plan.
const PLAN_DESCRIPTION: &str = "Enable plan mode or view the current session plan";

/// Shown for a user message that is only images.
const IMAGE_PLACEHOLDER: &str = "[image]";

/// Launch-time settings. Everything else changes at runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClaudeConfig {
    /// `--model` at spawn.
    pub model: Option<String>,
    /// `--permission-mode` at spawn.
    pub permission_mode: Option<String>,
    /// With a resume id: start a new session id from that conversation
    /// (`--fork-session`), which is how ACC forks a Claude session.
    pub fork: bool,
    /// Prefix for this client's control request ids. Clients sharing one
    /// detached session see each other's responses in the shared log, so
    /// each needs its own (`[A-Za-z0-9_-]`). Defaults to `acc`.
    pub client_tag: Option<String>,
}

/// What a control request ACC sent was for, so its reply can be applied.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum Pending {
    Initialize,
    SetModel(String),
    SetEffort(String),
    SetPermission(String),
    /// `/plan`: entering plan mode. Once Claude confirms, `prompt` goes out
    /// as the first planning turn and `view` fetches the plan panel.
    Plan {
        prompt: Option<String>,
        view: bool,
    },
    /// A dialog's data, drawn as the panel for this palette entry.
    Panel(#[serde(deserialize_with = "crate::wire::static_str")] crate::wire::StaticStr),
    /// An action from a panel; the panel is fetched again afterwards.
    PanelAction(#[serde(deserialize_with = "crate::wire::static_str")] crate::wire::StaticStr),
    /// `reload_plugins`, which answers with the new palette.
    Reload,
    Other,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PendingPermission {
    tool_use_id: Option<String>,
    input: Value,
    suggestions: Value,
    /// For `AskUserQuestion`: the question text the answer is keyed by.
    question: Option<String>,
}

/// Content blocks of one assistant message that started streaming but whose
/// final `assistant` frame hasn't arrived yet, in stream order.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct OpenMessage {
    blocks: VecDeque<String>,
    seen: usize,
}

/// Claude Code stream-json driver. See the module docs.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ClaudeDriver {
    config: ClaudeConfig,
    /// Every item the harness reported, for their typed detail.
    items: ItemFeed,
    framer: LineFramer,
    session_id: Option<String>,
    model: Option<String>,
    state: Option<RunState>,
    native_state_events: bool,
    next_request: u64,
    pending: HashMap<String, Pending>,
    permissions: HashMap<String, PendingPermission>,
    open: HashMap<String, OpenMessage>,
    /// The message streaming under each parent tool call (`""` for the top
    /// level): newer CLIs name it only in `message_start`.
    #[serde(default)]
    streaming: HashMap<String, String>,
    /// Tool-use items waiting for their result, with their kind.
    open_tools: HashMap<String, ItemKind>,
    /// Background tasks (subagents, shells, monitors) by task id: the
    /// `task_*` frames merged, so each update carries the whole record.
    tasks: HashMap<String, Value>,
    /// Subagents running in the background, from `background_tasks_changed`:
    /// each one wakes the agent for another turn when it reports back.
    background_agents: usize,
    /// Usage of the latest top-level assistant message (context fill).
    last_context: Option<u64>,
    context_window: Option<u64>,
    models: Vec<Value>,
    hidden_commands: Vec<String>,
    skills: Vec<String>,
    /// From the `initialize` reply, for the `/output-style` picker.
    output_styles: Vec<String>,
    /// The palette as Claude last listed it.
    listed_commands: Vec<Value>,
    commands: Vec<CommandEntry>,
    options: Vec<OptionDef>,
    transmit: Vec<u8>,
}

impl Default for ClaudeDriver {
    fn default() -> Self {
        Self::new(ClaudeConfig::default())
    }
}

impl ClaudeDriver {
    pub fn new(config: ClaudeConfig) -> Self {
        let mut d = Self {
            config,
            items: ItemFeed::new(Harness::Claude),
            framer: LineFramer::new(wire::MAX_FRAME),
            session_id: None,
            model: None,
            state: None,
            native_state_events: false,
            next_request: 0,
            pending: HashMap::new(),
            permissions: HashMap::new(),
            open: HashMap::new(),
            streaming: HashMap::new(),
            open_tools: HashMap::new(),
            tasks: HashMap::new(),
            background_agents: 0,
            last_context: None,
            context_window: None,
            models: Vec::new(),
            hidden_commands: TERMINAL_COMMANDS.iter().map(|c| (*c).to_owned()).collect(),
            skills: Vec::new(),
            output_styles: Vec::new(),
            listed_commands: Vec::new(),
            commands: Vec::new(),
            options: Vec::new(),
            transmit: Vec::new(),
        };
        d.rebuild_options(None, None);
        d.set_commands(&[]);
        d
    }

    /// The Claude session id, once the harness has reported it.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn control(&mut self, request: Value, kind: Pending) -> Vec<u8> {
        self.next_request += 1;
        let tag = self.config.client_tag.as_deref().unwrap_or("acc");
        let id = format!("{tag}-{}", self.next_request);
        self.pending.insert(id.clone(), kind);
        wire::line(&json!({"type": "control_request", "request_id": id, "request": request}))
    }

    fn user_text(text: &str) -> Vec<u8> {
        Self::user_content(Value::String(text.to_owned()))
    }

    /// A user message with images: Anthropic content blocks. The API refuses
    /// an empty text block, so an image-only prompt carries none.
    fn user_blocks(text: &str, attachments: &[Attachment]) -> Vec<u8> {
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
        for a in attachments {
            content.push(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": a.media_type, "data": a.data},
            }));
        }
        Self::user_content(Value::Array(content))
    }

    fn user_content(content: Value) -> Vec<u8> {
        wire::line(&json!({
            "type": "user",
            "message": {"role": "user", "content": content},
            "parent_tool_use_id": null,
            "session_id": "",
        }))
    }

    fn set_state(&mut self, state: RunState, message: Option<String>, out: &mut Vec<Event>) {
        if self.state.as_ref() == Some(&state) && message.is_none() {
            return;
        }
        self.state = Some(state.clone());
        out.push(Event::StateChanged(SessionState {
            state,
            message,
            source: StateSource::Native,
            observed_at_ms: 0,
        }));
    }

    fn handle(&mut self, v: Value, raw: &str, out: &mut Vec<Event>) {
        match str_at(&v, "type") {
            Some("system") => self.on_system(&v, raw, out),
            Some("stream_event") => self.on_stream_event(&v, out),
            Some("assistant") => self.on_assistant(&v, out),
            Some("user") => self.on_user(&v, out),
            Some("result") => self.on_result(&v, out),
            Some("rate_limit_event") => self.on_rate_limit(&v, out),
            Some("control_request") => self.on_control_request(&v, out),
            Some("control_response") => self.on_control_response(&v, out),
            Some("control_cancel_request") => {
                if let Some(id) = owned(&v, "request_id")
                    && self.permissions.remove(&id).is_some()
                {
                    out.push(Event::PermissionResolved { request_id: id });
                }
            }
            Some("conversation_reset") => {
                if let Some(id) = owned(&v, "new_conversation_id") {
                    self.session_id = Some(id.clone());
                    out.push(Event::SessionStarted {
                        harness: Harness::Claude,
                        native_id: id,
                        model: self.model.clone(),
                    });
                }
            }
            // Bookkeeping frames with nothing for a client.
            Some(
                "active_goal"
                | "autocompact_state"
                | "keep_alive"
                | "tool_progress"
                | "auth_status"
                | "streamlined_text"
                | "streamlined_tool_use_summary",
            ) => {}
            _ => out.push(Event::Unrecognized { raw: raw.to_owned() }),
        }
    }

    fn on_system(&mut self, v: &Value, raw: &str, out: &mut Vec<Event>) {
        match str_at(v, "subtype") {
            Some("init") => {
                let id = owned(v, "session_id");
                let model = owned(v, "model");
                if let Some(names) = v.get("terminal_slash_commands").and_then(Value::as_array) {
                    let hidden: Vec<String> = names
                        .iter()
                        .filter_map(|n| n.as_str().map(str::to_owned))
                        .collect();
                    if hidden != self.hidden_commands {
                        self.hidden_commands = hidden;
                        let listed = std::mem::take(&mut self.listed_commands);
                        self.set_commands(&listed);
                        out.push(Event::CommandsChanged);
                    }
                }
                if let Some(names) = v.get("skills").and_then(Value::as_array) {
                    self.skills = names
                        .iter()
                        .filter_map(|n| n.as_str().map(str::to_owned))
                        .collect();
                }
                // Without the rich `initialize` list, fall back to bare names.
                if self.listed_commands.is_empty()
                    && let Some(names) = v.get("slash_commands").and_then(Value::as_array)
                {
                    let list: Vec<Value> = names
                        .iter()
                        .filter_map(|n| n.as_str().map(|s| json!({"name": s, "description": ""})))
                        .collect();
                    self.set_commands(&list);
                    out.push(Event::CommandsChanged);
                }
                let changed = id != self.session_id || model != self.model;
                self.model = model.clone().or(self.model.take());
                let permission = owned(v, "permissionMode");
                let effort = owned(v, "effort");
                self.rebuild_options(permission, effort);
                out.push(Event::OptionsChanged);
                if changed && let Some(id) = id {
                    self.session_id = Some(id.clone());
                    out.push(Event::SessionStarted {
                        harness: Harness::Claude,
                        native_id: id,
                        model,
                    });
                }
            }
            Some("session_state_changed") => {
                self.native_state_events = true;
                match str_at(v, "state") {
                    Some("running") => self.set_state(RunState::Working { progress: None }, None, out),
                    Some("requires_action") => {
                        let kind = self.block_kind();
                        self.set_state(RunState::Blocked { kind }, None, out);
                    }
                    // `done`/`error`/`waiting` from the result stay visible until the next turn.
                    Some("idle")
                        if !matches!(
                            self.state,
                            Some(RunState::Done | RunState::Waiting | RunState::Error)
                        ) =>
                    {
                        self.set_state(RunState::Idle, None, out);
                    }
                    _ => {}
                }
            }
            Some("status") => {
                if let Some(mode) = owned(v, "permissionMode") {
                    self.set_option_current(OptionId::Permission, mode);
                    out.push(Event::OptionsChanged);
                }
                if str_at(v, "status") == Some("compacting") {
                    self.set_state(
                        RunState::Working { progress: None },
                        Some("Compacting".into()),
                        out,
                    );
                }
            }
            Some("api_retry") => {
                let attempt = u64_at(v, "attempt").unwrap_or(0);
                let max = u64_at(v, "max_retries").unwrap_or(0);
                self.set_state(
                    RunState::Working { progress: None },
                    Some(format!("Retrying API request ({attempt}/{max})")),
                    out,
                );
            }
            Some("commands_changed") => {
                if let Some(list) = v.get("commands").and_then(Value::as_array) {
                    self.set_commands(list);
                    out.push(Event::CommandsChanged);
                }
            }
            Some("compact_boundary") => {
                let pre = v.pointer("/compact_metadata/pre_tokens").and_then(Value::as_u64);
                out.push(Event::Notice {
                    message: match pre {
                        Some(n) => format!("Conversation compacted ({n} tokens before)"),
                        None => "Conversation compacted".into(),
                    },
                });
            }
            Some("local_command_output") => {
                if let Some(text) = str_at(v, "content") {
                    out.push(Event::Notice {
                        message: text.to_owned(),
                    });
                }
            }
            Some("task_started" | "task_progress" | "task_updated" | "task_notification") => {
                self.on_task(v, out)
            }
            Some("background_tasks_changed") => {
                let tasks = v.get("tasks").cloned().unwrap_or_else(|| json!([]));
                self.background_agents = tasks.as_array().map_or(0, |t| {
                    t.iter()
                        .filter(|t| str_at(t, "task_type").is_some_and(|k| k.ends_with("_agent")))
                        .count()
                });
                // The last one's report is queued for the model, which takes
                // it as a new turn without a `running` state of its own.
                if self.background_agents == 0 && self.state == Some(RunState::Waiting) {
                    self.set_state(RunState::Working { progress: None }, None, out);
                }
                out.push(Event::Native {
                    kind: "background_tasks".into(),
                    payload: json!({"tasks": tasks}),
                });
            }
            Some(
                "post_turn_summary" | "task_summary" | "hook_started" | "hook_progress" | "hook_response"
                | "files_persisted" | "informational" | "memory_recall" | "thinking_tokens" | "notification",
            ) => {}
            _ => out.push(Event::Unrecognized { raw: raw.to_owned() }),
        }
    }

    fn block_kind(&self) -> BlockKind {
        if self.permissions.values().any(|p| p.question.is_some()) {
            BlockKind::Question
        } else {
            BlockKind::Permission
        }
    }

    /// A background task's lifecycle (`task_started`, `task_progress`,
    /// `task_updated` with a patch, `task_notification` at the end). Tasks
    /// behind a tool call (subagents) update that item under `task`; the
    /// rest (ambient monitors, background shells) pass through as native
    /// events. A subagent launched in the background stays running until
    /// its notification, whatever its tool result said.
    fn on_task(&mut self, v: &Value, out: &mut Vec<Event>) {
        let Some(task_id) = owned(v, "task_id") else {
            return;
        };
        let subtype = str_at(v, "subtype").unwrap_or_default().to_owned();
        let patch = if subtype == "task_updated" {
            v.get("patch").cloned().unwrap_or(Value::Null)
        } else {
            v.clone()
        };
        let task = self
            .tasks
            .entry(task_id.clone())
            .or_insert_with(|| json!({"task_id": task_id}));
        if let (Some(t), Some(p)) = (task.as_object_mut(), patch.as_object()) {
            for (k, val) in p {
                if matches!(k.as_str(), "type" | "subtype" | "uuid" | "session_id") || val.is_null() {
                    continue;
                }
                t.insert(k.clone(), val.clone());
            }
            if subtype == "task_started" {
                t.insert("status".into(), json!("running"));
            }
        }
        let task = task.clone();
        let done = subtype == "task_notification";
        match owned(&task, "tool_use_id") {
            Some(tool) if done && self.open_tools.remove(&tool).is_some() => {
                let failed = matches!(str_at(&task, "status"), Some("failed" | "stopped"));
                self.items
                    .complete(tool, json!({"task": task, "is_error": failed}), out);
            }
            Some(tool) => self.items.update(tool, json!({"task": task}), out),
            None => out.push(Event::Native {
                kind: subtype.clone(),
                payload: task,
            }),
        }
        if done {
            self.tasks.remove(&task_id);
        }
    }

    fn on_stream_event(&mut self, v: &Value, out: &mut Vec<Event>) {
        let Some(event) = v.get("event") else { return };
        let parent = parent_of(v).unwrap_or_default();
        let started = event
            .pointer("/message/id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(id) = &started {
            self.streaming.insert(parent.clone(), id.clone());
        }
        let msg_id = owned(v, "api_message_id")
            .or(started)
            .or_else(|| self.streaming.get(&parent).cloned());
        if str_at(event, "type") == Some("message_stop") {
            self.streaming.remove(&parent);
        }
        let Some(msg_id) = msg_id else { return };
        let index = u64_at(event, "index").unwrap_or(0);
        let item_id = format!("{msg_id}:{index}");
        match str_at(event, "type") {
            Some("content_block_start") => {
                let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                let open = self.open.entry(msg_id.clone()).or_default();
                match str_at(&block, "type") {
                    // Tool calls are announced with their full input once the
                    // `assistant` frame arrives; reserve their slot here.
                    Some("tool_use" | "server_tool_use") => {
                        open.blocks.push_back(owned(&block, "id").unwrap_or(item_id));
                    }
                    Some(kind @ ("text" | "thinking")) => {
                        open.blocks.push_back(item_id.clone());
                        let kind = if kind == "text" {
                            ItemKind::AgentMessage
                        } else {
                            ItemKind::Reasoning
                        };
                        self.items.start(item_id, kind, parent_of(v), Value::Null, out);
                    }
                    _ => open.blocks.push_back(item_id),
                }
            }
            Some("content_block_delta") => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                let text = match str_at(delta, "type") {
                    Some("text_delta") => str_at(delta, "text"),
                    Some("thinking_delta") => str_at(delta, "thinking"),
                    _ => None,
                };
                if let Some(text) = text
                    && !text.is_empty()
                {
                    self.items.delta(item_id, text.to_owned(), out);
                }
            }
            _ => {}
        }
    }

    fn on_assistant(&mut self, v: &Value, out: &mut Vec<Event>) {
        let Some(message) = v.get("message") else { return };
        let msg_id = owned(message, "id").unwrap_or_default();
        let top_level = v.get("parent_tool_use_id").is_none_or(Value::is_null);
        if top_level && let Some(usage) = message.get("usage") {
            self.last_context = wire::sum(
                usage,
                &[
                    "input_tokens",
                    "cache_read_input_tokens",
                    "cache_creation_input_tokens",
                    "output_tokens",
                ],
            );
        }
        let blocks = message
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for block in blocks {
            let streamed = self.open.get_mut(&msg_id).and_then(|o| o.blocks.pop_front());
            let open = self.open.entry(msg_id.clone()).or_default();
            let fallback = format!("{msg_id}:{}", open.seen);
            open.seen += 1;
            match str_at(&block, "type") {
                Some("text" | "thinking") => {
                    let kind = if str_at(&block, "type") == Some("text") {
                        ItemKind::AgentMessage
                    } else {
                        ItemKind::Reasoning
                    };
                    let id = match streamed {
                        Some(id) => id,
                        None => {
                            // Not streamed (no partial messages): announce it whole.
                            let text = str_at(&block, "text").or_else(|| str_at(&block, "thinking"));
                            self.items
                                .start(fallback.clone(), kind, parent_of(v), Value::Null, out);
                            if let Some(text) = text {
                                self.items.delta(fallback.clone(), text.to_owned(), out);
                            }
                            fallback
                        }
                    };
                    self.items.complete(id, Value::Null, out);
                }
                Some("tool_use" | "server_tool_use") => {
                    let id = owned(&block, "id").or(streamed).unwrap_or(fallback);
                    let name = str_at(&block, "name").unwrap_or_default();
                    let ext =
                        json!({"name": name, "input": block.get("input").cloned().unwrap_or(Value::Null)});
                    let kind = tool_kind(name);
                    self.open_tools.insert(id.clone(), kind);
                    self.items.start(id, kind, parent_of(v), ext, out);
                }
                // A server tool's result (`web_search_tool_result`, ...).
                Some(kind) if kind.ends_with("_tool_result") => {
                    if let Some(id) = owned(&block, "tool_use_id")
                        && self.open_tools.remove(&id).is_some()
                    {
                        self.items.complete(
                            id,
                            json!({"content": block.get("content").cloned().unwrap_or(Value::Null)}),
                            out,
                        );
                    }
                }
                _ => {}
            }
        }
        // Kept until the turn ends: the message's next block may come in its
        // own frame and must not reuse this one's id.
    }

    fn on_user(&mut self, v: &Value, out: &mut Vec<Event>) {
        let content = v.pointer("/message/content").cloned().unwrap_or(Value::Null);
        if let Some(blocks) = content.as_array() {
            let mut text = String::new();
            let mut images = 0;
            for block in blocks {
                match str_at(block, "type") {
                    Some("tool_result") => {
                        let Some(id) = owned(block, "tool_use_id") else {
                            continue;
                        };
                        self.resolve_tool(&id, out);
                        let Some(kind) = self.open_tools.get(&id).copied() else {
                            continue;
                        };
                        let mut ext = json!({
                            "is_error": block.get("is_error").cloned().unwrap_or(Value::Bool(false)),
                            "content": block.get("content").cloned().unwrap_or(Value::Null),
                        });
                        // The structured result names the agent (`agentId`,
                        // `agentType`, usage) and whether it only launched.
                        let result = v.get("tool_use_result").filter(|r| r.is_object());
                        if kind == ItemKind::Subagent
                            && let Some(result) = result
                        {
                            ext["result"] = result.clone();
                        }
                        let launched =
                            result.is_some_and(|r| r.get("isAsync").and_then(Value::as_bool) == Some(true));
                        if launched {
                            // Still running: its `task_notification` completes it.
                            self.items.update(id, ext, out);
                        } else {
                            self.open_tools.remove(&id);
                            self.items.complete(id, ext, out);
                        }
                    }
                    Some("text") => text.push_str(str_at(block, "text").unwrap_or_default()),
                    Some("image") => images += 1,
                    _ => {}
                }
            }
            if !text.is_empty() {
                self.user_message(v, &text, out);
            } else if images > 0 {
                self.user_message(v, IMAGE_PLACEHOLDER, out);
            }
        } else if let Some(text) = content.as_str() {
            if text.trim_start().starts_with("<task-notification>") {
                // A background task's report, fed to the model as a user
                // message; the `task_notification` frame already closed it.
                return;
            }
            if let Some(output) = local_command_output(text) {
                out.push(Event::Notice {
                    message: output.to_owned(),
                });
            } else {
                self.user_message(v, text, out);
            }
        }
    }

    fn user_message(&mut self, v: &Value, text: &str, out: &mut Vec<Event>) {
        if v.get("isSynthetic").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let id = owned(v, "uuid").unwrap_or_else(|| {
            self.next_request += 1;
            format!("user-{}", self.next_request)
        });
        let parent = parent_of(v);
        // Older CLIs without session-state frames: a prompt starts a turn.
        // A subagent's prompt is the parent's tool call at work, not a turn.
        if !self.native_state_events && parent.is_none() {
            self.set_state(RunState::Working { progress: None }, None, out);
        }
        self.items
            .start(id.clone(), ItemKind::UserMessage, parent, Value::Null, out);
        self.items.delta(id.clone(), text.to_owned(), out);
        self.items.complete(id, Value::Null, out);
    }

    fn on_result(&mut self, v: &Value, out: &mut Vec<Event>) {
        // Messages of the finished turn; a background subagent's may still stream.
        self.open.retain(|_, o| !o.blocks.is_empty());
        let usage = v.get("usage").cloned().unwrap_or(Value::Null);
        if let Some(window) = self
            .model
            .as_deref()
            .and_then(|m| v.pointer(&format!("/modelUsage/{}/contextWindow", escape_pointer(m))))
            .and_then(Value::as_u64)
        {
            self.context_window = Some(window);
        }
        out.push(Event::Usage(Usage {
            input_tokens: u64_at(&usage, "input_tokens"),
            output_tokens: u64_at(&usage, "output_tokens"),
            cache_read_tokens: u64_at(&usage, "cache_read_input_tokens"),
            cache_write_tokens: u64_at(&usage, "cache_creation_input_tokens"),
            cost_usd: f64_at(v, "total_cost_usd"),
            context_used: self.last_context,
            context_window: self.context_window,
        }));
        // A turn can't end while a prompt is open, so anything left was
        // answered or withdrawn in a way the log doesn't show.
        let mut left: Vec<String> = self.permissions.drain().map(|(id, _)| id).collect();
        left.sort_unstable();
        out.extend(
            left.into_iter()
                .map(|request_id| Event::PermissionResolved { request_id }),
        );
        let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
            || str_at(v, "subtype").is_some_and(|s| s != "success");
        if is_error {
            let message = v
                .get("errors")
                .and_then(Value::as_array)
                .and_then(|e| e.first())
                .and_then(Value::as_str)
                .or_else(|| str_at(v, "result"))
                .or_else(|| str_at(v, "subtype"))
                .map(|m| wire::one_line(m, 200));
            self.set_state(RunState::Error, message, out);
        } else if self.background_agents > 0 {
            self.set_state(RunState::Waiting, None, out);
        } else {
            self.set_state(RunState::Done, None, out);
        }
        out.push(Event::TurnCompleted);
    }

    fn on_rate_limit(&mut self, v: &Value, out: &mut Vec<Event>) {
        let Some(info) = v.get("rate_limit_info") else {
            return;
        };
        let status = owned(info, "status");
        let mut windows = Vec::new();
        if let Some(unified) = info.get("unifiedWindows").and_then(Value::as_object) {
            for (id, w) in unified {
                windows.push(QuotaWindow {
                    id: id.clone(),
                    used_percent: f64_at(w, "utilization").map(fraction_to_percent),
                    window_minutes: window_minutes(id),
                    resets_at: u64_at(w, "resetsAt"),
                    status: (Some(id.as_str()) == str_at(info, "rateLimitType"))
                        .then(|| status.clone())
                        .flatten(),
                });
            }
        } else if let Some(id) = owned(info, "rateLimitType") {
            windows.push(QuotaWindow {
                window_minutes: window_minutes(&id),
                used_percent: f64_at(info, "utilization").map(fraction_to_percent),
                resets_at: u64_at(info, "resetsAt"),
                status,
                id,
            });
        }
        out.push(Event::Quota(Quota {
            account: None,
            plan: None,
            windows,
        }));
    }

    fn on_control_request(&mut self, v: &Value, out: &mut Vec<Event>) {
        let Some(id) = owned(v, "request_id") else { return };
        let req = v.get("request").cloned().unwrap_or(Value::Null);
        match str_at(&req, "subtype") {
            Some("can_use_tool") => self.permission_request(id, &req, out),
            // ACC registers no hooks, SDK MCP servers or dialogs, so anything
            // else is answered with an error rather than left hanging.
            other => {
                let error = format!("ACC does not handle {}", other.unwrap_or("this request"));
                self.transmit.extend(wire::line(&json!({
                    "type": "control_response",
                    "response": {"subtype": "error", "request_id": id, "error": error},
                })));
            }
        }
    }

    fn permission_request(&mut self, id: String, req: &Value, out: &mut Vec<Event>) {
        let tool = str_at(req, "tool_name").unwrap_or("tool");
        let input = req.get("input").cloned().unwrap_or(Value::Null);
        let suggestions = req.get("permission_suggestions").cloned().unwrap_or(Value::Null);
        let (title, options, question) = if tool == "AskUserQuestion" {
            let q = input.pointer("/questions/0").cloned().unwrap_or(Value::Null);
            let question = owned(&q, "question").unwrap_or_default();
            let options: Vec<PermissionOption> = q
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .filter_map(|o| str_at(o, "label"))
                        .map(|l| PermissionOption::new(l.to_owned(), l.to_owned()))
                        .collect()
                })
                .unwrap_or_default();
            (question.clone(), options, Some(question))
        } else {
            let name = str_at(req, "display_name").unwrap_or(tool);
            let detail = str_at(req, "title")
                .or_else(|| str_at(&input, "command"))
                .or_else(|| str_at(&input, "file_path"))
                .or_else(|| str_at(req, "description"));
            let title = match detail {
                Some(d) => format!("{name}: {}", wire::one_line(d, 200)),
                None => name.to_owned(),
            };
            let mut options =
                vec![PermissionOption::new("allow", "Allow").with_kind(PermissionOptionKind::AllowOnce)];
            let always = suggestions.as_array().is_some_and(|s| !s.is_empty())
                && req.get("suppress_always_allow_rule").and_then(Value::as_bool) != Some(true);
            if always {
                options.push(
                    PermissionOption::new("allow_always", "Always allow")
                        .with_kind(PermissionOptionKind::AllowAlways),
                );
            }
            options.push(PermissionOption::new("deny", "Deny").with_kind(PermissionOptionKind::RejectOnce));
            (title, options, None)
        };
        let is_question = question.is_some();
        let item_id = owned(req, "tool_use_id");
        let detail = (!is_question).then(|| {
            self.items.preview(
                item_id.as_deref(),
                tool_kind(tool),
                &json!({"name": tool, "input": input}),
            )
        });
        let preview = input.clone();
        self.permissions.insert(
            id.clone(),
            PendingPermission {
                tool_use_id: owned(req, "tool_use_id"),
                input,
                suggestions,
                question,
            },
        );
        out.push(Event::PermissionRequested {
            request_id: id,
            title,
            options,
            kind: if is_question {
                BlockKind::Question
            } else {
                BlockKind::Permission
            },
            item_id,
            detail,
            native: preview,
        });
        if !self.native_state_events {
            let kind = if is_question {
                BlockKind::Question
            } else {
                BlockKind::Permission
            };
            self.set_state(RunState::Blocked { kind }, None, out);
        }
    }

    fn on_control_response(&mut self, v: &Value, out: &mut Vec<Event>) {
        let Some(resp) = v.get("response") else { return };
        let id = str_at(resp, "request_id").unwrap_or_default();
        let kind = self.pending.remove(id).unwrap_or(Pending::Other);
        if str_at(resp, "subtype") == Some("error") {
            if !self.is_ours(id) {
                return;
            }
            let error = str_at(resp, "error").unwrap_or("request failed");
            out.push(Event::Notice {
                message: format!("Claude Code: {error}"),
            });
            return;
        }
        let body = resp.get("response").cloned().unwrap_or(Value::Null);
        // Another client's (or an earlier connection's) `initialize` reply,
        // read from a shared log: adopt the palette and options it carries.
        let kind = match kind {
            Pending::Other if body.get("commands").is_some() && body.get("models").is_some() => {
                Pending::Initialize
            }
            k => k,
        };
        match kind {
            Pending::Initialize => {
                if let Some(styles) = body.get("available_output_styles").and_then(Value::as_array) {
                    self.output_styles = styles
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect();
                }
                if let Some(list) = body.get("commands").and_then(Value::as_array) {
                    self.set_commands(list);
                    out.push(Event::CommandsChanged);
                }
                if let Some(models) = body.get("models").and_then(Value::as_array) {
                    self.models.clone_from(models);
                }
                self.rebuild_options(owned(&body, "current_permission_mode"), None);
                out.push(Event::OptionsChanged);
                if str_at(&body, "session_state") == Some("idle") && self.state.is_none() {
                    self.set_state(RunState::Idle, None, out);
                }
                // Prompts already waiting when ACC attached to a running process.
                if let Some(pending) = body.get("pending_permission_requests").and_then(Value::as_array) {
                    for p in pending.clone() {
                        if let (Some(rid), Some(req)) = (owned(&p, "request_id"), p.get("request")) {
                            self.permission_request(rid, req, out);
                        }
                    }
                }
            }
            Pending::SetModel(model) => {
                self.model = Some(model.clone());
                self.set_option_current(OptionId::Model, model);
                self.rebuild_options(None, None);
                out.push(Event::OptionsChanged);
            }
            Pending::SetEffort(level) => {
                self.set_option_current(OptionId::Effort, level);
                out.push(Event::OptionsChanged);
            }
            Pending::SetPermission(mode) => {
                self.set_option_current(OptionId::Permission, mode);
                out.push(Event::OptionsChanged);
            }
            Pending::Plan { prompt, view } => {
                self.set_option_current(OptionId::Permission, "plan".into());
                out.push(Event::OptionsChanged);
                out.push(Event::Notice {
                    message: "Entered plan mode".into(),
                });
                if let Some(prompt) = prompt {
                    self.transmit.extend(Self::user_text(&prompt));
                }
                if view {
                    let bytes = self.open_panel("plan");
                    self.transmit.extend(bytes);
                }
            }
            Pending::Panel(id) => out.push(Event::Panel(panel(id, &body))),
            Pending::PanelAction(id) => {
                let bytes = self.open_panel(id);
                self.transmit.extend(bytes);
            }
            Pending::Reload => {
                if let Some(list) = body.get("commands").and_then(Value::as_array) {
                    self.set_commands(list);
                    out.push(Event::CommandsChanged);
                }
                out.push(Event::Notice {
                    message: "Reloaded plugins".into(),
                });
            }
            Pending::Other => {
                if let (Some(used), Some(max)) = (u64_at(&body, "totalTokens"), u64_at(&body, "maxTokens")) {
                    self.last_context = Some(used);
                    self.context_window = Some(max);
                    out.push(Event::Usage(Usage {
                        context_used: Some(used),
                        context_window: Some(max),
                        ..Usage::default()
                    }));
                }
                if let Some(models) = body.get("models").and_then(Value::as_array) {
                    self.models.clone_from(models);
                    self.rebuild_options(None, None);
                    out.push(Event::OptionsChanged);
                }
            }
        }
    }

    /// Whether a control request id was minted by this client.
    fn is_ours(&self, id: &str) -> bool {
        let tag = self.config.client_tag.as_deref().unwrap_or("acc");
        id.strip_prefix(tag).is_some_and(|rest| rest.starts_with('-'))
    }

    /// A permission is over once its tool reports a result (allowed and run,
    /// or denied). Answers travel on stdin, so a shared log or a replay only
    /// shows the outcome; this is how every client learns of it.
    fn resolve_tool(&mut self, tool_use_id: &str, out: &mut Vec<Event>) {
        let done: Vec<String> = self
            .permissions
            .iter()
            .filter(|(_, p)| p.tool_use_id.as_deref() == Some(tool_use_id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in done {
            self.permissions.remove(&id);
            out.push(Event::PermissionResolved { request_id: id });
        }
    }

    fn set_commands(&mut self, list: &[Value]) {
        self.listed_commands = list.to_vec();
        let mut entries: Vec<CommandEntry> = list
            .iter()
            .filter_map(|c| {
                let name = str_at(c, "name")?.trim_start_matches('/').to_owned();
                if self.hidden_commands.contains(&name) {
                    return None;
                }
                let source = if c.get("builtin").and_then(Value::as_bool) == Some(true) {
                    CommandSource::Builtin
                } else if name.starts_with("mcp__") || name.ends_with("(MCP)") {
                    CommandSource::Mcp
                } else if self.skills.contains(&name) {
                    CommandSource::Skill
                } else if name.contains(':') {
                    CommandSource::Plugin
                } else {
                    CommandSource::Custom
                };
                Some(CommandEntry {
                    description: owned(c, "description").unwrap_or_default(),
                    arg_hint: owned(c, "argumentHint").filter(|h| !h.is_empty()),
                    source,
                    exec: Exec::Text,
                    input: CommandInput::None,
                    name,
                })
            })
            .collect();
        // Commands whose TUI dialog has a control request behind it run
        // that instead and answer with a panel.
        for (name, description, subtype) in PANELS {
            match entries.iter_mut().find(|e| e.name == *name) {
                Some(e) => {
                    e.exec = Exec::Rpc {
                        method: (*subtype).into(),
                    }
                }
                None => entries.push(CommandEntry::rpc(name, description, None, subtype)),
            }
        }
        // `/plan` is TUI-only and unlisted headless; ACC runs it over the
        // protocol (see `plan`). Listed in a future version, it is taken over.
        let plan = CommandEntry::rpc(
            "plan",
            PLAN_DESCRIPTION,
            Some("[<description>]"),
            "set_permission_mode",
        );
        match entries.iter_mut().find(|e| e.name == "plan") {
            Some(e) => {
                e.exec = plan.exec;
                e.arg_hint = plan.arg_hint;
            }
            None => entries.push(plan),
        }
        // Terminal-only commands with a control request behind them.
        if self.hidden_commands.iter().any(|c| c == "reload-plugins") {
            entries.push(CommandEntry::rpc(
                "reload-plugins",
                "Reload plugins, skills and MCP servers",
                None,
                "reload_plugins",
            ));
        }
        // The rest need Claude's own TUI.
        for name in &self.hidden_commands {
            if !entries.iter().any(|e| &e.name == name) {
                entries.push(CommandEntry {
                    name: name.clone(),
                    description: "Opens in Claude Code's terminal UI".into(),
                    arg_hint: None,
                    source: CommandSource::Builtin,
                    exec: Exec::Terminal,
                    input: CommandInput::None,
                });
            }
        }
        offer_option(
            &mut entries,
            &self.options,
            OptionId::Model,
            CommandEntry::rpc("model", "Switch model", Some("<model>"), "set_model"),
        );
        offer_option(
            &mut entries,
            &self.options,
            OptionId::Effort,
            CommandEntry::rpc(
                "effort",
                "Set reasoning effort",
                Some("<level>"),
                "apply_flag_settings",
            ),
        );
        offer_option(
            &mut entries,
            &self.options,
            OptionId::Permission,
            CommandEntry::rpc(
                "mode",
                "Permission mode: plan, accept edits, auto, ...",
                Some("<mode>"),
                "set_permission_mode",
            ),
        );
        if let Some(e) = entries.iter_mut().find(|e| e.name == "output-style")
            && !self.output_styles.is_empty()
        {
            e.input = CommandInput::Choice {
                choices: self.output_styles.iter().map(Choice::new).collect(),
                required: false,
            };
        }
        if let Some(e) = entries.iter_mut().find(|e| e.name == "rename") {
            e.exec = Exec::Rpc {
                method: "rename_session".into(),
            };
        }
        entries.push(CommandEntry::rpc(
            "interrupt",
            "Stop the current turn",
            None,
            "interrupt",
        ));
        self.commands = entries;
    }

    /// Runs a palette entry. Entries backed by a control request use it;
    /// the rest go to Claude as `/name args`.
    fn invoke(&mut self, name: &str, args: &str) -> Result<Vec<u8>, DriverError> {
        let name = name.trim_start_matches('/');
        let args = args.trim();
        let set = |id| Command::SetOption {
            id,
            value: args.to_owned(),
        };
        let entry = self.commands.iter().find(|e| e.name == name);
        if entry.is_some_and(|e| e.exec == Exec::Terminal) {
            return Err(DriverError::Unsupported {
                harness: Harness::Claude,
                what: "this command outside its terminal UI",
            });
        }
        Ok(match (name, args) {
            ("interrupt", _) => return self.command(Command::Interrupt),
            ("model", a) if !a.is_empty() => return self.command(set(OptionId::Model)),
            ("effort", a) if !a.is_empty() => return self.command(set(OptionId::Effort)),
            ("mode", "") => return Err(DriverError::MissingArgument("a permission mode")),
            ("mode", _) => return self.command(set(OptionId::Permission)),
            ("plan", a) => return self.plan(a),
            ("rename", a) if !a.is_empty() => {
                return self.command(Command::Rename { name: a.to_owned() });
            }
            ("reload-plugins", _) if entry.is_some_and(|e| e.exec != Exec::Text) => {
                self.control(json!({"subtype": "reload_plugins"}), Pending::Reload)
            }
            ("mcp", a) if !a.is_empty() => {
                let (verb, server) = split_verb(a);
                if server.is_empty() {
                    return Err(DriverError::MissingArgument("an MCP server name"));
                }
                let request = match verb {
                    "reconnect" => json!({"subtype": "mcp_reconnect", "serverName": server}),
                    "enable" | "disable" => {
                        json!({"subtype": "mcp_toggle", "serverName": server, "enabled": verb == "enable"})
                    }
                    _ => return Err(DriverError::InvalidOption(verb.to_owned())),
                };
                self.control(request, Pending::PanelAction("mcp"))
            }
            (n, _) if PANELS.iter().any(|(p, ..)| *p == n) => {
                let id = PANELS.iter().find(|(p, ..)| *p == n).map_or("", |(p, ..)| *p);
                self.open_panel(id)
            }
            _ => Self::user_text(&slash(name, args)),
        })
    }

    /// `/plan [open|<description>]`, as Claude's TUI runs it: outside plan
    /// mode it enters plan mode, then sends the description as the first
    /// prompt; in plan mode it shows the plan. `open` (the plan file in a
    /// local editor) shows the plan too; `share` (Ultraplan) has no headless
    /// equivalent.
    fn plan(&mut self, args: &str) -> Result<Vec<u8>, DriverError> {
        let planning = self
            .options
            .iter()
            .find(|o| o.id == OptionId::Permission)
            .and_then(|o| o.current.as_deref())
            == Some("plan");
        let view = args.is_empty() || args == "open";
        Ok(match (planning, view) {
            _ if args == "share" => {
                return Err(DriverError::Unsupported {
                    harness: Harness::Claude,
                    what: "sharing a plan with Ultraplan",
                });
            }
            (true, true) => self.open_panel("plan"),
            (true, false) => Self::user_text(args),
            (false, view) => self.control(
                json!({"subtype": "set_permission_mode", "mode": "plan"}),
                Pending::Plan {
                    prompt: (!view).then(|| args.to_owned()),
                    view: args == "open",
                },
            ),
        })
    }

    /// Asks for the data behind panel `id`.
    fn open_panel(&mut self, id: &'static str) -> Vec<u8> {
        let subtype = match id {
            "plan" => Some("get_plan"),
            _ => PANELS.iter().find(|(p, ..)| *p == id).map(|(.., s)| *s),
        };
        match subtype {
            Some(subtype) => self.control(json!({"subtype": subtype}), Pending::Panel(id)),
            None => Vec::new(),
        }
    }

    /// Rebuilds option definitions from the model list, keeping current values.
    fn rebuild_options(&mut self, permission: Option<String>, effort: Option<String>) {
        let current = |id| {
            self.options
                .iter()
                .find(|o| o.id == id)
                .and_then(|o| o.current.clone())
        };
        let permission = permission.or_else(|| current(OptionId::Permission));
        let effort = effort.or_else(|| current(OptionId::Effort));
        let model = self.model.clone().or_else(|| current(OptionId::Model));

        let model_values: Vec<String> = self.models.iter().filter_map(|m| owned(m, "value")).collect();
        // The CLI reports the resolved id (`claude-sonnet-5-5`); show the alias it came from.
        let model_info = model.as_deref().and_then(|m| {
            self.models
                .iter()
                .filter(|i| str_at(i, "value") == Some(m) || str_at(i, "resolvedModel") == Some(m))
                .min_by_key(|i| str_at(i, "value") == Some("default"))
        });
        let model_current = model_info.and_then(|i| owned(i, "value")).or(model);
        let efforts: Vec<String> = model_info
            .and_then(|i| i.get("supportedEffortLevels"))
            .and_then(Value::as_array)
            .map(|l| l.iter().filter_map(|e| e.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        let mut modes: Vec<String> = PERMISSION_MODES.iter().map(|m| (*m).to_owned()).collect();
        if permission.as_deref() == Some("bypassPermissions") {
            modes.push("bypassPermissions".into());
        }
        self.options = vec![
            OptionDef {
                id: OptionId::Model,
                values: model_values,
                labels: Vec::new(),
                current: model_current,
                apply: Apply::Live,
            },
            OptionDef {
                id: OptionId::Effort,
                values: efforts,
                labels: Vec::new(),
                current: effort,
                apply: Apply::Live,
            },
            OptionDef {
                id: OptionId::Permission,
                values: modes,
                labels: Vec::new(),
                current: permission,
                apply: Apply::Live,
            },
        ];
    }

    fn set_option_current(&mut self, id: OptionId, value: String) {
        if let Some(o) = self.options.iter_mut().find(|o| o.id == id) {
            o.current = Some(value);
        }
    }

    fn answer(&mut self, request_id: &str, option_id: &str) -> Result<Vec<u8>, DriverError> {
        let p = self
            .permissions
            .remove(request_id)
            .ok_or_else(|| DriverError::UnknownRequest(request_id.to_owned()))?;
        let mut result = match (option_id, &p.question) {
            ("deny", _) => json!({"behavior": "deny", "message": "The user denied this request."}),
            (_, Some(question)) => {
                let mut input = p.input.clone();
                let answers = crate::question::decode(option_id);
                let questions = crate::question::parse(&p.input);
                let mut map = serde_json::Map::new();
                if questions.is_empty() {
                    map.insert(question.clone(), json!(answers.concat().join(", ")));
                }
                // Several picks to one question are joined, as Claude's own UI does.
                for (q, a) in questions.iter().zip(&answers) {
                    if !a.is_empty() {
                        map.insert(q.question.clone(), json!(a.join(", ")));
                    }
                }
                input["answers"] = Value::Object(map);
                json!({"behavior": "allow", "updatedInput": input})
            }
            ("allow", None) => json!({"behavior": "allow", "updatedInput": p.input}),
            ("allow_always", None) => {
                json!({"behavior": "allow", "updatedInput": p.input, "updatedPermissions": p.suggestions})
            }
            (other, None) => {
                self.permissions.insert(request_id.to_owned(), p);
                return Err(DriverError::InvalidOption(other.to_owned()));
            }
        };
        if let Some(tool) = p.tool_use_id {
            result["toolUseID"] = Value::String(tool);
        }
        Ok(wire::line(&json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": request_id, "response": result},
        })))
    }

    fn validate(&self, id: OptionId, value: &str) -> Result<(), DriverError> {
        let ok = self
            .options
            .iter()
            .find(|o| o.id == id)
            .is_none_or(|o| o.values.is_empty() || o.values.iter().any(|v| v == value));
        if ok {
            Ok(())
        } else {
            Err(DriverError::InvalidOption(value.to_owned()))
        }
    }
}

impl Driver for ClaudeDriver {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    fn snapshot(&self) -> Option<Value> {
        serde_json::to_value(self).ok()
    }

    fn launch(&self, resume: Option<&str>) -> LaunchSpec {
        let mut spec = Harness::Claude.launch(resume);
        if let Some(model) = &self.config.model {
            spec.args.extend(["--model".to_owned(), model.clone()]);
        }
        if let Some(mode) = &self.config.permission_mode {
            spec.args.extend(["--permission-mode".to_owned(), mode.clone()]);
        }
        if self.config.fork && resume.is_some() {
            spec.args.push("--fork-session".to_owned());
        }
        spec
    }

    fn handshake(&mut self) -> Vec<u8> {
        self.control(json!({"subtype": "initialize"}), Pending::Initialize)
    }

    /// The palette and options come from the first client's `initialize`
    /// reply in the log; only usage and the model list are asked again.
    fn attach_handshake(&mut self) -> Vec<u8> {
        let mut bytes = self.control(json!({"subtype": "get_context_usage"}), Pending::Other);
        bytes.extend(self.control(json!({"subtype": "list_models"}), Pending::Other));
        bytes
    }

    fn on_output(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        for frame in self.framer.feed(bytes) {
            match frame {
                Frame::Line(line) => match serde_json::from_str::<Value>(&line) {
                    Ok(v) => self.handle(v, &line, &mut out),
                    Err(_) => out.push(Event::Unrecognized { raw: line }),
                },
                Frame::Oversized => out.push(Event::Unrecognized {
                    raw: "<oversized frame>".into(),
                }),
                Frame::InvalidUtf8 => out.push(Event::Unrecognized {
                    raw: "<invalid utf-8>".into(),
                }),
            }
        }
        out
    }

    fn command(&mut self, cmd: Command) -> Result<Vec<u8>, DriverError> {
        let unsupported = |what| DriverError::Unsupported {
            harness: Harness::Claude,
            what,
        };
        Ok(match cmd {
            Command::Prompt { text, attachments } => {
                validate_attachments(&attachments)?;
                if attachments.is_empty() {
                    Self::user_text(&text)
                } else {
                    Self::user_blocks(&text, &attachments)
                }
            }
            Command::Interrupt => self.control(json!({"subtype": "interrupt"}), Pending::Other),
            Command::Compact { hint } => Self::user_text(&slash("compact", hint.as_deref().unwrap_or(""))),
            Command::New => Self::user_text("/clear"),
            Command::Review => Self::user_text("/review"),
            Command::Invoke { name, args } => return self.invoke(&name, &args),
            Command::Rename { name } => self.control(
                json!({"subtype": "rename_session", "title": name}),
                Pending::Other,
            ),
            // A fork is a relaunch with `ClaudeConfig::fork` and the session id.
            Command::Fork => return Err(unsupported("fork without a relaunch")),
            Command::Shell { .. } => return Err(unsupported("shell commands")),
            Command::SetOption { id, value } => match id {
                OptionId::Model => {
                    let model = if value == "default" {
                        Value::Null
                    } else {
                        Value::String(value.clone())
                    };
                    self.control(
                        json!({"subtype": "set_model", "model": model}),
                        Pending::SetModel(value),
                    )
                }
                OptionId::Effort => {
                    self.validate(id, &value)?;
                    self.control(
                        json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": value}}),
                        Pending::SetEffort(value),
                    )
                }
                OptionId::Permission => {
                    self.validate(id, &value)?;
                    self.control(
                        json!({"subtype": "set_permission_mode", "mode": value}),
                        Pending::SetPermission(value),
                    )
                }
                OptionId::Mode => return Err(unsupported("a mode option (plan is a permission mode)")),
            },
            Command::AnswerPermission {
                request_id,
                option_id,
            } => self.answer(&request_id, &option_id)?,
            Command::Handoff { .. } => return Err(unsupported("handoff without a relaunch")),
            Command::Refresh => {
                let mut bytes = self.control(json!({"subtype": "get_context_usage"}), Pending::Other);
                bytes.extend(self.control(json!({"subtype": "list_models"}), Pending::Other));
                bytes
            }
        })
    }

    fn commands(&self) -> &[CommandEntry] {
        &self.commands
    }

    fn options(&self) -> &[OptionDef] {
        &self.options
    }

    fn poll_transmit(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.transmit)
    }
}

/// Draws a control response as panel `id`.
fn panel(id: &str, body: &Value) -> Panel {
    let list = |v: &Value, key: &str| -> Vec<Value> {
        v.get(key).and_then(Value::as_array).cloned().unwrap_or_default()
    };
    let text = |v: &Value, key: &str| owned(v, key).filter(|s| !s.is_empty());
    let mut panel = Panel {
        id: id.to_owned(),
        ..Panel::default()
    };
    match id {
        "mcp" => {
            panel.title = "MCP servers".into();
            let rows = list(body, "mcpServers")
                .iter()
                .filter_map(|s| {
                    let name = owned(s, "name")?;
                    let status = owned(s, "status").unwrap_or_default();
                    let kind = s.get("config").and_then(|c| owned(c, "type"));
                    let detail = [owned(s, "scope"), kind, text(s, "error")]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · ");
                    let toggle = if status == "disabled" { "enable" } else { "disable" };
                    let mut actions = vec![PanelAction::new("Reconnect", "mcp", format!("reconnect {name}"))];
                    actions.push(PanelAction {
                        destructive: toggle == "disable",
                        ..PanelAction::new(capitalize(toggle), "mcp", format!("{toggle} {name}"))
                    });
                    Some(PanelRow {
                        label: name,
                        value: Some(status),
                        detail: Some(detail).filter(|d| !d.is_empty()),
                        actions,
                    })
                })
                .collect();
            panel.sections.push(PanelSection {
                rows,
                ..PanelSection::default()
            });
        }
        "status" => {
            panel.title = "Status".into();
            panel.sections = list(body, "sections")
                .iter()
                .map(|sec| PanelSection {
                    title: text(sec, "title"),
                    rows: list(sec, "rows")
                        .iter()
                        .map(|r| PanelRow {
                            label: owned(r, "label").unwrap_or_default(),
                            value: owned(r, "value"),
                            ..PanelRow::default()
                        })
                        .collect(),
                    ..PanelSection::default()
                })
                .collect();
        }
        "permissions" => {
            panel.title = "Permission rules".into();
            let state = body.get("state").unwrap_or(body);
            let rules = list(state, "rules");
            for (behavior, title) in [("allow", "Allow"), ("ask", "Ask"), ("deny", "Deny")] {
                let rows: Vec<PanelRow> = rules
                    .iter()
                    .filter(|r| str_at(r, "behavior") == Some(behavior))
                    .map(|r| PanelRow {
                        label: owned(r, "rule").unwrap_or_default(),
                        value: owned(r, "source"),
                        detail: r.get("description").map(|d| {
                            ["prefix", "emphasis", "suffix"]
                                .iter()
                                .filter_map(|k| str_at(d, k))
                                .collect::<String>()
                        }),
                        actions: Vec::new(),
                    })
                    .collect();
                if !rows.is_empty() {
                    panel.sections.push(PanelSection {
                        title: Some(title.into()),
                        rows,
                        ..PanelSection::default()
                    });
                }
            }
            let dirs: Vec<PanelRow> = list(state, "workspaceDirectories")
                .iter()
                .filter_map(|d| {
                    Some(PanelRow {
                        label: owned(d, "path")?,
                        value: owned(d, "source"),
                        ..PanelRow::default()
                    })
                })
                .collect();
            if !dirs.is_empty() {
                panel.sections.push(PanelSection {
                    title: Some("Additional directories".into()),
                    rows: dirs,
                    ..PanelSection::default()
                });
            }
        }
        "memory" => {
            panel.title = "Memory".into();
            let files = list(body, "files")
                .iter()
                .filter_map(|f| {
                    Some(PanelRow {
                        label: owned(f, "label")?,
                        value: Some(if f.get("exists").and_then(Value::as_bool) == Some(true) {
                            "present".into()
                        } else {
                            "not created".into()
                        }),
                        detail: owned(f, "path"),
                        actions: Vec::new(),
                    })
                })
                .collect();
            panel.sections.push(PanelSection {
                title: Some("Files".into()),
                rows: files,
                ..PanelSection::default()
            });
            let auto: Vec<PanelRow> = [("auto_memory", "Auto-memory"), ("auto_dream", "Auto-dream")]
                .iter()
                .filter_map(|(key, label)| {
                    let v = body.get(*key)?;
                    Some(PanelRow {
                        label: (*label).into(),
                        value: owned(v, "status"),
                        detail: text(v, "detail"),
                        actions: Vec::new(),
                    })
                })
                .collect();
            if !auto.is_empty() {
                panel.sections.push(PanelSection {
                    rows: auto,
                    ..PanelSection::default()
                });
            }
        }
        "plan" => {
            panel.title = "Plan".into();
            let rows = match owned(body, "content").filter(|c| !c.trim().is_empty()) {
                Some(content) => plan_rows(&content),
                None => vec![PanelRow {
                    label: "No plan written yet".into(),
                    ..PanelRow::default()
                }],
            };
            panel.sections.push(PanelSection {
                title: text(body, "path"),
                rows,
                ..PanelSection::default()
            });
        }
        "hooks" => {
            panel.title = "Hooks".into();
            let hooks = list(body, "hooks");
            let mut events: Vec<String> = Vec::new();
            for h in &hooks {
                if let Some(e) = owned(h, "event")
                    && !events.contains(&e)
                {
                    events.push(e);
                }
            }
            panel.sections = events
                .into_iter()
                .map(|event| PanelSection {
                    rows: hooks
                        .iter()
                        .filter(|h| str_at(h, "event") == Some(event.as_str()))
                        .map(|h| PanelRow {
                            label: owned(h, "displayText").unwrap_or_default(),
                            value: text(h, "matcher"),
                            detail: owned(h, "sourceLabel"),
                            actions: Vec::new(),
                        })
                        .collect(),
                    title: Some(event),
                    actions: Vec::new(),
                })
                .collect();
        }
        _ => {}
    }
    panel
}

/// A plan's markdown as panel rows: one per heading, with the text under it
/// as the detail. Text before the first heading is the "Plan" row.
fn plan_rows(markdown: &str) -> Vec<PanelRow> {
    let mut rows: Vec<PanelRow> = Vec::new();
    let mut body = String::new();
    let mut label = "Plan".to_owned();
    let flush = |rows: &mut Vec<PanelRow>, label: &str, body: &mut String| {
        let detail = body.trim();
        if !detail.is_empty() || label != "Plan" {
            rows.push(PanelRow {
                label: label.to_owned(),
                detail: Some(detail.to_owned()).filter(|d| !d.is_empty()),
                ..PanelRow::default()
            });
        }
        body.clear();
    };
    let mut fenced = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        let heading = (!fenced)
            .then(|| line.strip_prefix('#'))
            .flatten()
            .map(|rest| rest.trim_start_matches('#'))
            .filter(|rest| rest.starts_with(' '))
            .map(str::trim);
        match heading {
            Some(h) => {
                flush(&mut rows, &label, &mut body);
                label = h.to_owned();
            }
            None => {
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    flush(&mut rows, &label, &mut body);
    rows
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn slash(name: &str, args: &str) -> String {
    let name = name.trim_start_matches('/');
    if args.trim().is_empty() {
        format!("/{name}")
    } else {
        format!("/{name} {args}")
    }
}

/// The item kind of a call to Claude tool `name`.
pub fn tool_kind(name: &str) -> ItemKind {
    match name {
        "Bash" | "PowerShell" => ItemKind::CommandExecution,
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => ItemKind::FileChange,
        // `Task` is what 2.0 called `Agent`.
        "Agent" | "Task" => ItemKind::Subagent,
        _ => ItemKind::ToolCall,
    }
}

/// The `Agent` call a subagent's frame belongs to, when it is one.
fn parent_of(v: &Value) -> Option<String> {
    owned(v, "parent_tool_use_id")
}

/// A background task's report as Claude feeds it to the model (and writes
/// it to the transcript): `<task-notification>` with the tool call, status
/// and result. Returns the `Agent` call's id and the fields as a `task`
/// record shaped like the stream-json `task_notification` frame.
pub fn task_notification(text: &str) -> Option<(String, Value)> {
    let body = text.trim_start().strip_prefix("<task-notification>")?;
    let tag = |name: &str| {
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        let start = body.find(&open)? + open.len();
        let end = body[start..].find(&close)? + start;
        Some(body[start..end].trim().to_owned())
    };
    let tool = tag("tool-use-id")?;
    let task = json!({
        "task_id": tag("task-id"),
        "status": tag("status"),
        "summary": tag("result").or_else(|| tag("summary")),
        "output_file": tag("output-file"),
    });
    Some((tool, task))
}

/// Output of a local slash command, which Claude echoes as a user message.
fn local_command_output(text: &str) -> Option<&str> {
    let inner = text.strip_prefix("<local-command-stdout>")?;
    Some(
        inner
            .strip_suffix("</local-command-stdout>")
            .unwrap_or(inner)
            .trim(),
    )
}

/// Claude reports window utilization as a fraction (0.23 = 23%; checked
/// against Claude Code 2.1.296).
fn fraction_to_percent(f: f64) -> f64 {
    (f * 100.0).clamp(0.0, 100.0)
}

fn window_minutes(id: &str) -> Option<u64> {
    if id == "five_hour" {
        Some(5 * 60)
    } else if id.starts_with("seven_day") {
        Some(7 * 24 * 60)
    } else {
        None
    }
}

/// Escapes a JSON Pointer reference token (RFC 6901).
fn escape_pointer(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests;

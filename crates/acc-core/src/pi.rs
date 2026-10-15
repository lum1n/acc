//! pi driver (`pi --mode rpc`) and pi transcript reader.
//!
//! The RPC protocol is strict JSONL on stdio: commands carry an optional `id`
//! that their `response` echoes; session events carry none. Built-in TUI
//! commands (`/compact`, `/name`, ...) are not expanded in RPC mode, so the
//! driver maps them to their RPC equivalents; extension, prompt-template and
//! skill commands are sent as prompt text and pi expands them. pi has no
//! approval gate of its own: "blocked" means an extension dialog
//! (`extension_ui_request`), which ACC surfaces as a permission request.
//!
//! Checked against pi 1.1.0 (`@earendil-works/pi-coding-agent`); the fixtures
//! in `tests/fixtures/pi/` are recorded from that version. See
//! `research/commands-and-options/pi-opencode.md`.
//!
//! [`Transcript`] reads pi's session JSONL files, which is how ACC observes a
//! pi session it didn't start (an interactive TUI, possibly inside a
//! multiplexer) without touching the process.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::command::{
    Apply, Attachment, Command, CommandEntry, CommandInput, CommandSource, Exec, OptionDef, OptionId,
    split_verb, validate_attachments,
};
use crate::driver::{Driver, DriverError};
use crate::event::{
    Event, ItemKind, Panel, PanelAction, PanelRow, PanelSection, PermissionOption, PermissionOptionKind,
    Usage,
};
use crate::harness::{Harness, LaunchSpec};
use crate::ndjson::{Frame, LineFramer};
use crate::state::{BlockKind, RunState, SessionState, StateSource};

/// Longest RPC record accepted. `agent_end` repeats the run's messages, so
/// records with large tool outputs can be several megabytes.
const MAX_LINE: usize = 64 * 1024 * 1024;

/// Option id that cancels a pi dialog in [`Command::AnswerPermission`].
pub const CANCEL: &str = "cancel";

/// What an RPC request ACC sent was for, keyed by its `id`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum Pending {
    SetThinking(String),
    /// Data for a panel ACC draws; the response is also handled as usual.
    Panel(#[serde(deserialize_with = "crate::wire::static_str")] crate::wire::StaticStr),
    /// A change made from a panel; the panel is fetched again afterwards.
    PanelAction(#[serde(deserialize_with = "crate::wire::static_str")] crate::wire::StaticStr),
    /// Any other request; its response is handled by `command`.
    Plain,
}

/// An open extension dialog.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum Dialog {
    Select(Vec<String>),
    Confirm,
    /// `input` and `editor`: the answer is free text.
    Text,
}

/// Drives `pi --mode rpc`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct PiDriver {
    framer: LineFramer,
    id_prefix: String,
    next_id: u64,
    pending: HashMap<String, Pending>,
    dialogs: HashMap<String, Dialog>,
    /// Dialogs ACC answered, reported as resolved once pi moves on.
    answered: Vec<String>,
    outgoing: Vec<u8>,
    native_id: Option<String>,
    streaming: bool,
    blocked: bool,
    last_error: Option<String>,
    msg_seq: u64,
    current_msg: Option<String>,
    shell_items: HashMap<String, bool>,
    /// pi-subagents runs launched with `async: true` and not yet reported
    /// back, by run id, with the `subagent` call that launched them. Each
    /// one's `subagent-notify` wakes the agent for another turn.
    background: HashMap<String, String>,
    usage: Usage,
    commands: Vec<CommandEntry>,
    options: Vec<OptionDef>,
}

impl Default for PiDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl PiDriver {
    pub fn new() -> Self {
        Self {
            framer: LineFramer::new(MAX_LINE),
            id_prefix: "acc".into(),
            next_id: 0,
            pending: HashMap::new(),
            dialogs: HashMap::new(),
            answered: Vec::new(),
            outgoing: Vec::new(),
            native_id: None,
            streaming: false,
            blocked: false,
            last_error: None,
            msg_seq: 0,
            current_msg: None,
            shell_items: HashMap::new(),
            background: HashMap::new(),
            usage: Usage::default(),
            commands: canonical_commands(),
            options: vec![
                OptionDef {
                    id: OptionId::Model,
                    values: vec![],
                    labels: Vec::new(),
                    current: None,
                    apply: Apply::NextTurn,
                },
                OptionDef {
                    id: OptionId::Effort,
                    values: vec![],
                    labels: Vec::new(),
                    current: None,
                    apply: Apply::NextTurn,
                },
            ],
        }
    }

    /// Uses `prefix` for request ids. Clients sharing one detached session
    /// see each other's responses in the shared log, so each should use its
    /// own prefix (`[A-Za-z0-9_-]`).
    pub fn with_id_prefix(prefix: &str) -> Self {
        Self {
            id_prefix: prefix.to_owned(),
            ..Self::new()
        }
    }

    /// The session id ACC resumes with: the session file when pi saves one,
    /// otherwise the session UUID.
    pub fn native_id(&self) -> Option<&str> {
        self.native_id.as_deref()
    }

    /// True while pi is running an agent turn.
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    fn request(&mut self, mut body: Map<String, Value>, pending: Pending) -> Vec<u8> {
        self.next_id += 1;
        let id = format!("{}-{}", self.id_prefix, self.next_id);
        body.insert("id".into(), Value::String(id.clone()));
        self.pending.insert(id, pending);
        let mut line = Value::Object(body).to_string().into_bytes();
        line.push(b'\n');
        line
    }

    fn simple(&mut self, kind: &str) -> Vec<u8> {
        self.request(obj(json!({ "type": kind })), Pending::Plain)
    }

    fn state(&self, state: RunState, message: Option<String>) -> Event {
        Event::StateChanged(SessionState {
            state,
            message,
            source: StateSource::Native,
            observed_at_ms: 0,
        })
    }

    fn option_mut(&mut self, id: OptionId) -> Option<&mut OptionDef> {
        self.options.iter_mut().find(|o| o.id == id)
    }

    fn set_current(&mut self, id: OptionId, value: Option<String>) {
        if let Some(o) = self.option_mut(id) {
            o.current = value;
        }
    }

    fn handle(&mut self, v: Value, out: &mut Vec<Event>) {
        let Some(kind) = v.get("type").and_then(Value::as_str) else {
            out.push(Event::Unrecognized { raw: v.to_string() });
            return;
        };
        match kind {
            "response" => self.on_response(&v, out),
            "agent_start" => {
                self.streaming = true;
                self.last_error = None;
                out.push(self.state(RunState::Working { progress: None }, None));
            }
            "agent_settled" => {
                self.streaming = false;
                self.blocked = false;
                self.resolve_dialogs(out);
                let aborted = v.get("aborted").and_then(Value::as_bool).unwrap_or(false);
                let state = match (aborted, self.last_error.take()) {
                    (true, _) => self.state(RunState::Idle, None),
                    (false, Some(err)) => self.state(RunState::Error, Some(err)),
                    (false, None) if !self.background.is_empty() => self.state(RunState::Waiting, None),
                    (false, None) => self.state(RunState::Done, None),
                };
                out.push(state);
                out.push(Event::TurnCompleted);
            }
            "agent_end" | "turn_start" | "turn_end" | "queue_update" | "entry_appended" => {}
            "message_start" => self.on_message_start(&v, out),
            "message_update" => self.on_message_update(&v, out),
            "message_end" => self.on_message_end(&v, out),
            "tool_execution_start" => {
                self.resume_working(out);
                let id = str_field(&v, "toolCallId");
                let tool = str_field(&v, "toolName");
                out.push(Event::ItemStarted {
                    id,
                    kind: tool_kind(&tool),
                    parent: None, extensions: json!({ "tool": tool, "args": v.get("args").cloned().unwrap_or(Value::Null) }),
                });
            }
            "tool_execution_update" => self.resume_working(out),
            "tool_execution_end" => {
                self.resume_working(out);
                let id = str_field(&v, "toolCallId");
                let is_error = v.get("isError").and_then(Value::as_bool).unwrap_or(false);
                let extensions = json!({
                    "result": v.get("result").cloned().unwrap_or(Value::Null),
                    "is_error": is_error,
                    "duration_ms": v.get("durationMs").cloned().unwrap_or(Value::Null),
                });
                let details = v.pointer("/result/details");
                if let Some(run) = async_run(&str_field(&v, "toolName"), details, is_error) {
                    self.background.insert(run.to_owned(), id.clone());
                    out.push(Event::ItemUpdated { id, extensions });
                } else {
                    out.push(Event::ItemCompleted { id, extensions });
                }
            }
            "bash_execution_update" => {
                let id = format!("bash-{}", str_field(&v, "id"));
                if self.shell_items.insert(id.clone(), true).is_none() {
                    out.push(Event::ItemStarted {
                        id: id.clone(),
                        kind: ItemKind::CommandExecution,
                        parent: None,
                        extensions: Value::Null,
                    });
                }
                out.push(Event::ItemDelta {
                    id,
                    text: str_field(&v, "delta"),
                });
            }
            "compaction_start" => {
                out.push(self.state(RunState::Working { progress: None }, Some("compacting".into())));
            }
            "compaction_end" => {
                if !self.streaming {
                    out.push(self.state(RunState::Idle, None));
                }
            }
            "auto_retry_start" => {
                let msg = v.get("errorMessage").and_then(Value::as_str).map(str::to_owned);
                out.push(self.state(RunState::Working { progress: None }, msg));
            }
            "auto_retry_end" => {
                if v.get("success").and_then(Value::as_bool) == Some(false) {
                    self.last_error = v.get("finalError").and_then(Value::as_str).map(str::to_owned);
                }
            }
            "summarization_retry_scheduled"
            | "summarization_retry_attempt_start"
            | "summarization_retry_finished" => {}
            "thinking_level_changed" => {
                let level = v.get("level").and_then(Value::as_str).map(str::to_owned);
                self.set_current(OptionId::Effort, level);
                out.push(Event::OptionsChanged);
            }
            "session_info_changed" => {}
            "extension_ui_request" => self.on_ui_request(&v, out),
            "extension_error" => out.push(Event::Unrecognized { raw: v.to_string() }),
            _ => out.push(Event::Unrecognized { raw: v.to_string() }),
        }
    }

    /// Run activity after a dialog means it was answered (by ACC, another
    /// client sharing the session, or pi's own timeout): pi is working again.
    fn resume_working(&mut self, out: &mut Vec<Event>) {
        if self.blocked {
            self.blocked = false;
            self.resolve_dialogs(out);
            out.push(self.state(RunState::Working { progress: None }, None));
        }
    }

    fn resolve_dialogs(&mut self, out: &mut Vec<Event>) {
        let mut ids: Vec<String> = self.dialogs.drain().map(|(id, _)| id).collect();
        ids.append(&mut self.answered);
        ids.sort_unstable();
        out.extend(
            ids.into_iter()
                .map(|request_id| Event::PermissionResolved { request_id }),
        );
    }

    fn on_response(&mut self, v: &Value, out: &mut Vec<Event>) {
        let id = v.get("id").and_then(Value::as_str).unwrap_or_default();
        let pending = self.pending.remove(id);
        let command = str_field(v, "command");
        if v.get("success").and_then(Value::as_bool) != Some(true) {
            let id = id.to_owned();
            if self.shell_items.remove(&format!("bash-{id}")).is_some() {
                out.push(Event::ItemCompleted {
                    id: format!("bash-{id}"),
                    extensions: Value::Null,
                });
            }
            out.push(Event::Notice {
                message: format!("{command} failed: {}", str_field(v, "error")),
            });
            return;
        }
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        match &pending {
            Some(Pending::Panel(panel_id)) => out.push(Event::Panel(panel(panel_id, &data))),
            Some(Pending::PanelAction(panel_id)) => {
                let req = self.open_panel(panel_id);
                self.outgoing.extend(req);
            }
            _ => {}
        }
        if command == "export_html"
            && let Some(path) = data.get("path").and_then(Value::as_str)
        {
            out.push(Event::Notice {
                message: format!("Exported to {path}"),
            });
        }
        let changed = match command.as_str() {
            "get_commands" => Some(Event::CommandsChanged),
            "get_state"
            | "get_available_models"
            | "get_available_thinking_levels"
            | "set_model"
            | "set_thinking_level"
            | "cycle_thinking_level" => Some(Event::OptionsChanged),
            _ => None,
        };
        match command.as_str() {
            "get_state" => self.on_state(&data, out),
            "get_commands" => self.on_commands(&data),
            "get_available_models" => {
                let values = data
                    .get("models")
                    .and_then(Value::as_array)
                    .map(|ms| ms.iter().filter_map(model_ref).collect())
                    .unwrap_or_default();
                if let Some(o) = self.option_mut(OptionId::Model) {
                    o.values = values;
                }
            }
            "get_available_thinking_levels" => {
                let values = data
                    .get("levels")
                    .and_then(Value::as_array)
                    .map(|ls| ls.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                    .unwrap_or_default();
                if let Some(o) = self.option_mut(OptionId::Effort) {
                    o.values = values;
                }
            }
            "set_model" => {
                if let Some(window) = data.get("contextWindow").and_then(Value::as_u64) {
                    self.usage.context_window = Some(window);
                }
                self.set_current(OptionId::Model, model_ref(&data));
                // Thinking levels depend on the model.
                let req = self.simple("get_available_thinking_levels");
                self.outgoing.extend(req);
            }
            "set_thinking_level" => {
                if let Some(Pending::SetThinking(level)) = pending {
                    self.set_current(OptionId::Effort, Some(level));
                }
            }
            "cycle_thinking_level" => {
                let level = data.get("level").and_then(Value::as_str).map(str::to_owned);
                self.set_current(OptionId::Effort, level);
            }
            "new_session" | "switch_session" | "clone" | "fork" => {
                if data.get("cancelled").and_then(Value::as_bool) != Some(true) {
                    self.usage = Usage {
                        context_window: self.usage.context_window,
                        ..Usage::default()
                    };
                    let req = self.simple("get_state");
                    self.outgoing.extend(req);
                }
            }
            "get_session_stats" => {
                let tokens = data.get("tokens");
                let tok = |k: &str| tokens.and_then(|t| t.get(k)).and_then(Value::as_u64);
                self.usage.input_tokens = tok("input");
                self.usage.output_tokens = tok("output");
                self.usage.cache_read_tokens = tok("cacheRead");
                self.usage.cache_write_tokens = tok("cacheWrite");
                self.usage.cost_usd = data.get("cost").and_then(Value::as_f64);
                if let Some(ctx) = data.get("contextUsage") {
                    self.usage.context_used = ctx.get("tokens").and_then(Value::as_u64);
                    if let Some(w) = ctx.get("contextWindow").and_then(Value::as_u64) {
                        self.usage.context_window = Some(w);
                    }
                }
                out.push(Event::Usage(self.usage.clone()));
            }
            "bash" => {
                let item = format!("bash-{id}");
                if self.shell_items.remove(&item).is_none() {
                    // No output was streamed; still report the command.
                    out.push(Event::ItemStarted {
                        id: item.clone(),
                        kind: ItemKind::CommandExecution,
                        parent: None,
                        extensions: Value::Null,
                    });
                }
                out.push(Event::ItemCompleted {
                    id: item,
                    extensions: data,
                });
            }
            _ => {}
        }
        out.extend(changed);
    }

    fn on_state(&mut self, data: &Value, out: &mut Vec<Event>) {
        let native = data
            .get("sessionFile")
            .and_then(Value::as_str)
            .or_else(|| data.get("sessionId").and_then(Value::as_str))
            .map(str::to_owned);
        let model = data.get("model").and_then(model_ref);
        if let Some(window) = data.pointer("/model/contextWindow").and_then(Value::as_u64) {
            self.usage.context_window = Some(window);
        }
        self.set_current(OptionId::Model, model.clone());
        let level = data
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.set_current(OptionId::Effort, level);
        let streaming = data.get("isStreaming").and_then(Value::as_bool).unwrap_or(false);
        if let Some(native) = native
            && self.native_id.as_deref() != Some(native.as_str())
        {
            self.native_id = Some(native.clone());
            out.push(Event::SessionStarted {
                harness: Harness::Pi,
                native_id: native,
                model,
            });
            self.streaming = streaming;
            out.push(if streaming {
                self.state(RunState::Working { progress: None }, None)
            } else {
                self.state(RunState::Idle, None)
            });
        }
    }

    fn on_commands(&mut self, data: &Value) {
        let mut entries = canonical_commands();
        let listed = data.get("commands").and_then(Value::as_array);
        for c in listed.into_iter().flatten() {
            let Some(name) = c.get("name").and_then(Value::as_str) else {
                continue;
            };
            if entries.iter().any(|e| e.name == name) {
                continue;
            }
            let source = match c.get("source").and_then(Value::as_str) {
                Some("skill") => CommandSource::Skill,
                Some("prompt") => CommandSource::Custom,
                _ => CommandSource::Plugin,
            };
            entries.push(CommandEntry {
                name: name.to_owned(),
                description: c
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                arg_hint: None,
                source,
                exec: Exec::Text,
                input: CommandInput::None,
            });
        }
        self.commands = entries;
    }

    fn on_message_start(&mut self, v: &Value, out: &mut Vec<Event>) {
        let Some(msg) = v.get("message") else { return };
        match msg.get("role").and_then(Value::as_str) {
            Some("user") => {
                self.msg_seq += 1;
                let id = format!("u{}", self.msg_seq);
                out.push(Event::ItemStarted {
                    id: id.clone(),
                    kind: ItemKind::UserMessage,
                    parent: None,
                    extensions: Value::Null,
                });
                let text = user_text(msg.get("content"));
                if !text.is_empty() {
                    out.push(Event::ItemDelta { id, text });
                }
            }
            Some("assistant") => {
                self.resume_working(out);
                self.msg_seq += 1;
                let id = format!("m{}", self.msg_seq);
                self.current_msg = Some(id.clone());
                // The start record already holds the first chunk, which is
                // then repeated as a delta, so its content is ignored here.
                out.push(Event::ItemStarted {
                    id,
                    kind: ItemKind::AgentMessage,
                    parent: None,
                    extensions: Value::Null,
                });
            }
            _ => {}
        }
    }

    fn on_message_update(&mut self, v: &Value, out: &mut Vec<Event>) {
        let Some(ev) = v.get("assistantMessageEvent") else {
            return;
        };
        let Some(msg) = self.current_msg.clone() else {
            return;
        };
        let idx = ev.get("contentIndex").and_then(Value::as_u64).unwrap_or(0);
        let reasoning = format!("{msg}.{idx}");
        match ev.get("type").and_then(Value::as_str) {
            Some("text_delta") => out.push(Event::ItemDelta {
                id: msg,
                text: str_field(ev, "delta"),
            }),
            Some("thinking_start") => out.push(Event::ItemStarted {
                id: reasoning,
                kind: ItemKind::Reasoning,
                parent: None,
                extensions: Value::Null,
            }),
            Some("thinking_delta") => out.push(Event::ItemDelta {
                id: reasoning,
                text: str_field(ev, "delta"),
            }),
            Some("thinking_end") => out.push(Event::ItemCompleted {
                id: reasoning,
                extensions: Value::Null,
            }),
            _ => {}
        }
    }

    fn on_message_end(&mut self, v: &Value, out: &mut Vec<Event>) {
        let Some(msg) = v.get("message") else { return };
        match msg.get("role").and_then(Value::as_str) {
            Some("custom") => subagent_notify(&mut self.background, msg, out),
            Some("user") => out.push(Event::ItemCompleted {
                id: format!("u{}", self.msg_seq),
                extensions: Value::Null,
            }),
            Some("assistant") => {
                let Some(id) = self.current_msg.take() else { return };
                let stop = msg.get("stopReason").and_then(Value::as_str).unwrap_or_default();
                if stop == "error" {
                    self.last_error = Some(
                        msg.get("errorMessage")
                            .and_then(Value::as_str)
                            .unwrap_or("error")
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .to_owned(),
                    );
                }
                out.push(Event::ItemCompleted {
                    id,
                    extensions: json!({
                        "provider": msg.get("provider"),
                        "model": msg.get("model"),
                        "stop_reason": stop,
                        "error": msg.get("errorMessage"),
                    }),
                });
                if let Some(u) = msg.get("usage") {
                    add_usage(&mut self.usage, u);
                    out.push(Event::Usage(self.usage.clone()));
                }
            }
            _ => {}
        }
    }

    fn on_ui_request(&mut self, v: &Value, out: &mut Vec<Event>) {
        let id = str_field(v, "id");
        let title = str_field(v, "title");
        let (dialog, options, kind) = match v.get("method").and_then(Value::as_str) {
            Some("select") => {
                let labels: Vec<String> = v
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|o| o.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                    .unwrap_or_default();
                let options = labels
                    .iter()
                    .enumerate()
                    .map(|(i, l)| PermissionOption::new(i.to_string(), l.clone()))
                    .collect();
                (Dialog::Select(labels), options, BlockKind::Permission)
            }
            Some("confirm") => (
                Dialog::Confirm,
                vec![
                    PermissionOption::new("yes", "Yes").with_kind(PermissionOptionKind::AllowOnce),
                    PermissionOption::new("no", "No").with_kind(PermissionOptionKind::RejectOnce),
                ],
                BlockKind::Permission,
            ),
            Some("input") | Some("editor") => (Dialog::Text, vec![], BlockKind::Question),
            // notify, setStatus, setWidget, setTitle, set_editor_text: TUI decoration.
            _ => return,
        };
        let title = match v.get("message").and_then(Value::as_str) {
            Some(m) if !m.is_empty() => format!("{title}\n{m}"),
            _ => title,
        };
        let mut options: Vec<PermissionOption> = options;
        options.push(PermissionOption::new(CANCEL, "Cancel"));
        self.dialogs.insert(id.clone(), dialog);
        self.blocked = true;
        out.push(self.state(RunState::Blocked { kind }, Some(title.clone())));
        out.push(Event::PermissionRequested {
            request_id: id,
            title,
            options,
            kind,
            item_id: None,
            input: v.clone(),
        });
    }

    fn answer(&mut self, request_id: &str, option_id: &str) -> Result<Vec<u8>, DriverError> {
        let dialog = self
            .dialogs
            .remove(request_id)
            .ok_or_else(|| DriverError::UnknownRequest(request_id.to_owned()))?;
        self.answered.push(request_id.to_owned());
        let mut body = obj(json!({ "type": "extension_ui_response", "id": request_id }));
        match dialog {
            Dialog::Select(_) | Dialog::Confirm if option_id == CANCEL => {
                body.insert("cancelled".into(), Value::Bool(true));
            }
            Dialog::Select(labels) => {
                let label = option_id
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| labels.get(i))
                    .ok_or_else(|| DriverError::UnknownRequest(format!("{request_id}/{option_id}")))?;
                body.insert("value".into(), Value::String(label.clone()));
            }
            Dialog::Confirm => {
                body.insert("confirmed".into(), Value::Bool(option_id == "yes"));
            }
            // Free-text dialogs take the answer text as the option id.
            Dialog::Text => {
                body.insert("value".into(), Value::String(option_id.to_owned()));
            }
        }
        let mut line = Value::Object(body).to_string().into_bytes();
        line.push(b'\n');
        Ok(line)
    }

    fn prompt(&mut self, text: String, attachments: &[Attachment]) -> Vec<u8> {
        // A slash command with an image attached is a prompt for the model.
        if attachments.is_empty()
            && let Some(bytes) = self.builtin(&text)
        {
            return bytes;
        }
        let mut body = obj(json!({ "type": "prompt", "message": text }));
        if !attachments.is_empty() {
            let images: Vec<Value> = attachments
                .iter()
                .map(|a| json!({ "type": "image", "data": a.data, "mimeType": a.media_type }))
                .collect();
            body.insert("images".into(), Value::Array(images));
        }
        if self.streaming {
            // Like pressing Enter in pi's TUI while it works.
            body.insert("streamingBehavior".into(), Value::String("steer".into()));
        }
        self.request(body, Pending::Plain)
    }

    /// Maps pi's built-in TUI commands to RPC calls. pi would otherwise send
    /// them to the model as plain text (checked against 1.1.0).
    fn builtin(&mut self, text: &str) -> Option<Vec<u8>> {
        let rest = text.strip_prefix('/')?;
        let (name, arg) = match rest.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (rest, ""),
        };
        let bytes = match (name, arg) {
            ("compact", hint) => self.compact(Some(hint).filter(|h| !h.is_empty())),
            ("new", "") => self.simple("new_session"),
            ("clone", "") => self.simple("clone"),
            ("session", "") => self.simple("get_session_stats"),
            ("name", name) if !name.is_empty() => self.request(
                obj(json!({ "type": "set_session_name", "name": name })),
                Pending::Plain,
            ),
            ("model", model) if !model.is_empty() => self.set_model(model).ok()?,
            ("thinking", level) if !level.is_empty() => self.set_thinking(level),
            _ => return None,
        };
        Some(bytes)
    }

    /// Runs a palette entry. Built-in names go through `builtin` or a
    /// panel; pi expands the rest.
    fn invoke(&mut self, name: &str, args: &str) -> Result<Vec<u8>, DriverError> {
        let (verb, rest) = split_verb(args);
        Ok(match (name, args) {
            // pi would send these to the model as text.
            ("model" | "thinking" | "name", "") => return Err(DriverError::MissingArgument("a value")),
            ("session", "") => self.open_panel("session"),
            ("settings", "") => self.open_panel("settings"),
            ("settings", _) => {
                let body = match (verb, rest) {
                    ("auto-compaction", v @ ("on" | "off")) => {
                        json!({ "type": "set_auto_compaction", "enabled": v == "on" })
                    }
                    ("steering", mode) if !mode.is_empty() => {
                        json!({ "type": "set_steering_mode", "mode": mode })
                    }
                    ("follow-up", mode) if !mode.is_empty() => {
                        json!({ "type": "set_follow_up_mode", "mode": mode })
                    }
                    _ => return Err(DriverError::InvalidOption(args.to_owned())),
                };
                self.request(obj(body), Pending::PanelAction("settings"))
            }
            ("fork", "") => self.open_panel("fork"),
            ("fork", entry) => self.request(obj(json!({ "type": "fork", "entryId": entry })), Pending::Plain),
            ("export", path) => {
                let mut body = obj(json!({ "type": "export_html" }));
                if !path.is_empty() {
                    body.insert("outputPath".into(), Value::String(path.to_owned()));
                }
                self.request(body, Pending::Plain)
            }
            (name, "") => self.prompt(format!("/{name}"), &[]),
            (name, args) => self.prompt(format!("/{name} {args}"), &[]),
        })
    }

    fn open_panel(&mut self, id: &'static str) -> Vec<u8> {
        let kind = match id {
            "session" => "get_session_stats",
            "settings" => "get_state",
            "fork" => "get_fork_messages",
            _ => return Vec::new(),
        };
        self.request(obj(json!({ "type": kind })), Pending::Panel(id))
    }

    fn compact(&mut self, hint: Option<&str>) -> Vec<u8> {
        let mut body = obj(json!({ "type": "compact" }));
        if let Some(h) = hint {
            body.insert("customInstructions".into(), Value::String(h.to_owned()));
        }
        self.request(body, Pending::Plain)
    }

    fn set_model(&mut self, value: &str) -> Result<Vec<u8>, DriverError> {
        let (provider, model) = value.split_once('/').ok_or(DriverError::Unsupported {
            harness: Harness::Pi,
            what: "a model id without a provider (use provider/model)",
        })?;
        Ok(self.request(
            obj(json!({ "type": "set_model", "provider": provider, "modelId": model })),
            Pending::Plain,
        ))
    }

    fn set_thinking(&mut self, level: &str) -> Vec<u8> {
        self.request(
            obj(json!({ "type": "set_thinking_level", "level": level })),
            Pending::SetThinking(level.to_owned()),
        )
    }
}

impl Driver for PiDriver {
    fn harness(&self) -> Harness {
        Harness::Pi
    }

    fn snapshot(&self) -> Option<Value> {
        serde_json::to_value(self).ok()
    }

    fn launch(&self, resume: Option<&str>) -> LaunchSpec {
        Harness::Pi.launch(resume)
    }

    fn handshake(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for kind in [
            "get_state",
            "get_commands",
            "get_available_models",
            "get_available_thinking_levels",
            "get_session_stats",
        ] {
            let req = self.simple(kind);
            out.extend(req);
        }
        out
    }

    /// pi's handshake is read-only queries, so a joining client repeats it.
    fn attach_handshake(&mut self) -> Vec<u8> {
        self.handshake()
    }

    fn on_output(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        for frame in self.framer.feed(bytes) {
            match frame {
                Frame::Line(line) => match serde_json::from_str::<Value>(&line) {
                    Ok(v) => self.handle(v, &mut out),
                    Err(_) => out.push(Event::Unrecognized { raw: line }),
                },
                Frame::Oversized => out.push(Event::Unrecognized {
                    raw: "<oversized record dropped>".into(),
                }),
                Frame::InvalidUtf8 => out.push(Event::Unrecognized {
                    raw: "<invalid UTF-8 record dropped>".into(),
                }),
            }
        }
        out
    }

    fn command(&mut self, cmd: Command) -> Result<Vec<u8>, DriverError> {
        Ok(match cmd {
            Command::Prompt { text, attachments } => {
                validate_attachments(&attachments)?;
                self.prompt(text, &attachments)
            }
            Command::Interrupt => self.simple("abort"),
            Command::Compact { hint } => self.compact(hint.as_deref()),
            Command::New => self.simple("new_session"),
            // pi's `fork` branches from an earlier entry; `clone` copies the
            // current branch into a new session, which is ACC's "fork".
            Command::Fork => self.simple("clone"),
            Command::Rename { name } => self.request(
                obj(json!({ "type": "set_session_name", "name": name })),
                Pending::Plain,
            ),
            Command::Review => {
                return Err(DriverError::Unsupported {
                    harness: Harness::Pi,
                    what: "review",
                });
            }
            Command::Shell { command } => {
                self.request(obj(json!({ "type": "bash", "command": command })), Pending::Plain)
            }
            Command::SetOption { id, value } => match id {
                OptionId::Model => self.set_model(&value)?,
                OptionId::Effort => self.set_thinking(&value),
                OptionId::Mode | OptionId::Permission => {
                    return Err(DriverError::Unsupported {
                        harness: Harness::Pi,
                        what: "modes or permission settings",
                    });
                }
            },
            Command::AnswerPermission {
                request_id,
                option_id,
            } => self.answer(&request_id, &option_id)?,
            Command::Invoke { name, args } => self.invoke(name.trim_start_matches('/'), args.trim())?,
            Command::Refresh => self.handshake(),
            Command::Handoff { .. } => {
                return Err(DriverError::Unsupported {
                    harness: Harness::Pi,
                    what: "handoff without a relaunch",
                });
            }
        })
    }

    fn poll_transmit(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.outgoing)
    }

    fn commands(&self) -> &[CommandEntry] {
        &self.commands
    }

    fn options(&self) -> &[OptionDef] {
        &self.options
    }
}

/// Built-in pi commands ACC runs through RPC.
fn canonical_commands() -> Vec<CommandEntry> {
    let e = |name: &str, description: &str, arg: Option<&str>, method: &str| CommandEntry {
        source: CommandSource::Builtin,
        ..CommandEntry::rpc(name, description, arg, method)
    };
    let option = |id| CommandInput::Option { id };
    vec![
        e(
            "compact",
            "Summarize older history",
            Some("[instructions]"),
            "compact",
        ),
        e("new", "Start a new session", None, "new_session"),
        e("clone", "Copy this session into a new one", None, "clone"),
        e(
            "fork",
            "Continue from an earlier message in a new session",
            None,
            "fork",
        ),
        e("name", "Name this session", Some("<name>"), "set_session_name"),
        e("model", "Switch model", Some("<provider/model>"), "set_model").with_input(option(OptionId::Model)),
        e(
            "thinking",
            "Set thinking level",
            Some("<level>"),
            "set_thinking_level",
        )
        .with_input(option(OptionId::Effort)),
        e(
            "session",
            "Session info, tokens and cost",
            None,
            "get_session_stats",
        ),
        e(
            "settings",
            "Auto-compaction and message queueing",
            None,
            "get_state",
        ),
        e(
            "export",
            "Export the session to HTML",
            Some("[path]"),
            "export_html",
        ),
    ]
}

/// Draws an RPC response as panel `id`.
fn panel(id: &str, data: &Value) -> Panel {
    let row = |label: &str, value: Option<String>| PanelRow {
        label: label.into(),
        value,
        ..PanelRow::default()
    };
    let num = |ptr: &str| data.pointer(ptr).and_then(Value::as_u64).map(|n| n.to_string());
    let text = |key: &str| data.get(key).and_then(Value::as_str).map(str::to_owned);
    let (title, rows) = match id {
        "session" => (
            "Session",
            vec![
                row("Session", text("sessionId")),
                row("File", text("sessionFile")),
                row("User messages", num("/userMessages")),
                row("Assistant messages", num("/assistantMessages")),
                row("Tool calls", num("/toolCalls")),
                row("Input tokens", num("/tokens/input")),
                row("Output tokens", num("/tokens/output")),
                row("Cache read", num("/tokens/cacheRead")),
                row("Cache write", num("/tokens/cacheWrite")),
                row(
                    "Cost",
                    data.get("cost")
                        .and_then(Value::as_f64)
                        .map(|c| format!("${c:.4}")),
                ),
                row(
                    "Context",
                    num("/contextUsage/tokens")
                        .map(|t| format!("{t} / {}", num("/contextUsage/contextWindow").unwrap_or_default())),
                ),
            ],
        ),
        "settings" => {
            let auto = data.get("autoCompactionEnabled").and_then(Value::as_bool);
            let modes = |key: &str, verb: &str| {
                let current = text(key);
                PanelRow {
                    actions: ["all", "one-at-a-time"]
                        .iter()
                        .filter(|m| current.as_deref() != Some(**m))
                        .map(|m| PanelAction::new(format!("Use {m}"), "settings", format!("{verb} {m}")))
                        .collect(),
                    ..row(
                        if verb == "steering" {
                            "Steering"
                        } else {
                            "Follow-ups"
                        },
                        current,
                    )
                }
            };
            (
                "Settings",
                vec![
                    PanelRow {
                        actions: auto
                            .map(|on| {
                                let (label, v) = if on {
                                    ("Turn off", "off")
                                } else {
                                    ("Turn on", "on")
                                };
                                vec![PanelAction::new(
                                    label,
                                    "settings",
                                    format!("auto-compaction {v}"),
                                )]
                            })
                            .unwrap_or_default(),
                        ..row(
                            "Auto-compaction",
                            auto.map(|on| if on { "on" } else { "off" }.into()),
                        )
                    },
                    modes("steeringMode", "steering"),
                    modes("followUpMode", "follow-up"),
                ],
            )
        }
        "fork" => (
            "Fork from a message",
            data.get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|m| {
                    let entry = m.get("entryId").and_then(Value::as_str)?;
                    let text = m.get("text").and_then(Value::as_str).unwrap_or_default();
                    Some(PanelRow {
                        label: text.lines().next().unwrap_or_default().to_owned(),
                        actions: vec![PanelAction::new("Fork here", "fork", entry)],
                        ..PanelRow::default()
                    })
                })
                .collect(),
        ),
        _ => ("", Vec::new()),
    };
    Panel {
        id: id.to_owned(),
        title: title.into(),
        sections: vec![PanelSection {
            rows: rows
                .into_iter()
                .filter(|r| r.value.is_some() || !r.actions.is_empty())
                .collect(),
            ..PanelSection::default()
        }],
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// `provider/id` for a pi `Model` object.
fn model_ref(m: &Value) -> Option<String> {
    let provider = m.get("provider").and_then(Value::as_str)?;
    let id = m.get("id").and_then(Value::as_str)?;
    Some(format!("{provider}/{id}"))
}

fn tool_kind(tool: &str) -> ItemKind {
    match tool {
        "bash" | "powershell" => ItemKind::CommandExecution,
        "edit" | "write" => ItemKind::FileChange,
        // The pi-subagents extension's tool.
        "subagent" => ItemKind::Subagent,
        _ => ItemKind::ToolCall,
    }
}

/// Text of a pi message `content`: a string, or blocks of which only `text` counts.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// What to show for a user message: its text, or `[image]` when it is only images.
/// An async `subagent` launch's run id (pi-subagents `async: true`): its
/// result only acknowledges it, and its `subagent-notify` completes it.
fn async_run<'a>(tool: &str, details: Option<&'a Value>, is_error: bool) -> Option<&'a str> {
    details
        .and_then(|d| d.get("asyncId"))
        .and_then(Value::as_str)
        .filter(|_| !is_error && tool == "subagent")
}

/// pi-subagents' report of finished async runs (a `subagent-notify` custom
/// message): completes the calls that launched them, with the report as
/// their output.
fn subagent_notify(background: &mut HashMap<String, String>, msg: &Value, out: &mut Vec<Event>) {
    if msg.get("customType").and_then(Value::as_str) != Some("subagent-notify") {
        return;
    }
    let Some(runs) = msg.pointer("/details/runs").and_then(Value::as_array) else {
        return;
    };
    let text = content_text(msg.get("content"));
    for run in runs {
        let Some(id) = run
            .get("runId")
            .and_then(Value::as_str)
            .and_then(|r| background.remove(r))
        else {
            continue;
        };
        let failed = run.get("status").and_then(Value::as_str) != Some("completed");
        out.push(Event::ItemCompleted {
            id,
            extensions: json!({
                "notify": run,
                "result": {"content": [{"type": "text", "text": text}]},
                "is_error": failed,
            }),
        });
    }
}

fn user_text(content: Option<&Value>) -> String {
    let text = content_text(content);
    let images = content.and_then(Value::as_array).is_some_and(|blocks| {
        blocks
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("image"))
    });
    if text.is_empty() && images {
        "[image]".to_owned()
    } else {
        text
    }
}

/// Adds one assistant message's usage to the running totals. pi's context
/// fill is the last response's total tokens.
fn add_usage(total: &mut Usage, u: &Value) {
    let get = |k: &str| u.get(k).and_then(Value::as_u64);
    let add = |slot: &mut Option<u64>, v: Option<u64>| {
        if let Some(v) = v {
            *slot = Some(slot.unwrap_or(0) + v);
        }
    };
    add(&mut total.input_tokens, get("input"));
    add(&mut total.output_tokens, get("output"));
    add(&mut total.cache_read_tokens, get("cacheRead"));
    add(&mut total.cache_write_tokens, get("cacheWrite"));
    if let Some(cost) = u.pointer("/cost/total").and_then(Value::as_f64) {
        total.cost_usd = Some(total.cost_usd.unwrap_or(0.0) + cost);
    }
    if let Some(t) = get("totalTokens").filter(|t| *t > 0) {
        total.context_used = Some(t);
    }
}

/// Default directory pi stores sessions for `cwd` in, relative to `$HOME`:
/// `.pi/agent/sessions/--<cwd with separators as dashes>--`.
pub fn session_dir(cwd: &str) -> String {
    let trimmed = cwd.trim_start_matches(['/', '\\']);
    let encoded: String = trimmed
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':') { '-' } else { c })
        .collect();
    format!(".pi/agent/sessions/--{encoded}--")
}

/// Reads a pi session file (JSONL) incrementally, as appended by a running pi.
///
/// State comes from the transcript ([`StateSource::Transcript`]): a user
/// message means working, an assistant message that stops for tool use means
/// still working, a final one means done, an error stop means error and an
/// aborted one means idle, and a final one with async pi-subagents runs still
/// out means waiting. pi never shows "blocked" in its transcript.
#[derive(Debug)]
pub struct Transcript {
    framer: LineFramer,
    open_tools: Vec<String>,
    /// Async pi-subagents runs not yet reported back, as in [`PiDriver`].
    background: HashMap<String, String>,
    usage: Usage,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            framer: LineFramer::new(MAX_LINE),
            open_tools: Vec::new(),
            background: HashMap::new(),
            usage: Usage::default(),
        }
    }

    /// Tool calls with no result yet.
    pub fn pending_tools(&self) -> usize {
        self.open_tools.len()
    }

    /// Feeds bytes from the session file (from a `tail -c +N -f`).
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        for frame in self.framer.feed(bytes) {
            if let Frame::Line(line) = frame
                && let Ok(v) = serde_json::from_str::<Value>(&line)
            {
                self.entry(&v, &mut out);
            }
        }
        out
    }

    fn state(state: RunState, message: Option<String>, at: u64) -> Event {
        Event::StateChanged(SessionState {
            state,
            message,
            source: StateSource::Transcript,
            observed_at_ms: at,
        })
    }

    fn entry(&mut self, v: &Value, out: &mut Vec<Event>) {
        match v.get("type").and_then(Value::as_str) {
            Some("session") => out.push(Event::SessionStarted {
                harness: Harness::Pi,
                native_id: str_field(v, "id"),
                model: None,
            }),
            Some("message") => {
                let id = str_field(v, "id");
                let Some(msg) = v.get("message") else { return };
                let at = msg.get("timestamp").and_then(Value::as_u64).unwrap_or(0);
                match msg.get("role").and_then(Value::as_str) {
                    Some("user") => {
                        out.push(Event::ItemStarted {
                            id: id.clone(),
                            kind: ItemKind::UserMessage,
                            parent: None,
                            extensions: Value::Null,
                        });
                        out.push(Event::ItemDelta {
                            id: id.clone(),
                            text: user_text(msg.get("content")),
                        });
                        out.push(Event::ItemCompleted {
                            id,
                            extensions: Value::Null,
                        });
                        out.push(Self::state(RunState::Working { progress: None }, None, at));
                    }
                    Some("assistant") => self.assistant(id, msg, at, out),
                    Some("toolResult") => {
                        let call = str_field(msg, "toolCallId");
                        self.open_tools.retain(|t| *t != call);
                        let is_error = msg.get("isError").and_then(Value::as_bool).unwrap_or(false);
                        let details = msg.get("details");
                        let extensions = json!({
                            "result": { "content": msg.get("content"), "details": details },
                            "is_error": is_error,
                        });
                        if let Some(run) = async_run(&str_field(msg, "toolName"), details, is_error) {
                            self.background.insert(run.to_owned(), call.clone());
                            out.push(Event::ItemUpdated { id: call, extensions });
                        } else {
                            out.push(Event::ItemCompleted { id: call, extensions });
                        }
                    }
                    _ => {}
                }
            }
            Some("custom_message") => subagent_notify(&mut self.background, v, out),
            _ => {}
        }
    }

    fn assistant(&mut self, id: String, msg: &Value, at: u64, out: &mut Vec<Event>) {
        out.push(Event::ItemStarted {
            id: id.clone(),
            kind: ItemKind::AgentMessage,
            parent: None,
            extensions: Value::Null,
        });
        let text = content_text(msg.get("content"));
        if !text.is_empty() {
            out.push(Event::ItemDelta { id: id.clone(), text });
        }
        let stop = msg.get("stopReason").and_then(Value::as_str).unwrap_or_default();
        out.push(Event::ItemCompleted {
            id,
            extensions: json!({
                "provider": msg.get("provider"),
                "model": msg.get("model"),
                "stop_reason": stop,
            }),
        });
        for block in msg.get("content").and_then(Value::as_array).into_iter().flatten() {
            if block.get("type").and_then(Value::as_str) == Some("toolCall") {
                let call = str_field(block, "id");
                let tool = str_field(block, "name");
                self.open_tools.push(call.clone());
                out.push(Event::ItemStarted {
                    id: call,
                    kind: tool_kind(&tool),
                    parent: None,
                    extensions: json!({ "tool": tool, "args": block.get("arguments") }),
                });
            }
        }
        if let Some(u) = msg.get("usage") {
            add_usage(&mut self.usage, u);
            out.push(Event::Usage(self.usage.clone()));
        }
        let state = match stop {
            "toolUse" => RunState::Working { progress: None },
            "error" => RunState::Error,
            "aborted" => RunState::Idle,
            _ if !self.background.is_empty() => RunState::Waiting,
            _ => RunState::Done,
        };
        let message = (stop == "error")
            .then(|| msg.get("errorMessage").and_then(Value::as_str))
            .flatten()
            .and_then(|m| m.lines().next())
            .map(str::to_owned);
        out.push(Self::state(state, message, at));
        if stop == "stop" {
            out.push(Event::TurnCompleted);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    const TURN: &str = include_str!("../tests/fixtures/pi/rpc-turn-with-dialog.jsonl");
    const COMMANDS: &str = include_str!("../tests/fixtures/pi/rpc-commands.jsonl");
    const HANDSHAKE: &str = include_str!("../tests/fixtures/pi/rpc-handshake.jsonl");
    const SESSION: &str = include_str!("../tests/fixtures/pi/session.jsonl");

    /// pi's stdout from a recorded fixture (lines starting `>> ` are what the
    /// recorder wrote to stdin).
    fn stdout(fixture: &str) -> String {
        fixture
            .lines()
            .filter(|l| !l.starts_with(">> "))
            .map(|l| format!("{l}\n"))
            .collect()
    }

    fn sent(bytes: &[u8]) -> Vec<Value> {
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn states(events: &[Event]) -> Vec<RunState> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::StateChanged(s) => Some(s.state.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn handshake_fills_session_options_and_commands() {
        let mut d = PiDriver::new();
        let hs = sent(&d.handshake());
        assert_eq!(hs[0]["type"], "get_state");
        assert_eq!(hs[0]["id"], "acc-1");
        let events = d.on_output(stdout(HANDSHAKE).as_bytes());
        let Some(Event::SessionStarted { native_id, model, .. }) = events.first() else {
            panic!("expected SessionStarted, got {events:?}")
        };
        assert!(
            native_id.ends_with(".jsonl"),
            "session file is the resume id: {native_id}"
        );
        assert_eq!(model.as_deref(), Some("fake/fake-1"));
        assert_eq!(states(&events), [RunState::Idle]);
        let model = &d.options()[0];
        assert_eq!(model.values, ["fake/fake-1"]);
        assert_eq!(model.current.as_deref(), Some("fake/fake-1"));
        assert_eq!(d.options()[1].values, ["off"]);
        let names: Vec<_> = d.commands().iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"compact") && names.contains(&"mcp"), "{names:?}");
        let mcp = d.commands().iter().find(|c| c.name == "mcp").unwrap();
        assert_eq!(mcp.exec, Exec::Text);
    }

    #[test]
    fn recorded_turn_with_extension_dialog() {
        let mut d = PiDriver::new();
        // Fed in small chunks to exercise framing across reads.
        let bytes = stdout(TURN).into_bytes();
        let mut events = Vec::new();
        for chunk in bytes.chunks(97) {
            events.extend(d.on_output(chunk));
        }
        assert!(
            !events.iter().any(|e| matches!(e, Event::Unrecognized { .. })),
            "{events:?}"
        );

        assert_eq!(
            states(&events),
            [
                RunState::Working { progress: None },
                RunState::Blocked {
                    kind: BlockKind::Permission
                },
                RunState::Working { progress: None },
                RunState::Done,
            ]
        );
        let Some(Event::PermissionRequested { options, title, .. }) = events
            .iter()
            .find(|e| matches!(e, Event::PermissionRequested { .. }))
            .cloned()
        else {
            panic!("no permission request")
        };
        assert!(title.starts_with("Allow bash"));
        // Answered by the recorder (another client), so pi moving on resolves it.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::PermissionResolved { .. }))
        );
        assert_eq!(
            options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(),
            ["Allow", "Block", "Cancel"]
        );

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                Event::ItemDelta { id, text } if id.starts_with('m') => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Listing.Done: hi");
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ItemStarted { id, kind: ItemKind::CommandExecution, .. } if id == "call_1"
        )));
        let usage = events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Usage(u) => Some(u.clone()),
                _ => None,
            })
            .unwrap();
        // Same totals pi reports from get_session_stats at the end of the fixture.
        assert_eq!(usage.input_tokens, Some(230));
        assert_eq!(usage.output_tokens, Some(13));
        assert_eq!(usage.context_used, Some(133));
        assert_eq!(usage.context_window, Some(128_000));
        assert!(events.iter().any(|e| matches!(e, Event::TurnCompleted)));
    }

    #[test]
    fn answers_dialogs_by_kind() {
        let mut d = PiDriver::new();
        d.on_output(
            b"{\"type\":\"extension_ui_request\",\"id\":\"s\",\"method\":\"select\",\"title\":\"t\",\"options\":[\"Allow\",\"Block\"]}\n\
              {\"type\":\"extension_ui_request\",\"id\":\"c\",\"method\":\"confirm\",\"title\":\"t\",\"message\":\"m\"}\n\
              {\"type\":\"extension_ui_request\",\"id\":\"i\",\"method\":\"input\",\"title\":\"Name?\"}\n\
              {\"type\":\"extension_ui_request\",\"id\":\"n\",\"method\":\"notify\",\"message\":\"x\"}\n",
        );
        let answer = |d: &mut PiDriver, r: &str, o: &str| {
            sent(
                &d.command(Command::AnswerPermission {
                    request_id: r.into(),
                    option_id: o.into(),
                })
                .unwrap(),
            )
            .remove(0)
        };
        assert_eq!(answer(&mut d, "s", "1")["value"], "Block");
        assert_eq!(answer(&mut d, "c", "yes")["confirmed"], true);
        assert_eq!(answer(&mut d, "i", "Ada")["value"], "Ada");
        // Answered dialogs and notifications are not pending.
        assert!(
            d.command(Command::AnswerPermission {
                request_id: "s".into(),
                option_id: "0".into()
            })
            .is_err()
        );
        assert!(
            d.command(Command::AnswerPermission {
                request_id: "n".into(),
                option_id: "0".into()
            })
            .is_err()
        );

        d.on_output(b"{\"type\":\"extension_ui_request\",\"id\":\"x\",\"method\":\"select\",\"title\":\"t\",\"options\":[\"A\"]}\n");
        assert_eq!(answer(&mut d, "x", CANCEL)["cancelled"], true);
    }

    #[test]
    fn builtins_map_to_rpc_and_others_are_prompts() {
        let mut d = PiDriver::new();
        let one = |d: &mut PiDriver, text: &str| sent(&d.command(Command::prompt(text)).unwrap()).remove(0);
        assert_eq!(
            one(&mut d, "/compact keep the API notes")["customInstructions"],
            "keep the API notes"
        );
        assert_eq!(one(&mut d, "/name refactor")["type"], "set_session_name");
        let m = one(&mut d, "/model anthropic/claude-sonnet-4-5");
        assert_eq!(
            (m["provider"].as_str(), m["modelId"].as_str()),
            (Some("anthropic"), Some("claude-sonnet-4-5"))
        );
        assert_eq!(one(&mut d, "/thinking high")["level"], "high");
        assert_eq!(one(&mut d, "/new")["type"], "new_session");
        // Extension, template and skill commands are expanded by pi itself.
        let p = one(&mut d, "/skill:review src");
        assert_eq!(
            (p["type"].as_str(), p["message"].as_str()),
            (Some("prompt"), Some("/skill:review src"))
        );
        assert!(p.get("streamingBehavior").is_none());

        d.on_output(b"{\"type\":\"agent_start\"}\n");
        assert_eq!(one(&mut d, "also check tests")["streamingBehavior"], "steer");
    }

    #[test]
    fn prompts_with_images_carry_them_and_skip_builtins() {
        let mut d = PiDriver::new();
        let png = Attachment::from_bytes("image/png", b"PNG");
        let p = sent(
            &d.command(Command::Prompt {
                text: "what is this".into(),
                attachments: vec![png.clone()],
            })
            .unwrap(),
        )
        .remove(0);
        assert_eq!(p["type"], "prompt");
        assert_eq!(p["message"], "what is this");
        assert_eq!(
            p["images"],
            json!([{ "type": "image", "data": "UE5H", "mimeType": "image/png" }])
        );
        // A slash command with an image is for the model, not an RPC.
        let c = sent(
            &d.command(Command::Prompt {
                text: "/compact".into(),
                attachments: vec![png],
            })
            .unwrap(),
        )
        .remove(0);
        assert_eq!(c["type"], "prompt");
        assert_eq!(c["images"].as_array().unwrap().len(), 1);

        // pi echoes the user message; image-only shows as `[image]`.
        let events = d.on_output(
            br#"{"type":"message_start","message":{"role":"user","content":[{"type":"image","data":"UE5H","mimeType":"image/png"}]}}
"#,
        );
        assert!(events.contains(&Event::ItemDelta {
            id: "u1".into(),
            text: "[image]".into()
        }));
    }

    #[test]
    fn commands_map_to_rpc() {
        let mut d = PiDriver::new();
        let one = |d: &mut PiDriver, c: Command| sent(&d.command(c).unwrap()).remove(0)["type"].clone();
        assert_eq!(one(&mut d, Command::Interrupt), "abort");
        assert_eq!(one(&mut d, Command::Fork), "clone");
        assert_eq!(one(&mut d, Command::Shell { command: "ls".into() }), "bash");
        assert_eq!(
            one(
                &mut d,
                Command::SetOption {
                    id: OptionId::Effort,
                    value: "high".into()
                }
            ),
            "set_thinking_level"
        );
        assert!(matches!(
            d.command(Command::Review),
            Err(DriverError::Unsupported { .. })
        ));
        assert!(
            d.command(Command::SetOption {
                id: OptionId::Model,
                value: "nomodel".into()
            })
            .is_err()
        );
    }

    #[test]
    fn recorded_command_responses() {
        let mut d = PiDriver::new();
        // Mirror what the recorder sent: ids a..g. set_thinking_level is "c".
        d.pending.insert("c".into(), Pending::SetThinking("high".into()));
        let events = d.on_output(stdout(COMMANDS).as_bytes());
        let failed: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Notice { message } => message.split(' ').next(),
                _ => None,
            })
            .collect();
        assert_eq!(failed, ["set_model", "compact"]);
        assert_eq!(d.options()[1].current.as_deref(), Some("high"));
        // The shell command streamed "x" and completed.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ItemDelta { id, text } if id == "bash-e" && text == "x"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ItemCompleted { id, .. } if id == "bash-e"))
        );
        // Manual compaction outside a run returns to idle.
        assert!(states(&events).contains(&RunState::Idle));
    }

    #[test]
    fn session_switch_refreshes_state() {
        let mut d = PiDriver::new();
        let _ = d.command(Command::New).unwrap();
        assert!(d
            .on_output(b"{\"id\":\"acc-1\",\"type\":\"response\",\"command\":\"new_session\",\"success\":true,\"data\":{\"cancelled\":false}}\n")
            .is_empty());
        let next = sent(&d.poll_transmit());
        assert_eq!(next[0]["type"], "get_state");
        assert!(d.poll_transmit().is_empty());
        let events = d.on_output(b"{\"id\":\"acc-2\",\"type\":\"response\",\"command\":\"get_state\",\"success\":true,\"data\":{\"sessionId\":\"s2\",\"thinkingLevel\":\"off\",\"isStreaming\":false}}\n");
        assert!(matches!(&events[0], Event::SessionStarted { native_id, .. } if native_id == "s2"));
    }

    #[test]
    fn error_stop_ends_in_error_state() {
        let mut d = PiDriver::new();
        let events = d.on_output(
            b"{\"type\":\"agent_start\"}\n\
              {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"content\":[]}}\n\
              {\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[],\"stopReason\":\"error\",\"errorMessage\":\"429 rate limited\\nretry later\"}}\n\
              {\"type\":\"agent_settled\",\"aborted\":false}\n",
        );
        let last = events.iter().rev().find_map(|e| match e {
            Event::StateChanged(s) => Some(s.clone()),
            _ => None,
        });
        let last = last.unwrap();
        assert_eq!(last.state, RunState::Error);
        assert_eq!(last.message.as_deref(), Some("429 rate limited"));
    }

    /// Synthetic (pi isn't recorded with pi-subagents): an `async: true`
    /// launch, the turn ending while it runs, then pi-subagents' idle wake —
    /// the `subagent-notify` custom message and a steered prompt that starts
    /// a second run. Shapes from pi's `agent-session.ts` and pi-subagents'
    /// `async-execution.ts`/`notify.ts`.
    const ASYNC_SUBAGENT: &str = r#"{"type":"agent_start"}
{"type":"tool_execution_start","toolCallId":"call_1","toolName":"subagent","args":{"agent":"scout","task":"Map the repo","async":true}}
{"type":"tool_execution_end","toolCallId":"call_1","toolName":"subagent","result":{"content":[{"type":"text","text":"Async: scout [run-7]"}],"details":{"mode":"single","runId":"run-7","results":[],"asyncId":"run-7"}},"isError":false}
{"type":"agent_settled","aborted":false}
{"type":"message_start","message":{"role":"custom","customType":"subagent-notify","content":"scout completed: 12 files","display":false,"details":{"runs":[{"agent":"scout","status":"completed","source":"async","runId":"run-7"}]}}}
{"type":"message_end","message":{"role":"custom","customType":"subagent-notify","content":"scout completed: 12 files","display":false,"details":{"runs":[{"agent":"scout","status":"completed","source":"async","runId":"run-7"}]}}}
{"type":"agent_start"}
{"type":"agent_settled","aborted":false}
"#;

    #[test]
    fn a_run_left_waiting_on_an_async_subagent_is_not_done() {
        use crate::item::ItemDetail;
        use crate::transcript::{ItemStatus, Transcript as Items};
        let mut d = PiDriver::new();
        let mut t = Items::new();
        let mut states = Vec::new();
        let mut while_waiting = None;
        for line in ASYNC_SUBAGENT.split_inclusive('\n') {
            for e in d.on_output(line.as_bytes()) {
                t.settle(&e);
                t.apply(&e);
                if let Event::StateChanged(s) = &e {
                    states.push(s.state.clone());
                    if s.state == RunState::Waiting {
                        while_waiting = t.get("call_1").map(|i| (i.status, i.detail(Harness::Pi)));
                    }
                }
            }
        }
        assert_eq!(
            states,
            [
                RunState::Working { progress: None },
                RunState::Waiting,
                RunState::Working { progress: None },
                RunState::Done,
            ]
        );
        let (status, detail) = while_waiting.unwrap();
        assert_eq!(status, ItemStatus::Running);
        assert!(matches!(detail, ItemDetail::Subagent { status: Some(s), .. } if s == "running"));
        let item = t.get("call_1").unwrap();
        assert_eq!(item.status, ItemStatus::Completed);
        let ItemDetail::Subagent { status, output, .. } = item.detail(Harness::Pi) else {
            panic!()
        };
        assert_eq!(status.as_deref(), Some("completed"));
        assert_eq!(output.as_deref(), Some("scout completed: 12 files"));
    }

    #[test]
    fn transcript_waits_on_an_async_subagent() {
        let session = r#"{"type":"message","id":"u1","message":{"role":"user","content":"map it","timestamp":1}}
{"type":"message","id":"a1","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_1","name":"subagent","arguments":{"agent":"scout","async":true}}],"stopReason":"toolUse","timestamp":2}}
{"type":"message","id":"r1","message":{"role":"toolResult","toolCallId":"call_1","toolName":"subagent","content":[{"type":"text","text":"Async: scout [run-7]"}],"details":{"asyncId":"run-7","runId":"run-7"},"isError":false,"timestamp":3}}
{"type":"message","id":"a2","message":{"role":"assistant","content":[{"type":"text","text":"Launched."}],"stopReason":"stop","timestamp":4}}
{"type":"custom_message","id":"c1","customType":"subagent-notify","content":"scout completed","display":false,"details":{"runs":[{"agent":"scout","status":"completed","runId":"run-7"}]}}
{"type":"message","id":"u2","message":{"role":"user","content":"Inspect subagent updates above.","timestamp":5}}
{"type":"message","id":"a3","message":{"role":"assistant","content":[{"type":"text","text":"Done."}],"stopReason":"stop","timestamp":6}}
"#;
        let events = Transcript::new().feed(session.as_bytes());
        assert_eq!(
            states(&events),
            [
                RunState::Working { progress: None },
                RunState::Working { progress: None },
                RunState::Waiting,
                RunState::Working { progress: None },
                RunState::Done,
            ]
        );
        let call = |completed: bool| {
            events.iter().position(|e| match e {
                Event::ItemCompleted { id, .. } => completed && id == "call_1",
                Event::ItemUpdated { id, .. } => !completed && id == "call_1",
                _ => false,
            })
        };
        assert!(call(false) < call(true), "launched, then reported back");
    }

    #[test]
    fn transcript_replays_recorded_session() {
        let mut t = Transcript::new();
        let events = t.feed(SESSION.as_bytes());
        assert!(
            matches!(&events[0], Event::SessionStarted { native_id, .. } if native_id.starts_with("01a1"))
        );
        assert_eq!(
            states(&events),
            [
                RunState::Working { progress: None },
                RunState::Working { progress: None },
                RunState::Done,
            ]
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ItemCompleted { id, .. } if id == "call_1"))
        );
        assert!(matches!(events.last(), Some(Event::TurnCompleted)));
    }

    #[test]
    fn session_dir_encoding() {
        assert_eq!(
            session_dir("/home/u/my proj"),
            ".pi/agent/sessions/--home-u-my proj--"
        );
        assert_eq!(session_dir("C:\\src\\x"), ".pi/agent/sessions/--C--src-x--");
    }

    fn invoke(d: &mut PiDriver, name: &str, args: &str) -> Result<Vec<Value>, DriverError> {
        d.command(Command::Invoke {
            name: name.into(),
            args: args.into(),
        })
        .map(|b| sent(&b))
    }

    fn reply(id: &Value, command: &str, data: Value) -> String {
        format!(
            "{}\n",
            json!({ "type": "response", "id": id, "command": command, "success": true, "data": data })
        )
    }

    fn panel(events: &[Event]) -> &Panel {
        events
            .iter()
            .find_map(|e| match e {
                Event::Panel(p) => Some(p),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no panel in {events:?}"))
    }

    #[test]
    fn pickers_never_reach_the_model_without_a_value() {
        let mut d = PiDriver::new();
        let model = d.commands().iter().find(|c| c.name == "model").unwrap();
        assert_eq!(model.input, CommandInput::Option { id: OptionId::Model });
        assert!(matches!(
            invoke(&mut d, "model", ""),
            Err(DriverError::MissingArgument(_))
        ));
        assert!(matches!(
            invoke(&mut d, "thinking", ""),
            Err(DriverError::MissingArgument(_))
        ));
        assert_eq!(
            invoke(&mut d, "thinking", "high").unwrap()[0]["type"],
            "set_thinking_level"
        );
    }

    #[test]
    fn session_and_fork_panels() {
        let mut d = PiDriver::new();
        let req = invoke(&mut d, "session", "").unwrap();
        assert_eq!(req[0]["type"], "get_session_stats");
        let events = d.on_output(
            reply(
                &req[0]["id"],
                "get_session_stats",
                json!({ "sessionId": "abc", "userMessages": 2, "tokens": { "input": 10, "output": 5 }, "cost": 0.5 }),
            )
            .as_bytes(),
        );
        let rows = &panel(&events).sections[0].rows;
        assert_eq!(rows[0].value.as_deref(), Some("abc"));
        assert!(
            rows.iter()
                .any(|r| r.label == "Cost" && r.value.as_deref() == Some("$0.5000"))
        );
        // The stats still update usage.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Usage(u) if u.input_tokens == Some(10)))
        );

        let req = invoke(&mut d, "fork", "").unwrap();
        assert_eq!(req[0]["type"], "get_fork_messages");
        let events = d.on_output(
            reply(
                &req[0]["id"],
                "get_fork_messages",
                json!({ "messages": [{ "entryId": "e1", "text": "First prompt\nmore" }] }),
            )
            .as_bytes(),
        );
        let row = &panel(&events).sections[0].rows[0];
        assert_eq!(row.label, "First prompt");
        assert_eq!(row.actions[0].args, "e1");
        let fork = invoke(&mut d, "fork", "e1").unwrap();
        assert_eq!(fork[0]["type"], "fork");
        assert_eq!(fork[0]["entryId"], "e1");
    }

    #[test]
    fn settings_panel_toggles_and_refreshes() {
        let mut d = PiDriver::new();
        let req = invoke(&mut d, "settings", "").unwrap();
        let events = d.on_output(
            reply(
                &req[0]["id"],
                "get_state",
                json!({ "autoCompactionEnabled": true, "steeringMode": "all", "followUpMode": "one-at-a-time" }),
            )
            .as_bytes(),
        );
        let rows = &panel(&events).sections[0].rows;
        assert_eq!(rows[0].actions[0].args, "auto-compaction off");
        assert_eq!(rows[1].actions[0].args, "steering one-at-a-time");
        assert_eq!(rows[2].actions[0].args, "follow-up all");

        let set = invoke(&mut d, "settings", "auto-compaction off").unwrap();
        assert_eq!(set[0]["type"], "set_auto_compaction");
        assert_eq!(set[0]["enabled"], false);
        d.on_output(reply(&set[0]["id"], "set_auto_compaction", Value::Null).as_bytes());
        assert_eq!(sent(&d.poll_transmit())[0]["type"], "get_state");
    }
}

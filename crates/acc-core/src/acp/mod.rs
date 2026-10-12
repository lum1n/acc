//! Agent Client Protocol driver: the baseline for Cursor (`agent acp`),
//! Copilot (`copilot --acp`) and opencode (`opencode acp`).
//!
//! ACP is JSON-RPC 2.0 over NDJSON on stdio. ACC is the *client*: it sends
//! `initialize`, opens a session (`session/new`, `session/load` or
//! `session/resume`), sends `session/prompt` per turn and answers the agent's
//! `session/request_permission` requests. Everything else arrives as
//! `session/update` notifications. Targets protocol version 1 as published in
//! `@agentclientprotocol/sdk` 1.8.0, including `configOptions`.
//!
//! Several clients can share one detached agent (desktop and phone on the
//! same FIFO and log, see [`crate::detached`]). For that, request ids carry a
//! per-client tag and responses to other clients' requests are read by shape:
//! a `stopReason` ends the turn, `configOptions` update the options, and so on.
//! A client joining a running agent uses [`Start::Attach`], which skips the
//! handshake and adopts the session it sees in the log.

mod options;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

pub use options::{ConfigOption, ConfigValue};
use options::{OptionState, Route, set_config_params};

use crate::command::{
    Attachment, Command, CommandEntry, CommandInput, CommandSource, Exec, OptionDef, OptionId, offer,
    offer_option, validate_attachments,
};
use crate::driver::{Driver, DriverError};
use crate::event::{Event, ItemId, ItemKind, PermissionOption, Usage};
use crate::harness::{Harness, LaunchSpec, Protocol};
use crate::ndjson::{Frame, LineFramer};
use crate::state::{BlockKind, RunState, SessionState, StateSource};

/// The ACP protocol version this driver speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// Largest single message accepted from the agent. `session/load` replays
/// and tool diffs can be large; anything bigger is dropped, not buffered.
const MAX_LINE: usize = 16 << 20;

/// JSON-RPC "method not found", sent for agent requests ACC doesn't serve.
const METHOD_NOT_FOUND: i64 = -32601;
/// Cap on updates held for a session that is still opening.
const MAX_EARLY: usize = 1024;
/// ACP "authentication required".
const AUTH_REQUIRED: i64 = -32000;

/// How the driver gets a session once the agent is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// `session/new` in the working directory.
    New,
    /// Reopen a session by the agent's own id: `session/load` when the agent
    /// supports it (replays history as updates), otherwise `session/resume`.
    Resume { session_id: String },
    /// Join an agent another client already initialized (a detached session
    /// being re-attached or shared). No handshake; the session is adopted
    /// from the traffic in the log.
    Attach,
}

/// Per-session settings for [`AcpDriver`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpConfig {
    /// Absolute working directory on the host (ACP requires absolute paths).
    pub cwd: String,
    pub start: Start,
    /// Prefix for this client's JSON-RPC ids. Must be unique among clients
    /// sharing one agent, and per connection, so replayed responses to an
    /// earlier connection are never mistaken for this one's.
    pub client_tag: String,
    /// Model passed on the command line (`--model`), for agents that only
    /// take it at spawn (Cursor) or to pick the first turn's model.
    pub model: Option<String>,
}

impl AcpConfig {
    pub fn new(cwd: impl Into<String>, start: Start) -> Self {
        Self {
            cwd: cwd.into(),
            start,
            client_tag: "acc".into(),
            model: None,
        }
    }
}

/// What a request this client sent was for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    Initialize,
    Authenticate,
    NewSession,
    Load { session_id: String },
    Resume { session_id: String },
    Fork,
    Prompt,
    SetConfig,
    SetMode { mode: String },
    SetModel { model: String },
}

/// Where the connection is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Initializing,
    Opening,
    NeedsAuth,
    Ready,
    Failed,
}

/// A permission request the agent is waiting on.
#[derive(Debug, Clone)]
struct PermissionWait {
    /// The agent's JSON-RPC id, echoed verbatim in the answer.
    rpc_id: Value,
    tool_call_id: Option<String>,
    option_ids: Vec<String>,
}

/// Sans-IO ACP client for one agent process and one active session.
#[derive(Debug)]
pub struct AcpDriver {
    harness: Harness,
    cfg: AcpConfig,
    framer: LineFramer,
    next_id: u64,
    pending: BTreeMap<String, Pending>,
    outgoing: Vec<u8>,
    phase: Phase,

    // From `initialize`.
    load_session: bool,
    /// `promptCapabilities.image`: whether `session/prompt` may carry images.
    prompt_images: bool,
    resume_session: bool,
    fork_session: bool,
    auth_methods: Vec<(String, String)>,

    session_id: Option<String>,
    /// `session/load` is replaying history; don't treat it as live activity.
    replaying: bool,
    opts: OptionState,
    harness_commands: Vec<CommandEntry>,
    commands: Vec<CommandEntry>,
    permissions: BTreeMap<String, PermissionWait>,
    /// Updates for a session whose `session/new` or `session/fork` response
    /// hasn't arrived yet, replayed once it does.
    early: Vec<(String, Value)>,

    /// The message or thought currently streaming in chunks.
    open_item: Option<(ItemId, ItemKind)>,
    open_tools: BTreeSet<String>,
    /// Subagent tool calls that haven't been matched to a child session
    /// yet, oldest first, with the child session they run in themselves
    /// (`None` for the main conversation's).
    unclaimed_subagents: Vec<(String, Option<String>)>,
    /// Subagent tool calls whose kind has been reported.
    subagent_tools: BTreeSet<String>,
    /// Child session id → the subagent tool call it runs for.
    child_sessions: BTreeMap<String, String>,
    /// The message or thought streaming in each child session.
    child_open: BTreeMap<String, (ItemId, ItemKind)>,
    item_seq: u64,
    /// Prompts this client sent, as items, until the session takes them:
    /// ACP agents only repeat a prompt when `session/load` replays history.
    echoes: Vec<Event>,
    echo_seq: u64,
    /// Echoed prompts whose turn hasn't ended. An agent that repeats the
    /// prompt anyway is ignored meanwhile, so it doesn't show twice.
    echoed_turns: usize,
    /// Every prompt of the session in order, from the harness's own store
    /// (see [`Driver::history_script`]), and the next one's index: each turn
    /// takes one, whether its prompt is replayed, echoed or taken from here.
    history: Vec<String>,
    history_next: usize,
    turn_open: bool,

    state: Option<(RunState, Option<String>)>,
}

impl AcpDriver {
    /// A driver for one of the ACP-native harnesses.
    pub fn new(harness: Harness, cfg: AcpConfig) -> Self {
        let mut opts = OptionState {
            spawn_model: cfg.model.clone(),
            ..OptionState::default()
        };
        opts.rebuild(harness);
        let attach = cfg.start == Start::Attach;
        let mut d = Self {
            harness,
            cfg,
            framer: LineFramer::new(MAX_LINE),
            next_id: 0,
            pending: BTreeMap::new(),
            outgoing: Vec::new(),
            phase: if attach { Phase::Ready } else { Phase::Initializing },
            load_session: false,
            prompt_images: false,
            resume_session: false,
            fork_session: false,
            auth_methods: Vec::new(),
            session_id: None,
            replaying: false,
            opts,
            harness_commands: Vec::new(),
            commands: Vec::new(),
            permissions: BTreeMap::new(),
            early: Vec::new(),
            open_item: None,
            open_tools: BTreeSet::new(),
            unclaimed_subagents: Vec::new(),
            subagent_tools: BTreeSet::new(),
            child_sessions: BTreeMap::new(),
            child_open: BTreeMap::new(),
            item_seq: 0,
            echoes: Vec::new(),
            echo_seq: 0,
            echoed_turns: 0,
            history: Vec::new(),
            history_next: 0,
            turn_open: false,
            state: None,
        };
        d.rebuild_commands();
        d
    }

    /// The agent's id for the active session, once known.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Every config option the agent reports, including ones with no
    /// [`OptionId`] (Copilot's custom agent, opencode variants, ...).
    pub fn config_options(&self) -> &[ConfigOption] {
        &self.opts.config
    }

    /// `(id, name)` of the agent's authentication methods, from `initialize`.
    pub fn auth_methods(&self) -> &[(String, String)] {
        &self.auth_methods
    }

    /// Sets any config option by its ACP id (see [`Self::config_options`]).
    pub fn set_config(&mut self, config_id: &str, value: &str) -> Result<Vec<u8>, DriverError> {
        let sid = self.require_session()?;
        let boolean = self
            .opts
            .config
            .iter()
            .any(|o| o.id == config_id && matches!(o.value, ConfigValue::Boolean { .. }));
        let params = set_config_params(&sid, config_id, value, boolean);
        Ok(self.request("session/set_config_option", params, Pending::SetConfig))
    }

    /// Runs one of the agent's [`Self::auth_methods`], then opens the session.
    /// Some methods (Cursor `cursor_login`) open a browser on the host, so
    /// this is only sent when the user asks for it.
    pub fn authenticate(&mut self, method_id: &str) -> Vec<u8> {
        self.request(
            "authenticate",
            json!({ "methodId": method_id }),
            Pending::Authenticate,
        )
    }

    fn require_session(&self) -> Result<String, DriverError> {
        self.session_id.clone().ok_or(DriverError::NotStarted)
    }

    fn unsupported(&self, what: &'static str) -> DriverError {
        DriverError::Unsupported {
            harness: self.harness,
            what,
        }
    }

    // ---- Outgoing -------------------------------------------------------

    fn request(&mut self, method: &str, params: Value, kind: Pending) -> Vec<u8> {
        let id = format!("{}-{}", self.cfg.client_tag, self.next_id);
        self.next_id += 1;
        self.pending.insert(id.clone(), kind);
        line(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
    }

    fn notification(method: &str, params: Value) -> Vec<u8> {
        line(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn open_session(&mut self) -> Vec<u8> {
        let cwd = self.cfg.cwd.clone();
        match self.cfg.start.clone() {
            Start::New | Start::Attach => self.request(
                "session/new",
                json!({ "cwd": cwd, "mcpServers": [] }),
                Pending::NewSession,
            ),
            Start::Resume { session_id } => {
                let params = json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] });
                if self.load_session {
                    self.replaying = true;
                    self.request("session/load", params, Pending::Load { session_id })
                } else if self.resume_session {
                    self.request("session/resume", params, Pending::Resume { session_id })
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// `session/prompt` with text and image blocks. An image-only prompt
    /// carries no text block.
    fn prompt(&mut self, text: &str, attachments: &[Attachment]) -> Result<Vec<u8>, DriverError> {
        let sid = self.require_session()?;
        let mut prompt = Vec::new();
        if !text.is_empty() || attachments.is_empty() {
            prompt.push(json!({ "type": "text", "text": text }));
        }
        for a in attachments {
            prompt.push(json!({ "type": "image", "mimeType": a.media_type, "data": a.data }));
        }
        let bytes = self.request(
            "session/prompt",
            json!({ "sessionId": sid, "prompt": prompt }),
            Pending::Prompt,
        );
        Ok(bytes)
    }

    fn echo_prompt(&mut self, text: &str) {
        self.echo_seq += 1;
        self.echoed_turns += 1;
        self.history_next += 1;
        self.turn_open = true;
        push_prompt(format!("prompt-{}", self.echo_seq), text, &mut self.echoes);
    }

    /// A turn whose prompt nobody showed began: show it from the history.
    fn open_turn(&mut self, out: &mut Vec<Event>) {
        if self.turn_open {
            return;
        }
        self.turn_open = true;
        let n = self.history_next;
        self.history_next += 1;
        if let Some(text) = self.history.get(n).cloned() {
            push_prompt(format!("history-{n}"), &text, out);
        }
    }

    /// Sends `/name args` if the agent advertises one of `names`, or if the
    /// harness is known to handle the first name without advertising it.
    fn slash(&mut self, names: &[&str], args: &str, what: &'static str) -> Result<Vec<u8>, DriverError> {
        let advertised = names
            .iter()
            .find(|n| self.harness_commands.iter().any(|c| c.name == **n));
        // opencode 1.18.35 runs `/compact` over ACP without listing it.
        let known = names
            .first()
            .filter(|_| self.harness == Harness::Opencode && what == "compact");
        let name = advertised.or(known).ok_or_else(|| self.unsupported(what))?;
        let text = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        self.prompt(&text, &[])
    }

    fn answer_permission(&mut self, rpc_id: Value, outcome: Value) {
        self.outgoing.extend(line(
            &json!({ "jsonrpc": "2.0", "id": rpc_id, "result": { "outcome": outcome } }),
        ));
    }

    // ---- Incoming -------------------------------------------------------

    fn handle_line(&mut self, text: &str, out: &mut Vec<Event>) {
        let Ok(msg) = serde_json::from_str::<Value>(text) else {
            out.push(Event::Unrecognized { raw: text.to_owned() });
            return;
        };
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").filter(|v| !v.is_null());
        match (method, id) {
            (Some(m), Some(id)) => self.on_agent_request(m, id.clone(), &msg, out),
            (Some(m), None) => self.on_notification(m, &msg, out),
            (None, Some(id)) if msg.get("result").is_some() || msg.get("error").is_some() => {
                self.on_response(id, &msg, out);
            }
            _ => out.push(Event::Unrecognized { raw: text.to_owned() }),
        }
    }

    fn on_response(&mut self, id: &Value, msg: &Value, out: &mut Vec<Event>) {
        let key = id_key(id);
        let result = msg.get("result");
        let error = msg.get("error");
        let Some(kind) = self.pending.remove(&key) else {
            // Another client's request (shared agent, or an earlier connection
            // in a replayed log). Learn what we can from the shape.
            if let Some(r) = result {
                self.observe_foreign_result(r, out);
            }
            return;
        };
        if let Some(err) = error {
            self.on_error(kind, err, out);
            return;
        }
        let r = result.cloned().unwrap_or(Value::Null);
        match kind {
            Pending::Initialize => self.on_initialized(&r, out),
            Pending::Authenticate => {
                self.phase = Phase::Opening;
                let bytes = self.open_session();
                self.outgoing.extend(bytes);
            }
            Pending::NewSession | Pending::Fork => {
                let sid = r.get("sessionId").and_then(Value::as_str).map(str::to_owned);
                if let Some(sid) = sid {
                    self.on_session_open(sid, &r, out);
                } else {
                    self.fail("agent returned no sessionId", out);
                }
            }
            Pending::Load { session_id } | Pending::Resume { session_id } => {
                self.on_session_open(session_id, &r, out);
            }
            Pending::Prompt => {
                self.echoed_turns = self.echoed_turns.saturating_sub(1);
                self.on_turn_end(&r, out);
            }
            Pending::SetConfig => {
                if r.get("configOptions").is_some() {
                    self.opts.absorb_response(&r);
                    self.options_changed(out);
                }
            }
            Pending::SetMode { mode } => {
                self.opts.set_current_mode(&mode);
                self.options_changed(out);
            }
            Pending::SetModel { model } => {
                self.opts.set_current_model(&model);
                self.options_changed(out);
            }
        }
    }

    fn on_error(&mut self, kind: Pending, err: &Value, out: &mut Vec<Event>) {
        let code = err.get("code").and_then(Value::as_i64);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_owned();
        match kind {
            Pending::NewSession | Pending::Load { .. } | Pending::Resume { .. }
                if code == Some(AUTH_REQUIRED) =>
            {
                self.replaying = false;
                self.phase = Phase::NeedsAuth;
                let methods = self
                    .auth_methods
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let msg = if methods.is_empty() {
                    message
                } else {
                    format!("{message} (methods: {methods})")
                };
                self.set_state(
                    RunState::Blocked {
                        kind: BlockKind::Auth,
                    },
                    Some(msg),
                    out,
                );
            }
            Pending::Initialize
            | Pending::Authenticate
            | Pending::NewSession
            | Pending::Load { .. }
            | Pending::Resume { .. } => {
                self.replaying = false;
                self.fail(&message, out);
            }
            Pending::Prompt => {
                self.echoed_turns = self.echoed_turns.saturating_sub(1);
                self.turn_open = false;
                self.close_turn_items(out);
                self.set_state(RunState::Error, Some(message), out);
                out.push(Event::TurnCompleted);
            }
            // A rejected fork or setting leaves the session as it was.
            Pending::Fork | Pending::SetConfig | Pending::SetMode { .. } | Pending::SetModel { .. } => {
                out.push(Event::Native {
                    kind: "request_failed".into(),
                    payload: err.clone(),
                });
            }
        }
    }

    fn fail(&mut self, message: &str, out: &mut Vec<Event>) {
        self.phase = Phase::Failed;
        self.set_state(RunState::Error, Some(message.to_owned()), out);
    }

    fn on_initialized(&mut self, r: &Value, out: &mut Vec<Event>) {
        let version = r.get("protocolVersion").and_then(Value::as_u64);
        if version != Some(PROTOCOL_VERSION) {
            let v = version.map_or_else(|| "none".to_owned(), |v| v.to_string());
            self.fail(
                &format!("agent speaks ACP version {v}, ACC speaks {PROTOCOL_VERSION}"),
                out,
            );
            return;
        }
        self.absorb_capabilities(r);
        if matches!(self.cfg.start, Start::Resume { .. }) && !self.load_session && !self.resume_session {
            self.fail("agent can't load or resume sessions", out);
            return;
        }
        self.phase = Phase::Opening;
        let bytes = self.open_session();
        self.outgoing.extend(bytes);
    }

    fn absorb_capabilities(&mut self, r: &Value) {
        let caps = r.get("agentCapabilities").unwrap_or(&Value::Null);
        let session_caps = caps.get("sessionCapabilities").unwrap_or(&Value::Null);
        let has = |k: &str| session_caps.get(k).is_some_and(|v| !v.is_null());
        self.load_session = caps.get("loadSession").and_then(Value::as_bool).unwrap_or(false);
        self.prompt_images = caps
            .pointer("/promptCapabilities/image")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.resume_session = has("resume");
        self.fork_session = has("fork");
        self.auth_methods = r
            .get("authMethods")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let id = m.get("id")?.as_str()?.to_owned();
                let name = m.get("name").and_then(Value::as_str).unwrap_or(&id).to_owned();
                Some((id, name))
            })
            .collect();
        self.rebuild_commands();
    }

    fn on_session_open(&mut self, session_id: String, r: &Value, out: &mut Vec<Event>) {
        if self.session_id.as_ref().is_some_and(|old| *old != session_id) {
            self.reset_session();
        }
        self.replaying = false;
        self.turn_open = false;
        self.phase = Phase::Ready;
        self.close_open_item(out);
        self.opts.absorb_response(r);
        self.opts.rebuild(self.harness);
        self.session_id = Some(session_id.clone());
        out.push(Event::SessionStarted {
            harness: self.harness,
            native_id: session_id,
            model: self.opts.current_model(),
        });
        out.push(Event::OptionsChanged);
        self.refresh_palette(out);
        self.set_state(RunState::Idle, None, out);
        for (sid, msg) in std::mem::take(&mut self.early) {
            if self.session_id.as_deref() == Some(sid.as_str()) {
                self.on_notification("session/update", &msg, out);
            }
        }
    }

    fn reset_session(&mut self) {
        self.permissions.clear();
        self.open_item = None;
        self.open_tools.clear();
        self.unclaimed_subagents.clear();
        self.subagent_tools.clear();
        self.child_sessions.clear();
        self.child_open.clear();
        self.harness_commands.clear();
        self.rebuild_commands();
        self.history.clear();
        self.history_next = 0;
    }

    /// Completes whatever the turn left open. A cancelled or failed turn can
    /// leave tool calls without a final status.
    fn close_turn_items(&mut self, out: &mut Vec<Event>) {
        self.close_open_item(out);
        for (_, (id, _)) in std::mem::take(&mut self.child_open) {
            out.push(Event::ItemCompleted {
                id,
                extensions: Value::Null,
            });
        }
        for id in std::mem::take(&mut self.open_tools) {
            out.push(Event::ItemCompleted {
                id,
                extensions: Value::Null,
            });
        }
        for request_id in std::mem::take(&mut self.permissions).into_keys() {
            out.push(Event::PermissionResolved { request_id });
        }
    }

    fn on_turn_end(&mut self, r: &Value, out: &mut Vec<Event>) {
        self.turn_open = false;
        self.close_turn_items(out);
        if let Some(u) = r.get("usage").filter(|u| u.is_object()) {
            let n = |k: &str| u.get(k).and_then(Value::as_u64);
            out.push(Event::Usage(Usage {
                input_tokens: n("inputTokens"),
                output_tokens: n("outputTokens"),
                cache_read_tokens: n("cachedReadTokens"),
                cache_write_tokens: n("cachedWriteTokens"),
                ..Usage::default()
            }));
        }
        let stop = r.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn");
        let (state, message) = match stop {
            "end_turn" => (RunState::Done, None),
            "cancelled" => (RunState::Idle, None),
            other => (RunState::Done, Some(other.to_owned())),
        };
        self.set_state(state, message, out);
        out.push(Event::TurnCompleted);
    }

    /// A response to a request this client didn't send.
    fn observe_foreign_result(&mut self, r: &Value, out: &mut Vec<Event>) {
        if r.get("agentCapabilities").is_some() {
            self.absorb_capabilities(r);
        } else if r.get("stopReason").is_some() {
            self.on_turn_end(r, out);
        } else if let Some(sid) = r.get("sessionId").and_then(Value::as_str) {
            // Another client opened or forked a session; follow it.
            self.on_session_open(sid.to_owned(), r, out);
        } else if r.get("configOptions").is_some() || r.get("modes").is_some() {
            // Also how another client's `session/load` ends, after the
            // history it replayed.
            self.turn_open = false;
            self.opts.absorb_response(r);
            self.options_changed(out);
        }
    }

    fn on_agent_request(&mut self, method: &str, id: Value, msg: &Value, out: &mut Vec<Event>) {
        let params = msg.get("params").unwrap_or(&Value::Null);
        if method == "session/request_permission" {
            self.on_permission(id, params, out);
            return;
        }
        // ACC advertises no fs, terminal or elicitation capability. Answer
        // anything else (including extensions like `cursor/ask_question`) so
        // the agent never waits on a request nobody will serve.
        self.outgoing.extend(line(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": METHOD_NOT_FOUND, "message": format!("{method} is not supported by ACC") },
        })));
        out.push(Event::Native {
            kind: method.to_owned(),
            payload: params.clone(),
        });
    }

    fn on_permission(&mut self, id: Value, params: &Value, out: &mut Vec<Event>) {
        let tool = params.get("toolCall").unwrap_or(&Value::Null);
        let tool_call_id = tool.get("toolCallId").and_then(Value::as_str).map(str::to_owned);
        if let Some(tid) = &tool_call_id
            && !self.open_tools.contains(tid)
        {
            self.start_tool(tid.clone(), tool, None, out);
        }
        let options: Vec<PermissionOption> = params
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|o| {
                let id = o.get("optionId")?.as_str()?.to_owned();
                let label = o.get("name").and_then(Value::as_str).unwrap_or(&id).to_owned();
                Some(PermissionOption { id, label })
            })
            .collect();
        let title = tool
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Permission requested")
            .to_owned();
        let request_id = id_key(&id);
        self.permissions.insert(
            request_id.clone(),
            PermissionWait {
                rpc_id: id,
                tool_call_id,
                option_ids: options.iter().map(|o| o.id.clone()).collect(),
            },
        );
        let input = tool.get("rawInput").cloned().unwrap_or(Value::Null);
        let item_id = self
            .permissions
            .get(&request_id)
            .and_then(|w| w.tool_call_id.clone());
        out.push(Event::PermissionRequested {
            request_id,
            title: title.clone(),
            options,
            kind: BlockKind::Permission,
            item_id,
            input,
        });
        self.set_state(
            RunState::Blocked {
                kind: BlockKind::Permission,
            },
            Some(title),
            out,
        );
    }

    fn on_notification(&mut self, method: &str, msg: &Value, out: &mut Vec<Event>) {
        let params = msg.get("params").unwrap_or(&Value::Null);
        if method != "session/update" {
            out.push(Event::Native {
                kind: method.to_owned(),
                payload: params.clone(),
            });
            return;
        }
        let sid = params.get("sessionId").and_then(Value::as_str);
        match (&self.session_id, sid) {
            (Some(cur), Some(sid)) if cur != sid => {
                // Updates for a session being opened can beat its response.
                let opening = self
                    .pending
                    .values()
                    .any(|p| matches!(p, Pending::NewSession | Pending::Fork));
                if opening && self.early.len() < MAX_EARLY {
                    self.early.push((sid.to_owned(), msg.clone()));
                }
                return;
            }
            (None, Some(sid)) if self.cfg.start == Start::Attach => {
                self.session_id = Some(sid.to_owned());
                out.push(Event::SessionStarted {
                    harness: self.harness,
                    native_id: sid.to_owned(),
                    model: self.opts.current_model(),
                });
            }
            _ => {}
        }
        let Some(update) = params.get("update") else {
            return;
        };
        let kind = update.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
        if let Some(child) = update.pointer("/_meta/opencode~1child-session") {
            self.on_child_update(kind, child, update, out);
            return;
        }
        match kind {
            "agent_message_chunk" => self.chunk(ItemKind::AgentMessage, update, out),
            "agent_thought_chunk" => self.chunk(ItemKind::Reasoning, update, out),
            "user_message_chunk" if self.echoed_turns > 0 && !self.replaying => {}
            "user_message_chunk" => self.chunk(ItemKind::UserMessage, update, out),
            "tool_call" => self.on_tool_call(update, out),
            "tool_call_update" => self.on_tool_update(update, out),
            "available_commands_update" => {
                self.harness_commands = parse_commands(update.get("availableCommands"));
                self.rebuild_commands();
                out.push(Event::CommandsChanged);
            }
            "config_option_update" => {
                self.opts.absorb_response(update);
                self.options_changed(out);
            }
            "current_mode_update" => {
                if let Some(mode) = update.get("currentModeId").and_then(Value::as_str) {
                    self.opts.set_current_mode(mode);
                    self.options_changed(out);
                }
            }
            "usage_update" => {
                let n = |k: &str| update.get(k).and_then(Value::as_u64);
                let cost = update.get("cost");
                let usd = cost
                    .filter(|c| c.get("currency").and_then(Value::as_str) == Some("USD"))
                    .and_then(|c| c.get("amount"))
                    .and_then(Value::as_f64);
                out.push(Event::Usage(Usage {
                    context_used: n("used"),
                    context_window: n("size"),
                    cost_usd: usd,
                    ..Usage::default()
                }));
            }
            "compaction_update" => {
                if !self.replaying && update.get("status").and_then(Value::as_str) == Some("in_progress") {
                    self.set_state(
                        RunState::Working { progress: None },
                        Some("Compacting".into()),
                        out,
                    );
                }
                out.push(native(kind, update));
            }
            "notice" => {
                let title = update.get("title").and_then(Value::as_str).unwrap_or("");
                out.push(Event::Notice {
                    message: title.to_owned(),
                });
            }
            _ => out.push(native(kind, update)),
        }
    }

    /// Marks live agent activity (also covers turns another client started).
    fn working(&mut self, out: &mut Vec<Event>) {
        if self.replaying {
            return;
        }
        self.open_turn(out);
        let blocked = matches!(self.state, Some((RunState::Blocked { .. }, _)));
        if !blocked || self.permissions.is_empty() {
            self.set_state(RunState::Working { progress: None }, None, out);
        }
    }

    fn chunk(&mut self, kind: ItemKind, update: &Value, out: &mut Vec<Event>) {
        let content = update.get("content").unwrap_or(&Value::Null);
        let message_id = update.get("messageId").and_then(Value::as_str);
        if kind != ItemKind::UserMessage {
            self.working(out);
        }
        let same = self
            .open_item
            .as_ref()
            .is_some_and(|(id, k)| *k == kind && message_id.is_none_or(|m| m == id));
        if !same {
            self.close_open_item(out);
            if kind == ItemKind::UserMessage {
                self.history_next += 1;
                self.turn_open = true;
            }
            let id = message_id.map_or_else(
                || {
                    self.item_seq += 1;
                    format!("item-{}", self.item_seq)
                },
                str::to_owned,
            );
            out.push(Event::ItemStarted {
                id: id.clone(),
                kind,
                parent: None,
                extensions: Value::Null,
            });
            self.open_item = Some((id, kind));
        }
        let Some((id, _)) = &self.open_item else { return };
        match content.get("text").and_then(Value::as_str) {
            Some(text) if content.get("type").and_then(Value::as_str) == Some("text") => {
                out.push(Event::ItemDelta {
                    id: id.clone(),
                    text: text.to_owned(),
                });
            }
            // Images, audio and resources ride along as updates to the item.
            _ => out.push(Event::ItemUpdated {
                id: id.clone(),
                extensions: content.clone(),
            }),
        }
    }

    fn close_open_item(&mut self, out: &mut Vec<Event>) {
        if let Some((id, _)) = self.open_item.take() {
            out.push(Event::ItemCompleted {
                id,
                extensions: Value::Null,
            });
        }
    }

    fn start_tool(
        &mut self,
        id: String,
        update: &Value,
        parent: Option<(String, String)>,
        out: &mut Vec<Event>,
    ) {
        match &parent {
            Some((child, _)) => self.close_child_item(child, out),
            None => self.close_open_item(out),
        }
        let kind = if crate::item::acp_is_subagent(update) {
            self.note_subagent(&id, parent.as_ref().map(|(c, _)| c.clone()));
            ItemKind::Subagent
        } else {
            match update.get("kind").and_then(Value::as_str) {
                Some("execute") => ItemKind::CommandExecution,
                Some("edit" | "delete" | "move") => ItemKind::FileChange,
                _ => ItemKind::ToolCall,
            }
        };
        self.open_tools.insert(id.clone());
        out.push(Event::ItemStarted {
            id,
            kind,
            parent: parent.map(|(_, item)| item),
            extensions: strip_tag(update),
        });
    }

    /// Records a subagent tool call, so the child session that shows up
    /// next under its session can be matched to it.
    fn note_subagent(&mut self, id: &str, session: Option<String>) {
        if self.subagent_tools.insert(id.to_owned()) {
            self.unclaimed_subagents.push((id.to_owned(), session));
        }
    }

    /// A tool call first reported without what identifies it as a subagent
    /// (opencode sends the input in a later update): report its kind now.
    fn upgrade_to_subagent(
        &mut self,
        id: &str,
        update: &Value,
        session: Option<String>,
        out: &mut Vec<Event>,
    ) {
        if self.subagent_tools.contains(id) || !crate::item::acp_is_subagent(update) {
            return;
        }
        self.note_subagent(id, session);
        out.push(Event::ItemStarted {
            id: id.to_owned(),
            kind: ItemKind::Subagent,
            parent: None,
            extensions: strip_tag(update),
        });
    }

    /// The subagent tool call a child session runs for. opencode streams a
    /// child session's updates under the parent's session id, marked with
    /// `_meta["opencode/child-session"] = {id, parentID, depth, title}`,
    /// before the tool's result names the session: the first update claims
    /// the oldest unmatched subagent call of its parent session.
    fn subagent_for(&mut self, child: &str, parent_session: Option<&str>) -> Option<String> {
        if let Some(item) = self.child_sessions.get(child) {
            return Some(item.clone());
        }
        let ours = |s: &Option<String>| match (s, parent_session) {
            (None, None) => true,
            (None, Some(p)) => self.session_id.as_deref() == Some(p),
            (Some(s), Some(p)) => s == p,
            (Some(_), None) => false,
        };
        let i = self.unclaimed_subagents.iter().position(|(_, s)| ours(s))?;
        let (item, _) = self.unclaimed_subagents.remove(i);
        self.child_sessions.insert(child.to_owned(), item.clone());
        Some(item)
    }

    fn on_child_update(&mut self, kind: &str, meta: &Value, update: &Value, out: &mut Vec<Event>) {
        let Some(child) = meta.get("id").and_then(Value::as_str).map(str::to_owned) else {
            return;
        };
        let parent_session = meta.get("parentID").and_then(Value::as_str);
        let Some(parent) = self.subagent_for(&child, parent_session) else {
            // No subagent call to hang it on; keep it visible as native.
            out.push(native(kind, update));
            return;
        };
        if !self.replaying {
            self.working(out);
        }
        match kind {
            "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" => {
                let item_kind = match kind {
                    "agent_message_chunk" => ItemKind::AgentMessage,
                    "agent_thought_chunk" => ItemKind::Reasoning,
                    _ => ItemKind::UserMessage,
                };
                let message_id = update.get("messageId").and_then(Value::as_str);
                let same = self
                    .child_open
                    .get(&child)
                    .is_some_and(|(id, k)| *k == item_kind && message_id.is_none_or(|m| m == id));
                if !same {
                    self.close_child_item(&child, out);
                    let id = message_id.map_or_else(
                        || {
                            self.item_seq += 1;
                            format!("item-{}", self.item_seq)
                        },
                        str::to_owned,
                    );
                    out.push(Event::ItemStarted {
                        id: id.clone(),
                        kind: item_kind,
                        parent: Some(parent.clone()),
                        extensions: Value::Null,
                    });
                    self.child_open.insert(child.clone(), (id, item_kind));
                }
                let Some((id, _)) = self.child_open.get(&child) else {
                    return;
                };
                let content = update.get("content").unwrap_or(&Value::Null);
                match content.get("text").and_then(Value::as_str) {
                    Some(text) if content.get("type").and_then(Value::as_str) == Some("text") => {
                        out.push(Event::ItemDelta {
                            id: id.clone(),
                            text: text.to_owned(),
                        });
                    }
                    _ => out.push(Event::ItemUpdated {
                        id: id.clone(),
                        extensions: content.clone(),
                    }),
                }
            }
            "tool_call" | "tool_call_update" => {
                let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
                    out.push(native(kind, update));
                    return;
                };
                if self.open_tools.contains(id) {
                    self.upgrade_to_subagent(id, update, Some(child.clone()), out);
                    if !is_final(update) {
                        out.push(Event::ItemUpdated {
                            id: id.to_owned(),
                            extensions: strip_tag(update),
                        });
                    }
                } else {
                    self.start_tool(id.to_owned(), update, Some((child, parent)), out);
                }
                self.finish_tool_if_done(id, update, out);
            }
            // Usage, plans and the rest of a child session are its own.
            _ => out.push(native(kind, update)),
        }
    }

    fn close_child_item(&mut self, child: &str, out: &mut Vec<Event>) {
        if let Some((id, _)) = self.child_open.remove(child) {
            out.push(Event::ItemCompleted {
                id,
                extensions: Value::Null,
            });
        }
    }

    fn on_tool_call(&mut self, update: &Value, out: &mut Vec<Event>) {
        let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
            out.push(native("tool_call", update));
            return;
        };
        self.working(out);
        if self.open_tools.contains(id) {
            self.upgrade_to_subagent(id, update, None, out);
            out.push(Event::ItemUpdated {
                id: id.to_owned(),
                extensions: strip_tag(update),
            });
        } else {
            self.start_tool(id.to_owned(), update, None, out);
        }
        self.finish_tool_if_done(id, update, out);
    }

    fn on_tool_update(&mut self, update: &Value, out: &mut Vec<Event>) {
        let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
            out.push(native("tool_call_update", update));
            return;
        };
        // Progress on a tool call means its permission was answered,
        // possibly by another client sharing this agent.
        let answered: Vec<String> = self
            .permissions
            .iter()
            .filter(|(_, w)| w.tool_call_id.as_deref() == Some(id))
            .map(|(k, _)| k.clone())
            .collect();
        for request_id in answered {
            self.permissions.remove(&request_id);
            out.push(Event::PermissionResolved { request_id });
        }
        self.working(out);
        if !self.open_tools.contains(id) {
            self.start_tool(id.to_owned(), update, None, out);
        } else if !is_final(update) {
            self.upgrade_to_subagent(id, update, None, out);
            out.push(Event::ItemUpdated {
                id: id.to_owned(),
                extensions: strip_tag(update),
            });
        }
        self.finish_tool_if_done(id, update, out);
    }

    fn finish_tool_if_done(&mut self, id: &str, update: &Value, out: &mut Vec<Event>) {
        if is_final(update) && self.open_tools.remove(id) {
            // A finished subagent: its child session's last message is done.
            let children: Vec<String> = self
                .child_sessions
                .iter()
                .filter(|(_, item)| *item == id)
                .map(|(c, _)| c.clone())
                .collect();
            for child in children {
                self.close_child_item(&child, out);
            }
            self.unclaimed_subagents.retain(|(t, _)| t != id);
            out.push(Event::ItemCompleted {
                id: id.to_owned(),
                extensions: strip_tag(update),
            });
        }
    }

    fn set_state(&mut self, state: RunState, message: Option<String>, out: &mut Vec<Event>) {
        let next = (state, message);
        if self.state.as_ref() == Some(&next) {
            return;
        }
        let (state, message) = next.clone();
        self.state = Some(next);
        out.push(Event::StateChanged(SessionState {
            state,
            message,
            source: StateSource::Native,
            // Drivers have no clock; the session layer stamps it on receipt.
            observed_at_ms: 0,
        }));
    }

    fn options_changed(&mut self, out: &mut Vec<Event>) {
        self.opts.rebuild(self.harness);
        out.push(Event::OptionsChanged);
        self.refresh_palette(out);
    }

    /// Rebuilds the palette after the options changed, reporting it when
    /// pickers came or went.
    fn refresh_palette(&mut self, out: &mut Vec<Event>) {
        let before: Vec<String> = self.commands.iter().map(|c| c.name.clone()).collect();
        self.rebuild_commands();
        if self.commands.iter().map(|c| &c.name).ne(before.iter()) {
            out.push(Event::CommandsChanged);
        }
    }

    /// Harness commands first, then ACC's canonical ones the harness lacks,
    /// then pickers for the options the agent reports.
    fn rebuild_commands(&mut self) {
        let mut all = self.harness_commands.clone();
        let rpc =
            |name: &str, description: &str, method: &str| CommandEntry::rpc(name, description, None, method);
        offer(
            &mut all,
            rpc("interrupt", "Stop the current turn", "session/cancel"),
        );
        offer(&mut all, rpc("new", "Start a new session", "session/new"));
        if self.fork_session {
            offer(&mut all, rpc("fork", "Fork this session", "session/fork"));
        }
        // opencode 1.18.35 and 2.0.14 run `/compact` without listing it.
        if self.harness == Harness::Opencode {
            offer(
                &mut all,
                CommandEntry {
                    source: CommandSource::Builtin,
                    exec: Exec::Text,
                    ..rpc("compact", "Summarize the conversation to free context", "")
                },
            );
        }
        for (id, name, description) in OPTION_COMMANDS {
            let method = match self.opts.route(id) {
                Some(Route::SpawnFlag) => "restart",
                Some(Route::Config { .. }) => "session/set_config_option",
                Some(Route::LegacyMode) => "session/set_mode",
                Some(Route::LegacyModel) => "session/set_model",
                None => continue,
            };
            offer_option(
                &mut all,
                &self.opts.defs,
                id,
                CommandEntry::rpc(name, description, Some("<value>"), method),
            );
        }
        self.commands = all;
    }
}

/// Palette names for option pickers.
const OPTION_COMMANDS: [(OptionId, &str, &str); 4] = [
    (OptionId::Model, "model", "Choose the model"),
    (OptionId::Effort, "effort", "Choose reasoning effort"),
    (OptionId::Mode, "mode", "Choose the agent mode"),
    (
        OptionId::Permission,
        "permissions",
        "Choose what the agent may do without asking",
    ),
];

impl Driver for AcpDriver {
    fn harness(&self) -> Harness {
        self.harness
    }

    /// The ACP entry point. Resume happens in-protocol ([`Start::Resume`]),
    /// so `resume` doesn't change the command line.
    fn launch(&self, _resume: Option<&str>) -> LaunchSpec {
        launch_spec(self.harness, self.opts.spawn_model.as_deref())
    }

    fn handshake(&mut self) -> Vec<u8> {
        let sent = self.pending.values().any(|p| *p == Pending::Initialize);
        if self.cfg.start == Start::Attach || self.phase != Phase::Initializing || sent {
            return Vec::new();
        }
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false,
                    "session": { "configOptions": { "boolean": {} } },
                },
                "clientInfo": { "name": "acc", "title": "Agent Control Core", "version": crate::VERSION },
            }),
            Pending::Initialize,
        )
    }

    fn on_output(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        for frame in self.framer.feed(bytes) {
            match frame {
                Frame::Line(text) => self.handle_line(&text, &mut out),
                Frame::Oversized => out.push(Event::Unrecognized {
                    raw: format!("(line over {MAX_LINE} bytes dropped)"),
                }),
                Frame::InvalidUtf8 => out.push(Event::Unrecognized {
                    raw: "(line with invalid UTF-8 dropped)".into(),
                }),
            }
        }
        out
    }

    fn poll_transmit(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.outgoing)
    }

    fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.echoes)
    }

    /// Copilot keeps every prompt in its own event log; ACP only replays
    /// them on `session/load`, which would repeat the history to every
    /// client following the session's output.
    fn history_script(&self, native_id: &str) -> Option<String> {
        let safe = !native_id.is_empty() && native_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        (self.harness == Harness::Copilot && safe).then(|| {
            format!(
                "grep -F '\"user.message\"' \"$HOME/.copilot/session-state/{native_id}/events.jsonl\" 2>/dev/null || :\n"
            )
        })
    }

    fn on_history(&mut self, output: &str) {
        self.history = output
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v.get("type").and_then(Value::as_str) == Some("user.message"))
            .filter_map(|v| {
                v.pointer("/data/content")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
    }

    fn command(&mut self, cmd: Command) -> Result<Vec<u8>, DriverError> {
        match cmd {
            Command::Prompt { text, attachments } => {
                validate_attachments(&attachments)?;
                if !attachments.is_empty() && !self.prompt_images {
                    return Err(DriverError::Unsupported {
                        harness: self.harness,
                        what: "images (the agent did not advertise promptCapabilities.image)",
                    });
                }
                let bytes = self.prompt(&text, &attachments)?;
                self.echo_prompt(if text.is_empty() { "[image]" } else { &text });
                Ok(bytes)
            }
            Command::Interrupt => {
                let sid = self.require_session()?;
                // The spec requires answering open permission requests with
                // `cancelled` once the client cancels.
                for (_, wait) in std::mem::take(&mut self.permissions) {
                    self.answer_permission(wait.rpc_id, json!({ "outcome": "cancelled" }));
                }
                let mut bytes = std::mem::take(&mut self.outgoing);
                bytes.extend(Self::notification("session/cancel", json!({ "sessionId": sid })));
                Ok(bytes)
            }
            Command::New => {
                if self.phase == Phase::Initializing || self.phase == Phase::Failed {
                    return Err(DriverError::NotStarted);
                }
                let cwd = self.cfg.cwd.clone();
                Ok(self.request(
                    "session/new",
                    json!({ "cwd": cwd, "mcpServers": [] }),
                    Pending::NewSession,
                ))
            }
            Command::Fork => {
                if !self.fork_session {
                    return Err(self.unsupported("fork"));
                }
                let sid = self.require_session()?;
                let cwd = self.cfg.cwd.clone();
                Ok(self.request(
                    "session/fork",
                    json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }),
                    Pending::Fork,
                ))
            }
            Command::Compact { hint } => self.slash(
                &["compact", "summarize", "compress"],
                hint.as_deref().unwrap_or(""),
                "compact",
            ),
            Command::Rename { name } => self.slash(&["rename"], &name, "rename"),
            Command::Review => self.slash(&["review"], "", "review"),
            Command::Shell { .. } => Err(self.unsupported("shell commands")),
            Command::Invoke { name, args } => self.invoke(&name, &args),
            // ACP agents push commands, options and usage as they change;
            // there is nothing to ask for.
            Command::Refresh => Ok(Vec::new()),
            Command::SetOption { id, value } => self.set_option(id, value),
            Command::AnswerPermission {
                request_id,
                option_id,
            } => {
                let wait = self
                    .permissions
                    .remove(&request_id)
                    .ok_or_else(|| DriverError::UnknownRequest(request_id.clone()))?;
                if !wait.option_ids.is_empty() && !wait.option_ids.contains(&option_id) {
                    self.permissions.insert(request_id.clone(), wait);
                    return Err(DriverError::InvalidOption(option_id));
                }
                // The state leaves `blocked` on the agent's next update.
                self.answer_permission(
                    wait.rpc_id,
                    json!({ "outcome": "selected", "optionId": option_id }),
                );
                Ok(std::mem::take(&mut self.outgoing))
            }
        }
    }

    fn commands(&self) -> &[CommandEntry] {
        &self.commands
    }

    fn options(&self) -> &[OptionDef] {
        &self.opts.defs
    }
}

impl AcpDriver {
    /// Runs a palette entry: ACC's own entries map to their commands, the
    /// agent's are sent as `/name args` prompt text.
    fn invoke(&mut self, name: &str, args: &str) -> Result<Vec<u8>, DriverError> {
        let name = name.trim_start_matches('/');
        match name {
            "interrupt" => self.command(Command::Interrupt),
            "new" => self.command(Command::New),
            "fork" => self.command(Command::Fork),
            // An option picker the agent doesn't list as its own command.
            _ if !self.harness_commands.iter().any(|c| c.name == name)
                && let Some((id, ..)) = OPTION_COMMANDS.iter().find(|(_, n, _)| *n == name) =>
            {
                let value = args.trim();
                if value.is_empty() {
                    return Err(DriverError::MissingArgument("a value"));
                }
                self.command(Command::SetOption {
                    id: *id,
                    value: value.to_owned(),
                })
            }
            "compact" if self.harness == Harness::Opencode => self.command(Command::Compact {
                hint: Some(args.to_owned()).filter(|a| !a.trim().is_empty()),
            }),
            _ if self.harness_commands.iter().any(|c| c.name == name) => {
                let text = if args.is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {args}")
                };
                self.prompt(&text, &[])
            }
            _ => Err(DriverError::InvalidOption(name.to_owned())),
        }
    }

    fn set_option(&mut self, id: OptionId, value: String) -> Result<Vec<u8>, DriverError> {
        let route = self
            .opts
            .route(id)
            .cloned()
            .ok_or_else(|| self.unsupported("this option"))?;
        // Cursor takes a model at spawn; remember it for the next restart
        // whether or not the live switch below is honoured.
        if self.harness == Harness::Cursor && id == OptionId::Model {
            self.opts.spawn_model = Some(value.clone());
            self.opts.rebuild(self.harness);
        }
        match route {
            Route::SpawnFlag => Ok(Vec::new()),
            Route::Config { config_id, boolean } => {
                let sid = self.require_session()?;
                let params = set_config_params(&sid, &config_id, &value, boolean);
                Ok(self.request("session/set_config_option", params, Pending::SetConfig))
            }
            Route::LegacyMode => {
                let sid = self.require_session()?;
                Ok(self.request(
                    "session/set_mode",
                    json!({ "sessionId": sid, "modeId": value }),
                    Pending::SetMode { mode: value },
                ))
            }
            Route::LegacyModel => {
                let sid = self.require_session()?;
                Ok(self.request(
                    "session/set_model",
                    json!({ "sessionId": sid, "modelId": value }),
                    Pending::SetModel { model: value },
                ))
            }
        }
    }
}

/// How to start a harness's ACP server. Cursor takes global flags before
/// the subcommand (`agent --model X acp`).
pub fn launch_spec(harness: Harness, model: Option<&str>) -> LaunchSpec {
    let mut args: Vec<String> = Vec::new();
    let model_flag = |args: &mut Vec<String>| {
        if let Some(m) = model {
            args.extend(["--model".to_owned(), m.to_owned()]);
        }
    };
    match harness {
        Harness::Cursor => {
            model_flag(&mut args);
            args.push("acp".into());
        }
        Harness::Copilot => {
            args.push("--acp".into());
            model_flag(&mut args);
        }
        // opencode acp has no model flag; the model is a config option.
        _ => args.push("acp".into()),
    }
    LaunchSpec {
        program: harness.program().to_owned(),
        args,
        env: Vec::new(),
        protocol: Protocol::Acp,
    }
}

fn push_prompt(id: ItemId, text: &str, out: &mut Vec<Event>) {
    out.push(Event::ItemStarted {
        id: id.clone(),
        kind: ItemKind::UserMessage,
        parent: None,
        extensions: Value::Null,
    });
    out.push(Event::ItemDelta {
        id: id.clone(),
        text: text.to_owned(),
    });
    out.push(Event::ItemCompleted {
        id,
        extensions: Value::Null,
    });
}

fn line(v: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(v).unwrap_or_default();
    bytes.push(b'\n');
    bytes
}

/// JSON-RPC ids may be strings or numbers; both map to one string key.
fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn is_final(update: &Value) -> bool {
    matches!(
        update.get("status").and_then(Value::as_str),
        Some("completed" | "failed")
    )
}

/// The update without its `sessionUpdate` discriminator, for `extensions`.
fn strip_tag(update: &Value) -> Value {
    let mut v = update.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("sessionUpdate");
    }
    v
}

fn native(kind: &str, update: &Value) -> Event {
    Event::Native {
        kind: kind.to_owned(),
        payload: strip_tag(update),
    }
}

/// `availableCommands` entries; ACP doesn't say which are built in or skills.
fn parse_commands(list: Option<&Value>) -> Vec<CommandEntry> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let name = c.get("name")?.as_str()?.trim_start_matches('/').to_owned();
            Some(CommandEntry {
                description: c
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                arg_hint: c
                    .get("input")
                    .and_then(|i| i.get("hint"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                source: CommandSource::Builtin,
                exec: Exec::Text,
                input: CommandInput::None,
                name,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;

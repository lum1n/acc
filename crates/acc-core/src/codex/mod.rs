//! Codex driver.
//!
//! Speaks the `codex app-server` protocol: JSON-RPC 2.0 over stdio, one
//! message per line, without the `"jsonrpc"` member. ACC is the client; the
//! server also sends requests back (approvals, user input), which must always
//! be answered. Checked against `codex-cli` 0.162.1 with
//! `codex app-server generate-ts --experimental`; the fixtures under
//! `tests/fixtures/codex` are recorded from that version.
//!
//! What maps where:
//! - Session: `thread/start` (new) or `thread/resume` (takeover and
//!   reconnect after the process died) in the handshake. The thread id is the
//!   native id; `thread/fork` and `thread/start` switch to a new one.
//! - State: `thread/status/changed` (idle / active with waiting flags /
//!   systemError) plus `turn/started` and `turn/completed`.
//! - Items: `item/started`, the `item/*/delta` notifications and
//!   `item/completed`.
//! - Permissions: `item/commandExecution/requestApproval`,
//!   `item/fileChange/requestApproval`, `item/permissions/requestApproval`,
//!   `item/tool/requestUserInput` and `mcpServer/elicitation/request`.
//! - Usage: `thread/tokenUsage/updated`; quota: `account/rateLimits/read` and
//!   `account/rateLimits/updated`.
//! - Commands: Codex's slash commands live in its TUI, not on the wire, so
//!   ACC's canonical commands map to RPCs and skills become `skill` input
//!   items.
//! - Options: model and effort from `model/list`, mode from
//!   `collaborationMode/list` (experimental), permissions from
//!   `permissionProfile/list`. Changes ride on the next `turn/start`, where
//!   Codex makes them sticky.

pub mod rollout;

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::command::{Apply, Command, CommandEntry, CommandSource, Exec, OptionDef, OptionId};
use crate::driver::{Driver, DriverError};
use crate::event::{Event, ItemKind, PermissionOption, Quota, QuotaWindow, Usage};
use crate::harness::{Harness, LaunchSpec};
use crate::ndjson::{Frame, LineFramer};
use crate::state::{BlockKind, RunState, SessionState, StateSource};
use crate::wire::{self, owned, str_at, u64_at};

/// Notifications ACC never uses; the server skips them for this connection.
const OPT_OUT: [&str; 4] = [
    "remoteControl/status/changed",
    "rawResponseItem/completed",
    "rawResponse/completed",
    "fs/changed",
];

/// Launch and thread settings. Everything else changes at runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexConfig {
    /// Thread to resume instead of starting a new one.
    pub resume: Option<String>,
    /// Working directory for a new thread (defaults to the process cwd).
    pub cwd: Option<String>,
    pub model: Option<String>,
    /// `untrusted`, `on-request` or `never`.
    pub approval_policy: Option<String>,
    /// Permission profile id such as `:workspace` (see `permissionProfile/list`).
    pub permissions: Option<String>,
}

/// What a request ACC sent was for, so its response can be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Req {
    Initialize,
    Thread,
    TurnStart,
    ModelList,
    Profiles,
    Modes,
    RateLimits,
    Skills,
    Other,
}

/// A request the server is waiting on ACC to answer.
#[derive(Debug, Clone)]
struct ServerRequest {
    id: Value,
    method: String,
    params: Value,
}

#[derive(Debug, Clone)]
struct Skill {
    name: String,
    path: String,
    description: String,
}

/// Option changes to send with the next `turn/start`.
#[derive(Debug, Clone, Default)]
struct Overrides {
    model: Option<String>,
    effort: Option<String>,
    mode: Option<String>,
    permissions: Option<String>,
}

/// Codex app-server driver. See the module docs.
#[derive(Debug)]
pub struct CodexDriver {
    config: CodexConfig,
    framer: LineFramer,
    next_id: u64,
    pending: HashMap<u64, Req>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    state: Option<RunState>,
    requests: HashMap<String, ServerRequest>,
    models: Vec<Value>,
    model: Option<String>,
    effort: Option<String>,
    mode: Option<String>,
    modes: Vec<String>,
    permission: Option<String>,
    profiles: Vec<String>,
    overrides: Overrides,
    skills: Vec<Skill>,
    commands: Vec<CommandEntry>,
    options: Vec<OptionDef>,
    transmit: Vec<u8>,
}

impl Default for CodexDriver {
    fn default() -> Self {
        Self::new(CodexConfig::default())
    }
}

impl CodexDriver {
    pub fn new(config: CodexConfig) -> Self {
        let mut d = Self {
            config,
            framer: LineFramer::new(wire::MAX_FRAME),
            next_id: 0,
            pending: HashMap::new(),
            thread_id: None,
            turn_id: None,
            state: None,
            requests: HashMap::new(),
            models: Vec::new(),
            model: None,
            effort: None,
            mode: None,
            modes: Vec::new(),
            permission: None,
            profiles: Vec::new(),
            overrides: Overrides::default(),
            skills: Vec::new(),
            commands: Vec::new(),
            options: Vec::new(),
            transmit: Vec::new(),
        };
        d.rebuild_commands();
        d.rebuild_options();
        d
    }

    /// The Codex thread id, once a thread is started or resumed.
    pub fn thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    fn request(&mut self, method: &str, params: Value, kind: Req) -> Vec<u8> {
        self.next_id += 1;
        self.pending.insert(self.next_id, kind);
        if params.is_null() {
            wire::line(&json!({"id": self.next_id, "method": method}))
        } else {
            wire::line(&json!({"id": self.next_id, "method": method, "params": params}))
        }
    }

    fn thread(&self) -> Result<String, DriverError> {
        self.thread_id.clone().ok_or(DriverError::NotStarted)
    }

    fn thread_params(&self) -> Value {
        let mut p = Map::new();
        let c = &self.config;
        for (key, value) in [
            ("cwd", &c.cwd),
            ("model", &c.model),
            ("approvalPolicy", &c.approval_policy),
            ("permissions", &c.permissions),
        ] {
            if let Some(v) = value {
                p.insert(key.into(), Value::String(v.clone()));
            }
        }
        Value::Object(p)
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
        let id = v.get("id").cloned();
        let method = owned(&v, "method");
        match (id, method) {
            (Some(id), Some(method)) => self.on_server_request(id, method, v, out),
            (Some(id), None) => self.on_response(&id, &v, out),
            (None, Some(method)) => self.on_notification(&method, &v, raw, out),
            (None, None) => out.push(Event::Unrecognized { raw: raw.to_owned() }),
        }
    }

    // ---- responses -------------------------------------------------------

    fn on_response(&mut self, id: &Value, v: &Value, out: &mut Vec<Event>) {
        let kind = id
            .as_u64()
            .and_then(|n| self.pending.remove(&n))
            .unwrap_or(Req::Other);
        if let Some(err) = v.get("error") {
            // Rate limits need a ChatGPT login; API-key users simply have none.
            if kind != Req::RateLimits {
                let msg = str_at(err, "message").unwrap_or("request failed");
                out.push(Event::Notice {
                    message: format!("Codex: {msg}"),
                });
            }
            return;
        }
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        match kind {
            Req::Thread => self.on_thread(&result, out),
            Req::TurnStart => {
                if let Some(turn) = result.pointer("/turn/id").and_then(Value::as_str) {
                    self.turn_id = Some(turn.to_owned());
                }
            }
            Req::ModelList => {
                if let Some(data) = result.get("data").and_then(Value::as_array) {
                    self.models.extend(data.iter().cloned());
                }
                if let Some(cursor) = owned(&result, "nextCursor") {
                    let next = self.request("model/list", json!({"cursor": cursor}), Req::ModelList);
                    self.transmit.extend(next);
                }
                self.rebuild_options();
                out.push(Event::OptionsChanged);
            }
            Req::Profiles => {
                self.profiles = list(&result, "data")
                    .filter(|p| p.get("allowed").and_then(Value::as_bool) != Some(false))
                    .filter_map(|p| owned(p, "id"))
                    .collect();
                self.rebuild_options();
                out.push(Event::OptionsChanged);
            }
            Req::Modes => {
                let mut modes: Vec<String> = list(&result, "data").filter_map(|m| owned(m, "mode")).collect();
                modes.dedup();
                self.modes = modes;
                self.rebuild_options();
                out.push(Event::OptionsChanged);
            }
            Req::RateLimits => {
                let account = owned(&result, "accountId");
                if let Some(snapshot) = result.get("rateLimits") {
                    out.push(Event::Quota(quota(snapshot, account)));
                }
            }
            Req::Skills => {
                self.skills = list(&result, "data")
                    .flat_map(|entry| list(entry, "skills"))
                    .filter(|s| s.get("enabled").and_then(Value::as_bool) != Some(false))
                    .filter_map(|s| {
                        Some(Skill {
                            name: owned(s, "name")?,
                            path: owned(s, "path")?,
                            description: owned(s, "shortDescription")
                                .or_else(|| owned(s, "description"))
                                .unwrap_or_default(),
                        })
                    })
                    .collect();
                self.rebuild_commands();
                out.push(Event::CommandsChanged);
            }
            Req::Initialize | Req::Other => {
                if let Some(turn) = result.pointer("/turn/id").and_then(Value::as_str) {
                    // review/start runs as a turn too.
                    self.turn_id = Some(turn.to_owned());
                }
            }
        }
    }

    /// A `thread/start`, `thread/resume` or `thread/fork` result.
    fn on_thread(&mut self, result: &Value, out: &mut Vec<Event>) {
        let Some(thread) = result.get("thread") else {
            return;
        };
        let Some(id) = owned(thread, "id") else { return };
        self.thread_id = Some(id.clone());
        self.turn_id = None;
        self.model = owned(result, "model").or_else(|| owned(thread, "model"));
        self.effort = owned(result, "reasoningEffort");
        self.permission = result
            .pointer("/activePermissionProfile/id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.mode = result
            .pointer("/collaborationMode/mode")
            .and_then(Value::as_str)
            .map(str::to_owned);
        out.push(Event::SessionStarted {
            harness: Harness::Codex,
            native_id: id,
            model: self.model.clone(),
        });
        // History of a resumed thread, so a client that takes over sees it.
        for turn in list(thread, "turns") {
            for item in list(turn, "items") {
                self.item_whole(item, out);
            }
            if str_at(turn, "status") == Some("inProgress") {
                self.turn_id = owned(turn, "id");
            }
        }
        self.rebuild_options();
        out.push(Event::OptionsChanged);
        let state = if self.turn_id.is_some() {
            RunState::Working { progress: None }
        } else {
            RunState::Idle
        };
        self.set_state(state, None, out);
    }

    // ---- notifications ---------------------------------------------------

    fn on_notification(&mut self, method: &str, v: &Value, raw: &str, out: &mut Vec<Event>) {
        let p = v.get("params").cloned().unwrap_or(Value::Null);
        let ours = str_at(&p, "threadId").is_none_or(|t| Some(t) == self.thread_id.as_deref());
        match method {
            "thread/status/changed" if ours => {
                let status = p.get("status").cloned().unwrap_or(Value::Null);
                match str_at(&status, "type") {
                    Some("idle") if !matches!(self.state, Some(RunState::Done | RunState::Error)) => {
                        self.set_state(RunState::Idle, None, out);
                    }
                    Some("systemError") => self.set_state(RunState::Error, None, out),
                    Some("active") => {
                        let flags: Vec<&str> =
                            list(&status, "activeFlags").filter_map(Value::as_str).collect();
                        let state = if flags.contains(&"waitingOnApproval") {
                            RunState::Blocked {
                                kind: BlockKind::Permission,
                            }
                        } else if flags.contains(&"waitingOnUserInput") {
                            RunState::Blocked {
                                kind: BlockKind::Question,
                            }
                        } else {
                            RunState::Working { progress: None }
                        };
                        self.set_state(state, None, out);
                    }
                    _ => {}
                }
            }
            "turn/started" if ours => {
                self.turn_id = p.pointer("/turn/id").and_then(Value::as_str).map(str::to_owned);
                self.set_state(RunState::Working { progress: None }, None, out);
            }
            "turn/completed" if ours => {
                self.turn_id = None;
                out.push(Event::TurnCompleted);
                let turn = p.get("turn").cloned().unwrap_or(Value::Null);
                match str_at(&turn, "status") {
                    Some("failed") => {
                        let msg = turn
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .map(|m| wire::one_line(m, 200));
                        self.set_state(RunState::Error, msg, out);
                    }
                    Some("interrupted") => self.set_state(RunState::Idle, Some("Interrupted".into()), out),
                    _ => self.set_state(RunState::Done, None, out),
                }
            }
            "item/started" => {
                if let Some(item) = p.get("item") {
                    let ext = self.item_ext(item, &p);
                    if let (Some(id), Some(kind)) = (owned(item, "id"), item_kind(item)) {
                        out.push(Event::ItemStarted {
                            id: id.clone(),
                            kind,
                            extensions: ext,
                        });
                        // User messages and some others have no delta stream.
                        if let Some(text) = initial_text(item) {
                            out.push(Event::ItemDelta { id, text });
                        }
                    }
                }
            }
            "item/completed" => {
                if let Some(item) = p.get("item")
                    && let (Some(id), Some(_)) = (owned(item, "id"), item_kind(item))
                {
                    out.push(Event::ItemCompleted {
                        id,
                        extensions: item.clone(),
                    });
                }
            }
            "item/agentMessage/delta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/textDelta"
            | "item/commandExecution/outputDelta"
            | "item/fileChange/outputDelta"
            | "item/plan/delta" => {
                if let (Some(id), Some(text)) = (owned(&p, "itemId"), owned(&p, "delta"))
                    && !text.is_empty()
                {
                    out.push(Event::ItemDelta { id, text });
                }
            }
            "thread/tokenUsage/updated" if ours => {
                let usage = p.get("tokenUsage").cloned().unwrap_or(Value::Null);
                let total = usage.get("total").cloned().unwrap_or(Value::Null);
                out.push(Event::Usage(Usage {
                    input_tokens: u64_at(&total, "inputTokens"),
                    output_tokens: u64_at(&total, "outputTokens"),
                    cache_read_tokens: u64_at(&total, "cachedInputTokens"),
                    cache_write_tokens: u64_at(&total, "cacheWriteInputTokens"),
                    cost_usd: None,
                    context_used: usage.pointer("/last/totalTokens").and_then(Value::as_u64),
                    context_window: u64_at(&usage, "modelContextWindow"),
                }));
            }
            "account/rateLimits/updated" => {
                if let Some(snapshot) = p.get("rateLimits") {
                    out.push(Event::Quota(quota(snapshot, None)));
                }
            }
            "error" if ours => {
                let msg = p
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(|m| wire::one_line(m, 200));
                if p.get("willRetry").and_then(Value::as_bool) == Some(true) {
                    self.set_state(RunState::Working { progress: None }, msg, out);
                } else {
                    self.set_state(RunState::Error, msg, out);
                }
            }
            "serverRequest/resolved" => {
                if let Some(id) = p.get("requestId").map(request_key)
                    && self.requests.remove(&id).is_some()
                {
                    out.push(Event::PermissionResolved { request_id: id });
                }
            }
            "model/rerouted" if ours => {
                if let Some(to) = owned(&p, "toModel") {
                    self.model = Some(to);
                    self.rebuild_options();
                    out.push(Event::OptionsChanged);
                }
            }
            "skills/changed" => {
                let req = self.request("skills/list", self.skills_params(), Req::Skills);
                self.transmit.extend(req);
            }
            "thread/compacted" if ours => out.push(Event::Notice {
                message: "Conversation compacted".into(),
            }),
            "warning" | "guardianWarning" => {
                if let Some(m) = owned(&p, "message") {
                    out.push(Event::Notice { message: m });
                }
            }
            "configWarning" | "deprecationNotice" => {
                if let Some(m) = owned(&p, "summary") {
                    out.push(Event::Notice { message: m });
                }
            }
            // Other threads (subagents) and bookkeeping ACC doesn't surface.
            "thread/status/changed"
            | "turn/started"
            | "turn/completed"
            | "thread/tokenUsage/updated"
            | "error"
            | "model/rerouted"
            | "thread/compacted"
            | "thread/started"
            | "thread/name/updated"
            | "thread/settings/updated"
            | "turn/diff/updated"
            | "turn/plan/updated"
            | "item/reasoning/summaryPartAdded"
            | "item/commandExecution/terminalInteraction"
            | "item/fileChange/patchUpdated"
            | "item/mcpToolCall/progress"
            | "hook/started"
            | "hook/completed"
            | "mcpServer/startupStatus/updated"
            | "account/updated"
            | "app/list/updated"
            | "thread/goal/updated"
            | "thread/goal/cleared"
            | "thread/queue/changed"
            | "item/autoApprovalReview/started"
            | "item/autoApprovalReview/completed"
            | "turn/moderationMetadata"
            | "model/verification"
            | "model/safetyBuffering/updated"
            | "thread/archived"
            | "thread/unarchived"
            | "thread/closed"
            | "thread/deleted"
            | "thread/environment/connected"
            | "thread/environment/disconnected" => {}
            _ => out.push(Event::Unrecognized { raw: raw.to_owned() }),
        }
    }

    fn item_ext(&self, item: &Value, params: &Value) -> Value {
        let mut ext = item.clone();
        if let Some(thread) = str_at(params, "threadId")
            && Some(thread) != self.thread_id.as_deref()
            && let Some(obj) = ext.as_object_mut()
        {
            obj.insert("threadId".into(), Value::String(thread.to_owned()));
        }
        ext
    }

    /// Emits a finished item (from thread history) as start, text and completion.
    fn item_whole(&mut self, item: &Value, out: &mut Vec<Event>) {
        let (Some(id), Some(kind)) = (owned(item, "id"), item_kind(item)) else {
            return;
        };
        out.push(Event::ItemStarted {
            id: id.clone(),
            kind,
            extensions: Value::Null,
        });
        if let Some(text) = initial_text(item).or_else(|| final_text(item)) {
            out.push(Event::ItemDelta { id: id.clone(), text });
        }
        out.push(Event::ItemCompleted {
            id,
            extensions: item.clone(),
        });
    }

    // ---- server requests ---------------------------------------------------

    fn on_server_request(&mut self, id: Value, method: String, v: Value, out: &mut Vec<Event>) {
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        let opt = |id: &str, label: &str| PermissionOption {
            id: id.into(),
            label: label.into(),
        };
        let (title, options, kind) = match method.as_str() {
            "item/commandExecution/requestApproval" => {
                let what = str_at(&params, "command")
                    .or_else(|| str_at(&params, "reason"))
                    .unwrap_or("a command");
                let offered: Vec<&str> = list(&params, "availableDecisions")
                    .filter_map(Value::as_str)
                    .collect();
                let options = decision_options(&offered);
                (
                    format!("Run: {}", wire::one_line(what, 200)),
                    options,
                    BlockKind::Permission,
                )
            }
            "item/fileChange/requestApproval" => {
                let title = match str_at(&params, "reason") {
                    Some(r) => format!("Apply file changes: {}", wire::one_line(r, 200)),
                    None => "Apply file changes".into(),
                };
                (title, decision_options(&[]), BlockKind::Permission)
            }
            "item/permissions/requestApproval" => {
                let title = match str_at(&params, "reason") {
                    Some(r) => format!("Grant permissions: {}", wire::one_line(r, 200)),
                    None => "Grant additional permissions".into(),
                };
                let options = vec![
                    opt("turn", "Allow this turn"),
                    opt("session", "Allow for session"),
                    opt("decline", "Deny"),
                ];
                (title, options, BlockKind::Permission)
            }
            "item/tool/requestUserInput" => {
                let q = params.pointer("/questions/0").cloned().unwrap_or(Value::Null);
                let options = list(&q, "options")
                    .filter_map(|o| str_at(o, "label"))
                    .map(|l| opt(l, l))
                    .collect();
                (
                    owned(&q, "question").unwrap_or_default(),
                    options,
                    BlockKind::Question,
                )
            }
            "mcpServer/elicitation/request" => {
                let server = str_at(&params, "serverName").unwrap_or("MCP server");
                let text = str_at(&params, "message")
                    .or_else(|| str_at(&params, "title"))
                    .unwrap_or("");
                // Forms need structured content ACC can't fill in yet; only URL flows can be accepted.
                let mut options = Vec::new();
                if matches!(str_at(&params, "mode"), Some("url" | "openai/userVerification")) {
                    options.push(opt("accept", "Accept"));
                }
                options.extend([opt("decline", "Decline"), opt("cancel", "Cancel")]);
                (
                    format!("{server}: {}", wire::one_line(text, 200)),
                    options,
                    BlockKind::Question,
                )
            }
            // Dynamic tools, auth refresh, attestation and the v1 approval
            // methods aren't used by ACC; refuse them so Codex never hangs.
            _ => {
                self.transmit.extend(wire::line(&json!({
                    "id": id,
                    "error": {"code": -32601, "message": format!("ACC does not handle {method}")},
                })));
                return;
            }
        };
        let key = request_key(&id);
        self.requests
            .insert(key.clone(), ServerRequest { id, method, params });
        out.push(Event::PermissionRequested {
            request_id: key,
            title,
            options,
        });
        self.set_state(RunState::Blocked { kind }, None, out);
    }

    fn answer(&mut self, request_id: &str, option_id: &str) -> Result<Vec<u8>, DriverError> {
        let req = self
            .requests
            .get(request_id)
            .cloned()
            .ok_or_else(|| DriverError::UnknownRequest(request_id.to_owned()))?;
        let invalid = || DriverError::InvalidOption(option_id.to_owned());
        let result = match req.method.as_str() {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                if !matches!(option_id, "accept" | "acceptForSession" | "decline" | "cancel") {
                    return Err(invalid());
                }
                json!({"decision": option_id})
            }
            "item/permissions/requestApproval" => match option_id {
                "turn" | "session" => {
                    let requested = req.params.get("permissions").cloned().unwrap_or(Value::Null);
                    json!({"permissions": strip_nulls(requested), "scope": option_id})
                }
                "decline" => json!({"permissions": {}, "scope": "turn"}),
                _ => return Err(invalid()),
            },
            "item/tool/requestUserInput" => {
                let mut answers = Map::new();
                for (i, q) in list(&req.params, "questions").enumerate() {
                    if let Some(qid) = owned(q, "id") {
                        let chosen = if i == 0 {
                            vec![option_id.to_owned()]
                        } else {
                            Vec::new()
                        };
                        answers.insert(qid, json!({"answers": chosen}));
                    }
                }
                json!({"answers": answers})
            }
            "mcpServer/elicitation/request" => {
                if !matches!(option_id, "accept" | "decline" | "cancel") {
                    return Err(invalid());
                }
                json!({"action": option_id, "content": null, "_meta": null})
            }
            _ => return Err(DriverError::UnknownRequest(request_id.to_owned())),
        };
        self.requests.remove(request_id);
        Ok(wire::line(&json!({"id": req.id, "result": result})))
    }

    // ---- palette and options ---------------------------------------------

    fn skills_params(&self) -> Value {
        match &self.config.cwd {
            Some(cwd) => json!({"cwds": [cwd]}),
            None => json!({}),
        }
    }

    fn rebuild_commands(&mut self) {
        let rpc = |name: &str, desc: &str, hint: Option<&str>, method: &str| CommandEntry {
            name: name.into(),
            description: desc.into(),
            arg_hint: hint.map(str::to_owned),
            source: CommandSource::Acc,
            exec: Exec::Rpc {
                method: method.into(),
            },
        };
        let mut c = vec![
            rpc(
                "compact",
                "Summarize the conversation to free context",
                None,
                "thread/compact/start",
            ),
            rpc(
                "review",
                "Review uncommitted changes, or follow instructions",
                Some("[instructions]"),
                "review/start",
            ),
            rpc("new", "Start a new thread", None, "thread/start"),
            rpc("fork", "Fork this thread", None, "thread/fork"),
            rpc("rename", "Rename this thread", Some("<name>"), "thread/name/set"),
            rpc(
                "shell",
                "Run a shell command outside the sandbox",
                Some("<command>"),
                "thread/shellCommand",
            ),
            rpc("interrupt", "Stop the current turn", None, "turn/interrupt"),
        ];
        c.extend(self.skills.iter().map(|s| CommandEntry {
            name: s.name.clone(),
            description: s.description.clone(),
            arg_hint: None,
            source: CommandSource::Skill,
            exec: Exec::SkillItem,
        }));
        self.commands = c;
    }

    fn current_model(&self) -> Option<&str> {
        self.overrides.model.as_deref().or(self.model.as_deref())
    }

    fn model_info(&self, model: &str) -> Option<&Value> {
        self.models
            .iter()
            .find(|m| str_at(m, "id") == Some(model) || str_at(m, "model") == Some(model))
    }

    fn rebuild_options(&mut self) {
        let model = self.current_model().map(str::to_owned);
        let info = model.as_deref().and_then(|m| self.model_info(m));
        let efforts: Vec<String> = info
            .map(|i| {
                list(i, "supportedReasoningEfforts")
                    .filter_map(|e| owned(e, "reasoningEffort"))
                    .collect()
            })
            .unwrap_or_default();
        let effort = self
            .overrides
            .effort
            .clone()
            .or_else(|| self.effort.clone())
            .or_else(|| info.and_then(|i| owned(i, "defaultReasoningEffort")));
        let models = self
            .models
            .iter()
            .filter(|m| m.get("hidden").and_then(Value::as_bool) != Some(true))
            .filter_map(|m| owned(m, "id"))
            .collect();
        let mode = self.overrides.mode.clone().or_else(|| self.mode.clone());
        let mode = mode.or_else(|| (!self.modes.is_empty()).then(|| "default".to_owned()));
        let permission = self
            .overrides
            .permissions
            .clone()
            .or_else(|| self.permission.clone());
        self.options = vec![
            OptionDef {
                id: OptionId::Model,
                values: models,
                current: model,
                apply: Apply::NextTurn,
            },
            OptionDef {
                id: OptionId::Effort,
                values: efforts,
                current: effort,
                apply: Apply::NextTurn,
            },
            OptionDef {
                id: OptionId::Mode,
                values: self.modes.clone(),
                current: mode,
                apply: Apply::NextTurn,
            },
            OptionDef {
                id: OptionId::Permission,
                values: self.profiles.clone(),
                current: permission,
                apply: Apply::NextTurn,
            },
        ];
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

    /// `turn/start`, or `turn/steer` while a turn is running.
    fn turn(&mut self, input: Vec<Value>) -> Result<Vec<u8>, DriverError> {
        let thread = self.thread()?;
        if let Some(turn) = self.turn_id.clone() {
            return Ok(self.request(
                "turn/steer",
                json!({"threadId": thread, "input": input, "expectedTurnId": turn}),
                Req::Other,
            ));
        }
        let mut params = json!({"threadId": thread, "input": input});
        let o = std::mem::take(&mut self.overrides);
        if let Some(m) = &o.model {
            params["model"] = json!(m);
            self.model = Some(m.clone());
        }
        if let Some(e) = &o.effort {
            params["effort"] = json!(e);
            self.effort = Some(e.clone());
        }
        if let Some(p) = &o.permissions {
            params["permissions"] = json!(p);
            self.permission = Some(p.clone());
        }
        if let Some(mode) = &o.mode {
            params["collaborationMode"] = json!({
                "mode": mode,
                "settings": {
                    "model": self.model.clone().unwrap_or_default(),
                    "reasoning_effort": self.effort,
                    "developer_instructions": null,
                },
            });
            self.mode = Some(mode.clone());
        }
        self.rebuild_options();
        Ok(self.request("turn/start", params, Req::TurnStart))
    }
}

impl Driver for CodexDriver {
    fn harness(&self) -> Harness {
        Harness::Codex
    }

    /// Resume happens in-protocol: set [`CodexConfig::resume`] instead.
    fn launch(&self, resume: Option<&str>) -> LaunchSpec {
        Harness::Codex.launch(resume)
    }

    fn handshake(&mut self) -> Vec<u8> {
        let mut out = self.request(
            "initialize",
            json!({
                "clientInfo": {"name": "acc", "title": "Agent Control Core", "version": crate::VERSION},
                "capabilities": {
                    "experimentalApi": true,
                    "requestAttestation": false,
                    "optOutNotificationMethods": OPT_OUT,
                },
            }),
            Req::Initialize,
        );
        out.extend(wire::line(&json!({"method": "initialized"})));
        let thread = match self.config.resume.clone() {
            Some(id) => {
                let mut p = self.thread_params();
                p["threadId"] = json!(id);
                self.request("thread/resume", p, Req::Thread)
            }
            None => self.request("thread/start", self.thread_params(), Req::Thread),
        };
        out.extend(thread);
        out.extend(self.request("model/list", json!({}), Req::ModelList));
        out.extend(self.request("permissionProfile/list", json!({}), Req::Profiles));
        out.extend(self.request("collaborationMode/list", json!({}), Req::Modes));
        out.extend(self.request("account/rateLimits/read", Value::Null, Req::RateLimits));
        out.extend(self.request("skills/list", self.skills_params(), Req::Skills));
        out
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
            harness: Harness::Codex,
            what,
        };
        match cmd {
            Command::Prompt { text } => self.turn(vec![text_input(&text)]),
            Command::Interrupt => match (self.thread_id.clone(), self.turn_id.clone()) {
                (Some(thread), Some(turn)) => Ok(self.request(
                    "turn/interrupt",
                    json!({"threadId": thread, "turnId": turn}),
                    Req::Other,
                )),
                // Nothing is running; interrupting is a no-op.
                _ => Ok(Vec::new()),
            },
            // Codex's compaction takes no instructions; a hint is dropped.
            Command::Compact { .. } => {
                let thread = self.thread()?;
                Ok(self.request("thread/compact/start", json!({"threadId": thread}), Req::Other))
            }
            Command::New => Ok(self.request("thread/start", self.thread_params(), Req::Thread)),
            Command::Fork => {
                let thread = self.thread()?;
                Ok(self.request("thread/fork", json!({"threadId": thread}), Req::Thread))
            }
            Command::Rename { name } => {
                let thread = self.thread()?;
                Ok(self.request(
                    "thread/name/set",
                    json!({"threadId": thread, "name": name}),
                    Req::Other,
                ))
            }
            Command::Review => self.command(Command::Invoke {
                name: "review".into(),
                args: String::new(),
            }),
            Command::Shell { command } => {
                let thread = self.thread()?;
                Ok(self.request(
                    "thread/shellCommand",
                    json!({"threadId": thread, "command": command}),
                    Req::Other,
                ))
            }
            Command::SetOption { id, value } => {
                if id != OptionId::Model {
                    self.validate(id, &value)?;
                }
                match id {
                    OptionId::Model => {
                        self.overrides.model = Some(value);
                        // Effort levels differ per model; drop one the new model may not take.
                        self.overrides.effort = None;
                    }
                    OptionId::Effort => self.overrides.effort = Some(value),
                    OptionId::Mode => self.overrides.mode = Some(value),
                    OptionId::Permission => self.overrides.permissions = Some(value),
                }
                self.rebuild_options();
                Ok(Vec::new())
            }
            Command::AnswerPermission {
                request_id,
                option_id,
            } => self.answer(&request_id, &option_id),
            Command::Invoke { name, args } => {
                let args = args.trim().to_owned();
                match name.trim_start_matches('/') {
                    "compact" => self.command(Command::Compact { hint: None }),
                    "new" => self.command(Command::New),
                    "fork" => self.command(Command::Fork),
                    "interrupt" => self.command(Command::Interrupt),
                    "rename" => self.command(Command::Rename { name: args }),
                    "shell" => self.command(Command::Shell { command: args }),
                    "review" => {
                        let thread = self.thread()?;
                        let target = if args.is_empty() {
                            json!({"type": "uncommittedChanges"})
                        } else {
                            json!({"type": "custom", "instructions": args})
                        };
                        Ok(self.request(
                            "review/start",
                            json!({"threadId": thread, "target": target}),
                            Req::Other,
                        ))
                    }
                    skill => {
                        let Some(s) = self.skills.iter().find(|s| s.name == skill).cloned() else {
                            return Err(unsupported("commands other than ACC's and skills"));
                        };
                        let text = if args.is_empty() {
                            format!("${}", s.name)
                        } else {
                            format!("${} {args}", s.name)
                        };
                        self.turn(vec![
                            json!({"type": "skill", "name": s.name, "path": s.path}),
                            text_input(&text),
                        ])
                    }
                }
            }
            Command::Refresh => {
                let mut out = self.request("account/rateLimits/read", Value::Null, Req::RateLimits);
                self.models.clear();
                out.extend(self.request("model/list", json!({}), Req::ModelList));
                out.extend(self.request("skills/list", self.skills_params(), Req::Skills));
                Ok(out)
            }
        }
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

fn text_input(text: &str) -> Value {
    json!({"type": "text", "text": text, "text_elements": []})
}

fn list<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key).and_then(Value::as_array).into_iter().flatten()
}

/// JSON-RPC ids may be numbers or strings; ACC exposes them as strings.
fn request_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn decision_options(offered: &[&str]) -> Vec<PermissionOption> {
    let all = [
        ("accept", "Allow"),
        ("acceptForSession", "Allow for session"),
        ("decline", "Deny"),
        ("cancel", "Deny and stop"),
    ];
    all.iter()
        .filter(|(id, _)| offered.is_empty() || offered.contains(id))
        .map(|(id, label)| PermissionOption {
            id: (*id).into(),
            label: (*label).into(),
        })
        .collect()
}

fn item_kind(item: &Value) -> Option<ItemKind> {
    Some(match str_at(item, "type")? {
        "userMessage" => ItemKind::UserMessage,
        "agentMessage" | "plan" => ItemKind::AgentMessage,
        "reasoning" => ItemKind::Reasoning,
        "commandExecution" => ItemKind::CommandExecution,
        "fileChange" => ItemKind::FileChange,
        "hookPrompt" | "functionCallOutput" => return None,
        _ => ItemKind::ToolCall,
    })
}

/// Text an item carries when it starts (user input has no delta stream).
fn initial_text(item: &Value) -> Option<String> {
    match str_at(item, "type")? {
        "userMessage" => {
            let text: Vec<&str> = list(item, "content").filter_map(|c| str_at(c, "text")).collect();
            (!text.is_empty()).then(|| text.join("\n"))
        }
        "commandExecution" => owned(item, "command").map(|c| format!("$ {c}\n")),
        _ => None,
    }
}

/// Text of a finished item, used when replaying thread history.
fn final_text(item: &Value) -> Option<String> {
    let text = match str_at(item, "type")? {
        "agentMessage" | "plan" => owned(item, "text"),
        "reasoning" => Some(
            list(item, "summary")
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    };
    text.filter(|t| !t.is_empty())
}

fn quota(snapshot: &Value, account: Option<String>) -> Quota {
    let windows = ["primary", "secondary"]
        .iter()
        .filter_map(|id| {
            let w = snapshot.get(*id).filter(|w| !w.is_null())?;
            Some(QuotaWindow {
                id: (*id).to_owned(),
                used_percent: w.get("usedPercent").and_then(Value::as_f64),
                window_minutes: u64_at(w, "windowDurationMins"),
                resets_at: u64_at(w, "resetsAt"),
                status: None,
            })
        })
        .collect();
    Quota {
        account,
        plan: owned(snapshot, "planType"),
        windows,
    }
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests;

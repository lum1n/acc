//! wasm-bindgen surface of acc-core for browsers and Node.
//!
//! Transport-agnostic, like the Swift/Kotlin bindings: a [`Session`] turns
//! harness bytes (`Uint8Array`) into events and commands into bytes, and the
//! page moves them over whatever it has (a WebSocket to an SSH proxy, a
//! WebTransport stream, a Node child process).
//!
//! Values cross as plain JS objects shaped exactly like the core's serde
//! types (snake_case keys, `type`-tagged unions), so they match the JSON that
//! `acc run --json` prints. The TypeScript declarations below spell them out.
//!
//! Beside that live [`Session`], the detached-session objects acc-ffi has
//! ([`AccSession`], [`AccFanIn`] and the host scripts) are here under the
//! same names, so every embedder gets the
//! same API (`tests/parity.rs` keeps the two in step). Their records are JS
//! objects too; the ones that carry bytes ([`AccExec`], [`AccOutbound`],
//! [`AccFeedLine`]) are classes with `Uint8Array` fields. They throw `Error`s
//! named `AccError` with a `kind` (acc-ffi's `AccError` variant names).

use acc_core::Exec;
use acc_core::Harness;
use acc_core::command::Command;
use acc_core::detached::SessionDir;
use acc_core::harness::LaunchSpec;
use acc_core::live::{self as core_session, Opening};
use acc_core::state::SessionState;
use acc_core::transcript::Item;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

pub mod loadout;
pub mod session;

pub use loadout::*;
pub use session::*;

#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &str = r#"
export type HarnessId = "claude" | "codex" | "cursor" | "copilot" | "pi" | "opencode";

export interface HarnessInfo {
  harness: HarnessId;
  id: string;
  program: string;
  display_name: string;
  native_acp: boolean;
}

export type Opening =
  | { type: "new" }
  | { type: "resume"; native_id: string }
  | { type: "attach" };

export interface SessionConfig {
  harness: HarnessId;
  /** Absolute working directory on the host. */
  cwd: string;
  /** Defaults to `{ type: "new" }`. */
  opening?: Opening;
  model?: string | null;
  /** Unique among clients sharing one harness process. Defaults to "acc". */
  client_tag?: string | null;
}

export interface LaunchSpec {
  program: string;
  args: string[];
  env: [string, string][];
  protocol: "claude_stream_json" | "codex_app_server" | "acp" | "pi_rpc" | "opencode_http";
}

export type RunState =
  | { state: "idle" }
  | { state: "working"; progress: number | null }
  | { state: "blocked"; kind: "permission" | "question" | "auth" }
  | { state: "done" }
  | { state: "error" }
  | { state: "exited" };

export type StateSource = "heuristic" | "screen_match" | "transcript" | "osc7501" | "native";

export interface SessionState {
  state: RunState;
  message: string | null;
  source: StateSource;
  /** Milliseconds since the Unix epoch. */
  observed_at_ms: number;
}

export type ItemKind =
  | "user_message" | "agent_message" | "reasoning" | "tool_call" | "command_execution" | "file_change";

export interface Usage {
  input_tokens: number | null;
  output_tokens: number | null;
  cache_read_tokens: number | null;
  cache_write_tokens: number | null;
  cost_usd: number | null;
  context_used: number | null;
  context_window: number | null;
}

export interface QuotaWindow {
  id: string;
  used_percent: number | null;
  window_minutes: number | null;
  resets_at: number | null;
  status: string | null;
}

export interface Quota {
  account: string | null;
  plan: string | null;
  windows: QuotaWindow[];
}

export type Hook = { type: "command"; command: string } | { type: "webhook"; url: string };

/** One event a session follower recorded while no client was connected. */
export interface HostEvent {
  /** Seconds since the Unix epoch, by the host's clock. */
  at: number;
  session: string;
  state: "blocked" | "done" | "error" | "exited";
  kind?: "permission" | "question" | "auth";
  detail?: string;
  code?: number | null;
  /** The session log's length at the time. */
  offset: number;
}

export interface PermissionOption {
  id: string;
  label: string;
  /** What answering with it does, when the harness says. */
  kind?: "allow_once" | "allow_always" | "reject_once" | "reject_always";
}

export interface PolicyRule {
  decision: "allow" | "deny";
  /** The harness's tool name, or "shell" | "edit" | "read" | "fetch" | "mcp". */
  tool?: string | null;
  /** Glob over the files the tool acts on, relative to the session's directory. */
  path?: string | null;
  /** A shell command's start, in whole words. */
  command?: string | null;
}

export interface Spend {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  cost_usd: number;
}

/** One session's spending, from `AccSession.usageRecord()`. Keep it for sessions that are gone. */
export interface UsageRecord {
  session_id: string;
  harness: HarnessId;
  /** Quarter-hour buckets, oldest first; `start_ms` is ms since the Unix epoch. */
  buckets: { start_ms: number; spend: Spend }[];
  quota?: Quota;
  quota_at_ms?: number;
}

export interface UsageRollup {
  total: Spend;
  accounts: {
    harness: HarnessId;
    account: string | null;
    plan: string | null;
    sessions: number;
    spend: Spend;
    quota: Quota | null;
  }[];
  /** Oldest first; `date` is `YYYY-MM-DD` in the time zone given. */
  days: { date: string; spend: Spend; harnesses: { harness: HarnessId; spend: Spend }[] }[];
}

export type Event =
  | { type: "session_started"; harness: HarnessId; native_id: string; model: string | null }
  | ({ type: "state_changed" } & SessionState)
  | { type: "item_started"; id: string; kind: ItemKind; extensions?: unknown }
  | { type: "item_delta"; id: string; text: string }
  | { type: "item_updated"; id: string; extensions?: unknown }
  | { type: "item_completed"; id: string; extensions?: unknown }
  | { type: "permission_requested"; request_id: string; title: string; options: PermissionOption[] }
  | { type: "permission_resolved"; request_id: string }
  | ({ type: "usage" } & Usage)
  | ({ type: "quota" } & Quota)
  | { type: "commands_changed" }
  | { type: "options_changed" }
  | { type: "notice"; message: string }
  | { type: "turn_completed" }
  | { type: "exited"; code: number | null }
  | { type: "unrecognized"; raw: string }
  | { type: "native"; kind: string; payload?: unknown };

export type OptionId = "model" | "effort" | "mode" | "permission";

export type Command =
  | { type: "prompt"; text: string }
  | { type: "interrupt" }
  | { type: "compact"; hint?: string | null }
  | { type: "new" }
  | { type: "fork" }
  | { type: "rename"; name: string }
  | { type: "review" }
  | { type: "shell"; command: string }
  | { type: "set_option"; id: OptionId; value: string }
  | { type: "answer_permission"; request_id: string; option_id: string }
  | { type: "invoke"; name: string; args: string }
  | { type: "refresh" }
  | { type: "handoff"; harness: HarnessId; prompt?: string | null };

export interface CommandEntry {
  name: string;
  description: string;
  arg_hint: string | null;
  source: "builtin" | "custom" | "skill" | "mcp" | "plugin" | "acc";
  exec:
    | { exec: "text" }
    | { exec: "rpc"; method: string }
    | { exec: "endpoint"; method: string; path: string }
    | { exec: "skill_item" };
}

export interface OptionDef {
  id: OptionId;
  values: string[];
  current: string | null;
  apply: "live" | "next_turn" | "restart";
}

export interface Received {
  events: Event[];
  /** Bytes to write back to the harness now. Often empty. */
  transmit: Uint8Array;
}
"#;

/// Converts a Rust value to a plain JS value: objects rather than `Map`s,
/// `null` for `None`, numbers for u64 (all values ACC sends fit in 2^53),
/// and `Uint8Array` for bytes.
pub(crate) fn to_js<T: Serialize + ?Sized>(value: &T) -> Result<JsValue, JsError> {
    let serializer = serde_wasm_bindgen::Serializer::new()
        .serialize_maps_as_objects(true)
        .serialize_missing_as_null(true);
    value
        .serialize(&serializer)
        .map_err(|e| JsError::new(&e.to_string()))
}

pub(crate) fn from_js<T: for<'de> Deserialize<'de>>(value: JsValue, what: &str) -> Result<T, JsError> {
    serde_wasm_bindgen::from_value(value).map_err(|e| JsError::new(&format!("invalid {what}: {e}")))
}

/// JS numbers for epoch milliseconds; negative or NaN become 0.
pub(crate) fn ms(now_ms: f64) -> u64 {
    if now_ms.is_finite() && now_ms > 0.0 {
        now_ms as u64
    } else {
        0
    }
}

/// Version of the core library.
#[wasm_bindgen(js_name = version)]
pub fn acc_version() -> String {
    acc_core::VERSION.to_owned()
}

#[derive(Serialize)]
struct HarnessInfo {
    harness: Harness,
    id: &'static str,
    program: &'static str,
    display_name: &'static str,
    native_acp: bool,
}

/// The harnesses ACC supports.
#[wasm_bindgen(unchecked_return_type = "HarnessInfo[]")]
pub fn harnesses() -> Result<JsValue, JsError> {
    let list: Vec<_> = Harness::ALL
        .iter()
        .map(|h| HarnessInfo {
            harness: *h,
            id: h.id(),
            program: h.program(),
            display_name: h.display_name(),
            native_acp: h.native_acp(),
        })
        .collect();
    to_js(&list)
}

/// Quotes one argument for a POSIX `sh` on the host.
#[wasm_bindgen(js_name = shellQuote)]
pub fn shell_quote(arg: &str) -> String {
    acc_core::shell::quote(arg)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpeningJs {
    New,
    Resume { native_id: String },
    Attach,
}

#[derive(Deserialize)]
struct SessionConfigJs {
    harness: Harness,
    cwd: String,
    #[serde(default)]
    opening: Option<OpeningJs>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    client_tag: Option<String>,
}

#[derive(Serialize)]
struct ReceivedJs<'a> {
    events: &'a [acc_core::Event],
    #[serde(with = "serde_bytes_js")]
    transmit: &'a [u8],
}

/// Serializes bytes as a `Uint8Array` rather than a number array.
mod serde_bytes_js {
    pub fn serialize<S: serde::Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(bytes)
    }
}

/// One harness session, sans-IO. See the module docs.
#[wasm_bindgen]
#[derive(Debug)]
pub struct Session {
    inner: core_session::Session,
}

#[wasm_bindgen]
impl Session {
    #[wasm_bindgen(constructor)]
    pub fn new(
        #[wasm_bindgen(unchecked_param_type = "SessionConfig")] config: JsValue,
    ) -> Result<Session, JsError> {
        let c: SessionConfigJs = from_js(config, "session config")?;
        let tag = c.client_tag.unwrap_or_else(|| "acc".to_owned());
        if tag.is_empty()
            || !tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(JsError::new(&format!(
                "client_tag {tag:?} must be [A-Za-z0-9_-]+"
            )));
        }
        let opening = match c.opening.unwrap_or(OpeningJs::New) {
            OpeningJs::New => Opening::New,
            OpeningJs::Resume { native_id } => Opening::Resume { native_id },
            OpeningJs::Attach => Opening::Attach,
        };
        Ok(Self {
            inner: core_session::Session::new(core_session::SessionConfig {
                harness: c.harness,
                cwd: c.cwd,
                opening,
                model: c.model,
                client_tag: tag,
            }),
        })
    }

    #[wasm_bindgen(getter, unchecked_return_type = "HarnessId")]
    pub fn harness(&self) -> Result<JsValue, JsError> {
        to_js(&self.inner.harness())
    }

    /// The process to start on the host.
    #[wasm_bindgen(js_name = launchSpec, unchecked_return_type = "LaunchSpec")]
    pub fn launch_spec(&self) -> Result<JsValue, JsError> {
        to_js(self.inner.launch_spec())
    }

    /// Bytes to write once the process runs. Call once.
    pub fn start(&mut self) -> Vec<u8> {
        self.inner.start()
    }

    /// Feeds harness stdout. `nowMs` is `Date.now()`.
    #[wasm_bindgen(unchecked_return_type = "Received")]
    pub fn receive(&mut self, bytes: &[u8], now_ms: f64) -> Result<JsValue, JsError> {
        let got = self.inner.receive(bytes, ms(now_ms));
        to_js(&ReceivedJs {
            events: &got.events,
            transmit: &got.transmit,
        })
    }

    /// Bytes to write to the harness for a command. Throws if the harness
    /// can't carry it out.
    pub fn send(
        &mut self,
        #[wasm_bindgen(unchecked_param_type = "Command")] command: JsValue,
    ) -> Result<Vec<u8>, JsError> {
        let cmd: Command = from_js(command, "command")?;
        self.inner.send(cmd).map_err(|e| JsError::new(&e.to_string()))
    }

    /// The harness process ended; returns the events to show for it.
    #[wasm_bindgen(unchecked_return_type = "Event[]")]
    pub fn exited(&mut self, code: Option<i32>, now_ms: f64) -> Result<JsValue, JsError> {
        to_js(&self.inner.exited(code, ms(now_ms)))
    }

    /// Merges a state seen elsewhere (OSC 7501 in a pane, a transcript).
    /// Returns true if the visible state changed.
    pub fn observe(
        &mut self,
        #[wasm_bindgen(unchecked_param_type = "SessionState")] state: JsValue,
    ) -> Result<bool, JsError> {
        let s: SessionState = from_js(state, "session state")?;
        Ok(self.inner.observe(s))
    }

    /// The current state, merged across sources by fidelity.
    #[wasm_bindgen(unchecked_return_type = "SessionState | null")]
    pub fn state(&self) -> Result<JsValue, JsError> {
        to_js(&self.inner.state())
    }

    /// The harness's own session id, for resume, once known.
    #[wasm_bindgen(getter, js_name = nativeId)]
    pub fn native_id(&self) -> Option<String> {
        self.inner.native_id().map(str::to_owned)
    }

    /// The command palette. Read again after a `commands_changed` event.
    #[wasm_bindgen(unchecked_return_type = "CommandEntry[]")]
    pub fn commands(&self) -> Result<JsValue, JsError> {
        to_js(self.inner.commands())
    }

    /// Options and current values. Read again after an `options_changed` event.
    #[wasm_bindgen(unchecked_return_type = "OptionDef[]")]
    pub fn options(&self) -> Result<JsValue, JsError> {
        to_js(self.inner.options())
    }
}

/// Shell scripts for a detached, disconnect-proof session under
/// `$HOME/.acc/s/<id>` on the host. Run each with `sh` on the host.
#[wasm_bindgen]
#[derive(Debug)]
pub struct DetachedSession {
    dir: SessionDir,
}

#[wasm_bindgen]
impl DetachedSession {
    /// `id` must be `[A-Za-z0-9_-]{1,64}`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: &str) -> Result<DetachedSession, JsError> {
        let dir = SessionDir::new(id)
            .ok_or_else(|| JsError::new(&format!("session id {id:?} must be [A-Za-z0-9_-]{{1,64}}")))?;
        Ok(Self { dir })
    }

    #[wasm_bindgen(getter)]
    pub fn id(&self) -> String {
        self.dir.id().to_owned()
    }

    /// Starts the harness detached in `cwd`; prints its PID.
    #[wasm_bindgen(js_name = launchScript)]
    pub fn launch_script(
        &self,
        #[wasm_bindgen(unchecked_param_type = "LaunchSpec")] spec: JsValue,
        cwd: &str,
    ) -> Result<String, JsError> {
        let spec: LaunchSpec = from_js(spec, "launch spec")?;
        Ok(self.dir.launch_script(&spec, cwd))
    }

    /// Streams the harness output from byte `offset` and keeps following.
    #[wasm_bindgen(js_name = attachScript)]
    pub fn attach_script(&self, offset: f64) -> String {
        self.dir.attach_script(ms(offset))
    }

    /// Forwards the script's stdin to the harness.
    #[wasm_bindgen(js_name = inputScript)]
    pub fn input_script(&self) -> String {
        self.dir.input_script()
    }

    /// Prints `alive <pid>`, `exited <code>` or `missing`.
    #[wasm_bindgen(js_name = statusScript)]
    pub fn status_script(&self) -> String {
        self.dir.status_script()
    }
}

/// A shell command line that opens `harness`'s own TUI in `cwd`, for
/// palette entries whose `exec` is `terminal`. It starts a fresh session
/// rather than resuming the headless one, so the two never share a session.
#[wasm_bindgen(js_name = tuiCommand)]
pub fn tui_command(harness: &str, cwd: &str) -> Result<String, JsValue> {
    let h = crate::harness(harness)?;
    Ok(format!(
        "cd {} && exec {}",
        acc_core::shell::quote(cwd),
        acc_core::shell::quote(h.program())
    ))
}

/// A command to run on the host, for example over an SSH exec channel:
/// write `stdin` to it (when not empty), then close it.
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccExec {
    /// A POSIX `sh` script.
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
    fn script(script: String) -> Self {
        Self {
            script,
            stdin: Vec::new(),
        }
    }
}

/// An `Error` named `AccError` whose `kind` says what went wrong.
pub(crate) fn error(kind: &str, message: &str) -> JsValue {
    let e = js_sys::Error::new(message);
    e.set_name("AccError");
    // Setting a property on a fresh object can't fail.
    let _ = js_sys::Reflect::set(&e, &JsValue::from_str("kind"), &JsValue::from_str(kind));
    e.into()
}

/// [`to_js`] for values that always serialize (every core type does).
pub(crate) fn js<T: Serialize + ?Sized>(value: &T) -> JsValue {
    to_js(value).unwrap_or(JsValue::NULL)
}

/// [`from_js`] that throws an `AccError` (`InvalidArgument`).
pub(crate) fn arg<T: for<'de> Deserialize<'de>>(value: JsValue, what: &str) -> Result<T, JsValue> {
    serde_wasm_bindgen::from_value(value)
        .map_err(|e| error("InvalidArgument", &format!("invalid {what}: {e}")))
}

/// A harness by the name records use (`claude`) or its id (`claude-code`).
pub(crate) fn harness(name: &str) -> Result<Harness, JsValue> {
    Harness::ALL
        .into_iter()
        .find(|h| {
            serde_json::to_value(h)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .as_deref()
                == Some(name)
                || h.id() == name
        })
        .ok_or_else(|| error("InvalidArgument", &format!("unknown harness {name:?}")))
}

/// An item as the core serializes it, plus its harness-independent `detail`.
pub(crate) fn item_json(i: &Item, h: Harness) -> serde_json::Value {
    let mut v = serde_json::to_value(i).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "detail".into(),
            serde_json::to_value(i.detail(h)).unwrap_or_default(),
        );
    }
    v
}

/// Byte offsets as JS numbers (exact up to 2^53).
pub(crate) fn num(n: u64) -> f64 {
    n as f64
}

//! `AccSession`, `AccFanIn` and the host scripts.
//!
//! Everything is a byte pump: the app runs the [`AccExec`]s it is given over
//! its own connections and feeds back what they print.

use acc_core::command::Command;
use acc_core::detached::{self, FanInParser, FeedEntry};
use acc_core::driver::DriverError;
use acc_core::harness;
use acc_core::item;
use acc_core::session::{self, LaunchRequest, Outbound, Session, SessionError, SessionSpec, Update};
use serde_json::Value;
use wasm_bindgen::prelude::*;

use crate::{AccExec, error, from_json, item_json, ms, num, to_json};

pub(crate) fn session_error(e: SessionError) -> JsValue {
    let kind = match &e {
        SessionError::Driver(DriverError::UnknownRequest(_)) => "UnknownRequest",
        SessionError::Driver(DriverError::InvalidOption(_)) => "InvalidOption",
        SessionError::Driver(DriverError::NotStarted) => "NotStarted",
        SessionError::Driver(DriverError::MissingArgument(_)) => "MissingArgument",
        SessionError::Driver(DriverError::Unsupported { .. }) => "Unsupported",
        SessionError::Driver(DriverError::AttachmentRejected(_)) => "AttachmentRejected",
        _ => "InvalidSession",
    };
    error(kind, &e.to_string())
}

fn spec_from(json: &str) -> Result<SessionSpec, JsValue> {
    SessionSpec::from_json(json).ok_or_else(|| error("InvalidSession", "unreadable spec.json"))
}

fn updates(u: Vec<Update>) -> String {
    to_json(&u)
}

/// What to do for a command: `kind` is `send` (write `bytes` with
/// `input_exec`, possibly empty) or `relaunch` (launch `request`, a
/// `LaunchRequest` JSON, as a new session).
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccOutbound {
    pub kind: String,
    pub bytes: Vec<u8>,
    pub request: Option<String>,
}

impl From<Outbound> for AccOutbound {
    fn from(o: Outbound) -> Self {
        match o {
            Outbound::Send(bytes) => Self {
                kind: "send".into(),
                bytes,
                request: None,
            },
            Outbound::Relaunch(r) => Self {
                kind: "relaunch".into(),
                bytes: Vec::new(),
                request: Some(to_json(&r)),
            },
        }
    }
}

/// One detached agent session on a host, from this client's point of view.
///
/// 1. `launch` (then run `take_launch_exec`; its stdout is the PID) or
///    `attach` (then run `status_exec` and pass its stdout to `on_status`).
/// 2. Follow the log from `offset` and pass every chunk to `feed`.
/// 3. After `feed` and `command`, send `take_transmit` with `input_exec`.
#[wasm_bindgen]
#[derive(Debug)]
pub struct AccSession {
    inner: Session,
    launch: Option<AccExec>,
}

#[wasm_bindgen]
impl AccSession {
    /// Starts a new session from a `LaunchRequest` JSON. `id` comes from
    /// `new_session_id`; `client_tag` must be unique among clients that may
    /// share the session.
    pub fn launch(
        request_json: &str,
        id: &str,
        client_tag: &str,
        now_ms: f64,
    ) -> Result<AccSession, JsValue> {
        let request: LaunchRequest = from_json(request_json, "launch request")?;
        let (inner, exec) = Session::launch(request, id, client_tag, ms(now_ms)).map_err(session_error)?;
        Ok(Self {
            inner,
            launch: Some(exec.into()),
        })
    }

    /// Joins a session from its `spec.json`.
    pub fn attach(spec_json: &str, client_tag: &str) -> Result<AccSession, JsValue> {
        Ok(Self {
            inner: Session::attach(spec_from(spec_json)?, client_tag).map_err(session_error)?,
            launch: None,
        })
    }

    /// Picks a session up from `snapshot` (see `snapshot`), for the session
    /// in `spec_json` as the host lists it now. Continue as after `attach`.
    /// Fails for a snapshot of another session or ACC version: attach then.
    pub fn restore(snapshot: &str, spec_json: &str) -> Result<AccSession, JsValue> {
        Ok(Self {
            inner: Session::restore(snapshot, spec_from(spec_json)?).map_err(session_error)?,
            launch: None,
        })
    }

    /// Everything read of the session so far, to persist and `restore`
    /// later instead of replaying the whole log.
    pub fn snapshot(&self) -> Option<String> {
        self.inner.snapshot()
    }

    /// The script that starts the harness, once, for a launched session.
    pub fn take_launch_exec(&mut self) -> Option<AccExec> {
        self.launch.take()
    }

    pub fn status_exec(&self) -> AccExec {
        self.inner.status_exec().into()
    }

    /// Follows the log from the current offset on a channel of its own.
    pub fn attach_exec(&self) -> AccExec {
        self.inner.attach_exec().into()
    }

    pub fn input_exec(&self, bytes: Vec<u8>) -> AccExec {
        self.inner.input_exec(bytes).into()
    }

    pub fn kill_exec(&self) -> AccExec {
        self.inner.kill_exec().into()
    }

    pub fn remove_exec(&self) -> AccExec {
        self.inner.remove_exec().into()
    }

    /// A rewrite of `spec.json`, when the native id or title became known.
    pub fn take_spec_write(&mut self) -> Option<AccExec> {
        self.inner.take_spec_write().map(Into::into)
    }

    /// JSON array of `Update`s.
    pub fn on_status(&mut self, stdout: &str, now_ms: f64) -> String {
        updates(self.inner.on_status(stdout, ms(now_ms)))
    }

    /// The content of the session's exit file, from a fan-in `session_exit` entry.
    pub fn on_exit_file(&mut self, text: &str, now_ms: f64) -> String {
        updates(self.inner.on_exit_file(text, ms(now_ms)))
    }

    /// Bytes of the log, starting at `offset`. JSON array of `Update`s.
    pub fn feed(&mut self, bytes: &[u8], now_ms: f64) -> String {
        updates(self.inner.feed(bytes, ms(now_ms)))
    }

    /// Runs a `Command` JSON.
    pub fn command(&mut self, command_json: &str) -> Result<AccOutbound, JsValue> {
        let command: Command = from_json(command_json, "command")?;
        Ok(self.inner.command(command).map_err(session_error)?.into())
    }

    /// What the last command did that the harness won't report (the prompt
    /// of an ACP agent). Call after `command`; otherwise `feed` returns it.
    pub fn take_updates(&mut self, now_ms: f64) -> String {
        updates(self.inner.take_updates(ms(now_ms)))
    }

    /// Bytes to write to the harness now (send with `input_exec`).
    pub fn take_transmit(&mut self) -> Vec<u8> {
        self.inner.take_transmit()
    }

    /// The session's `spec.json`, for `attach` and `restore`.
    pub fn spec(&self) -> String {
        self.inner.spec().to_json()
    }

    pub fn offset(&self) -> f64 {
        num(self.inner.offset())
    }

    pub fn is_replaying(&self) -> bool {
        self.inner.is_replaying()
    }

    pub fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }

    /// `SessionState` JSON, or `null`.
    pub fn state(&self) -> String {
        to_json(&self.inner.state())
    }

    /// JSON array of items, each with its `detail`.
    pub fn items(&self) -> String {
        let h = self.inner.harness();
        items(self.inner.transcript().items().iter(), h)
    }

    pub fn item(&self, id: &str) -> Option<String> {
        let h = self.inner.harness();
        self.inner.transcript().get(id).map(|i| to_json(&item_json(i, h)))
    }

    /// The main conversation: items that didn't run inside a subagent.
    pub fn top_level_items(&self) -> String {
        let h = self.inner.harness();
        items(self.inner.transcript().top_level(), h)
    }

    /// The child conversation of the `subagent` item `parent`.
    pub fn children(&self, parent: &str) -> String {
        let h = self.inner.harness();
        items(self.inner.transcript().children(parent), h)
    }

    /// Every subagent run in the session.
    pub fn subagents(&self) -> String {
        let h = self.inner.harness();
        items(self.inner.transcript().subagents(), h)
    }

    pub fn usage(&self) -> String {
        to_json(self.inner.usage())
    }

    /// `Quota` JSON, or `null`.
    pub fn quota(&self) -> String {
        to_json(&self.inner.quota())
    }

    pub fn permissions(&self) -> String {
        to_json(self.inner.permissions())
    }

    pub fn commands(&self) -> String {
        to_json(self.inner.commands())
    }

    pub fn options(&self) -> String {
        to_json(self.inner.options())
    }
}

pub(crate) fn items<'a>(
    it: impl Iterator<Item = &'a acc_core::transcript::Item>,
    h: acc_core::Harness,
) -> String {
    to_json(&it.map(|i| item_json(i, h)).collect::<Vec<Value>>())
}

/// One line of a followed file.
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug, Clone, PartialEq)]
pub struct AccFeedLine {
    pub key: String,
    /// The line's bytes, newline included.
    pub bytes: Vec<u8>,
    /// File offset just past this line.
    pub offset: f64,
}

/// Follows many files on a host over one channel (see `exec`).
#[wasm_bindgen]
#[derive(Debug)]
pub struct AccFanIn {
    entries: Vec<FeedEntry>,
    parser: FanInParser,
}

#[wasm_bindgen]
impl AccFanIn {
    /// From a JSON array of `FeedEntry`s.
    #[wasm_bindgen(constructor)]
    pub fn new(entries_json: &str) -> Result<AccFanIn, JsValue> {
        let entries: Vec<FeedEntry> = from_json(entries_json, "feed entries")?;
        Ok(Self {
            parser: FanInParser::new(&entries),
            entries,
        })
    }

    /// The long-running script; run it on a long-lived channel.
    pub fn exec(&self) -> AccExec {
        AccExec::script(detached::fan_in_script(&self.entries))
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<AccFeedLine> {
        self.parser
            .feed(bytes)
            .into_iter()
            .map(|l| AccFeedLine {
                key: l.key,
                bytes: l.bytes,
                offset: num(l.offset),
            })
            .collect()
    }

    /// Bytes of `key`'s file consumed so far.
    pub fn offset(&self, key: &str) -> Option<f64> {
        self.parser.offset(key).map(num)
    }
}

/// Lists every detached session on the host.
#[wasm_bindgen]
pub fn list_sessions_exec() -> AccExec {
    AccExec::script(detached::list_script())
}

/// JSON array of `Listing`s.
#[wasm_bindgen]
pub fn parse_session_list(stdout: &str) -> String {
    to_json(&detached::parse_list(stdout))
}

/// `Status` JSON, or `null`.
#[wasm_bindgen]
pub fn parse_session_status(stdout: &str) -> String {
    to_json(&detached::parse_status(stdout))
}

/// Finds the installed harnesses (login `PATH`, with versions).
#[wasm_bindgen]
pub fn probe_exec() -> AccExec {
    AccExec::script(harness::probe_script())
}

/// JSON array of `Availability`s.
#[wasm_bindgen]
pub fn parse_probe(stdout: &str) -> String {
    to_json(&harness::parse_probe(stdout))
}

/// Prints the absolute path of a directory (`~` expanded) or fails.
#[wasm_bindgen]
pub fn resolve_dir_exec(dir: &str) -> AccExec {
    AccExec::script(session::resolve_dir_script(dir))
}

/// A session id from at least 15 random bytes supplied by the caller.
#[wasm_bindgen]
pub fn new_session_id(harness: &str, entropy: &[u8]) -> Result<String, JsValue> {
    Ok(session::new_session_id(crate::harness(harness)?, entropy))
}

/// Parses `git diff` output: JSON array of `FileDiff`s.
#[wasm_bindgen]
pub fn parse_unified_diff(text: &str) -> String {
    to_json(&item::parse_unified(text, ""))
}

/// Display name of a harness.
#[wasm_bindgen]
pub fn harness_display_name(harness: &str) -> Result<String, JsValue> {
    Ok(crate::harness(harness)?.display_name().to_owned())
}

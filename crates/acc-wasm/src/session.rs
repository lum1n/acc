//! `AccSession`, `AccFanIn` and the host scripts.
//!
//! Everything is a byte pump: the app runs the [`AccExec`]s it is given over
//! its own connections and feeds back what they print.

use acc_core::command::Command;
use acc_core::detached::{self, FanInParser, FeedEntry};
use acc_core::driver::DriverError;
use acc_core::harness;
use acc_core::item;
use acc_core::policy::{self, Policy, Rule};
use acc_core::session::{self, LaunchRequest, Outbound, Session, SessionError, SessionSpec, Update};
use wasm_bindgen::prelude::*;

use crate::{AccExec, arg, error, js, ms, num};

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

fn updates(u: Vec<Update>) -> JsValue {
    js(&u)
}

/// What to do for a command: `kind` is `send` (write `bytes` with
/// `input_exec`, possibly empty) or `relaunch` (launch `request`, a
/// `LaunchRequest` JSON, as a new session).
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug, Clone)]
pub struct AccOutbound {
    pub kind: String,
    pub bytes: Vec<u8>,
    pub request: JsValue,
}

impl From<Outbound> for AccOutbound {
    fn from(o: Outbound) -> Self {
        match o {
            Outbound::Send(bytes) => Self {
                kind: "send".into(),
                bytes,
                request: JsValue::NULL,
            },
            Outbound::Relaunch(r) => Self {
                kind: "relaunch".into(),
                bytes: Vec::new(),
                request: js(&r),
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
    pub fn launch(request: JsValue, id: &str, client_tag: &str, now_ms: f64) -> Result<AccSession, JsValue> {
        let request: LaunchRequest = arg(request.clone(), "launch request")?;
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
    #[wasm_bindgen(js_name = takeLaunchExec)]
    pub fn take_launch_exec(&mut self) -> Option<AccExec> {
        self.launch.take()
    }

    #[wasm_bindgen(js_name = statusExec)]
    pub fn status_exec(&self) -> AccExec {
        self.inner.status_exec().into()
    }

    /// Follows the log from the current offset on a channel of its own.
    #[wasm_bindgen(js_name = attachExec)]
    pub fn attach_exec(&self) -> AccExec {
        self.inner.attach_exec().into()
    }

    #[wasm_bindgen(js_name = inputExec)]
    pub fn input_exec(&self, bytes: Vec<u8>) -> AccExec {
        self.inner.input_exec(bytes).into()
    }

    #[wasm_bindgen(js_name = killExec)]
    pub fn kill_exec(&self) -> AccExec {
        self.inner.kill_exec().into()
    }

    /// Starts a follower on the host that records approvals, finished
    /// turns, errors and the exit in the session's events while no client is
    /// connected, running `hook` (a `Hook`, or `null`) for each. Run again
    /// after a relaunch, or to change the hook.
    #[wasm_bindgen(js_name = watchExec)]
    pub fn watch_exec(&self, hook: JsValue) -> Result<AccExec, JsValue> {
        let hook: Option<acc_core::watch::Hook> = arg(hook, "hook")?;
        Ok(self.inner.watch_exec(hook.as_ref()).into())
    }

    /// Prints the follower's events from byte `offset` (see `parseSessionEvents`).
    #[wasm_bindgen(js_name = eventsExec)]
    pub fn events_exec(&self, offset: f64) -> AccExec {
        self.inner.events_exec(ms(offset)).into()
    }

    #[wasm_bindgen(js_name = removeExec)]
    pub fn remove_exec(&self) -> AccExec {
        self.inner.remove_exec().into()
    }

    /// A rewrite of `spec.json`, when the native id or title became known.
    #[wasm_bindgen(js_name = takeSpecWrite)]
    pub fn take_spec_write(&mut self) -> Option<AccExec> {
        self.inner.take_spec_write().map(Into::into)
    }

    /// JSON array of `Update`s.
    #[wasm_bindgen(js_name = onStatus)]
    pub fn on_status(&mut self, stdout: &str, now_ms: f64) -> JsValue {
        updates(self.inner.on_status(stdout, ms(now_ms)))
    }

    /// The content of the session's exit file, from a fan-in `session_exit` entry.
    #[wasm_bindgen(js_name = onExitFile)]
    pub fn on_exit_file(&mut self, text: &str, now_ms: f64) -> JsValue {
        updates(self.inner.on_exit_file(text, ms(now_ms)))
    }

    /// Bytes of the log, starting at `offset`. JSON array of `Update`s.
    pub fn feed(&mut self, bytes: &[u8], now_ms: f64) -> JsValue {
        updates(self.inner.feed(bytes, ms(now_ms)))
    }

    /// Runs a `Command` JSON.
    pub fn command(&mut self, command: JsValue) -> Result<AccOutbound, JsValue> {
        let command: Command = arg(command.clone(), "command")?;
        Ok(self.inner.command(command).map_err(session_error)?.into())
    }

    /// What the last command did that the harness won't report (the prompt
    /// of an ACP agent). Call after `command`; otherwise `feed` returns it.
    #[wasm_bindgen(js_name = takeUpdates)]
    pub fn take_updates(&mut self, now_ms: f64) -> JsValue {
        updates(self.inner.take_updates(ms(now_ms)))
    }

    /// Bytes to write to the harness now (send with `input_exec`).
    #[wasm_bindgen(js_name = takeTransmit)]
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

    #[wasm_bindgen(js_name = isReplaying)]
    pub fn is_replaying(&self) -> bool {
        self.inner.is_replaying()
    }

    #[wasm_bindgen(js_name = isAlive)]
    pub fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }

    /// `SessionState` JSON, or `null`.
    pub fn state(&self) -> JsValue {
        js(&self.inner.state())
    }

    /// JSON array of `Item`s.
    pub fn items(&self) -> JsValue {
        items(self.inner.transcript().items().iter())
    }

    pub fn item(&self, id: &str) -> JsValue {
        js(&self.inner.transcript().get(id))
    }

    /// The main conversation: items that didn't run inside a subagent.
    #[wasm_bindgen(js_name = topLevelItems)]
    pub fn top_level_items(&self) -> JsValue {
        items(self.inner.transcript().top_level())
    }

    /// The child conversation of the `subagent` item `parent`.
    pub fn children(&self, parent: &str) -> JsValue {
        items(self.inner.transcript().children(parent))
    }

    /// Every subagent run in the session.
    pub fn subagents(&self) -> JsValue {
        items(self.inner.transcript().subagents())
    }

    pub fn usage(&self) -> JsValue {
        js(self.inner.usage())
    }

    /// `Quota` JSON, or `null`.
    pub fn quota(&self) -> JsValue {
        js(&self.inner.quota())
    }

    /// Answers approvals by these rules (a JSON array of `Rule`s) from now
    /// on, including any waiting (deny wins over allow; questions are left
    /// to the user). Set it on one client per session. An empty list stops.
    /// JSON array of `Update`s.
    #[wasm_bindgen(js_name = setPolicy)]
    pub fn set_policy(&mut self, rules: JsValue) -> Result<JsValue, JsValue> {
        let rules: Vec<Rule> = arg(rules, "policy rules")?;
        let policy = (!rules.is_empty()).then_some(Policy { rules });
        Ok(updates(self.inner.set_policy(policy)))
    }

    /// Prints what the session changed in its worktree as a `git diff`
    /// (parse it with `parseUnifiedDiff`).
    #[wasm_bindgen(js_name = worktreeDiffExec)]
    pub fn worktree_diff_exec(&self) -> Result<AccExec, JsValue> {
        Ok(self.inner.worktree_diff_exec().map_err(session_error)?.into())
    }

    /// Commits the worktree's changes and merges them into the branch
    /// checked out in the repository; exits 7 on a conflict.
    #[wasm_bindgen(js_name = worktreeMergeExec)]
    pub fn worktree_merge_exec(&self, message: &str, squash: bool) -> Result<AccExec, JsValue> {
        Ok(self
            .inner
            .worktree_merge_exec(message, squash)
            .map_err(session_error)?
            .into())
    }

    /// Stops the harness and deletes the worktree with its branch.
    #[wasm_bindgen(js_name = worktreeDiscardExec)]
    pub fn worktree_discard_exec(&self) -> Result<AccExec, JsValue> {
        Ok(self.inner.worktree_discard_exec().map_err(session_error)?.into())
    }

    /// What the session spent over time (`UsageRecord`), for `rollUpUsage`.
    #[wasm_bindgen(js_name = usageRecord)]
    pub fn usage_record(&self) -> JsValue {
        js(&self.inner.usage_record())
    }

    pub fn permissions(&self) -> JsValue {
        js(self.inner.permissions())
    }

    pub fn commands(&self) -> JsValue {
        js(self.inner.commands())
    }

    pub fn options(&self) -> JsValue {
        js(self.inner.options())
    }
}

pub(crate) fn items<'a>(it: impl Iterator<Item = &'a acc_core::transcript::Item>) -> JsValue {
    js(&it.collect::<Vec<_>>())
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
    pub fn new(entries: JsValue) -> Result<AccFanIn, JsValue> {
        let entries: Vec<FeedEntry> = arg(entries.clone(), "feed entries")?;
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
#[wasm_bindgen(js_name = listSessionsExec)]
pub fn list_sessions_exec() -> AccExec {
    AccExec::script(detached::list_script())
}

/// JSON array of `Listing`s.
#[wasm_bindgen(js_name = parseSessionList)]
pub fn parse_session_list(stdout: &str) -> JsValue {
    js(&detached::parse_list(stdout))
}

/// `Status` JSON, or `null`.
#[wasm_bindgen(js_name = parseSessionStatus)]
pub fn parse_session_status(stdout: &str) -> JsValue {
    js(&detached::parse_status(stdout))
}

/// A follower's events (from `eventsExec` or a `session_events` feed): a
/// JSON array of `HostEvent`s.
#[wasm_bindgen(js_name = parseSessionEvents)]
pub fn parse_session_events(stdout: &str) -> JsValue {
    js(&acc_core::watch::parse_events(stdout))
}

/// Finds the installed harnesses (login `PATH`, with versions).
#[wasm_bindgen(js_name = probeExec)]
pub fn probe_exec() -> AccExec {
    AccExec::script(harness::probe_script())
}

/// JSON array of `Availability`s.
#[wasm_bindgen(js_name = parseProbe)]
pub fn parse_probe(stdout: &str) -> JsValue {
    js(&harness::parse_probe(stdout))
}

/// Prints the absolute path of a directory (`~` expanded) or fails.
#[wasm_bindgen(js_name = resolveDirExec)]
pub fn resolve_dir_exec(dir: &str) -> AccExec {
    AccExec::script(session::resolve_dir_script(dir))
}

/// Makes git worktree `name` (use the session id) for the repository `cwd`
/// is in, from `base` (default `HEAD`), and prints the directory to launch
/// in with the request's `worktree: name`. Exits 6 outside a repository.
#[wasm_bindgen(js_name = worktreeAddExec)]
pub fn worktree_add_exec(cwd: &str, name: &str, base: Option<String>) -> Result<AccExec, JsValue> {
    if !acc_core::worktree::valid_name(name) {
        return Err(session_error(SessionError::InvalidWorktree(name.to_owned())));
    }
    Ok(AccExec::script(acc_core::worktree::add_script(
        cwd,
        name,
        base.as_deref(),
    )))
}

/// The settings file with the rules added, as a script, and the indexes of
/// rules the harness has no way to say.
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug, Clone)]
pub struct AccSettingsWrite {
    pub exec: AccExec,
    pub skipped: Vec<u32>,
}

/// Prints `harness`'s permission settings for a session in `cwd`, for
/// `permissionSettingsWriteExec`. `undefined` for Copilot and pi.
#[wasm_bindgen(js_name = permissionSettingsReadExec)]
pub fn permission_settings_read_exec(harness: &str, cwd: &str) -> Result<Option<AccExec>, JsValue> {
    Ok(policy::settings_read_script(crate::harness(harness)?, cwd).map(AccExec::script))
}

/// Adds `rules` (a JSON array of `Rule`s) to the settings `existing` (from
/// `permissionSettingsReadExec`) and writes them back, so the harness stops
/// asking: Claude, Codex, Cursor and opencode.
#[wasm_bindgen(js_name = permissionSettingsWriteExec)]
pub fn permission_settings_write_exec(
    harness: &str,
    cwd: &str,
    existing: &str,
    rules: JsValue,
) -> Result<AccSettingsWrite, JsValue> {
    let h = crate::harness(harness)?;
    let rules: Vec<Rule> = arg(rules, "policy rules")?;
    let (text, skipped) = policy::merge_settings(h, existing, &rules).map_err(|e| {
        let kind = match e {
            policy::SettingsError::Unsupported(_) => "Unsupported",
            policy::SettingsError::Unreadable(_) => "InvalidArgument",
        };
        error(kind, &e.to_string())
    })?;
    let script = policy::settings_write_script(h, cwd)
        .ok_or_else(|| error("Unsupported", &format!("{h:?} has no permission settings")))?;
    Ok(AccSettingsWrite {
        exec: AccExec {
            script,
            stdin: text.into_bytes(),
        },
        skipped: skipped.into_iter().map(|i| i as u32).collect(),
    })
}

/// A session id from at least 15 random bytes supplied by the caller.
#[wasm_bindgen(js_name = newSessionId)]
pub fn new_session_id(harness: &str, entropy: &[u8]) -> Result<String, JsValue> {
    Ok(session::new_session_id(crate::harness(harness)?, entropy))
}

/// Parses `git diff` output: JSON array of `FileDiff`s.
#[wasm_bindgen(js_name = parseUnifiedDiff)]
pub fn parse_unified_diff(text: &str) -> JsValue {
    js(&item::parse_unified(text, ""))
}

/// Adds `UsageRecord`s up per account and per local day: a `UsageRollup`.
/// `utc_offset_minutes` is the time zone days are split in (120 for UTC+2).
#[wasm_bindgen(js_name = rollUpUsage)]
pub fn roll_up_usage(records: JsValue, utc_offset_minutes: i32) -> Result<JsValue, JsValue> {
    let records: Vec<acc_core::usage::UsageRecord> = arg(records, "usage records")?;
    Ok(js(&acc_core::usage::roll_up(&records, utc_offset_minutes)))
}

/// Display name of a harness.
#[wasm_bindgen(js_name = harnessDisplayName)]
pub fn harness_display_name(harness: &str) -> Result<String, JsValue> {
    Ok(crate::harness(harness)?.display_name().to_owned())
}

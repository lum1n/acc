//! Detached sessions: a driver bound to a session directory on a host.
//!
//! [`Session`] is what a client holds for one agent. It knows how to start
//! the harness detached ([`crate::detached`]), how to join one that is
//! already running (replaying its log through a fresh driver without
//! re-answering anything), and it folds the driver's events into a
//! [`Transcript`], a [`StateTracker`], usage, quota and open permission
//! requests. Like the rest of the core it does no I/O: it hands out
//! [`Exec`]s to run on the host and takes the bytes they print.
//!
//! A typical client:
//! 1. `Session::launch` (run the returned exec; its stdout is the PID), or
//!    `Session::attach` with a spec from [`crate::detached::list_script`]
//!    and then [`Session::status_exec`] → [`Session::on_status`].
//! 2. Follow the log from [`Session::offset`] (with
//!    [`Session::attach_exec`] or a shared [`crate::detached::fan_in_script`])
//!    and pass every chunk to [`Session::feed`].
//! 3. After each `feed` and `command`, write [`Session::take_transmit`]
//!    through [`Session::input_exec`].
//!
//! A client that persists [`Session::snapshot`] can [`Session::restore`]
//! it later (after a relaunch) and read only what the log gained since.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::acp::{AcpConfig, AcpDriver, Start, launch_spec as acp_launch};
use crate::claude::{ClaudeConfig, ClaudeDriver};
use crate::codex::{CodexConfig, CodexDriver};
use crate::command::{Apply, Attachment, Command, CommandEntry, OptionDef, OptionId};
use crate::detached::{SessionDir, Status, parse_status};
use crate::driver::{Driver, DriverError};
use crate::event::{Event, ItemId, ItemKind, PermissionOption, Quota, Usage};
use crate::harness::{Harness, LaunchSpec, Launcher};
use crate::multiplexer::Exec;
use crate::pi::PiDriver;
use crate::state::{BlockKind, RunState, SessionState, StateSource, StateTracker};
use crate::transcript::{ItemChange, Transcript};

/// Version of the `spec.json` format.
pub const SPEC_VERSION: u32 = 1;

/// Version of [`Session::snapshot`]. Bump it whenever a driver's state
/// changes meaning without changing shape; older snapshots are then refused.
pub const SNAPSHOT_VERSION: u32 = 1;

/// After this long without news, a lower-fidelity state source may override.
const STALE_MS: u64 = 30_000;

/// What to start.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRequest {
    pub harness: Option<Harness>,
    /// Working directory on the host. Absolute for ACP harnesses (Cursor,
    /// Copilot, opencode); see [`resolve_dir_script`].
    pub cwd: String,
    pub model: Option<String>,
    /// Claude permission mode, or Codex permission profile.
    pub permission: Option<String>,
    /// Harness session to resume (Claude session id, Codex thread id, ACP
    /// session id, pi session file).
    pub resume: Option<String>,
    /// With `resume`: continue in a new session forked from it.
    pub fork: bool,
    pub title: Option<String>,
    /// First prompt, sent as soon as the harness can take it.
    pub prompt: Option<String>,
    /// Images sent with the first prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// The session this one continues (fork) or replaces (restart).
    pub parent: Option<String>,
    /// Set when the new session replaces `parent`, which should be stopped.
    pub replaces_parent: bool,
    pub origin: Origin,
    /// Start the harness the way a taken-over TUI was started (its sandbox
    /// or other wrapper, executable and flags).
    #[serde(default)]
    pub launcher: Option<Launcher>,
}

/// How a session came to be.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Origin {
    #[default]
    Started,
    /// Resumed headless from a TUI session ACC took over.
    TakenOver {
        /// Multiplexer pane the TUI ran in, as `<kind>:<target>`.
        pane: Option<String>,
        transcript: Option<String>,
    },
}

/// Everything another client needs to attach; stored as `spec.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpec {
    pub v: u32,
    pub id: String,
    pub harness: Harness,
    pub cwd: String,
    pub launch: LaunchSpec,
    pub model: Option<String>,
    pub permission: Option<String>,
    pub resume: Option<String>,
    #[serde(default)]
    pub fork: bool,
    /// The harness's own session id, once known.
    pub native_id: Option<String>,
    pub title: Option<String>,
    pub created_ms: u64,
    pub parent: Option<String>,
    #[serde(default)]
    pub origin: Origin,
    /// Kept so relaunches (fork, model change) and hand-back use it too.
    #[serde(default)]
    pub launcher: Option<Launcher>,
}

impl SessionSpec {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }
}

/// A request the harness is waiting on the user for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Permission {
    pub request_id: String,
    pub title: String,
    pub options: Vec<PermissionOption>,
    pub kind: BlockKind,
    pub item_id: Option<ItemId>,
    pub input: Value,
}

/// One event with what it changed in the transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Update {
    pub event: Event,
    pub item: Option<ItemChange>,
    /// True while replaying the log of a session being attached: the event
    /// already happened, so clients shouldn't notify about it.
    pub replay: bool,
}

/// What to do for a command.
// Built once per action; boxing a launch request isn't worth the churn.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Outbound {
    /// Write these bytes through [`Session::input_exec`] (may be empty).
    Send(Vec<u8>),
    /// The harness can only do this by starting a new process: launch this
    /// (and stop this session first when `replaces_parent` is set).
    Relaunch(LaunchRequest),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionError {
    #[error("invalid session id {0:?}")]
    InvalidId(String),
    #[error("{0:?} needs an absolute working directory, got {1:?}")]
    RelativeCwd(Harness, String),
    #[error("no harness given")]
    NoHarness,
    /// The snapshot is unreadable, from another version, or of another
    /// session. Attach afresh instead.
    #[error("unusable snapshot: {0}")]
    Snapshot(String),
    #[error(transparent)]
    Driver(#[from] DriverError),
}

/// One detached agent session, from one client's point of view.
pub struct Session {
    spec: SessionSpec,
    dir: SessionDir,
    driver: Box<dyn Driver>,
    client_tag: String,
    offset: u64,
    /// While `offset` is below this, fed bytes are a replay of history.
    replay_until: Option<u64>,
    alive: bool,
    tracker: StateTracker,
    transcript: Transcript,
    usage: Usage,
    quota: Option<Quota>,
    permissions: Vec<Permission>,
    transmit: Vec<u8>,
    /// First prompt, held until the harness has a session to take it.
    pending_prompt: Option<Command>,
    spec_dirty: bool,
    exited: bool,
    /// The harness has been sent its handshake (or the attach queries)
    /// by this client. They aren't repeated on a reconnect or a restore:
    /// every query's answer lands in the log, which would grow per attach.
    handshaken: bool,
}

/// What [`Session::snapshot`] writes; borrowed on the way out.
#[derive(Serialize, Deserialize)]
struct Snapshot<'a> {
    v: u32,
    spec: Cow<'a, SessionSpec>,
    client_tag: Cow<'a, str>,
    driver: Value,
    offset: u64,
    tracker: Cow<'a, StateTracker>,
    transcript: Cow<'a, Transcript>,
    usage: Cow<'a, Usage>,
    quota: Cow<'a, Option<Quota>>,
    permissions: Cow<'a, [Permission]>,
    pending_prompt: Cow<'a, Option<Command>>,
    spec_dirty: bool,
    exited: bool,
    handshaken: bool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.spec.id)
            .field("harness", &self.spec.harness)
            .field("offset", &self.offset)
            .field("replay_until", &self.replay_until)
            .field("items", &self.transcript.len())
            .finish_non_exhaustive()
    }
}

/// Separates the status line from the history in [`Session::status_exec`].
const HISTORY_MARK: &str = "--acc-history--";

fn driver_for(spec: &SessionSpec, attach: bool, tag: &str) -> Box<dyn Driver> {
    let resume = if attach { None } else { spec.resume.clone() };
    match spec.harness {
        Harness::Claude => Box::new(ClaudeDriver::new(ClaudeConfig {
            model: spec.model.clone(),
            permission_mode: spec.permission.clone(),
            fork: spec.fork,
            client_tag: Some(tag.to_owned()),
        })),
        Harness::Codex => Box::new(CodexDriver::new(CodexConfig {
            resume,
            // The process already runs in the session's directory.
            cwd: None,
            model: spec.model.clone(),
            approval_policy: None,
            permissions: spec.permission.clone(),
            client_tag: Some(tag.to_owned()),
        })),
        Harness::Pi => Box::new(PiDriver::with_id_prefix(tag)),
        h @ (Harness::Cursor | Harness::Copilot | Harness::Opencode) => {
            let start = match (attach, resume) {
                (true, _) => Start::Attach,
                (false, Some(session_id)) => Start::Resume { session_id },
                (false, None) => Start::New,
            };
            let mut cfg = AcpConfig::new(spec.cwd.clone(), start);
            cfg.client_tag = tag.to_owned();
            cfg.model = spec.model.clone();
            Box::new(AcpDriver::new(h, cfg))
        }
    }
}

fn restore_driver(harness: Harness, state: Value) -> Result<Box<dyn Driver>, serde_json::Error> {
    Ok(match harness {
        Harness::Claude => Box::new(serde_json::from_value::<ClaudeDriver>(state)?),
        Harness::Codex => Box::new(serde_json::from_value::<CodexDriver>(state)?),
        Harness::Pi => Box::new(serde_json::from_value::<PiDriver>(state)?),
        Harness::Cursor | Harness::Copilot | Harness::Opencode => {
            Box::new(serde_json::from_value::<AcpDriver>(state)?)
        }
    })
}

/// Whether the harness takes a prompt before it reports its session.
fn prompt_before_session(h: Harness) -> bool {
    matches!(h, Harness::Claude | Harness::Pi)
}

impl Session {
    /// Starts a new detached session. Run the returned exec on the host
    /// (its stdout is the harness PID), then follow the log from offset 0.
    pub fn launch(
        req: LaunchRequest,
        id: &str,
        client_tag: &str,
        now_ms: u64,
    ) -> Result<(Self, Exec), SessionError> {
        let dir = SessionDir::new(id).ok_or_else(|| SessionError::InvalidId(id.to_owned()))?;
        let harness = req.harness.ok_or(SessionError::NoHarness)?;
        if harness.native_acp() && !req.cwd.starts_with('/') {
            return Err(SessionError::RelativeCwd(harness, req.cwd));
        }
        let mut spec = SessionSpec {
            v: SPEC_VERSION,
            id: id.to_owned(),
            harness,
            cwd: req.cwd.clone(),
            launch: LaunchSpec {
                program: String::new(),
                args: Vec::new(),
                env: Vec::new(),
                protocol: harness.preferred_protocol(),
            },
            model: req.model.clone(),
            permission: req.permission.clone(),
            resume: req.resume.clone(),
            fork: req.fork,
            native_id: if req.fork { None } else { req.resume.clone() },
            title: req.title.clone(),
            created_ms: now_ms,
            parent: req.parent.clone(),
            origin: req.origin.clone(),
            launcher: req.launcher.clone(),
        };
        let mut driver = driver_for(&spec, false, client_tag);
        spec.launch = match harness {
            h if h.native_acp() => acp_launch(h, spec.model.as_deref()),
            Harness::Codex => Harness::Codex.launch(None),
            _ => driver.launch(spec.resume.as_deref()),
        };
        if let Some(launcher) = &spec.launcher {
            spec.launch = launcher.wrap(spec.launch);
        }
        let exec = Exec::new(dir.launch_script_with_spec(&spec.launch, &spec.cwd, Some(&spec.to_json())));
        let mut transmit = driver.handshake();
        let has_text = req.prompt.as_deref().is_some_and(|p| !p.trim().is_empty());
        let mut pending_prompt = (has_text || !req.attachments.is_empty()).then(|| Command::Prompt {
            text: req.prompt.unwrap_or_default(),
            attachments: req.attachments,
        });
        if harness == Harness::Pi
            && let Some(model) = &spec.model
        {
            transmit.extend(driver.command(Command::SetOption {
                id: OptionId::Model,
                value: model.clone(),
            })?);
        }
        if prompt_before_session(harness)
            && let Some(prompt) = pending_prompt.take()
        {
            transmit.extend(driver.command(prompt)?);
        }
        let session = Self {
            dir,
            driver,
            client_tag: client_tag.to_owned(),
            offset: 0,
            replay_until: Some(0),
            alive: true,
            tracker: StateTracker::new(STALE_MS),
            transcript: Transcript::new(),
            usage: Usage::default(),
            quota: None,
            permissions: Vec::new(),
            transmit,
            pending_prompt,
            spec_dirty: false,
            exited: false,
            handshaken: true,
            spec,
        };
        Ok((session, exec))
    }

    /// Joins a session started earlier (by this client or another). Run
    /// [`Session::status_exec`] and pass its output to
    /// [`Session::on_status`] before following the log.
    pub fn attach(spec: SessionSpec, client_tag: &str) -> Result<Self, SessionError> {
        let dir = SessionDir::new(&spec.id).ok_or_else(|| SessionError::InvalidId(spec.id.clone()))?;
        let driver = driver_for(&spec, true, client_tag);
        Ok(Self {
            dir,
            driver,
            client_tag: client_tag.to_owned(),
            offset: 0,
            replay_until: None,
            alive: false,
            tracker: StateTracker::new(STALE_MS),
            transcript: Transcript::new(),
            usage: Usage::default(),
            quota: None,
            permissions: Vec::new(),
            transmit: Vec::new(),
            pending_prompt: None,
            spec_dirty: false,
            exited: false,
            handshaken: false,
            spec,
        })
    }

    /// Everything this client has read of the session, as JSON, for
    /// [`Session::restore`]. `None` if a driver can't be saved.
    pub fn snapshot(&self) -> Option<String> {
        let snap = Snapshot {
            v: SNAPSHOT_VERSION,
            spec: Cow::Borrowed(&self.spec),
            client_tag: Cow::Borrowed(&self.client_tag),
            driver: self.driver.snapshot()?,
            offset: self.offset,
            tracker: Cow::Borrowed(&self.tracker),
            transcript: Cow::Borrowed(&self.transcript),
            usage: Cow::Borrowed(&self.usage),
            quota: Cow::Borrowed(&self.quota),
            permissions: Cow::Borrowed(&self.permissions),
            pending_prompt: Cow::Borrowed(&self.pending_prompt),
            spec_dirty: self.spec_dirty,
            exited: self.exited,
            handshaken: self.handshaken,
        };
        serde_json::to_string(&snap).ok()
    }

    /// Picks a session up from a [`Session::snapshot`], for the session
    /// `spec` describes (as listed on the host now). Continue as after
    /// [`Session::attach`]: [`Session::on_status`], then follow the log from
    /// [`Session::offset`]. What the log gained since the snapshot replays
    /// like history. If the log turns out shorter than the snapshot read
    /// (it was replaced), `on_status` starts over from offset 0.
    pub fn restore(snapshot: &str, spec: SessionSpec) -> Result<Self, SessionError> {
        let bad = |why: &str| SessionError::Snapshot(why.to_owned());
        let snap: Snapshot<'static> = serde_json::from_str(snapshot).map_err(|e| bad(&e.to_string()))?;
        if snap.v != SNAPSHOT_VERSION {
            return Err(bad(&format!("version {}", snap.v)));
        }
        let saved = snap.spec.into_owned();
        if saved.id != spec.id || saved.harness != spec.harness || saved.created_ms != spec.created_ms {
            return Err(bad("another session"));
        }
        let dir = SessionDir::new(&spec.id).ok_or_else(|| SessionError::InvalidId(spec.id.clone()))?;
        let driver = restore_driver(spec.harness, snap.driver).map_err(|e| bad(&e.to_string()))?;
        // The host's spec may be newer (another client named the session);
        // what only this client learned is kept.
        let mut spec = spec;
        if spec.native_id.is_none() {
            spec.native_id = saved.native_id;
        }
        if spec.title.is_none() {
            spec.title = saved.title;
        }
        if spec.model.is_none() {
            spec.model = saved.model;
        }
        Ok(Self {
            dir,
            driver,
            client_tag: snap.client_tag.into_owned(),
            offset: snap.offset,
            replay_until: None,
            alive: false,
            tracker: snap.tracker.into_owned(),
            transcript: snap.transcript.into_owned(),
            usage: snap.usage.into_owned(),
            quota: snap.quota.into_owned(),
            permissions: snap.permissions.into_owned(),
            transmit: Vec::new(),
            pending_prompt: snap.pending_prompt.into_owned(),
            spec_dirty: snap.spec_dirty,
            exited: snap.exited,
            handshaken: snap.handshaken,
            spec,
        })
    }

    // ---- execs -------------------------------------------------------------

    /// Checks the session and, for harnesses that keep history their
    /// protocol won't replay, reads it, so pass the output to
    /// [`Session::on_status`] before feeding the log.
    pub fn status_exec(&self) -> Exec {
        let history = self
            .spec
            .native_id
            .as_deref()
            .and_then(|id| self.driver.history_script(id));
        match history {
            Some(h) => Exec::new(format!("{}echo {HISTORY_MARK}\n{h}", self.dir.status_script())),
            None => Exec::new(self.dir.status_script()),
        }
    }

    /// Follows the log from the current offset, for clients that use one
    /// channel per session rather than a shared fan-in.
    pub fn attach_exec(&self) -> Exec {
        Exec::new(self.dir.attach_script(self.offset))
    }

    /// Writes `bytes` to the harness (typically [`Session::take_transmit`]).
    pub fn input_exec(&self, bytes: Vec<u8>) -> Exec {
        Exec::with_stdin(self.dir.input_script(), bytes)
    }

    pub fn kill_exec(&self) -> Exec {
        Exec::new(self.dir.kill_script(3_000))
    }

    pub fn remove_exec(&self) -> Exec {
        Exec::new(self.dir.remove_script())
    }

    /// `spec.json` needs rewriting (the native id or title became known).
    /// Returns the exec once; run it when convenient.
    pub fn take_spec_write(&mut self) -> Option<Exec> {
        if !std::mem::take(&mut self.spec_dirty) {
            return None;
        }
        Some(Exec::new(self.dir.write_spec_script(&self.spec.to_json())))
    }

    // ---- input ---------------------------------------------------------------

    /// Applies the output of [`Session::status_exec`].
    pub fn on_status(&mut self, stdout: &str, now_ms: u64) -> Vec<Update> {
        let (stdout, history) = match stdout.split_once(&format!("\n{HISTORY_MARK}\n")) {
            Some((status, history)) => (status, Some(history)),
            None => (stdout, None),
        };
        let status = parse_status(stdout);
        if let Some(status) = &status
            && !matches!(status, Status::Missing)
            && status.size() < self.offset
        {
            // Not the log this client read (restored from an old snapshot).
            self.start_over();
        }
        if let Some(history) = history {
            self.driver.on_history(history);
        }
        let Some(status) = status else {
            return Vec::new();
        };
        if self.replay_until.is_none() {
            self.replay_until = Some(status.size());
        }
        let mut out = Vec::new();
        match status {
            Status::Alive { .. } => {
                self.alive = true;
                if self.offset >= self.replay_until.unwrap_or(0) {
                    self.finish_replay();
                }
            }
            Status::Exited { code, .. } => self.exit(code, now_ms, &mut out),
            Status::Missing => self.exit(None, now_ms, &mut out),
        }
        out
    }

    /// Applies the content of the session's exit file (from a fan-in entry).
    pub fn on_exit_file(&mut self, text: &str, now_ms: u64) -> Vec<Update> {
        let mut out = Vec::new();
        self.exit(text.trim().parse().ok(), now_ms, &mut out);
        out
    }

    /// Feeds bytes from the log, which must start at [`Session::offset`].
    pub fn feed(&mut self, bytes: &[u8], now_ms: u64) -> Vec<Update> {
        let mut out = self.take_updates(now_ms);
        let until = self.replay_until.unwrap_or(u64::MAX);
        let split = if self.offset < until {
            (until - self.offset).min(bytes.len() as u64) as usize
        } else {
            0
        };
        let (old, new) = bytes.split_at(split);
        if !old.is_empty() {
            self.offset += old.len() as u64;
            let events = self.driver.on_output(old);
            self.apply(events, true, now_ms, &mut out);
            // Replies to requests that were answered long ago must not be sent again.
            let _ = self.driver.poll_transmit();
            if self.offset >= until && self.replay_until.is_some() {
                self.finish_replay();
            }
        }
        if !new.is_empty() {
            self.offset += new.len() as u64;
            let events = self.driver.on_output(new);
            self.apply(events, false, now_ms, &mut out);
            let t = self.driver.poll_transmit();
            self.transmit.extend(t);
        }
        out.extend(self.take_updates(now_ms));
        out
    }

    /// What the last command did that the harness won't report, such as the
    /// prompt of a harness that doesn't repeat it. Call after
    /// [`Session::command`] to show it at once; otherwise the next
    /// [`Session::feed`] returns it.
    pub fn take_updates(&mut self, now_ms: u64) -> Vec<Update> {
        let mut out = Vec::new();
        let events = self.driver.take_events();
        self.apply(events, false, now_ms, &mut out);
        out
    }

    /// Turns a command into bytes for the harness, or a relaunch.
    pub fn command(&mut self, cmd: Command) -> Result<Outbound, SessionError> {
        match &cmd {
            Command::Fork if self.spec.harness == Harness::Claude => {
                return Ok(Outbound::Relaunch(self.relaunch(true, None)));
            }
            Command::SetOption { id, value } => {
                let restart = self
                    .driver
                    .options()
                    .iter()
                    .any(|o| o.id == *id && o.apply == Apply::Restart);
                if restart && *id == OptionId::Model {
                    return Ok(Outbound::Relaunch(self.relaunch(false, Some(value.clone()))));
                }
            }
            Command::AnswerPermission { request_id, .. } => {
                let request_id = request_id.clone();
                let bytes = self.driver.command(cmd)?;
                self.permissions.retain(|p| p.request_id != request_id);
                return Ok(Outbound::Send(bytes));
            }
            _ => {}
        }
        Ok(Outbound::Send(self.driver.command(cmd)?))
    }

    fn relaunch(&self, fork: bool, model: Option<String>) -> LaunchRequest {
        LaunchRequest {
            harness: Some(self.spec.harness),
            cwd: self.spec.cwd.clone(),
            model: model.or_else(|| self.spec.model.clone()),
            permission: self.spec.permission.clone(),
            resume: self.spec.native_id.clone().or_else(|| self.spec.resume.clone()),
            fork,
            title: self.spec.title.clone(),
            prompt: None,
            attachments: Vec::new(),
            parent: Some(self.spec.id.clone()),
            replaces_parent: !fork,
            origin: self.spec.origin.clone(),
            launcher: self.spec.launcher.clone(),
        }
    }

    /// Bytes the session wants written to the harness.
    pub fn take_transmit(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.transmit)
    }

    // ---- state ---------------------------------------------------------------

    pub fn spec(&self) -> &SessionSpec {
        &self.spec
    }

    pub fn id(&self) -> &str {
        &self.spec.id
    }

    pub fn harness(&self) -> Harness {
        self.spec.harness
    }

    /// Bytes of the log consumed so far; follow the log from here.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn is_replaying(&self) -> bool {
        self.replay_until.is_none_or(|u| self.offset < u)
    }

    pub fn is_alive(&self) -> bool {
        self.alive && !self.exited
    }

    pub fn state(&self) -> Option<&SessionState> {
        self.tracker.current()
    }

    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    pub fn quota(&self) -> Option<&Quota> {
        self.quota.as_ref()
    }

    pub fn permissions(&self) -> &[Permission] {
        &self.permissions
    }

    pub fn commands(&self) -> &[CommandEntry] {
        self.driver.commands()
    }

    pub fn options(&self) -> &[OptionDef] {
        self.driver.options()
    }

    // ---- internals -----------------------------------------------------------

    fn finish_replay(&mut self) {
        let Some(until) = self.replay_until else { return };
        self.replay_until = Some(until.min(self.offset));
        if self.alive && !self.exited && until > 0 && !self.handshaken {
            let t = self.driver.attach_handshake();
            self.transmit.extend(t);
            self.handshaken = true;
        }
    }

    /// Forgets everything read and replays the log from the start, as a
    /// fresh [`Session::attach`] would.
    fn start_over(&mut self) {
        self.driver = driver_for(&self.spec, true, &self.client_tag);
        self.offset = 0;
        self.replay_until = None;
        self.tracker = StateTracker::new(STALE_MS);
        self.transcript = Transcript::new();
        self.usage = Usage::default();
        self.quota = None;
        self.permissions.clear();
        self.transmit.clear();
        self.pending_prompt = None;
        self.exited = false;
        self.handshaken = false;
    }

    fn exit(&mut self, code: Option<i32>, now_ms: u64, out: &mut Vec<Update>) {
        self.alive = false;
        if self.exited {
            return;
        }
        self.exited = true;
        let replay = self.is_replaying();
        let mut events = vec![Event::Exited { code }];
        events.extend(self.permissions.drain(..).map(|p| Event::PermissionResolved {
            request_id: p.request_id,
        }));
        // 128+N is death by signal N: stopped (by the user, or the host
        // shutting down), not a failure of the harness.
        let (state, message) = match code {
            None | Some(0) => (RunState::Exited, None),
            Some(c) if c > 128 => (RunState::Exited, Some(format!("Stopped (signal {})", c - 128))),
            Some(c) => (RunState::Error, Some(format!("Exited with code {c}"))),
        };
        events.push(Event::StateChanged(SessionState {
            state,
            message,
            source: StateSource::Native,
            observed_at_ms: now_ms,
        }));
        self.apply(events, replay, now_ms, out);
        out.extend(completions(self.transcript.close_running(), replay));
    }

    fn apply(&mut self, events: Vec<Event>, replay: bool, now_ms: u64, out: &mut Vec<Update>) {
        for mut event in events {
            match &mut event {
                Event::StateChanged(s) => {
                    // Once the process is gone, history replayed after the
                    // status check can't bring it back to life.
                    if self.exited && !matches!(s.state, RunState::Exited | RunState::Error) {
                        continue;
                    }
                    s.observed_at_ms = now_ms;
                    if !self.tracker.observe(s.clone()) {
                        continue;
                    }
                }
                Event::SessionStarted { native_id, model, .. } => {
                    if self.spec.native_id.as_deref() != Some(native_id) {
                        self.spec.native_id = Some(native_id.clone());
                        self.spec_dirty = !replay;
                    }
                    if model.is_some() && self.spec.model.is_none() {
                        self.spec.model.clone_from(model);
                    }
                    if !replay && let Some(prompt) = self.pending_prompt.take() {
                        match self.driver.command(prompt) {
                            Ok(bytes) => self.transmit.extend(bytes),
                            Err(e) => out.push(Update {
                                event: Event::Notice {
                                    message: format!("Couldn't send the first prompt: {e}"),
                                },
                                item: None,
                                replay,
                            }),
                        }
                    }
                }
                Event::Usage(u) => merge_usage(&mut self.usage, u),
                Event::Quota(q) => self.quota = Some(q.clone()),
                Event::PermissionRequested {
                    request_id,
                    title,
                    options,
                    kind,
                    item_id,
                    input,
                } => {
                    self.permissions.retain(|p| p.request_id != *request_id);
                    self.permissions.push(Permission {
                        request_id: request_id.clone(),
                        title: title.clone(),
                        options: options.clone(),
                        kind: *kind,
                        item_id: item_id.clone(),
                        input: input.clone(),
                    });
                }
                Event::PermissionResolved { request_id } => {
                    self.permissions.retain(|p| p.request_id != *request_id);
                }
                _ => {}
            }
            out.extend(completions(self.transcript.settle(&event), replay));
            let item = self.transcript.apply(&event);
            if self.spec.title.is_none()
                && let Some(ItemChange::Completed { id }) = &item
                && let Some(it) = self.transcript.get(id)
                && it.kind == ItemKind::UserMessage
                && !it.text.trim().is_empty()
            {
                self.spec.title = Some(crate::wire::one_line(&it.text, 80));
                self.spec_dirty = !replay;
            }
            out.push(Update { event, item, replay });
        }
    }
}

/// Updates for items [`Transcript`] closed itself, as if the harness had
/// completed them.
pub(crate) fn completions(changes: Vec<ItemChange>, replay: bool) -> impl Iterator<Item = Update> {
    changes.into_iter().map(move |change| Update {
        event: Event::ItemCompleted {
            id: change.id().to_owned(),
            extensions: Value::Null,
        },
        item: Some(change),
        replay,
    })
}

fn merge_usage(into: &mut Usage, from: &Usage) {
    macro_rules! take {
        ($($f:ident),*) => { $( if from.$f.is_some() { into.$f = from.$f; } )* };
    }
    take!(
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        cost_usd,
        context_used,
        context_window
    );
}

/// Script printing the absolute path of `dir` (`~` expanded), or failing
/// when it isn't a directory. ACP harnesses need absolute paths.
pub fn resolve_dir_script(dir: &str) -> String {
    let path = match dir.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", crate::shell::quote(rest)),
        None if dir == "~" || dir.is_empty() => "\"$HOME\"".to_owned(),
        None => crate::shell::quote(dir),
    };
    format!("cd {path} && pwd -P\n")
}

/// A new session id from caller-supplied randomness (the core has no RNG):
/// `<harness>-<base32 of entropy>`.
pub fn new_session_id(harness: Harness, entropy: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::from(harness.program());
    out.push('-');
    let mut bits = 0u32;
    let mut acc = 0u32;
    for &b in entropy.iter().take(15) {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const CLAUDE_PERMISSION: &str = include_str!("../tests/fixtures/claude/permission-deny.ndjson");
    const CLAUDE_HI: &str = include_str!("../tests/fixtures/claude/hi-and-controls.ndjson");
    const CODEX_HANDSHAKE: &str = include_str!("../tests/fixtures/codex/handshake.ndjson");
    const PI_TURN: &str = include_str!("../tests/fixtures/pi/rpc-turn-with-dialog.jsonl");

    fn req(h: Harness) -> LaunchRequest {
        LaunchRequest {
            harness: Some(h),
            cwd: "/w".into(),
            ..LaunchRequest::default()
        }
    }

    fn attached(h: Harness, log: &str, alive: bool) -> (Session, Vec<Update>) {
        let (s, _) = Session::launch(req(h), "s1", "first", 1).unwrap();
        let mut a = Session::attach(s.spec().clone(), "second").unwrap();
        let status = if alive {
            format!("alive 7 {}", log.len())
        } else {
            format!("exited 0 {}", log.len())
        };
        let mut ups = a.on_status(&status, 2);
        for chunk in log.as_bytes().chunks(611) {
            ups.extend(a.feed(chunk, 3));
        }
        (a, ups)
    }

    #[test]
    fn launches_inside_the_launcher_and_relaunches_keep_it() {
        let mut r = req(Harness::Claude);
        r.launcher = Some(Launcher {
            prefix: vec!["bwrap".into(), "--die-with-parent".into(), "--".into()],
            program: Some("/usr/bin/claude".into()),
            args: vec!["--dangerously-skip-permissions".into()],
        });
        let (mut s, exec) = Session::launch(r, "s1", "t", 1).unwrap();
        assert!(
            exec.script
                .contains("bwrap --die-with-parent -- /usr/bin/claude -p"),
            "{}",
            exec.script
        );
        assert!(exec.script.contains("--dangerously-skip-permissions"));
        let Ok(Outbound::Relaunch(again)) = s.command(Command::Fork) else {
            panic!("fork relaunches")
        };
        assert_eq!(again.launcher, s.spec().launcher);
    }

    #[test]
    fn launch_writes_spec_and_queues_handshake_and_prompt() {
        let mut r = req(Harness::Claude);
        r.prompt = Some("hello".into());
        r.cwd = "~/proj".into();
        let (mut s, exec) = Session::launch(r, "c1", "ph", 5).unwrap();
        assert!(exec.script.contains("spec.json"));
        assert!(exec.script.contains("\"$HOME\"/proj"));
        let t = String::from_utf8(s.take_transmit()).unwrap();
        assert!(t.contains("\"ph-1\"") && t.contains("initialize"));
        assert!(t.contains("hello"));
        assert!(!s.is_replaying());
        let spec = SessionSpec::from_json(&s.spec().to_json()).unwrap();
        assert_eq!(spec.launch.program, "claude");
    }

    #[test]
    fn acp_needs_absolute_cwd_and_codex_waits_for_thread_to_prompt() {
        let mut r = req(Harness::Opencode);
        r.cwd = "proj".into();
        assert!(matches!(
            Session::launch(r, "o1", "t", 0),
            Err(SessionError::RelativeCwd(..))
        ));

        let mut r = req(Harness::Codex);
        r.prompt = Some("go".into());
        let (mut s, _) = Session::launch(r, "x1", "t", 0).unwrap();
        assert!(
            !String::from_utf8(s.take_transmit())
                .unwrap()
                .contains("turn/start")
        );
        s.feed(CODEX_HANDSHAKE.as_bytes(), 1);
        let t = String::from_utf8(s.take_transmit()).unwrap();
        assert!(t.contains("turn/start") && t.contains("\"go\""));
    }

    #[test]
    fn launch_sends_images_with_or_without_text() {
        let png = Attachment::from_bytes("image/png", b"PNG");
        let mut r = req(Harness::Claude);
        r.attachments = vec![png.clone()];
        let (mut s, _) = Session::launch(r, "c2", "ph", 5).unwrap();
        let t = String::from_utf8(s.take_transmit()).unwrap();
        assert!(t.contains("\"type\":\"image\"") && t.contains("UE5H"));

        // Codex holds the whole prompt, images included, until it has a thread.
        let mut r = req(Harness::Codex);
        r.prompt = Some("go".into());
        r.attachments = vec![png];
        let (mut s, _) = Session::launch(r, "x2", "t", 0).unwrap();
        assert!(!String::from_utf8(s.take_transmit()).unwrap().contains("image"));
        s.feed(CODEX_HANDSHAKE.as_bytes(), 1);
        let t = String::from_utf8(s.take_transmit()).unwrap();
        assert!(t.contains("turn/start") && t.contains("data:image/png;base64,UE5H"));
    }

    #[test]
    fn replay_never_transmits_and_resolves_answered_permissions() {
        let (mut a, ups) = attached(Harness::Claude, CLAUDE_PERMISSION, true);
        assert!(ups.iter().all(|u| u.replay));
        assert!(a.permissions().is_empty());
        assert!(!a.is_replaying());
        assert_eq!(a.offset(), CLAUDE_PERMISSION.len() as u64);
        // Only the attach queries go out, tagged as this client's.
        let t = String::from_utf8(a.take_transmit()).unwrap();
        assert!(t.contains("second-") && !t.contains("initialize"));
        assert!(a.transcript().len() >= 2);
    }

    #[test]
    fn live_bytes_after_replay_are_not_replay() {
        let (s, _) = Session::launch(req(Harness::Claude), "s1", "a", 1).unwrap();
        let mut b = Session::attach(s.spec().clone(), "b").unwrap();
        let (head, tail) = CLAUDE_HI.split_at(CLAUDE_HI.len() / 2);
        let cut = head.rfind('\n').unwrap() + 1;
        let (head, tail) = (&CLAUDE_HI[..cut], format!("{}{tail}", &head[cut..]));
        b.on_status(&format!("alive 1 {}", head.len()), 0);
        let replayed = b.feed(head.as_bytes(), 0);
        let live = b.feed(tail.as_bytes(), 0);
        assert!(replayed.iter().all(|u| u.replay));
        assert!(live.iter().all(|u| !u.replay));
        assert!(live.iter().any(|u| u.event == Event::TurnCompleted));
    }

    #[test]
    fn exited_session_closes_items_and_permissions() {
        let log: String = CLAUDE_PERMISSION
            .lines()
            .take_while(|l| !l.contains("\"tool_result\""))
            .map(|l| format!("{l}\n"))
            .collect();
        let (s, _) = Session::launch(req(Harness::Claude), "s1", "a", 1).unwrap();
        let mut b = Session::attach(s.spec().clone(), "b").unwrap();
        b.on_status(&format!("alive 1 {}", log.len()), 0);
        b.feed(log.as_bytes(), 0);
        assert_eq!(b.permissions().len(), 1);
        let ups = b.on_exit_file("0\n", 9);
        assert!(
            ups.iter()
                .any(|u| matches!(u.event, Event::Exited { code: Some(0) }))
        );
        assert!(b.permissions().is_empty());
        assert_eq!(b.state().map(|s| &s.state), Some(&RunState::Exited));
        assert!(!b.is_alive());
        assert!(b.take_transmit().is_empty() || !b.is_alive());
    }

    #[test]
    fn stopped_by_signal_is_exited_and_failure_is_error() {
        let (s, _) = Session::launch(req(Harness::Pi), "p1", "a", 1).unwrap();
        let mut a = Session::attach(s.spec().clone(), "b").unwrap();
        a.on_exit_file("143\n", 2);
        assert_eq!(a.state().map(|s| &s.state), Some(&RunState::Exited));
        let mut b = Session::attach(s.spec().clone(), "c").unwrap();
        b.on_exit_file("1\n", 2);
        assert_eq!(b.state().map(|s| &s.state), Some(&RunState::Error));
    }

    #[test]
    fn exit_known_before_replay_stays_exited() {
        let (a, _) = attached(Harness::Claude, CLAUDE_HI, false);
        assert_eq!(a.state().map(|s| &s.state), Some(&RunState::Exited));
    }

    #[test]
    fn copilot_status_reads_the_prompts_its_log_lacks() {
        let (s, _) = Session::launch(req(Harness::Copilot), "s1", "first", 1).unwrap();
        let mut spec = s.spec().clone();
        let a = Session::attach(spec.clone(), "second").unwrap();
        assert!(!a.status_exec().script.contains(HISTORY_MARK), "no native id yet");

        spec.native_id = Some("c0ffee".into());
        let mut a = Session::attach(spec, "second").unwrap();
        assert!(
            a.status_exec()
                .script
                .contains("session-state/c0ffee/events.jsonl")
        );
        let log = concat!(
            r#"{"jsonrpc":"2.0","id":"first-1","result":{"sessionId":"c0ffee"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"c0ffee","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ok"}}}}"#,
            "\n",
        );
        let status = format!(
            "alive 7 {}\n{HISTORY_MARK}\n{}\n",
            log.len(),
            r#"{"type":"user.message","data":{"content":"say ok"}}"#
        );
        a.on_status(&status, 2);
        assert!(a.is_alive());
        a.feed(log.as_bytes(), 3);
        let items: Vec<_> = a
            .transcript()
            .items()
            .iter()
            .map(|i| (i.kind, i.text.as_str()))
            .collect();
        assert_eq!(
            items,
            [(ItemKind::UserMessage, "say ok"), (ItemKind::AgentMessage, "ok")]
        );
    }

    #[test]
    fn dead_session_replays_without_queries() {
        let (mut a, _) = attached(Harness::Pi, PI_TURN, false);
        assert!(a.take_transmit().is_empty());
        assert!(!a.transcript().is_empty());
    }

    #[test]
    fn claude_fork_is_a_relaunch_from_the_native_id() {
        let (mut a, _) = attached(Harness::Claude, CLAUDE_HI, true);
        let Ok(Outbound::Relaunch(r)) = a.command(Command::Fork) else {
            panic!("relaunch")
        };
        assert!(r.fork && !r.replaces_parent);
        assert_eq!(r.resume.as_deref(), Some("5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11"));
        assert_eq!(r.parent.as_deref(), Some("s1"));
    }

    #[test]
    fn native_id_and_title_mark_spec_for_rewrite_once() {
        let (mut s, _) = Session::launch(req(Harness::Claude), "s1", "a", 1).unwrap();
        s.feed(CLAUDE_HI.as_bytes(), 2);
        assert!(s.spec().native_id.is_some());
        let exec = s.take_spec_write().expect("spec write");
        assert!(exec.script.contains("spec.json"));
        assert!(s.take_spec_write().is_none());
    }

    /// Reads the first half of `log` (at a line boundary), saves a
    /// snapshot, restores it and reads the rest: the result must match a
    /// client that read the whole log in one go.
    fn restored_matches_full_read(h: Harness, log: &str) {
        let (full, _) = attached(h, log, true);
        let (s, _) = Session::launch(req(h), "s1", "first", 1).unwrap();
        let cut = log[..log.len() / 2].rfind('\n').map_or(0, |i| i + 1);
        let (head, tail) = log.split_at(cut);
        let mut a = Session::attach(s.spec().clone(), "second").unwrap();
        a.on_status(&format!("alive 7 {}", head.len()), 2);
        a.feed(head.as_bytes(), 3);
        let sent = a.take_transmit();
        let snap = a.snapshot().expect("snapshot");

        let mut b = Session::restore(&snap, s.spec().clone()).unwrap();
        assert_eq!(b.offset(), head.len() as u64);
        b.on_status(&format!("alive 7 {}", log.len()), 4);
        assert!(b.is_replaying() || tail.is_empty());
        let ups = b.feed(tail.as_bytes(), 5);
        assert!(ups.iter().all(|u| u.replay), "{h:?}: gained history is replay");
        assert!(!b.is_replaying());
        assert_eq!(b.transcript().items(), full.transcript().items(), "{h:?}");
        assert_eq!(b.usage(), full.usage(), "{h:?}");
        assert_eq!(b.commands(), full.commands(), "{h:?}");
        assert_eq!(b.options(), full.options(), "{h:?}");
        if !sent.is_empty() {
            assert!(b.take_transmit().is_empty(), "{h:?}: handshake sent once");
        }
    }

    #[test]
    fn restored_snapshot_reads_on_like_a_full_replay() {
        restored_matches_full_read(Harness::Claude, CLAUDE_HI);
        restored_matches_full_read(Harness::Claude, CLAUDE_PERMISSION);
        restored_matches_full_read(Harness::Codex, CODEX_HANDSHAKE);
        restored_matches_full_read(Harness::Pi, PI_TURN);
        restored_matches_full_read(
            Harness::Opencode,
            include_str!("../tests/fixtures/acp/opencode-1.18.35.out.ndjson"),
        );
    }

    #[test]
    fn reattaching_doesnt_repeat_the_attach_queries() {
        let (mut a, _) = attached(Harness::Claude, CLAUDE_HI, true);
        assert!(!a.take_transmit().is_empty());
        a.on_status(&format!("alive 7 {}", CLAUDE_HI.len()), 9);
        assert!(a.take_transmit().is_empty());
        let mut b = Session::restore(&a.snapshot().unwrap(), a.spec().clone()).unwrap();
        b.on_status(&format!("alive 7 {}", CLAUDE_HI.len()), 10);
        assert!(!b.is_replaying());
        assert!(b.take_transmit().is_empty());
    }

    #[test]
    fn a_shorter_log_than_the_snapshot_read_starts_over() {
        let (a, _) = attached(Harness::Claude, CLAUDE_HI, true);
        let mut b = Session::restore(&a.snapshot().unwrap(), a.spec().clone()).unwrap();
        b.on_status("alive 8 10", 2);
        assert_eq!(b.offset(), 0);
        assert!(b.transcript().is_empty());
        assert!(b.is_replaying());
        let ups = b.feed(&CLAUDE_HI.as_bytes()[..10], 3);
        assert!(ups.iter().all(|u| u.replay));
    }

    #[test]
    fn snapshots_of_another_session_or_version_are_refused() {
        let (a, _) = attached(Harness::Claude, CLAUDE_HI, false);
        let snap = a.snapshot().unwrap();
        let mut other = a.spec().clone();
        other.id = "s2".into();
        assert!(matches!(
            Session::restore(&snap, other),
            Err(SessionError::Snapshot(_))
        ));
        let old = snap.replacen(&format!("\"v\":{SNAPSHOT_VERSION}"), "\"v\":0", 1);
        assert!(matches!(
            Session::restore(&old, a.spec().clone()),
            Err(SessionError::Snapshot(_))
        ));
        assert!(Session::restore("{", a.spec().clone()).is_err());
        // An exited session stays exited and keeps its title.
        let b = Session::restore(&snap, a.spec().clone()).unwrap();
        assert!(!b.is_alive());
        assert_eq!(b.state().map(|s| &s.state), Some(&RunState::Exited));
        assert_eq!(b.spec().title, a.spec().title);
    }

    #[test]
    fn session_ids_are_valid_and_dirs_resolve() {
        let id = new_session_id(Harness::Codex, &[0xff; 15]);
        assert_eq!(id.len(), "codex-".len() + 24);
        assert!(SessionDir::new(&id).is_some());
        assert_eq!(resolve_dir_script("~/a b"), "cd \"$HOME\"/'a b' && pwd -P\n");
    }
}

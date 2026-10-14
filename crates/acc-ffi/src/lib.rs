//! UniFFI surface of acc-core for Swift (iOS/macOS) and Kotlin (Android/JVM).
//!
//! Two ways in:
//! - Sans-IO: a [`Session`] turns harness bytes into events and commands into
//!   bytes, and the app moves those bytes over whatever it has. These calls
//!   are short and never block on I/O.
//! - With the SSH transport built in ([`remote`]): an [`SshHost`] launches and
//!   rejoins detached sessions ([`RemoteSession`]) and co-drives harness TUIs
//!   in tmux, zellij, screen or Rex panes ([`PaneSession`]). These calls are
//!   `async`/`suspend` and survive dropped connections.
//!
//! - Detached sessions as a byte pump ([`AccSession`], [`AccFanIn`],
//!   [`AccObserved`], [`AccTakeover`]): the app runs the scripts ACC hands it
//!   over its own SSH channels and feeds back their output. Sessions are
//!   launched, listed and attached by any client, replayed from their log,
//!   and foreign TUIs are observed and taken over.
//!
//! Objects can be shared across threads. Panics are caught by UniFFI and
//! surfaced as errors instead of crashing the app.

use std::sync::{Mutex, MutexGuard};

use acc_core::detached::SessionDir;
use acc_core::driver::DriverError;
use acc_core::live as core_session;
use acc_core::osc7501::{self, Osc7501};

pub mod acc_types;
pub mod observe;
mod remote;
pub mod session;
mod types;

pub use acc_types::*;
pub use observe::*;
pub use remote::*;
pub use session::*;
pub use types::*;

uniffi::setup_scaffolding!();

/// Version of the core library.
#[uniffi::export]
pub fn acc_version() -> String {
    acc_core::VERSION.to_owned()
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct HarnessInfo {
    pub harness: Harness,
    pub id: String,
    pub program: String,
    pub display_name: String,
    pub native_acp: bool,
}

/// The harnesses ACC supports.
#[uniffi::export]
pub fn harnesses() -> Vec<HarnessInfo> {
    acc_core::Harness::ALL
        .iter()
        .map(|h| HarnessInfo {
            harness: (*h).into(),
            id: h.id().to_owned(),
            program: h.program().to_owned(),
            display_name: h.display_name().to_owned(),
            native_acp: h.native_acp(),
        })
        .collect()
}

/// Quotes one argument for a POSIX `sh` on the host.
#[uniffi::export]
pub fn shell_quote(arg: String) -> String {
    acc_core::shell::quote(&arg)
}

/// Errors from the bindings.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum AccError {
    /// The harness can't do this headlessly, or ACC doesn't support it yet.
    // Fields are not called `message`: Kotlin errors extend Throwable,
    // whose `message` property they would clash with.
    #[error("{reason}")]
    Unsupported { reason: String },
    #[error("no session is active yet")]
    NotStarted,
    #[error("unknown permission request {request_id}")]
    UnknownRequest { request_id: String },
    #[error("unknown option value {value}")]
    InvalidOption { value: String },
    #[error("invalid argument: {reason}")]
    InvalidArgument { reason: String },
    /// No answer from the host in time. Calling again retries.
    #[error("timed out talking to the host")]
    Timeout,
    /// The connection dropped. Calling again redials.
    #[error("the connection to the host was lost")]
    Disconnected,
    #[error("the host rejected the credentials for {user}")]
    AuthRejected { user: String },
    /// The host's key isn't trusted; show the fingerprint to the user.
    #[error("untrusted host key {fingerprint}")]
    HostKeyRejected { fingerprint: String },
    #[error("ssh-agent: {reason}")]
    Agent { reason: String },
    #[error("ssh: {reason}")]
    Ssh { reason: String },
    /// The detached session's harness is not running.
    #[error("no harness is running in session {session_id}")]
    NotRunning { session_id: String },
    #[error("remote script failed (exit {exit_code:?}): {stderr}")]
    Remote { exit_code: Option<u32>, stderr: String },
    #[error("multiplexer: {reason}")]
    Multiplexer { reason: String },
    /// A detached session's spec, snapshot or agent record didn't read.
    #[error("invalid session: {reason}")]
    InvalidSession { reason: String },
    /// The palette entry needs input (see `AccCommandEntry::input`).
    #[error("{reason}")]
    MissingArgument { reason: String },
    /// An image the harnesses or the session log can't carry (over 5 MiB,
    /// over 10 MiB per prompt, or not an image). Resize and retry.
    #[error("{reason}")]
    AttachmentRejected { reason: String },
}

impl From<DriverError> for AccError {
    fn from(e: DriverError) -> Self {
        match e {
            DriverError::Unsupported { .. } => Self::Unsupported {
                reason: e.to_string(),
            },
            DriverError::NotStarted => Self::NotStarted,
            DriverError::UnknownRequest(request_id) => Self::UnknownRequest { request_id },
            DriverError::InvalidOption(value) => Self::InvalidOption { value },
            DriverError::MissingArgument(_) => Self::MissingArgument {
                reason: e.to_string(),
            },
            DriverError::AttachmentRejected(_) => Self::AttachmentRejected {
                reason: e.to_string(),
            },
        }
    }
}

/// Locks a mutex, recovering from poisoning: every core call leaves its
/// state consistent, so a panic in one call shouldn't brick the object.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Sessions

/// How a session begins.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum Opening {
    New,
    /// Resume the harness's own session id (for pi, the session file path).
    Resume {
        native_id: String,
    },
    /// Join a process another client already started (a detached session
    /// being re-attached). Nothing is written at start; replay the log from
    /// offset 0 so the driver can adopt the session.
    Attach,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionConfig {
    pub harness: Harness,
    /// Absolute working directory on the host.
    pub cwd: String,
    pub opening: Opening,
    #[uniffi(default = None)]
    pub model: Option<String>,
    /// Tag for this client's request ids, unique among clients sharing one
    /// harness process (`[A-Za-z0-9_-]`). Defaults to `acc`.
    #[uniffi(default = None)]
    pub client_tag: Option<String>,
}

fn core_session_config(config: SessionConfig) -> Result<core_session::SessionConfig, AccError> {
    let tag = config.client_tag.unwrap_or_else(|| "acc".to_owned());
    let tag_ok = !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !tag_ok {
        return Err(AccError::InvalidArgument {
            reason: format!("client tag {tag:?} must be [A-Za-z0-9_-]+"),
        });
    }
    let opening = match config.opening {
        Opening::New => core_session::Opening::New,
        Opening::Resume { native_id } => core_session::Opening::Resume { native_id },
        Opening::Attach => core_session::Opening::Attach,
    };
    Ok(core_session::SessionConfig {
        harness: config.harness.into(),
        cwd: config.cwd,
        opening,
        model: config.model,
        client_tag: tag,
    })
}

/// What one chunk of harness output produced.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Received {
    pub events: Vec<Event>,
    /// Bytes to write back to the harness now. Often empty.
    pub transmit: Vec<u8>,
}

/// One harness session, sans-IO.
///
/// Start the process from [`Session::launch_spec`] on the host, write
/// [`Session::start`] to its stdin, then for every chunk read from its
/// stdout call [`Session::receive`] and write back `transmit`. Commands from
/// the UI go through [`Session::send`], whose bytes also go to stdin.
#[derive(Debug, uniffi::Object)]
pub struct Session {
    inner: Mutex<core_session::Session>,
}

#[uniffi::export]
impl Session {
    #[uniffi::constructor]
    pub fn new(config: SessionConfig) -> Result<Self, AccError> {
        Ok(Self {
            inner: Mutex::new(core_session::Session::new(core_session_config(config)?)),
        })
    }

    pub fn harness(&self) -> Harness {
        lock(&self.inner).harness().into()
    }

    /// The process to start on the host.
    pub fn launch_spec(&self) -> LaunchSpec {
        lock(&self.inner).launch_spec().into()
    }

    /// Bytes to write once the process runs. Call once.
    pub fn start(&self) -> Vec<u8> {
        lock(&self.inner).start()
    }

    /// Feeds harness stdout. `now_ms` is milliseconds since the Unix epoch.
    pub fn receive(&self, bytes: Vec<u8>, now_ms: u64) -> Received {
        let got = lock(&self.inner).receive(&bytes, now_ms);
        Received {
            events: got.events.into_iter().map(Event::from).collect(),
            transmit: got.transmit,
        }
    }

    /// Bytes to write to the harness for a command.
    pub fn send(&self, command: Command) -> Result<Vec<u8>, AccError> {
        Ok(lock(&self.inner).send(command.into())?)
    }

    /// The harness process ended; returns the events to show for it.
    pub fn exited(&self, code: Option<i32>, now_ms: u64) -> Vec<Event> {
        lock(&self.inner)
            .exited(code, now_ms)
            .into_iter()
            .map(Event::from)
            .collect()
    }

    /// Merges a state seen elsewhere, such as an OSC 7501 report from the
    /// pane the harness runs in. Returns true if the visible state changed.
    /// `Clear` is ignored.
    pub fn observe(&self, state: SessionState) -> bool {
        match state.into_core() {
            Some(s) => lock(&self.inner).observe(s),
            None => false,
        }
    }

    /// The current state, merged across sources by fidelity.
    pub fn state(&self) -> Option<SessionState> {
        lock(&self.inner).state().map(SessionState::from)
    }

    /// The harness's own session id, for resume and takeover, once known.
    pub fn native_id(&self) -> Option<String> {
        lock(&self.inner).native_id().map(str::to_owned)
    }

    /// The command palette. Read again after `Event::CommandsChanged`.
    pub fn commands(&self) -> Vec<CommandEntry> {
        lock(&self.inner)
            .commands()
            .iter()
            .map(CommandEntry::from)
            .collect()
    }

    /// Options and their current values. Read again after `Event::OptionsChanged`.
    pub fn options(&self) -> Vec<OptionDef> {
        lock(&self.inner).options().iter().map(OptionDef::from).collect()
    }
}

/// Follows a harness's own transcript file (Claude, Codex, pi) to observe a
/// session ACC didn't start, before or instead of taking it over.
#[derive(Debug, uniffi::Object)]
pub struct TranscriptObserver {
    inner: Mutex<core_session::TranscriptObserver>,
}

#[uniffi::export]
impl TranscriptObserver {
    #[uniffi::constructor]
    pub fn new(harness: Harness) -> Result<Self, AccError> {
        let h: acc_core::Harness = harness.into();
        let inner = core_session::TranscriptObserver::new(h).ok_or_else(|| AccError::Unsupported {
            reason: format!("ACC doesn't read {} transcripts", h.display_name()),
        })?;
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    /// Feeds bytes appended to the transcript.
    pub fn feed(&self, bytes: Vec<u8>, now_ms: u64) -> Vec<Event> {
        lock(&self.inner)
            .feed(&bytes, now_ms)
            .into_iter()
            .map(Event::from)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Daemonless sessions

/// Shell scripts for a detached, disconnect-proof session under
/// `$HOME/.acc/s/<id>` on the host. Run each with `sh` on the host (for
/// example as an SSH exec command); nothing needs installing there.
#[derive(Debug, uniffi::Object)]
pub struct DetachedSession {
    dir: SessionDir,
}

#[uniffi::export]
impl DetachedSession {
    /// `id` must be `[A-Za-z0-9_-]{1,64}`.
    #[uniffi::constructor]
    pub fn new(id: String) -> Result<Self, AccError> {
        let dir = SessionDir::new(&id).ok_or_else(|| AccError::InvalidArgument {
            reason: format!("session id {id:?} must be [A-Za-z0-9_-]{{1,64}}"),
        })?;
        Ok(Self { dir })
    }

    pub fn id(&self) -> String {
        self.dir.id().to_owned()
    }

    /// Starts the harness detached in `cwd`; prints its PID.
    pub fn launch_script(&self, spec: LaunchSpec, cwd: String) -> String {
        self.dir.launch_script(&spec.into(), &cwd)
    }

    /// Streams the harness output from byte `offset` and keeps following.
    pub fn attach_script(&self, offset: u64) -> String {
        self.dir.attach_script(offset)
    }

    /// Forwards the script's stdin to the harness.
    pub fn input_script(&self) -> String {
        self.dir.input_script()
    }

    /// Prints `alive <pid>`, `exited <code>` or `missing`.
    pub fn status_script(&self) -> String {
        self.dir.status_script()
    }
}

// ---------------------------------------------------------------------------
// OSC 7501

/// One OSC 7501 message seen in terminal output.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ProgramStatus {
    /// True for a probe (`OSC 7501 ; ?`); the terminal should reply with
    /// [`osc7501_probe_reply`] before its DA1 response.
    pub is_probe: bool,
    pub state: Option<ProgramState>,
    pub progress: Option<u8>,
    pub id: Option<String>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub message: Option<String>,
}

/// Bytes a terminal writes back to answer an OSC 7501 probe.
#[uniffi::export]
pub fn osc7501_probe_reply() -> Vec<u8> {
    osc7501::PROBE_REPLY.to_vec()
}

/// Incremental OSC 7501 extractor for a terminal byte stream.
#[derive(Debug, uniffi::Object)]
pub struct Osc7501Scanner {
    inner: Mutex<osc7501::Scanner>,
}

#[uniffi::export]
impl Osc7501Scanner {
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(osc7501::Scanner::new()),
        }
    }

    /// Feed raw terminal output; returns any status messages it completed.
    pub fn feed(&self, bytes: Vec<u8>) -> Vec<ProgramStatus> {
        lock(&self.inner)
            .feed(&bytes)
            .into_iter()
            .map(to_status)
            .collect()
    }
}

impl Default for Osc7501Scanner {
    fn default() -> Self {
        Self::new()
    }
}

fn to_status(msg: Osc7501) -> ProgramStatus {
    match msg {
        Osc7501::Probe => ProgramStatus {
            is_probe: true,
            state: None,
            progress: None,
            id: None,
            app: None,
            title: None,
            message: None,
        },
        Osc7501::Report(r) => {
            let (state, progress) = from_run_state(r.state.as_ref());
            ProgramStatus {
                is_probe: false,
                state: Some(state),
                progress,
                id: r.id,
                app: r.app,
                title: r.title,
                message: r.message,
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn config(harness: Harness) -> SessionConfig {
        SessionConfig {
            harness,
            cwd: "/tmp".into(),
            opening: Opening::New,
            model: None,
            client_tag: None,
        }
    }

    #[test]
    fn every_harness_round_trips() {
        for info in harnesses() {
            let core: acc_core::Harness = info.harness.into();
            assert_eq!(core.id(), info.id);
            let s = Session::new(config(info.harness)).unwrap();
            assert_eq!(s.harness(), info.harness);
            assert_eq!(s.launch_spec().program, info.program);
        }
    }

    #[test]
    fn rejects_bad_client_tags() {
        let mut c = config(Harness::Copilot);
        c.client_tag = Some("a b".into());
        assert!(matches!(Session::new(c), Err(AccError::InvalidArgument { .. })));
    }

    #[test]
    fn pi_session_end_to_end() {
        let s = Session::new(config(Harness::Pi)).unwrap();
        let hello = String::from_utf8(s.start()).unwrap();
        assert!(hello.contains("get_state"));
        let bytes = s
            .send(Command::Prompt {
                text: "hi".into(),
                attachments: vec![],
            })
            .unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains("\"hi\""));
        let events = s.exited(Some(0), 9);
        assert!(matches!(events.last(), Some(Event::Exited { code: Some(0) })));
        let state = s.state().unwrap();
        assert_eq!(state.state, ProgramState::Exited);
        assert_eq!(state.observed_at_ms, 9);
    }

    #[test]
    fn observe_merges_outside_states_and_ignores_clear() {
        let s = Session::new(config(Harness::Claude)).unwrap();
        let obs = |state| SessionState {
            state,
            progress: None,
            message: None,
            source: StateSource::Osc7501,
            observed_at_ms: 1,
        };
        assert!(!s.observe(obs(ProgramState::Clear)));
        assert!(s.observe(obs(ProgramState::BlockedPermission)));
        assert_eq!(s.state().unwrap().state, ProgramState::BlockedPermission);
    }

    #[test]
    fn driver_errors_map_to_ffi_errors() {
        let s = Session::new(config(Harness::Claude)).unwrap();
        let err = s
            .send(Command::AnswerPermission {
                request_id: "nope".into(),
                option_id: "allow".into(),
            })
            .unwrap_err();
        assert!(matches!(err, AccError::UnknownRequest { .. }), "{err:?}");
    }

    #[test]
    fn events_keep_native_payloads_as_json() {
        let e: Event = acc_core::Event::Native {
            kind: "plan".into(),
            payload: serde_json::json!({"steps": 2}),
        }
        .into();
        assert_eq!(
            e,
            Event::Native {
                kind: "plan".into(),
                payload_json: Some(r#"{"steps":2}"#.into()),
            }
        );
    }

    #[test]
    fn state_conversion_round_trips() {
        for st in [
            ProgramState::Idle,
            ProgramState::Working,
            ProgramState::BlockedPermission,
            ProgramState::BlockedQuestion,
            ProgramState::BlockedAuth,
            ProgramState::Done,
            ProgramState::Error,
            ProgramState::Exited,
        ] {
            let core = to_run_state(st, Some(5));
            assert_eq!(from_run_state(core.as_ref()).0, st);
        }
    }

    #[test]
    fn detached_scripts_validate_ids() {
        assert!(DetachedSession::new("../x".into()).is_err());
        let d = DetachedSession::new("s1".into()).unwrap();
        let spec = Session::new(config(Harness::Pi)).unwrap().launch_spec();
        assert!(d.launch_script(spec, "/tmp".into()).contains("mkfifo"));
        assert!(d.attach_script(10).contains("tail -c +11 -f"));
    }
}

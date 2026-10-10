//! Hosts over SSH: detached harness sessions and multiplexer panes, with the
//! transport built in (acc-ssh, pure Rust, no OpenSSH needed on the device).
//!
//! These calls do I/O, so they are `async` in Swift and `suspend` in Kotlin.
//! They run on a tokio runtime the library starts on first use; the app
//! needs no runtime of its own. Every object is safe to share: one task can
//! wait on `next_events` while another sends commands.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use acc_core::multiplexer as core_mux;
use acc_ssh as ssh;

use crate::{AccError, Command, Event, Harness, LaunchSpec, OptionDef, SessionConfig, SessionState};
use crate::{CommandEntry, core_session_config};

impl From<ssh::Error> for AccError {
    fn from(e: ssh::Error) -> Self {
        use ssh::Error as E;
        match e {
            E::Timeout => Self::Timeout,
            E::Disconnected => Self::Disconnected,
            E::Auth { user } => Self::AuthRejected { user },
            E::HostKey { fingerprint } => Self::HostKeyRejected { fingerprint },
            E::Agent(reason) => Self::Agent { reason },
            E::NotRunning(session_id) => Self::NotRunning { session_id },
            E::Remote { code, stderr } => Self::Remote {
                exit_code: code,
                stderr,
            },
            E::Mux(reason) => Self::Multiplexer { reason },
            E::Driver(e) => e.into(),
            e @ (E::Key(_) | E::InvalidId(_) | E::Invalid(_)) => Self::InvalidArgument {
                reason: e.to_string(),
            },
            e @ E::Ssh(_) => Self::Ssh {
                reason: e.to_string(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Connecting

/// How to log in.
#[derive(Clone, uniffi::Enum)]
pub enum SshAuth {
    Password {
        password: String,
    },
    /// A private key in OpenSSH, PEM or PuTTY format, as the app stores it.
    PrivateKey {
        key: String,
        passphrase: Option<String>,
    },
    /// The keys held by an SSH agent (desktop apps). `socket_path` defaults to
    /// `$SSH_AUTH_SOCK`.
    Agent {
        socket_path: Option<String>,
    },
}

impl fmt::Debug for SshAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Password { .. } => f.write_str("Password(..)"),
            Self::PrivateKey { .. } => f.write_str("PrivateKey(..)"),
            Self::Agent { socket_path } => write!(f, "Agent({socket_path:?})"),
        }
    }
}

/// Which host keys to trust.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum HostKeyCheck {
    /// SHA-256 fingerprints as `ssh-keygen -lf` prints them (`SHA256:…`).
    Fingerprints { fingerprints: Vec<String> },
    /// An OpenSSH `known_hosts` file.
    KnownHosts { path: String },
    /// Trust any key: for trust on first use, connect, show
    /// `SshHost::host_key_fingerprint` and pin it.
    AcceptAny,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    pub host_key: HostKeyCheck,
    /// Limit for dialing, logging in and opening a channel. Default 10 s.
    #[uniffi(default = None)]
    pub connect_timeout_ms: Option<u64>,
    /// Keepalive period. Default 10 s.
    #[uniffi(default = None)]
    pub keepalive_interval_ms: Option<u64>,
    /// Unanswered keepalives before the connection counts as lost. Default 3.
    #[uniffi(default = None)]
    pub keepalive_max: Option<u32>,
}

/// A script's result.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// `None` when the script was killed by a signal.
    pub exit_code: Option<u32>,
}

/// Liveness of a detached session.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum DetachedStatus {
    Alive { pid: u32 },
    Exited { code: Option<i32> },
    Missing,
}

/// A detached session on a host.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DetachedInfo {
    pub id: String,
    pub status: DetachedStatus,
    /// Size of the output log.
    pub log_bytes: u64,
    /// What it runs, and where; `None` for sessions an older ACC started.
    pub spec: Option<LaunchSpec>,
    pub cwd: Option<String>,
}

impl From<ssh::Status> for DetachedStatus {
    fn from(s: ssh::Status) -> Self {
        match s {
            ssh::Status::Alive { pid } => Self::Alive { pid },
            ssh::Status::Exited { code } => Self::Exited { code },
            ssh::Status::Missing => Self::Missing,
        }
    }
}

/// A host reachable over SSH.
///
/// Connects on first use and redials whenever the connection has dropped.
/// Call `reconnect` when the app returns to the foreground: a socket the OS
/// froze can look alive until keepalives time out.
#[derive(Debug, uniffi::Object)]
pub struct SshHost {
    host: Arc<ssh::Host>,
}

#[uniffi::export(async_runtime = "tokio")]
impl SshHost {
    #[uniffi::constructor]
    pub fn new(config: SshConfig) -> Result<Self, AccError> {
        let auth = match config.auth {
            SshAuth::Password { password } => ssh::Auth::Password(password),
            SshAuth::PrivateKey { key, passphrase } => ssh::Auth::key(&key, passphrase.as_deref())?,
            SshAuth::Agent { socket_path } => ssh::Auth::Agent(match socket_path {
                Some(p) => ssh::AgentSocket::Path(p.into()),
                None => ssh::AgentSocket::Env,
            }),
        };
        let check = match config.host_key {
            HostKeyCheck::Fingerprints { fingerprints } => ssh::HostKeyCheck::Fingerprints(fingerprints),
            HostKeyCheck::KnownHosts { path } => ssh::HostKeyCheck::KnownHosts(path.into()),
            HostKeyCheck::AcceptAny => ssh::HostKeyCheck::AcceptAny,
        };
        let mut c = ssh::SshConfig::new(config.host, config.user, auth, check);
        c.port = config.port;
        if let Some(ms) = config.connect_timeout_ms {
            c.connect_timeout = Duration::from_millis(ms);
        }
        if let Some(ms) = config.keepalive_interval_ms {
            c.keepalive_interval = Duration::from_millis(ms);
        }
        if let Some(n) = config.keepalive_max {
            c.keepalive_max = n as usize;
        }
        Ok(Self {
            host: Arc::new(ssh::Host::new(c)),
        })
    }

    /// Connects now, to surface login and host-key errors early.
    pub async fn connect(&self) -> Result<(), AccError> {
        Ok(self.host.connect().await?)
    }

    /// Drops the connection and dials a fresh one.
    pub async fn reconnect(&self) -> Result<(), AccError> {
        Ok(self.host.reconnect().await?)
    }

    /// Closes the connection. Detached sessions and panes keep running.
    // Not `close`: Kotlin objects already have `AutoCloseable.close()`.
    pub async fn disconnect(&self) {
        self.host.close().await;
    }

    /// SHA-256 fingerprint of the key the host presented on the last dial.
    pub fn host_key_fingerprint(&self) -> Option<String> {
        self.host.host_key_fingerprint()
    }

    /// Connections made so far; it grows when a reconnect happened.
    pub fn connections(&self) -> u64 {
        self.host.connections()
    }

    /// Runs a POSIX shell script on the host with `stdin`.
    pub async fn exec(&self, script: String, stdin: Vec<u8>) -> Result<ExecOutput, AccError> {
        let out = self.host.exec(&script, &stdin).await?;
        Ok(ExecOutput {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code: out.code,
        })
    }

    /// The detached sessions on the host, from any client.
    pub async fn list_sessions(&self) -> Result<Vec<DetachedInfo>, AccError> {
        Ok(ssh::list(&self.host)
            .await?
            .into_iter()
            .map(|l| DetachedInfo {
                id: l.id,
                status: l.status.into(),
                log_bytes: l.log_bytes,
                cwd: l.spec.as_ref().map(|s| s.cwd.clone()),
                spec: l.spec.map(|s| LaunchSpec::from(&s.spec)),
            })
            .collect())
    }

    /// Starts a harness detached on the host as session `id`
    /// (`[A-Za-z0-9_-]{1,64}`, unique on the host). It keeps running when the
    /// app disconnects; any client can `attach_session` to it later.
    pub async fn launch_session(&self, id: String, config: SessionConfig) -> Result<RemoteSession, AccError> {
        let config = core_session_config(config)?;
        let session = ssh::Session::launch(self.host.clone(), &id, config).await?;
        Ok(RemoteSession { inner: session })
    }

    /// Joins running session `id`, replaying its log so the session rebuilds
    /// its state, then following it live. `config.opening` is ignored.
    pub fn attach_session(&self, id: String, config: SessionConfig) -> Result<RemoteSession, AccError> {
        let config = core_session_config(config)?;
        Ok(RemoteSession {
            inner: ssh::Session::attach(self.host.clone(), &id, config, 0)?,
        })
    }

    /// Deletes an exited session's files from the host.
    pub async fn remove_session(&self, id: String) -> Result<(), AccError> {
        Ok(ssh::Detached::attach(self.host.clone(), &id, 0)?.remove().await?)
    }

    /// The panes of `target` (every session when it names none).
    pub async fn list_panes(&self, target: MuxTarget) -> Result<Vec<PaneInfo>, AccError> {
        let panes = ssh::pane::list_panes(&self.host, &target.into()).await?;
        Ok(panes.into_iter().map(PaneInfo::from).collect())
    }

    /// The pane process `pid` runs in, if it runs in a multiplexer: for taking
    /// over a harness the user started in tmux, zellij, screen or Rex.
    pub async fn locate_pane(&self, pid: u32) -> Result<Option<PaneInfo>, AccError> {
        Ok(ssh::pane::locate(&self.host, pid).await?.map(PaneInfo::from))
    }

    /// Co-drives `harness`'s TUI running in `pane`.
    pub fn pane_session(&self, pane: PaneRef, harness: Harness) -> PaneSession {
        PaneSession {
            inner: ssh::Pane::new(self.host.clone(), pane.into(), harness.into()),
        }
    }

    /// Starts `harness`'s TUI in a new background pane of `target` in `cwd`,
    /// resuming the harness's session id where its TUI can. `name` labels the
    /// pane (a window, tab or block) and should be unique.
    pub async fn spawn_pane(
        &self,
        target: MuxTarget,
        harness: Harness,
        cwd: String,
        name: String,
        resume: Option<String>,
    ) -> Result<PaneSession, AccError> {
        let pane = ssh::Pane::spawn(
            self.host.clone(),
            &target.into(),
            harness.into(),
            &cwd,
            &name,
            resume.as_deref(),
        )
        .await?;
        Ok(PaneSession { inner: pane })
    }
}

// ---------------------------------------------------------------------------
// Detached sessions

/// A harness running detached on a host: commands in, events out, across
/// dropped connections and app restarts.
#[derive(Debug, uniffi::Object)]
pub struct RemoteSession {
    inner: ssh::Session,
}

#[uniffi::export(async_runtime = "tokio")]
impl RemoteSession {
    /// The detached session id on the host.
    pub fn id(&self) -> String {
        self.inner.pipe().id().to_owned()
    }

    pub fn harness(&self) -> Harness {
        self.inner.harness().into()
    }

    /// Log bytes delivered so far.
    pub fn offset(&self) -> u64 {
        self.inner.pipe().offset()
    }

    /// Waits for the next events, reconnecting as needed. After the harness
    /// exits it returns the exit events, then `[Exited]` on every call.
    /// Fails with `Disconnected` or `Timeout` after several failed attempts;
    /// calling again keeps trying. Keep one task waiting on it rather than
    /// cancelling and re-calling: a cancelled call can drop the events it
    /// was about to return.
    pub async fn next_events(&self) -> Result<Vec<Event>, AccError> {
        let events = self.inner.next_events().await?;
        Ok(events.into_iter().map(Event::from).collect())
    }

    /// Sends a command, delivered whole and at most once.
    pub async fn send(&self, command: Command) -> Result<(), AccError> {
        Ok(self.inner.send(command.into()).await?)
    }

    /// Asks the harness to exit; `next_events` then reports the exit.
    pub async fn stop(&self) -> Result<(), AccError> {
        Ok(self.inner.pipe().stop().await?)
    }

    pub async fn status(&self) -> Result<DetachedStatus, AccError> {
        Ok(self.inner.pipe().status().await?.into())
    }

    /// The current state, merged across sources by fidelity.
    pub fn state(&self) -> Option<SessionState> {
        self.inner.state().as_ref().map(SessionState::from)
    }

    /// Merges a state seen elsewhere. Returns true if the visible state changed.
    pub fn observe(&self, state: SessionState) -> bool {
        state.into_core().is_some_and(|s| self.inner.observe(s))
    }

    /// The harness's own session id, once known.
    pub fn native_id(&self) -> Option<String> {
        self.inner.native_id()
    }

    /// The command palette. Read again after `Event::CommandsChanged`.
    pub fn commands(&self) -> Vec<CommandEntry> {
        self.inner.commands().iter().map(CommandEntry::from).collect()
    }

    /// Options and their values. Read again after `Event::OptionsChanged`.
    pub fn options(&self) -> Vec<OptionDef> {
        self.inner.options().iter().map(OptionDef::from).collect()
    }
}

// ---------------------------------------------------------------------------
// Multiplexers

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Multiplexer {
    Tmux,
    Zellij,
    Screen,
    Rex,
}

impl From<Multiplexer> for core_mux::Multiplexer {
    fn from(m: Multiplexer) -> Self {
        match m {
            Multiplexer::Tmux => Self::Tmux,
            Multiplexer::Zellij => Self::Zellij,
            Multiplexer::Screen => Self::Screen,
            Multiplexer::Rex => Self::Rex,
        }
    }
}

impl From<core_mux::Multiplexer> for Multiplexer {
    fn from(m: core_mux::Multiplexer) -> Self {
        match m {
            core_mux::Multiplexer::Tmux => Self::Tmux,
            core_mux::Multiplexer::Zellij => Self::Zellij,
            core_mux::Multiplexer::Screen => Self::Screen,
            core_mux::Multiplexer::Rex => Self::Rex,
        }
    }
}

/// A multiplexer server, optionally narrowed to one session.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MuxTarget {
    pub multiplexer: Multiplexer,
    /// tmux socket path (empty: the default server), zellij or screen
    /// session name, or Rex server address (empty: the local server).
    pub server: String,
    #[uniffi(default = None)]
    pub session: Option<String>,
}

impl From<MuxTarget> for core_mux::Target {
    fn from(t: MuxTarget) -> Self {
        Self {
            multiplexer: t.multiplexer.into(),
            server: t.server,
            session: t.session,
        }
    }
}

/// Where a harness runs inside a multiplexer.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PaneRef {
    pub multiplexer: Multiplexer,
    pub server: String,
    pub session: Option<String>,
    /// tmux `%N`, zellij `terminal_N`, screen window number, Rex block id.
    pub pane: String,
}

impl From<PaneRef> for core_mux::PaneRef {
    fn from(p: PaneRef) -> Self {
        Self {
            multiplexer: p.multiplexer.into(),
            server: p.server,
            session: p.session,
            pane: p.pane,
        }
    }
}

impl From<core_mux::PaneRef> for PaneRef {
    fn from(p: core_mux::PaneRef) -> Self {
        Self {
            multiplexer: p.multiplexer.into(),
            server: p.server,
            session: p.session,
            pane: p.pane,
        }
    }
}

/// One pane as the multiplexer lists it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PaneInfo {
    pub pane: PaneRef,
    /// The process the pane started (tmux only).
    pub pid: Option<u32>,
    pub tty: Option<String>,
    /// The foreground process name, where reported (`claude`, `node`...).
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub active: bool,
    /// The program exited but the pane is kept.
    pub exited: bool,
}

impl From<core_mux::PaneInfo> for PaneInfo {
    fn from(p: core_mux::PaneInfo) -> Self {
        Self {
            pane: p.pane.into(),
            pid: p.pid,
            tty: p.tty,
            command: p.command,
            cwd: p.cwd,
            title: p.title,
            active: p.active,
            exited: p.exited,
        }
    }
}

/// A key press.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PaneKey {
    Enter,
    Escape,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    /// Ctrl plus one ASCII letter.
    Ctrl {
        letter: String,
    },
    /// One printable character, such as a menu digit.
    Char {
        character: String,
    },
}

/// Input for a pane.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PaneInput {
    /// Typed literally; no newlines (use `Paste`).
    Text {
        text: String,
    },
    /// One bracketed paste, so newlines don't submit early.
    Paste {
        text: String,
    },
    Keys {
        keys: Vec<PaneKey>,
    },
}

fn one_char(s: &str, what: &str) -> Result<char, AccError> {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(c),
        _ => Err(AccError::InvalidArgument {
            reason: format!("{what} must be one character, got {s:?}"),
        }),
    }
}

impl TryFrom<PaneInput> for core_mux::Input {
    type Error = AccError;

    fn try_from(i: PaneInput) -> Result<Self, AccError> {
        use core_mux::Key as K;
        Ok(match i {
            PaneInput::Text { text } => Self::Text(text),
            PaneInput::Paste { text } => Self::Paste(text),
            PaneInput::Keys { keys } => Self::Keys(
                keys.into_iter()
                    .map(|k| {
                        Ok(match k {
                            PaneKey::Enter => K::Enter,
                            PaneKey::Escape => K::Escape,
                            PaneKey::Tab => K::Tab,
                            PaneKey::Backspace => K::Backspace,
                            PaneKey::Up => K::Up,
                            PaneKey::Down => K::Down,
                            PaneKey::Left => K::Left,
                            PaneKey::Right => K::Right,
                            PaneKey::Ctrl { letter } => {
                                let c = one_char(&letter, "a Ctrl key")?;
                                if !c.is_ascii_alphabetic() {
                                    return Err(AccError::InvalidArgument {
                                        reason: format!("Ctrl takes an ASCII letter, got {letter:?}"),
                                    });
                                }
                                K::Ctrl(c)
                            }
                            PaneKey::Char { character } => K::Char(one_char(&character, "a key")?),
                        })
                    })
                    .collect::<Result<_, AccError>>()?,
            ),
        })
    }
}

/// A harness TUI in a multiplexer pane, co-driven over SSH: the user can keep
/// using the pane, and ACC types into it like they would.
#[derive(Debug, uniffi::Object)]
pub struct PaneSession {
    inner: ssh::Pane,
}

#[uniffi::export(async_runtime = "tokio")]
impl PaneSession {
    pub fn pane(&self) -> PaneRef {
        self.inner.pane().clone().into()
    }

    pub fn harness(&self) -> Harness {
        self.inner.harness().into()
    }

    /// The current state, merged across OSC 7501 and the screen.
    pub fn state(&self) -> Option<SessionState> {
        self.inner.state().as_ref().map(SessionState::from)
    }

    /// Waits for the next events: state changes, and permission dialogs
    /// opening and closing (read off the screen for Claude and pi). Returns
    /// `[Exited]` once the pane is gone.
    pub async fn next_events(&self) -> Result<Vec<Event>, AccError> {
        let events = self.inner.next_events().await?;
        Ok(events.into_iter().map(Event::from).collect())
    }

    /// Reads the screen once and returns what changed.
    pub async fn poll(&self) -> Result<Vec<Event>, AccError> {
        let events = self.inner.poll().await?;
        Ok(events.into_iter().map(Event::from).collect())
    }

    /// Carries out a command by typing into the TUI. Answers to permission
    /// requests are keyed from the dialog on screen now.
    pub async fn send(&self, command: Command) -> Result<(), AccError> {
        Ok(self.inner.send(command.into()).await?)
    }

    /// Delivers raw input, in order.
    pub async fn send_input(&self, inputs: Vec<PaneInput>) -> Result<(), AccError> {
        let inputs = inputs
            .into_iter()
            .map(core_mux::Input::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(self.inner.send_input(&inputs).await?)
    }

    /// The pane's rendered text.
    pub async fn capture(&self, scrollback: bool) -> Result<String, AccError> {
        Ok(self.inner.capture(scrollback).await?)
    }

    /// Asks the TUI to exit the way a user would.
    pub async fn exit(&self) -> Result<(), AccError> {
        Ok(self.inner.exit().await?)
    }

    /// Closes the pane, ending its program.
    pub async fn kill(&self) -> Result<(), AccError> {
        Ok(self.inner.kill().await?)
    }

    /// Stops watching the pane and undoes what watching set up on the host;
    /// the pane keeps running.
    pub async fn stop_watching(&self) -> Result<(), AccError> {
        Ok(self.inner.close().await?)
    }
}

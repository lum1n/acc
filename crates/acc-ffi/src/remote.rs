//! Hosts over SSH: detached harness sessions, with the transport built in (acc-ssh, pure Rust, no OpenSSH needed on the device).
//!
//! These calls do I/O, so they are `async` in Swift and `suspend` in Kotlin.
//! They run on a tokio runtime the library starts on first use; the app
//! needs no runtime of its own. Every object is safe to share: one task can
//! wait on `next_events` while another sends commands.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use acc_ssh as ssh;

use crate::{AccError, Command, Event, Harness, LaunchSpec, OptionDef, SessionConfig, SessionState};
use crate::{AccItem, CommandEntry, acc_types::item, core_session_config};

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
    /// The whole `spec.json`, for `AccSession.attach`.
    pub spec_json: Option<String>,
}

impl From<ssh::Status> for DetachedStatus {
    fn from(s: ssh::Status) -> Self {
        match s {
            ssh::Status::Alive { pid, .. } => Self::Alive { pid },
            ssh::Status::Exited { code, .. } => Self::Exited { code },
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

    /// Closes the connection. Detached sessions keep running.
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
            .map(|l| {
                let spec = l.spec_json.as_deref().and_then(ssh::SessionSpec::from_json);
                DetachedInfo {
                    id: l.id,
                    status: l.status.into(),
                    log_bytes: l.status.size(),
                    cwd: spec.as_ref().map(|s| s.cwd.clone()),
                    spec: spec.map(|s| LaunchSpec::from(&s.launch)),
                    spec_json: l.spec_json,
                }
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
}

impl SshHost {
    /// The connection, for Rust crates that run their own scripts on it.
    pub fn host(&self) -> Arc<ssh::Host> {
        self.host.clone()
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

    /// Every item the events so far described, with its text and detail.
    pub fn items(&self) -> Vec<AccItem> {
        self.inner
            .with_core(|c| c.transcript().items().iter().map(item).collect())
    }

    /// One item by its id, once its `ItemStarted` has been received.
    pub fn item(&self, id: String) -> Option<AccItem> {
        self.inner.with_core(|c| c.transcript().get(&id).map(item))
    }

    /// The main conversation: items that didn't run inside a subagent.
    pub fn top_level_items(&self) -> Vec<AccItem> {
        self.inner
            .with_core(|c| c.transcript().top_level().map(item).collect())
    }

    /// The child conversation of the `Subagent` item `parent`.
    pub fn children(&self, parent: String) -> Vec<AccItem> {
        self.inner
            .with_core(|c| c.transcript().children(&parent).map(item).collect())
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

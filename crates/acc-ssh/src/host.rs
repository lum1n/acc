//! One SSH connection to a host, redialed on demand.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use acc_core::shell::ssh_command;
use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{Channel, ChannelMsg, Disconnect};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::{Error, Result};

/// How to authenticate.
#[derive(Clone)]
pub enum Auth {
    Password(String),
    /// A decoded private key (see [`Auth::key`]).
    Key(Arc<PrivateKey>),
    /// The keys and certificates held by an SSH agent (OpenSSH's
    /// `ssh-agent`, 1Password, Secretive, gpg-agent...), tried in the
    /// agent's order. The private keys never leave the agent.
    Agent(AgentSocket),
}

/// Where to find the SSH agent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentSocket {
    /// `$SSH_AUTH_SOCK` on Unix; OpenSSH's `\\.\pipe\openssh-ssh-agent` on Windows.
    #[default]
    Env,
    /// A Unix socket path, or a named pipe on Windows.
    Path(std::path::PathBuf),
}

impl Auth {
    /// A private key in OpenSSH, PEM or PuTTY format, as the app stores it.
    pub fn key(text: &str, passphrase: Option<&str>) -> Result<Self> {
        russh::keys::decode_secret_key(text, passphrase)
            .map(|k| Auth::Key(Arc::new(k)))
            .map_err(|e| Error::Key(e.to_string()))
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::Password(_) => f.write_str("Password(..)"),
            Auth::Key(k) => write!(f, "Key({})", k.algorithm()),
            Auth::Agent(socket) => write!(f, "Agent({socket:?})"),
        }
    }
}

/// Which host keys to trust.
#[derive(Debug, Clone)]
pub enum HostKeyCheck {
    /// Trust only these SHA-256 fingerprints, written as `ssh-keygen -lf`
    /// prints them (`SHA256:…`).
    Fingerprints(Vec<String>),
    /// Trust keys recorded for the host in an OpenSSH `known_hosts` file
    /// (desktop clients, sharing `~/.ssh/known_hosts` with `ssh`).
    KnownHosts(std::path::PathBuf),
    /// Trust any key. For trust on first use: connect, read
    /// [`Host::host_key_fingerprint`], show it, and pin it for next time.
    AcceptAny,
}

/// Where and how to connect.
#[derive(Debug, Clone)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: Auth,
    pub host_key: HostKeyCheck,
    /// Limit for dialing, authenticating and opening a channel.
    pub connect_timeout: Duration,
    /// Keepalive period; after `keepalive_max` unanswered keepalives the
    /// connection counts as lost.
    pub keepalive_interval: Duration,
    pub keepalive_max: usize,
}

impl SshConfig {
    pub fn new(host: impl Into<String>, user: impl Into<String>, auth: Auth, host_key: HostKeyCheck) -> Self {
        Self {
            host: host.into(),
            port: 22,
            user: user.into(),
            auth,
            host_key,
            connect_timeout: Duration::from_secs(10),
            keepalive_interval: Duration::from_secs(10),
            keepalive_max: 3,
        }
    }
}

struct Client {
    host: String,
    port: u16,
    check: HostKeyCheck,
    seen: Arc<StdMutex<Option<String>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let key = key.public_key();
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let trusted = match &self.check {
            HostKeyCheck::AcceptAny => true,
            HostKeyCheck::KnownHosts(path) => {
                russh::keys::check_known_hosts_path(&self.host, self.port, &key, path).unwrap_or(false)
            }
            HostKeyCheck::Fingerprints(f) => f.contains(&fingerprint),
        };
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(fingerprint);
        Ok(trusted)
    }
}

type Conn = Arc<Handle<Client>>;

/// A host reachable over SSH.
///
/// Connects lazily on first use and redials whenever the connection has
/// dropped, so callers never manage connection state. Cheap to share in an
/// `Arc`; channels are multiplexed over one connection.
pub struct Host {
    config: SshConfig,
    /// Scripts run on this machine with `sh` rather than over SSH (see [`Host::local`]).
    local: bool,
    conn: Mutex<Option<Conn>>,
    fingerprint: Arc<StdMutex<Option<String>>>,
    dials: AtomicU64,
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host")
            .field("host", &self.config.host)
            .field("port", &self.config.port)
            .field("user", &self.config.user)
            .field("local", &self.local)
            .finish_non_exhaustive()
    }
}

/// Result of a script run to completion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// `None` if the script was killed by a signal.
    pub code: Option<u32>,
}

impl Output {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// The output if the script exited 0, otherwise [`Error::Remote`].
    pub fn ok(self) -> Result<Self> {
        if self.code == Some(0) {
            Ok(self)
        } else {
            Err(Error::Remote {
                code: self.code,
                stderr: String::from_utf8_lossy(&self.stderr).trim().to_owned(),
            })
        }
    }
}

impl Host {
    pub fn new(config: SshConfig) -> Self {
        Self {
            config,
            local: false,
            conn: Mutex::new(None),
            fingerprint: Arc::new(StdMutex::new(None)),
            dials: AtomicU64::new(0),
        }
    }

    /// This machine, as the user running the process: the same scripts run with `sh` instead of
    /// over SSH, so sessions started here, by this or any other client, can be listed, followed
    /// and driven without a login. Scripts get a login's environment (`HOME`, `USER`, `PATH`,
    /// the locale…), not the rest of this process's, which may hold its secrets.
    /// [`Host::config`] describes it as `localhost` for display; nothing dials.
    #[cfg(feature = "local")]
    pub fn local() -> Self {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_default();
        let config = SshConfig::new(
            "localhost",
            user,
            Auth::Agent(AgentSocket::Env),
            HostKeyCheck::AcceptAny,
        );
        Self {
            local: true,
            ..Self::new(config)
        }
    }

    /// Whether this is [`Host::local`].
    pub fn is_local(&self) -> bool {
        self.local
    }

    pub fn config(&self) -> &SshConfig {
        &self.config
    }

    /// SHA-256 fingerprint of the key the host presented on the last dial.
    pub fn host_key_fingerprint(&self) -> Option<String> {
        self.fingerprint
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many connections have been established, so an app can show a
    /// reconnect happened.
    pub fn connections(&self) -> u64 {
        self.dials.load(Ordering::Relaxed)
    }

    /// Connects now if not connected, to surface auth and host-key errors early.
    pub async fn connect(&self) -> Result<()> {
        if self.local {
            return Ok(());
        }
        self.conn().await.map(drop)
    }

    /// Drops the current connection and dials a fresh one. Call when the app
    /// returns to the foreground: a socket the OS froze may look alive until
    /// keepalives time out.
    pub async fn reconnect(&self) -> Result<()> {
        self.close().await;
        self.connect().await
    }

    /// Closes the connection. Detached sessions keep running on the host.
    pub async fn close(&self) {
        if let Some(conn) = self.conn.lock().await.take() {
            let _ = timeout(
                Duration::from_secs(1),
                conn.disconnect(Disconnect::ByApplication, "", "en"),
            )
            .await;
        }
    }

    /// The current connection, dialing one if needed; `true` if it was just dialed.
    async fn conn(&self) -> Result<(Conn, bool)> {
        let mut slot = self.conn.lock().await;
        if let Some(conn) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok((conn.clone(), false));
        }
        *slot = None;
        let conn = Arc::new(self.dial().await?);
        self.dials.fetch_add(1, Ordering::Relaxed);
        *slot = Some(conn.clone());
        Ok((conn, true))
    }

    /// Forgets `conn` if it is still the current one, so the next call redials.
    async fn discard(&self, conn: &Conn) {
        let mut slot = self.conn.lock().await;
        if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn)) {
            *slot = None;
        }
    }

    async fn dial(&self) -> Result<Handle<Client>> {
        let c = &self.config;
        let config = client::Config {
            keepalive_interval: Some(c.keepalive_interval),
            keepalive_max: c.keepalive_max,
            nodelay: true,
            ..client::Config::default()
        };
        let handler = Client {
            host: c.host.clone(),
            port: c.port,
            check: c.host_key.clone(),
            seen: self.fingerprint.clone(),
        };
        let mut handle = match timeout(
            c.connect_timeout,
            client::connect(Arc::new(config), (c.host.as_str(), c.port), handler),
        )
        .await
        {
            Err(_) => return Err(Error::Timeout),
            Ok(Err(russh::Error::UnknownKey)) => {
                return Err(Error::HostKey {
                    fingerprint: self.host_key_fingerprint().unwrap_or_default(),
                });
            }
            Ok(r) => r?,
        };
        let auth = async {
            Ok::<_, Error>(match &c.auth {
                Auth::Password(p) => handle.authenticate_password(&c.user, p).await?.success(),
                Auth::Key(key) => {
                    let hash = handle.best_supported_rsa_hash().await?.flatten();
                    let key = PrivateKeyWithHashAlg::new(key.clone(), hash);
                    handle.authenticate_publickey(&c.user, key).await?.success()
                }
                Auth::Agent(socket) => crate::agent::authenticate(&mut handle, &c.user, socket).await?,
            })
        };
        let ok = timeout(c.connect_timeout, auth)
            .await
            .map_err(|_| Error::Timeout)??;
        if !ok {
            return Err(Error::Auth { user: c.user.clone() });
        }
        Ok(handle)
    }

    /// Opens a channel running `script`. A connection that died unnoticed
    /// (the network changed, the app was suspended) fails before the script
    /// starts, so that is retried once on a fresh connection.
    async fn channel(&self, script: &str) -> Result<Channel<client::Msg>> {
        loop {
            let (conn, fresh) = self.conn().await?;
            let open = async {
                let ch = conn.channel_open_session().await?;
                ch.exec(true, ssh_command(script)).await?;
                Ok::<_, russh::Error>(ch)
            };
            match timeout(self.config.connect_timeout, open).await {
                Ok(Ok(ch)) => return Ok(ch),
                Ok(Err(e)) => {
                    self.discard(&conn).await;
                    match e {
                        russh::Error::SendError | russh::Error::Disconnect | russh::Error::IO(_)
                            if !fresh => {}
                        russh::Error::SendError | russh::Error::Disconnect | russh::Error::IO(_) => {
                            return Err(Error::Disconnected);
                        }
                        e => return Err(e.into()),
                    }
                }
                Err(_) => {
                    // A half-dead connection: don't wait for keepalives to notice.
                    self.discard(&conn).await;
                    return Err(Error::Timeout);
                }
            }
        }
    }

    /// Runs a POSIX shell script with `stdin`, waiting for it to finish.
    ///
    /// [`Error::Disconnected`] if the connection dropped before the script's
    /// exit status arrived (it may or may not have run).
    pub async fn exec(&self, script: &str, stdin: &[u8]) -> Result<Output> {
        #[cfg(feature = "local")]
        if self.local {
            return local::exec(script, stdin).await;
        }
        let mut ch = self.channel(script).await?;
        if !stdin.is_empty() {
            ch.data_bytes(stdin.to_vec())
                .await
                .map_err(|_| Error::Disconnected)?;
        }
        ch.eof().await.map_err(|_| Error::Disconnected)?;
        let mut out = Output::default();
        let mut exited = false;
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, ext: 1 } => out.stderr.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => {
                    out.code = Some(exit_status);
                    exited = true;
                }
                ChannelMsg::ExitSignal { .. } => exited = true,
                _ => {}
            }
        }
        if exited { Ok(out) } else { Err(Error::Disconnected) }
    }

    /// Starts a POSIX shell script and streams its stdout.
    pub async fn open(&self, script: &str) -> Result<Stream> {
        #[cfg(feature = "local")]
        if self.local {
            return Ok(Stream {
                inner: Inner::Local(Box::new(local::open(script)?)),
            });
        }
        let ch = self.channel(script).await?;
        Ok(Stream {
            inner: Inner::Ssh {
                ch,
                code: None,
                signaled: false,
            },
        })
    }
}

/// A running script's output, from [`Host::open`].
pub struct Stream {
    inner: Inner,
}

enum Inner {
    Ssh {
        ch: Channel<client::Msg>,
        code: Option<u32>,
        signaled: bool,
    },
    #[cfg(feature = "local")]
    Local(Box<local::Process>),
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stream").finish_non_exhaustive()
    }
}

/// What a [`Stream`] produced next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamMsg {
    Stdout(Vec<u8>),
    /// stderr output; detached followers send a heartbeat here.
    Stderr(Vec<u8>),
    /// The script exited (`None`: killed by a signal).
    Exited(Option<u32>),
    /// The channel closed without an exit status: the connection dropped.
    Lost,
}

impl Stream {
    pub async fn recv(&mut self) -> StreamMsg {
        let (ch, code, signaled) = match &mut self.inner {
            Inner::Ssh { ch, code, signaled } => (ch, code, signaled),
            #[cfg(feature = "local")]
            Inner::Local(process) => return process.recv().await,
        };
        loop {
            match ch.wait().await {
                Some(ChannelMsg::Data { data }) => return StreamMsg::Stdout(data.to_vec()),
                Some(ChannelMsg::ExtendedData { data, .. }) => return StreamMsg::Stderr(data.to_vec()),
                Some(ChannelMsg::ExitStatus { exit_status }) => *code = Some(exit_status),
                Some(ChannelMsg::ExitSignal { .. }) => *signaled = true,
                Some(_) => {}
                None if code.is_some() || *signaled => return StreamMsg::Exited(*code),
                None => return StreamMsg::Lost,
            }
        }
    }

    /// Writes to the script's stdin.
    pub async fn write(&self, bytes: &[u8]) -> Result<()> {
        match &self.inner {
            Inner::Ssh { ch, .. } => ch
                .data_bytes(bytes.to_vec())
                .await
                .map_err(|_| Error::Disconnected),
            #[cfg(feature = "local")]
            Inner::Local(process) => process.write(bytes).await,
        }
    }

    /// Closes the script's stdin.
    pub async fn finish(&self) -> Result<()> {
        match &self.inner {
            Inner::Ssh { ch, .. } => ch.eof().await.map_err(|_| Error::Disconnected),
            #[cfg(feature = "local")]
            Inner::Local(process) => process.finish().await,
        }
    }
}

/// Scripts on this machine, for [`Host::local`].
#[cfg(feature = "local")]
mod local {
    use std::process::Stdio;

    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
    use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
    use tokio::sync::Mutex;

    use super::{Output, StreamMsg};
    use crate::{Error, Result};

    /// What a login would have; the rest of this process's environment stays out.
    const KEEP: &[&str] = &[
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "PATH",
        "LANG",
        "LANGUAGE",
        "TERM",
        "TMPDIR",
        "TZ",
        "XDG_RUNTIME_DIR",
        "SSH_AUTH_SOCK",
    ];

    fn command(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script).env_clear();
        for (name, value) in std::env::vars_os() {
            let keep = name
                .to_str()
                .is_some_and(|n| KEEP.contains(&n) || n.starts_with("LC_"));
            if keep {
                command.env(name, value);
            }
        }
        command.kill_on_drop(false);
        command
    }

    fn spawn(script: &str) -> Result<Child> {
        command(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Local(e.to_string()))
    }

    fn code(status: std::process::ExitStatus) -> Option<u32> {
        status.code().and_then(|c| u32::try_from(c).ok())
    }

    pub(super) async fn exec(script: &str, stdin: &[u8]) -> Result<Output> {
        let mut child = spawn(script)?;
        let input = child.stdin.take();
        // Fed while the output is drained: a script that writes as it reads would otherwise
        // block on a full pipe while its input waits on it.
        let feed = async move {
            if let Some(mut input) = input {
                // A script that doesn't read its input closes it early; that's no failure.
                let _ = input.write_all(stdin).await;
            }
        };
        let ((), out) = tokio::join!(feed, child.wait_with_output());
        let out = out.map_err(|e| Error::Local(e.to_string()))?;
        Ok(Output {
            stdout: out.stdout,
            stderr: out.stderr,
            code: code(out.status),
        })
    }

    /// A running script: its stdout and stderr read as they come, and its stdin.
    pub(super) struct Process {
        child: Child,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
        stdin: Mutex<Option<ChildStdin>>,
    }

    pub(super) fn open(script: &str) -> Result<Process> {
        let mut child = spawn(script)?;
        Ok(Process {
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            stdin: Mutex::new(child.stdin.take()),
            child,
        })
    }

    async fn chunk(pipe: &mut Option<impl AsyncRead + Unpin>) -> Option<Vec<u8>> {
        let reader = pipe.as_mut()?;
        let mut buf = vec![0; 32 * 1024];
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => None,
            Ok(n) => {
                buf.truncate(n);
                Some(buf)
            }
        }
    }

    impl Process {
        pub(super) async fn recv(&mut self) -> StreamMsg {
            loop {
                if self.stdout.is_none() && self.stderr.is_none() {
                    return match self.child.wait().await {
                        Ok(status) => StreamMsg::Exited(code(status)),
                        Err(_) => StreamMsg::Lost,
                    };
                }
                tokio::select! {
                    got = chunk(&mut self.stdout), if self.stdout.is_some() => match got {
                        Some(bytes) => return StreamMsg::Stdout(bytes),
                        None => self.stdout = None,
                    },
                    got = chunk(&mut self.stderr), if self.stderr.is_some() => match got {
                        Some(bytes) => return StreamMsg::Stderr(bytes),
                        None => self.stderr = None,
                    },
                }
            }
        }

        pub(super) async fn write(&self, bytes: &[u8]) -> Result<()> {
            let mut stdin = self.stdin.lock().await;
            let input = stdin.as_mut().ok_or(Error::Disconnected)?;
            input.write_all(bytes).await.map_err(|_| Error::Disconnected)
        }

        pub(super) async fn finish(&self) -> Result<()> {
            self.stdin.lock().await.take();
            Ok(())
        }
    }
}

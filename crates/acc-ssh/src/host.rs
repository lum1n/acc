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
            conn: Mutex::new(None),
            fingerprint: Arc::new(StdMutex::new(None)),
            dials: AtomicU64::new(0),
        }
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

    async fn conn(&self) -> Result<Conn> {
        let mut slot = self.conn.lock().await;
        if let Some(conn) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok(conn.clone());
        }
        *slot = None;
        let conn = Arc::new(self.dial().await?);
        self.dials.fetch_add(1, Ordering::Relaxed);
        *slot = Some(conn.clone());
        Ok(conn)
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

    async fn channel(&self, script: &str) -> Result<Channel<client::Msg>> {
        let conn = self.conn().await?;
        let open = async {
            let ch = conn.channel_open_session().await?;
            ch.exec(true, ssh_command(script)).await?;
            Ok::<_, russh::Error>(ch)
        };
        match timeout(self.config.connect_timeout, open).await {
            Ok(Ok(ch)) => Ok(ch),
            Ok(Err(e)) => {
                self.discard(&conn).await;
                Err(match e {
                    russh::Error::SendError | russh::Error::Disconnect | russh::Error::IO(_) => {
                        Error::Disconnected
                    }
                    e => e.into(),
                })
            }
            Err(_) => {
                // A half-dead connection: don't wait for keepalives to notice.
                self.discard(&conn).await;
                Err(Error::Timeout)
            }
        }
    }

    /// Runs a POSIX shell script with `stdin`, waiting for it to finish.
    ///
    /// [`Error::Disconnected`] if the connection dropped before the script's
    /// exit status arrived (it may or may not have run).
    pub async fn exec(&self, script: &str, stdin: &[u8]) -> Result<Output> {
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
        let ch = self.channel(script).await?;
        Ok(Stream {
            ch,
            code: None,
            signaled: false,
        })
    }
}

/// A running script's output, from [`Host::open`].
pub struct Stream {
    ch: Channel<client::Msg>,
    code: Option<u32>,
    signaled: bool,
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stream")
            .field("code", &self.code)
            .finish_non_exhaustive()
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
        loop {
            match self.ch.wait().await {
                Some(ChannelMsg::Data { data }) => return StreamMsg::Stdout(data.to_vec()),
                Some(ChannelMsg::ExtendedData { data, .. }) => return StreamMsg::Stderr(data.to_vec()),
                Some(ChannelMsg::ExitStatus { exit_status }) => self.code = Some(exit_status),
                Some(ChannelMsg::ExitSignal { .. }) => self.signaled = true,
                Some(_) => {}
                None if self.code.is_some() || self.signaled => return StreamMsg::Exited(self.code),
                None => return StreamMsg::Lost,
            }
        }
    }

    /// Writes to the script's stdin.
    pub async fn write(&self, bytes: &[u8]) -> Result<()> {
        self.ch
            .data_bytes(bytes.to_vec())
            .await
            .map_err(|_| Error::Disconnected)
    }

    /// Closes the script's stdin.
    pub async fn finish(&self) -> Result<()> {
        self.ch.eof().await.map_err(|_| Error::Disconnected)
    }
}

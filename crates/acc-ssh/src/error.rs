use acc_core::driver::DriverError;

/// Errors from the SSH transport.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ssh: {0}")]
    Ssh(#[from] russh::Error),
    #[error("timed out talking to the host")]
    Timeout,
    #[error("the connection to the host was lost")]
    Disconnected,
    #[error("the host rejected the credentials for {user}")]
    Auth { user: String },
    /// The host presented a key that [`crate::HostKeyCheck`] doesn't trust.
    #[error("untrusted host key {fingerprint}")]
    HostKey { fingerprint: String },
    /// The SSH agent couldn't be reached or failed to sign.
    #[error("ssh-agent: {0}")]
    Agent(String),
    #[error("invalid private key: {0}")]
    Key(String),
    #[error("invalid argument: {0}")]
    Invalid(&'static str),
    #[error("invalid session id {0:?}")]
    InvalidId(String),
    /// The session's harness is not running (it exited, or never started).
    #[error("no harness is running in session {0}")]
    NotRunning(String),
    #[error("remote script failed (exit {code:?}): {stderr}")]
    Remote { code: Option<u32>, stderr: String },
    #[error(transparent)]
    Driver(#[from] DriverError),
}

impl Error {
    /// Whether retrying after a reconnect may succeed.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Timeout | Error::Disconnected => true,
            Error::Ssh(e) => !matches!(
                e,
                russh::Error::NotAuthenticated | russh::Error::UnknownKey | russh::Error::Keys(_)
            ),
            _ => false,
        }
    }
}

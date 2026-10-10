//! Public-key authentication through an SSH agent.

use russh::client::{Handle, Handler};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};

use crate::host::AgentSocket;
use crate::{Error, Result};

type Agent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

/// Offers the agent's identities in order until the host accepts one.
/// `Ok(false)` when none was accepted (or the agent holds none).
pub(crate) async fn authenticate<H: Handler>(
    handle: &mut Handle<H>,
    user: &str,
    socket: &AgentSocket,
) -> Result<bool> {
    let mut agent = connect(socket).await?;
    let identities = agent.request_identities().await.map_err(agent_error)?;
    let rsa_hash = handle.best_supported_rsa_hash().await?.flatten();
    for identity in identities {
        let result = match identity {
            AgentIdentity::PublicKey { key, .. } => {
                let hash = rsa_hash.filter(|_| key.algorithm().is_rsa());
                handle
                    .authenticate_publickey_with(user, key, hash, &mut agent)
                    .await
            }
            AgentIdentity::Certificate { certificate, .. } => {
                let hash = rsa_hash.filter(|_| certificate.algorithm().is_rsa());
                handle
                    .authenticate_certificate_with(user, certificate, hash, &mut agent)
                    .await
            }
        };
        match result {
            Ok(r) if r.success() => return Ok(true),
            Ok(_) => {}
            Err(russh::AgentAuthError::Send(_)) => return Err(Error::Disconnected),
            Err(russh::AgentAuthError::Key(e)) => return Err(agent_error(e)),
        }
    }
    Ok(false)
}

fn agent_error(e: russh::keys::Error) -> Error {
    Error::Agent(e.to_string())
}

#[cfg(unix)]
async fn connect(socket: &AgentSocket) -> Result<Agent> {
    let agent = match socket {
        AgentSocket::Env => AgentClient::connect_env().await,
        AgentSocket::Path(path) => AgentClient::connect_uds(path).await,
    };
    agent.map(AgentClient::dynamic).map_err(|e| match e {
        russh::keys::Error::EnvVar(_) => Error::Agent("SSH_AUTH_SOCK is not set".into()),
        e => agent_error(e),
    })
}

#[cfg(windows)]
async fn connect(socket: &AgentSocket) -> Result<Agent> {
    let path = match socket {
        AgentSocket::Env => std::path::Path::new(r"\\.\pipe\openssh-ssh-agent"),
        AgentSocket::Path(path) => path.as_path(),
    };
    AgentClient::connect_named_pipe(path)
        .await
        .map(AgentClient::dynamic)
        .map_err(agent_error)
}

#[cfg(not(any(unix, windows)))]
async fn connect(_: &AgentSocket) -> Result<Agent> {
    Err(Error::Agent("no SSH agent on this platform".into()))
}

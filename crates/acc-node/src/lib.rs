//! Node bindings (napi) for acc-core and the acc-ssh transport: hosts over
//! SSH and the detached harness sessions on them, for servers that drive
//! agents from Node.
//!
//! The same objects as acc-ffi's `SshHost` and `RemoteSession`. Options are
//! camelCase objects; the core's own values (events, commands, states,
//! items, listings) cross as plain JS objects in its serde shapes
//! (snake_case keys, `type`-tagged unions), exactly as acc-wasm passes them
//! and `acc run --json` prints them. `index.d.ts` spells them out.
//!
//! Errors are `Error`s whose `code` is acc-ffi's `AccError` variant name
//! (`HostKeyRejected`, `NotRunning`, `Disconnected`...).

use std::sync::Arc;
use std::time::Duration;

use acc_core::Harness;
use acc_core::command::Command;
use acc_core::harness::{parse_probe, probe_script};
use acc_core::live::{Opening, SessionConfig};
use acc_core::state::SessionState;
use acc_ssh as ssh;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The kind leads the message as `[Kind] …`; `index.js` moves it to `code`.
fn fail(kind: &str, reason: impl std::fmt::Display) -> Error {
    Error::from_reason(format!("[{kind}] {reason}"))
}

fn invalid(reason: impl std::fmt::Display) -> Error {
    fail("InvalidArgument", reason)
}

fn ssh_err(e: ssh::Error) -> Error {
    use ssh::Error as E;
    let kind = match &e {
        E::Timeout => "Timeout",
        E::Disconnected => "Disconnected",
        E::Auth { .. } => "AuthRejected",
        E::HostKey { .. } => "HostKeyRejected",
        E::Agent(_) => "Agent",
        E::NotRunning(_) => "NotRunning",
        E::Remote { .. } => "Remote",
        E::Driver(_) => "Driver",
        E::Key(_) | E::InvalidId(_) | E::Invalid(_) => "InvalidArgument",
        E::Ssh(_) => "Ssh",
    };
    fail(kind, e)
}

fn to_js<T: serde::Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| fail("Internal", e))
}

fn from_js<T: DeserializeOwned>(value: Value, what: &str) -> Result<T> {
    serde_json::from_value(value).map_err(|e| invalid(format!("invalid {what}: {e}")))
}

// ---------------------------------------------------------------------------
// Connecting

/// How to log in: exactly one of `password`, `privateKey` or `agent`.
#[napi(object)]
pub struct SshAuth {
    pub password: Option<String>,
    /// A private key in OpenSSH, PEM or PuTTY format.
    pub private_key: Option<String>,
    pub passphrase: Option<String>,
    /// Use an SSH agent; `agentSocket` defaults to `$SSH_AUTH_SOCK`.
    pub agent: Option<bool>,
    pub agent_socket: Option<String>,
}

/// Which host keys to trust: `fingerprints`, a `knownHosts` file, or
/// `acceptAny` (trust on first use: connect, read `hostKeyFingerprint`, pin it).
#[napi(object)]
pub struct HostKeyCheck {
    /// SHA-256 fingerprints as `ssh-keygen -lf` prints them (`SHA256:…`).
    pub fingerprints: Option<Vec<String>>,
    pub known_hosts: Option<String>,
    pub accept_any: Option<bool>,
}

#[napi(object)]
pub struct SshConfig {
    pub host: String,
    /// Default 22.
    pub port: Option<u32>,
    pub user: String,
    pub auth: SshAuth,
    pub host_key: HostKeyCheck,
    /// Limit for dialing, logging in and opening a channel. Default 10 s.
    pub connect_timeout_ms: Option<u32>,
    /// Keepalive period. Default 10 s.
    pub keepalive_interval_ms: Option<u32>,
    /// Unanswered keepalives before the connection counts as lost. Default 3.
    pub keepalive_max: Option<u32>,
}

#[napi(object)]
pub struct ExecOutput {
    pub stdout: Buffer,
    pub stderr: Buffer,
    /// `null` when the script was killed by a signal.
    pub exit_code: Option<u32>,
}

#[napi(object)]
pub struct KeyPair {
    /// OpenSSH format, for `auth.privateKey`.
    pub private_key: String,
    /// The `authorized_keys` line.
    pub public_key: String,
}

/// Generates an ed25519 key pair whose public key carries `comment`.
#[napi]
pub fn generate_key(comment: String) -> Result<KeyPair> {
    let pair = ssh::generate_key(&comment).map_err(ssh_err)?;
    Ok(KeyPair {
        private_key: pair.private_key,
        public_key: pair.public_key,
    })
}

/// A session id for `harness` (`claude-…`) from at least 15 random bytes.
#[napi]
pub fn new_session_id(harness: String, entropy: Buffer) -> Result<String> {
    let harness: Harness = from_js(Value::String(harness), "harness")?;
    if entropy.len() < 15 {
        return Err(invalid("entropy needs at least 15 bytes"));
    }
    Ok(acc_core::session::new_session_id(harness, &entropy))
}

/// The harnesses ACC drives, with their ids and programs.
#[napi(ts_return_type = "HarnessInfo[]")]
pub fn harnesses() -> Result<Value> {
    let all: Vec<Value> = Harness::ALL
        .into_iter()
        .map(|h| {
            serde_json::json!({
                "harness": h,
                "id": h.id(),
                "program": h.program(),
                "display_name": h.display_name(),
                "native_acp": h.native_acp(),
            })
        })
        .collect();
    Ok(Value::Array(all))
}

/// Opens a session: what acc-ffi's `SessionConfig` carries.
#[derive(Deserialize)]
struct SessionConfigJs {
    harness: Harness,
    cwd: String,
    #[serde(default)]
    resume: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    client_tag: Option<String>,
}

fn session_config(value: Value) -> Result<SessionConfig> {
    let c: SessionConfigJs = from_js(value, "session config")?;
    let mut config = SessionConfig::new(c.harness, c.cwd);
    config.model = c.model;
    if let Some(native_id) = c.resume {
        config.opening = Opening::Resume { native_id };
    }
    if let Some(tag) = c.client_tag {
        if tag.is_empty()
            || !tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid(format!("client_tag {tag:?} must be [A-Za-z0-9_-]+")));
        }
        config.client_tag = tag;
    }
    Ok(config)
}

/// A host reachable over SSH. Connects on first use and redials whenever
/// the connection has dropped; every call is safe to make concurrently.
#[napi]
pub struct SshHost {
    host: Arc<ssh::Host>,
}

#[napi]
impl SshHost {
    #[napi(constructor)]
    pub fn new(config: SshConfig) -> Result<Self> {
        let a = config.auth;
        let auth = match (a.password, a.private_key, a.agent.unwrap_or(false)) {
            (Some(p), None, false) => ssh::Auth::Password(p),
            (None, Some(k), false) => ssh::Auth::key(&k, a.passphrase.as_deref()).map_err(ssh_err)?,
            (None, None, true) => ssh::Auth::Agent(match a.agent_socket {
                Some(p) => ssh::AgentSocket::Path(p.into()),
                None => ssh::AgentSocket::Env,
            }),
            _ => return Err(invalid("auth needs exactly one of password, privateKey or agent")),
        };
        let k = config.host_key;
        let check = match (k.fingerprints, k.known_hosts, k.accept_any.unwrap_or(false)) {
            (Some(f), None, false) => ssh::HostKeyCheck::Fingerprints(f),
            (None, Some(p), false) => ssh::HostKeyCheck::KnownHosts(p.into()),
            (None, None, true) => ssh::HostKeyCheck::AcceptAny,
            _ => {
                return Err(invalid(
                    "hostKey needs exactly one of fingerprints, knownHosts or acceptAny",
                ));
            }
        };
        let mut c = ssh::SshConfig::new(config.host, config.user, auth, check);
        if let Some(port) = config.port {
            c.port = u16::try_from(port).map_err(|_| invalid("port out of range"))?;
        }
        if let Some(ms) = config.connect_timeout_ms {
            c.connect_timeout = Duration::from_millis(ms.into());
        }
        if let Some(ms) = config.keepalive_interval_ms {
            c.keepalive_interval = Duration::from_millis(ms.into());
        }
        if let Some(n) = config.keepalive_max {
            c.keepalive_max = n as usize;
        }
        Ok(Self {
            host: Arc::new(ssh::Host::new(c)),
        })
    }

    /// Connects now, to surface login and host-key errors early.
    #[napi]
    pub async fn connect(&self) -> Result<()> {
        self.host.connect().await.map_err(ssh_err)
    }

    /// Drops the connection and dials a fresh one.
    #[napi]
    pub async fn reconnect(&self) -> Result<()> {
        self.host.reconnect().await.map_err(ssh_err)
    }

    /// Closes the connection. Detached sessions keep running.
    #[napi]
    pub async fn disconnect(&self) {
        self.host.close().await;
    }

    /// SHA-256 fingerprint of the key the host presented on the last dial.
    #[napi]
    pub fn host_key_fingerprint(&self) -> Option<String> {
        self.host.host_key_fingerprint()
    }

    /// Runs a POSIX shell script on the host with `stdin`.
    #[napi]
    pub async fn exec(&self, script: String, stdin: Option<Buffer>) -> Result<ExecOutput> {
        let stdin = stdin.map(|b| b.to_vec()).unwrap_or_default();
        let out = self.host.exec(&script, &stdin).await.map_err(ssh_err)?;
        Ok(ExecOutput {
            stdout: out.stdout.into(),
            stderr: out.stderr.into(),
            exit_code: out.code,
        })
    }

    /// The harnesses installed on the host, with their versions.
    #[napi(ts_return_type = "Promise<Availability[]>")]
    pub async fn probe(&self) -> Result<Value> {
        let out = self.host.exec(&probe_script(), &[]).await.map_err(ssh_err)?;
        to_js(&parse_probe(&String::from_utf8_lossy(&out.stdout)))
    }

    /// The detached sessions on the host, from any client.
    #[napi(ts_return_type = "Promise<DetachedInfo[]>")]
    pub async fn list_sessions(&self) -> Result<Value> {
        let listings = ssh::list(&self.host).await.map_err(ssh_err)?;
        let out: Vec<Value> = listings
            .into_iter()
            .map(|l| {
                let spec = l.spec_json.as_deref().and_then(ssh::SessionSpec::from_json);
                serde_json::json!({
                    "id": l.id,
                    "status": l.status,
                    "modified": l.modified,
                    "spec": spec,
                })
            })
            .collect();
        Ok(Value::Array(out))
    }

    /// Starts a harness detached on the host as session `id`
    /// (`[A-Za-z0-9_-]{1,64}`, unique on the host). It keeps running when
    /// this process goes; any client can `attachSession` to it later.
    #[napi(ts_args_type = "id: string, config: SessionConfig")]
    pub async fn launch_session(&self, id: String, config: Value) -> Result<RemoteSession> {
        let config = session_config(config)?;
        let session = ssh::Session::launch(self.host.clone(), &id, config)
            .await
            .map_err(ssh_err)?;
        Ok(RemoteSession {
            inner: Arc::new(session),
        })
    }

    /// Joins running session `id`, replaying its log so the session rebuilds
    /// its state, then following it live.
    #[napi(ts_args_type = "id: string, config: SessionConfig")]
    pub fn attach_session(&self, id: String, config: Value) -> Result<RemoteSession> {
        let config = session_config(config)?;
        let session = ssh::Session::attach(self.host.clone(), &id, config, 0).map_err(ssh_err)?;
        Ok(RemoteSession {
            inner: Arc::new(session),
        })
    }

    /// Deletes an exited session's files from the host.
    #[napi]
    pub async fn remove_session(&self, id: String) -> Result<()> {
        let pipe = ssh::Detached::attach(self.host.clone(), &id, 0).map_err(ssh_err)?;
        pipe.remove().await.map_err(ssh_err)
    }
}

// ---------------------------------------------------------------------------
// Detached sessions

/// A harness running detached on a host: commands in, events out, across
/// dropped connections and restarts.
#[napi]
pub struct RemoteSession {
    inner: Arc<ssh::Session>,
}

#[napi]
impl RemoteSession {
    /// The detached session id on the host.
    #[napi(getter)]
    pub fn id(&self) -> String {
        self.inner.pipe().id().to_owned()
    }

    #[napi(getter, ts_return_type = "HarnessId")]
    pub fn harness(&self) -> Result<Value> {
        to_js(&self.inner.harness())
    }

    /// Log bytes delivered so far.
    #[napi(getter)]
    pub fn offset(&self) -> f64 {
        self.inner.pipe().offset() as f64
    }

    /// Waits for the next events, reconnecting as needed. After the harness
    /// exits it returns the exit events, then `[Exited]` on every call.
    /// Keep one caller waiting on it rather than abandoning and re-calling:
    /// an abandoned call can drop the events it was about to return.
    #[napi(ts_return_type = "Promise<Event[]>")]
    pub async fn next_events(&self) -> Result<Value> {
        let events = self.inner.next_events().await.map_err(ssh_err)?;
        to_js(&events)
    }

    /// Sends a command, delivered whole and at most once.
    #[napi(ts_args_type = "command: Command")]
    pub async fn send(&self, command: Value) -> Result<()> {
        let command: Command = from_js(command, "command")?;
        self.inner.send(command).await.map_err(ssh_err)
    }

    /// Asks the harness to exit; `nextEvents` then reports the exit.
    #[napi]
    pub async fn stop(&self) -> Result<()> {
        self.inner.pipe().stop().await.map_err(ssh_err)
    }

    #[napi(ts_return_type = "Promise<DetachedStatus>")]
    pub async fn status(&self) -> Result<Value> {
        to_js(&self.inner.pipe().status().await.map_err(ssh_err)?)
    }

    /// The current state, merged across sources by fidelity.
    #[napi(ts_return_type = "SessionState | null")]
    pub fn state(&self) -> Result<Value> {
        to_js(&self.inner.state())
    }

    /// Merges a state seen elsewhere. Returns true if the visible state changed.
    #[napi(ts_args_type = "state: SessionState")]
    pub fn observe(&self, state: Value) -> Result<bool> {
        let state: SessionState = from_js(state, "session state")?;
        Ok(self.inner.observe(state))
    }

    /// The harness's own session id, once known.
    #[napi]
    pub fn native_id(&self) -> Option<String> {
        self.inner.native_id()
    }

    /// The command palette. Read again after a `commands_changed` event.
    #[napi(ts_return_type = "CommandEntry[]")]
    pub fn commands(&self) -> Result<Value> {
        to_js(&self.inner.commands())
    }

    /// Options and their values. Read again after an `options_changed` event.
    #[napi(ts_return_type = "OptionDef[]")]
    pub fn options(&self) -> Result<Value> {
        to_js(&self.inner.options())
    }

    /// Every item the events so far described.
    #[napi(ts_return_type = "Item[]")]
    pub fn items(&self) -> Result<Value> {
        self.inner.with_core(|c| to_js(&c.transcript().items()))
    }

    /// The main conversation: items that didn't run inside a subagent.
    #[napi(ts_return_type = "Item[]")]
    pub fn top_level_items(&self) -> Result<Value> {
        self.inner
            .with_core(|c| to_js(&c.transcript().top_level().collect::<Vec<_>>()))
    }
}

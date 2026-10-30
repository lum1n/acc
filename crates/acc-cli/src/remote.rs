//! `acc run --ssh`: the same loop as a local run, with the harness detached
//! on a host over SSH through acc-ssh. Nothing is installed on the host.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use acc_ssh::{AgentSocket, Auth, Detached, Host, HostKeyCheck, Read, SshConfig};
use tokio::runtime::Runtime;

use crate::run::Input;

/// SSH options from the command line.
#[derive(Debug, Default)]
pub struct Options {
    /// `[user@]host[:port]`.
    pub dest: String,
    pub identity: Option<String>,
    /// Trust any host key (prints its fingerprint) instead of `~/.ssh/known_hosts`.
    pub insecure: bool,
}

pub struct Remote {
    rt: Runtime,
    host: Arc<Host>,
}

impl Remote {
    pub fn connect(opts: &Options) -> Result<Self, String> {
        let (user, rest) = match opts.dest.split_once('@') {
            Some((u, r)) => (u.to_owned(), r),
            None => (
                std::env::var("USER").map_err(|_| "no user given and $USER is unset")?,
                opts.dest.as_str(),
            ),
        };
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, p.parse().map_err(|_| format!("invalid port {p:?}"))?),
            _ => (rest, 22),
        };
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let auth = auth(opts.identity.as_deref(), &home)?;
        let check = if opts.insecure {
            HostKeyCheck::AcceptAny
        } else {
            HostKeyCheck::KnownHosts(home.join(".ssh/known_hosts"))
        };
        let mut config = SshConfig::new(host, user, auth, check);
        config.port = port;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let host = Arc::new(Host::new(config));
        rt.block_on(host.connect()).map_err(|e| match e {
            acc_ssh::Error::HostKey { fingerprint } => format!(
                "{} is not in ~/.ssh/known_hosts (its key is {fingerprint}); connect once with ssh, or pass --insecure",
                opts.dest
            ),
            e => format!("{}: {e}", opts.dest),
        })?;
        if opts.insecure {
            eprintln!(
                "acc: host key {} (not verified)",
                host.host_key_fingerprint().unwrap_or_default()
            );
        }
        Ok(Self { rt, host })
    }

    /// A detached session on the host, for rejoining it.
    pub fn find(&self, id: &str) -> Result<acc_ssh::Listing, String> {
        let listed = self
            .rt
            .block_on(acc_ssh::list(&self.host))
            .map_err(|e| e.to_string())?;
        listed
            .into_iter()
            .find(|l| l.id == id)
            .ok_or_else(|| format!("no session {id:?} on the host (see acc ssh-sessions)"))
    }

    /// Absolute path of `cwd` on the host (relative paths are from `$HOME`).
    pub fn resolve_cwd(&self, cwd: &str) -> Result<String, String> {
        let script = format!("cd {} && pwd", acc_core::shell::quote(cwd));
        let out = self
            .rt
            .block_on(self.host.exec(&script, b""))
            .and_then(acc_ssh::Output::ok)
            .map_err(|e| format!("{cwd} on the host: {e}"))?;
        Ok(out.stdout_str().trim_end().to_owned())
    }

    /// Launches `record` (its id and start time are filled in here), or
    /// rejoins session `attach`, replaying its log, and
    /// starts a thread that forwards the harness's output to `tx`. When
    /// rejoining, `Input::CaughtUp` follows once the replay reaches the
    /// `replay` bytes the log held when the client attached.
    pub fn start(
        self,
        mut record: acc_ssh::SessionSpec,
        attach: Option<&str>,
        replay: u64,
        tx: Sender<Input>,
    ) -> Result<Link, String> {
        let pipe = match attach {
            Some(id) => Detached::attach(self.host.clone(), id, 0).map_err(|e| e.to_string())?,
            None => {
                let id = new_id();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64);
                record.id.clone_from(&id);
                record.created_ms = now;
                let pipe = self
                    .rt
                    .block_on(Detached::launch(
                        self.host.clone(),
                        &id,
                        &record.launch,
                        &record.cwd,
                        Some(&record.to_json()),
                    ))
                    .map_err(|e| format!("launch on the host: {e}"))?;
                match pipe.sandbox() {
                    Some(a @ acc_core::sandbox::Applied::None { .. }) => {
                        eprintln!("acc: {}", a.notice().unwrap_or_default());
                    }
                    Some(a) => eprintln!("acc: sandboxed with {}", format!("{a:?}").to_lowercase()),
                    None => {}
                }
                pipe
            }
        };
        let pipe = Arc::new(pipe);
        let reader = pipe.clone();
        let handle = self.rt.handle().clone();
        let stop = Arc::new(tokio::sync::Notify::new());
        let stopped = stop.clone();
        let mut replaying = attach.is_some();
        let thread = std::thread::spawn(move || {
            loop {
                if replaying && reader.offset() >= replay {
                    replaying = false;
                    let _ = tx.send(Input::CaughtUp);
                }
                let read = handle.block_on(async {
                    tokio::select! {
                        r = reader.read() => Some(r),
                        () = stopped.notified() => None,
                    }
                });
                let Some(read) = read else { return };
                match read {
                    Ok(Read::Data(bytes)) => {
                        if tx.send(Input::Agent(bytes)).is_err() {
                            return;
                        }
                    }
                    Ok(Read::Exited(code)) => {
                        let _ = tx.send(Input::AgentClosed(Some(code)));
                        return;
                    }
                    Err(e) if e.is_transient() => eprintln!("acc: {e}; still trying"),
                    Err(e) => {
                        eprintln!("acc: {e}");
                        let _ = tx.send(Input::AgentClosed(Some(None)));
                        return;
                    }
                }
            }
        });
        Ok(Link {
            rt: Some(self.rt),
            pipe,
            open: true,
            reader: Some((stop, thread)),
        })
    }
}

/// Writes to a detached session.
pub struct Link {
    rt: Option<Runtime>,
    pipe: Arc<Detached>,
    open: bool,
    reader: Option<(Arc<tokio::sync::Notify>, std::thread::JoinHandle<()>)>,
}

impl Drop for Link {
    fn drop(&mut self) {
        // Stop the reader before its runtime goes away.
        if let Some((stop, thread)) = self.reader.take() {
            stop.notify_one();
            let _ = thread.join();
        }
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

impl Link {
    pub fn write(&self, bytes: &[u8]) -> Result<(), String> {
        let rt = self.rt.as_ref().ok_or("detached")?;
        rt.block_on(self.pipe.write(bytes))
            .map_err(|e| format!("write to harness: {e}"))
    }

    pub fn id(&self) -> &str {
        self.pipe.id()
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Stops sending input; the harness keeps running on the host.
    pub fn detach(&mut self) {
        self.open = false;
    }
}

fn auth(identity: Option<&str>, home: &std::path::Path) -> Result<Auth, String> {
    if let Ok(p) = std::env::var("ACC_SSH_PASSWORD") {
        return Ok(Auth::Password(p));
    }
    // Like ssh: a running agent is used unless a key file is named.
    if identity.is_none() && std::env::var_os("SSH_AUTH_SOCK").is_some() {
        return Ok(Auth::Agent(AgentSocket::Env));
    }
    let passphrase = std::env::var("ACC_SSH_PASSPHRASE").ok();
    let candidates: Vec<PathBuf> = match identity {
        Some(p) => vec![PathBuf::from(p)],
        None => ["id_ed25519", "id_ecdsa", "id_rsa"]
            .iter()
            .map(|n| home.join(".ssh").join(n))
            .filter(|p| p.exists())
            .collect(),
    };
    let path = candidates
        .first()
        .ok_or("no ssh-agent and no key in ~/.ssh (pass --identity, or set ACC_SSH_PASSWORD)")?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Auth::key(&text, passphrase.as_deref()).map_err(|e| {
        format!(
            "{}: {e} (encrypted keys need ACC_SSH_PASSPHRASE, or add the key to ssh-agent)",
            path.display()
        )
    })
}

/// A fresh session id, unique enough per host.
fn new_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("s{:x}", nanos & 0xffff_ffff_ffff)
}

/// `acc ssh-sessions`: lists detached sessions on a host.
pub fn sessions(dest: &str, args: &[&str]) -> Result<std::process::ExitCode, String> {
    let mut opts = Options {
        dest: dest.to_owned(),
        ..Options::default()
    };
    let mut stop = Vec::new();
    let mut prune = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match *arg {
            "--identity" => opts.identity = Some((*it.next().ok_or("--identity needs a value")?).to_owned()),
            "--insecure" => opts.insecure = true,
            "--stop" => stop.push(*it.next().ok_or("--stop needs a session id")?),
            "--prune" => prune = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let r = Remote::connect(&opts)?;
    for id in stop {
        let d = Detached::attach(r.host.clone(), id, 0).map_err(|e| e.to_string())?;
        r.rt.block_on(d.stop()).map_err(|e| format!("stop {id}: {e}"))?;
        println!("stopped {id}");
    }
    let mut listed = r.rt.block_on(acc_ssh::list(&r.host)).map_err(|e| e.to_string())?;
    if prune {
        listed.retain(|l| {
            if matches!(l.status, acc_ssh::Status::Alive { .. }) {
                return true;
            }
            let removed = Detached::attach(r.host.clone(), &l.id, 0)
                .ok()
                .is_some_and(|d| r.rt.block_on(d.remove()).is_ok());
            if removed {
                println!("removed {}", l.id);
            }
            !removed
        });
    }
    if listed.is_empty() {
        println!("no detached sessions on {dest}");
    }
    for l in listed {
        let status = match l.status {
            acc_ssh::Status::Alive { pid, .. } => format!("running (pid {pid})"),
            acc_ssh::Status::Exited { code: Some(c), .. } => format!("exited {c}"),
            acc_ssh::Status::Exited { code: None, .. } => "exited".to_owned(),
            acc_ssh::Status::Missing => "never started".to_owned(),
        };
        let what = l
            .spec_json
            .as_deref()
            .and_then(acc_ssh::SessionSpec::from_json)
            .map(|s| format!("{} in {}", s.launch.program, s.cwd))
            .unwrap_or_default();
        println!("{:<16} {status:<20} {:>9} B  {what}", l.id, l.status.size());
    }
    Ok(std::process::ExitCode::SUCCESS)
}

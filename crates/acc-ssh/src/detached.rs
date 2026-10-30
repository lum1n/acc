//! The bytes of one detached session, over SSH.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use acc_core::detached::{self as scripts, EXIT_INCOMPLETE, EXIT_NOT_RUNNING, Listing, SessionDir, Status};
use acc_core::harness::LaunchSpec;
use acc_core::sandbox::Applied as Sandboxed;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};

use crate::host::{Host, Stream, StreamMsg};
use crate::{Error, Result};

/// How hard to try before reporting [`Error::Disconnected`].
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    /// Consecutive failed attempts before giving up. Calling again after an
    /// error starts over, so an app can keep trying for as long as it likes.
    pub attempts: u32,
    /// First backoff; doubles per attempt up to `max_backoff`.
    pub backoff: Duration,
    pub max_backoff: Duration,
    /// A follower that sends nothing, not even its once-a-second heartbeat,
    /// for this long is treated as a lost connection and redialed.
    pub stall: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            attempts: 6,
            backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(8),
            stall: Duration::from_secs(5),
        }
    }
}

impl Retry {
    /// Sleeps before attempt `n` (1-based), or fails once attempts are used up.
    async fn wait(&self, n: u32, last: Error) -> Result<()> {
        if n >= self.attempts {
            return Err(last);
        }
        let factor = 1u32 << n.saturating_sub(1).min(16);
        sleep(self.backoff.saturating_mul(factor).min(self.max_backoff)).await;
        Ok(())
    }
}

/// What [`Detached::read`] returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// The next log bytes, in order, with nothing skipped or repeated.
    Data(Vec<u8>),
    /// The harness exited and the whole log has been read.
    Exited(Option<i32>),
}

#[derive(Debug, Default)]
struct Follow {
    stream: Option<Stream>,
    /// The harness exited; what is left is to read the log tail.
    draining: bool,
    exited: Option<Option<i32>>,
}

/// A detached session on a host: harness stdin on a FIFO, stdout in a log.
///
/// Survives dropped connections in both directions. [`Detached::read`] tracks
/// the byte offset it has delivered and resumes exactly there after a
/// reconnect. [`Detached::write`] uploads each write whole before it reaches
/// the harness and tags it with an id, so a retry after a lost
/// acknowledgement never delivers it twice.
#[derive(Debug)]
pub struct Detached {
    host: Arc<Host>,
    dir: SessionDir,
    retry: Retry,
    offset: AtomicU64,
    follow: Mutex<Follow>,
    /// Serializes writes so they reach the harness in call order.
    writes: Mutex<u64>,
    write_tag: String,
    /// What a sandboxed launch applied on the host.
    sandbox: Option<Sandboxed>,
}

/// Lists the detached sessions on a host.
pub async fn list(host: &Host) -> Result<Vec<Listing>> {
    let out = host.exec(&scripts::list_script(), b"").await?.ok()?;
    Ok(scripts::parse_list(&out.stdout_str()))
}

impl Detached {
    /// Starts `spec` detached in `cwd` on the host as session `id`,
    /// recording `spec_json` (a [`acc_core::session::SessionSpec`]) so any
    /// client can list and attach it.
    ///
    /// Relaunching an id whose harness has exited (to resume it) appends to
    /// the same log; the returned handle reads from the log's current end.
    pub async fn launch(
        host: Arc<Host>,
        id: &str,
        spec: &LaunchSpec,
        cwd: &str,
        spec_json: Option<&str>,
    ) -> Result<Self> {
        let dir = SessionDir::new(id).ok_or_else(|| Error::InvalidId(id.to_owned()))?;
        // Remember where this run's output starts.
        let before = list(&host).await?;
        let offset = before.iter().find(|l| l.id == id).map_or(0, |l| l.status.size());
        if before
            .iter()
            .any(|l| l.id == id && matches!(l.status, Status::Alive { .. }))
        {
            return Err(Error::Remote {
                code: None,
                stderr: format!("session {id} is already running"),
            });
        }
        let out = host
            .exec(&dir.launch_script_with_spec(spec, cwd, spec_json), b"")
            .await?
            .ok()?;
        let mut me = Self::attach(host, id, offset)?;
        me.sandbox = scripts::parse_sandbox(&out.stdout_str());
        Ok(me)
    }

    /// Joins an existing session, reading its log from byte `offset` (0
    /// replays everything, so a fresh driver can rebuild its state). No IO
    /// happens until the first read or write.
    pub fn attach(host: Arc<Host>, id: &str, offset: u64) -> Result<Self> {
        let dir = SessionDir::new(id).ok_or_else(|| Error::InvalidId(id.to_owned()))?;
        Ok(Self {
            host,
            dir,
            retry: Retry::default(),
            offset: AtomicU64::new(offset),
            follow: Mutex::new(Follow::default()),
            writes: Mutex::new(0),
            write_tag: write_tag(),
            sandbox: None,
        })
    }

    pub fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    pub fn id(&self) -> &str {
        self.dir.id()
    }

    /// What the host's sandbox applied, for a session this handle launched
    /// with [`LaunchSpec::sandbox`] set.
    pub fn sandbox(&self) -> Option<&Sandboxed> {
        self.sandbox.as_ref()
    }

    pub fn host(&self) -> &Arc<Host> {
        &self.host
    }

    /// Bytes of the log delivered by [`Detached::read`] so far. Persist it to
    /// resume from the same place after the app restarts.
    pub fn offset(&self) -> u64 {
        self.offset.load(Ordering::Acquire)
    }

    /// Waits for the next log bytes, reconnecting as needed.
    ///
    /// Returns [`Read::Exited`] once the harness has exited and the log is
    /// read to the end (and again on every later call). Errors after
    /// [`Retry::attempts`] consecutive failures; calling again resumes.
    pub async fn read(&self) -> Result<Read> {
        let mut f = self.follow.lock().await;
        if let Some(code) = f.exited {
            return Ok(Read::Exited(code));
        }
        let mut failures = 0;
        loop {
            if f.draining {
                // The harness is gone, so the log is final: read what's left.
                let offset = self.offset();
                let r = async {
                    let tail = self.host.exec(&self.dir.read_script(offset), b"").await?.ok()?;
                    let status = self.status().await?;
                    Ok::<_, Error>((tail.stdout, status))
                };
                match r.await {
                    Ok((tail, _)) if !tail.is_empty() => {
                        self.offset.fetch_add(tail.len() as u64, Ordering::AcqRel);
                        return Ok(Read::Data(tail));
                    }
                    Ok((_, status)) => {
                        let code = match status {
                            Status::Exited { code, .. } => code,
                            _ => None,
                        };
                        f.exited = Some(code);
                        return Ok(Read::Exited(code));
                    }
                    Err(e) if e.is_transient() => {
                        failures += 1;
                        self.retry.wait(failures, e).await?;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            let stream = match f.stream.as_mut() {
                Some(s) => s,
                None => match self.host.open(&self.dir.attach_script(self.offset())).await {
                    Ok(s) => f.stream.insert(s),
                    Err(e) if e.is_transient() => {
                        failures += 1;
                        self.retry.wait(failures, e).await?;
                        continue;
                    }
                    Err(e) => return Err(e),
                },
            };
            let msg = timeout(self.retry.stall, stream.recv()).await;
            match msg {
                Ok(StreamMsg::Stdout(bytes)) => {
                    self.offset.fetch_add(bytes.len() as u64, Ordering::AcqRel);
                    return Ok(Read::Data(bytes));
                }
                // Heartbeat: the connection and the follower are alive.
                Ok(StreamMsg::Stderr(_)) => failures = 0,
                Ok(StreamMsg::Exited(Some(0))) => {
                    f.stream = None;
                    f.draining = true;
                }
                Ok(StreamMsg::Exited(Some(EXIT_NOT_RUNNING))) => {
                    f.stream = None;
                    f.draining = true;
                }
                Ok(StreamMsg::Exited(_) | StreamMsg::Lost) => {
                    f.stream = None;
                    failures += 1;
                    self.retry.wait(failures, Error::Disconnected).await?;
                }
                Err(_) => {
                    // Nothing for longer than the heartbeat period: the
                    // connection is probably dead without having noticed.
                    f.stream = None;
                    failures += 1;
                    let _ = self.host.reconnect().await;
                    self.retry.wait(failures, Error::Timeout).await?;
                }
            }
        }
    }

    /// Delivers `bytes` to the harness's stdin, whole and at most once,
    /// retrying across reconnects. [`Error::NotRunning`] if the harness exited.
    pub async fn write(&self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut n = self.writes.lock().await;
        *n += 1;
        let id = format!("{}-{}", self.write_tag, *n);
        let script = self.dir.write_script(bytes.len(), &id);
        let mut failures = 0;
        loop {
            let err = match self.host.exec(&script, bytes).await {
                Ok(out) => match out.code {
                    Some(0) => return Ok(()),
                    Some(EXIT_NOT_RUNNING) => return Err(Error::NotRunning(self.id().to_owned())),
                    // The upload was cut short; nothing was written.
                    Some(EXIT_INCOMPLETE) => Error::Disconnected,
                    _ => return out.ok().map(drop),
                },
                Err(e) if e.is_transient() => e,
                Err(e) => return Err(e),
            };
            failures += 1;
            self.retry.wait(failures, err).await?;
        }
    }

    pub async fn status(&self) -> Result<Status> {
        let out = self.host.exec(&self.dir.status_script(), b"").await?.ok()?;
        Ok(scripts::parse_status(&out.stdout_str()).unwrap_or(Status::Missing))
    }

    /// Asks the harness to exit (SIGTERM). Readers then drain the log and
    /// return [`Read::Exited`].
    pub async fn stop(&self) -> Result<()> {
        let out = self.host.exec(&self.dir.stop_script(), b"").await?;
        match out.code {
            Some(EXIT_NOT_RUNNING) => Err(Error::NotRunning(self.id().to_owned())),
            _ => out.ok().map(drop),
        }
    }

    /// Deletes the session's files from the host. Fails while it is running.
    pub async fn remove(&self) -> Result<()> {
        self.host
            .exec(&self.dir.remove_script(), b"")
            .await?
            .ok()
            .map(drop)
    }
}

/// A tag that keeps write ids unique across clients and app restarts.
fn write_tag() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{:x}{n:x}", nanos & 0xffff_ffff_ffff, std::process::id())
}

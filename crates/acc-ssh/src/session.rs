//! A driver running over a detached SSH session.

use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use tokio::sync::Mutex;

use acc_core::Event;
use acc_core::command::Command;
use acc_core::driver::Driver;

use crate::detached::{Detached, Read};
use crate::host::Host;
use crate::{Error, Result};

/// A harness on a host, driven over SSH: commands in, events out.
///
/// Methods take `&self`, so one task can wait on [`Session::next_events`]
/// while others [`Session::send`] commands.
pub struct Session {
    pipe: Detached,
    driver: StdMutex<Box<dyn Driver>>,
    /// A driver reply that couldn't be delivered yet; sent before anything else.
    unsent: Mutex<Vec<u8>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("pipe", &self.pipe)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Starts the driver's harness detached on the host in `cwd`, optionally
    /// resuming a harness session (see [`Driver::launch`]), and sends the
    /// driver's handshake.
    pub async fn launch(
        host: Arc<Host>,
        id: &str,
        mut driver: Box<dyn Driver>,
        cwd: &str,
        resume: Option<&str>,
    ) -> Result<Self> {
        let spec = driver.launch(resume);
        let pipe = Detached::launch(host, id, &spec, cwd).await?;
        pipe.write(&driver.handshake()).await?;
        Ok(Self {
            pipe,
            driver: StdMutex::new(driver),
            unsent: Mutex::new(Vec::new()),
        })
    }

    /// Rejoins a running session from byte `offset` of its log, with a driver
    /// that has seen exactly the bytes before it (a fresh driver with 0
    /// replays the whole session). No handshake is sent: the harness already
    /// had one.
    pub fn attach(host: Arc<Host>, id: &str, driver: Box<dyn Driver>, offset: u64) -> Result<Self> {
        Ok(Self {
            pipe: Detached::attach(host, id, offset)?,
            driver: StdMutex::new(driver),
            unsent: Mutex::new(Vec::new()),
        })
    }

    /// Wraps an already-configured [`Detached`].
    pub fn from_parts(pipe: Detached, driver: Box<dyn Driver>) -> Self {
        Self {
            pipe,
            driver: StdMutex::new(driver),
            unsent: Mutex::new(Vec::new()),
        }
    }

    pub fn pipe(&self) -> &Detached {
        &self.pipe
    }

    /// Runs `f` with the driver, for its command palette and options.
    pub fn with_driver<R>(&self, f: impl FnOnce(&mut dyn Driver) -> R) -> R {
        let mut d = self.driver.lock().unwrap_or_else(PoisonError::into_inner);
        f(d.as_mut())
    }

    /// Waits for the next events, reconnecting as needed. Replies the driver
    /// owes the harness (JSON-RPC answers) are written before returning.
    /// After the harness exits this returns `[Exited]` on every call.
    pub async fn next_events(&self) -> Result<Vec<Event>> {
        self.flush().await?;
        loop {
            match self.pipe.read().await? {
                Read::Data(bytes) => {
                    let (mut events, reply) = self.with_driver(|d| {
                        let events = d.on_output(&bytes);
                        (events, d.poll_transmit())
                    });
                    match self.pipe.write(&reply).await {
                        // The harness can exit right after asking; the log
                        // still has to be read to the end.
                        Ok(()) | Err(Error::NotRunning(_)) => {}
                        // Keep the events; the reply goes out on the next call.
                        Err(_) => self.unsent.lock().await.extend(reply),
                    }
                    if events.is_empty() {
                        continue;
                    }
                    let now = now_ms();
                    for e in &mut events {
                        // Drivers have no clock; stamp state on receipt.
                        if let Event::StateChanged(s) = e {
                            s.observed_at_ms = now;
                        }
                    }
                    return Ok(events);
                }
                Read::Exited(code) => return Ok(vec![Event::Exited { code }]),
            }
        }
    }

    /// Sends a command to the harness.
    pub async fn send(&self, cmd: Command) -> Result<()> {
        self.flush().await?;
        let bytes = self.with_driver(|d| d.command(cmd))?;
        self.pipe.write(&bytes).await
    }
}

impl Session {
    /// Delivers a reply that failed earlier, keeping the harness's input in order.
    async fn flush(&self) -> Result<()> {
        let mut unsent = self.unsent.lock().await;
        if unsent.is_empty() {
            return Ok(());
        }
        match self.pipe.write(&unsent).await {
            Ok(()) | Err(Error::NotRunning(_)) => {
                unsent.clear();
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

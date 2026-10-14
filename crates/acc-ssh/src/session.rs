//! A harness session running over a detached SSH session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use tokio::sync::Mutex;

use acc_core::Event;
use acc_core::command::{Command, CommandEntry, OptionDef};
use acc_core::harness::Harness;
use acc_core::live::{Opening, Session as Core, SessionConfig};
use acc_core::session::SessionSpec;
use acc_core::state::SessionState;

use crate::detached::{Detached, Read};
use crate::host::Host;
use crate::{Error, Result};

/// A harness on a host, driven over SSH: commands in, events out.
///
/// The protocol side is [`acc_core::live::Session`], the same sans-IO
/// type the language bindings wrap; this adds the detached transport and a
/// clock. Methods take `&self`, so one task can wait on
/// [`Session::next_events`] while others [`Session::send`] commands.
pub struct Session {
    pipe: Detached,
    core: StdMutex<Core>,
    /// A reply that couldn't be delivered yet; sent before anything else.
    unsent: Mutex<Vec<u8>>,
    /// The exit was reported; later reads only repeat `Exited`.
    exited: AtomicBool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("pipe", &self.pipe)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Starts the harness detached on the host as session `id`, in
    /// `config.cwd`, and writes the opening handshake. `config.opening` is
    /// [`Opening::New`] or [`Opening::Resume`]; to join a running session
    /// use [`Session::attach`].
    pub async fn launch(host: Arc<Host>, id: &str, config: SessionConfig) -> Result<Self> {
        if config.opening == Opening::Attach {
            return Err(Error::Invalid("launch starts a harness; use attach to join one"));
        }
        let cwd = config.cwd.clone();
        let model = config.model.clone();
        let mut core = Core::new(config);
        let mut spec = SessionSpec::new(id, core.harness(), &cwd, core.launch_spec().clone(), now_ms());
        spec.model = model;
        let pipe = Detached::launch(host, id, core.launch_spec(), &cwd, Some(&spec.to_json())).await?;
        pipe.write(&core.start()).await?;
        Ok(Self::from_parts(pipe, core))
    }

    /// Rejoins running session `id` from byte `offset` of its log. Nothing is
    /// written: the harness already had its handshake. `config.opening` is
    /// replaced by [`Opening::Attach`].
    ///
    /// Offset 0 replays the whole session, so the driver rebuilds its command
    /// palette, options and pending requests from the traffic. A later offset
    /// is for a client resuming where it left off with the same driver state,
    /// so it only suits [`Session::from_parts`] with a kept core.
    pub fn attach(host: Arc<Host>, id: &str, mut config: SessionConfig, offset: u64) -> Result<Self> {
        config.opening = Opening::Attach;
        Ok(Self::from_parts(
            Detached::attach(host, id, offset)?,
            Core::new(config),
        ))
    }

    /// Wraps an already-configured [`Detached`] and core session. Writes
    /// nothing; the caller sends [`Core::start`] if the harness needs it.
    pub fn from_parts(pipe: Detached, core: Core) -> Self {
        Self {
            pipe,
            core: StdMutex::new(core),
            unsent: Mutex::new(Vec::new()),
            exited: AtomicBool::new(false),
        }
    }

    pub fn pipe(&self) -> &Detached {
        &self.pipe
    }

    /// Runs `f` with the core session.
    pub fn with_core<R>(&self, f: impl FnOnce(&mut Core) -> R) -> R {
        let mut core = self.core.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut core)
    }

    pub fn harness(&self) -> Harness {
        self.with_core(|c| c.harness())
    }

    /// The current state, merged across every source by fidelity.
    pub fn state(&self) -> Option<SessionState> {
        self.with_core(|c| c.state().cloned())
    }

    /// Merges a state seen elsewhere (a transcript, a pane's OSC 7501).
    /// Returns `true` if the visible state changed.
    pub fn observe(&self, state: SessionState) -> bool {
        self.with_core(|c| c.observe(state))
    }

    /// The harness's own session id, for resume and takeover, once known.
    pub fn native_id(&self) -> Option<String> {
        self.with_core(|c| c.native_id().map(str::to_owned))
    }

    /// The command palette. Read again after [`Event::CommandsChanged`].
    pub fn commands(&self) -> Vec<CommandEntry> {
        self.with_core(|c| c.commands().to_vec())
    }

    /// Options and their values. Read again after [`Event::OptionsChanged`].
    pub fn options(&self) -> Vec<OptionDef> {
        self.with_core(|c| c.options().to_vec())
    }

    /// Waits for the next events, reconnecting as needed. Replies the driver
    /// owes the harness (JSON-RPC answers) are written before returning.
    /// Once the harness has exited this returns the exit events, then
    /// `[Exited]` on every later call.
    pub async fn next_events(&self) -> Result<Vec<Event>> {
        self.flush().await?;
        loop {
            match self.pipe.read().await? {
                Read::Data(bytes) => {
                    let got = self.with_core(|c| c.receive(&bytes, now_ms()));
                    match self.pipe.write(&got.transmit).await {
                        // The harness can exit right after asking; the log
                        // still has to be read to the end.
                        Ok(()) | Err(Error::NotRunning(_)) => {}
                        // Keep the events; the reply goes out on the next call.
                        Err(_) => self.unsent.lock().await.extend(got.transmit),
                    }
                    if !got.events.is_empty() {
                        return Ok(got.events);
                    }
                }
                Read::Exited(code) => {
                    if self.exited.swap(true, Ordering::AcqRel) {
                        return Ok(vec![Event::Exited { code }]);
                    }
                    return Ok(self.with_core(|c| c.exited(code, now_ms())));
                }
            }
        }
    }

    /// Sends a command to the harness.
    pub async fn send(&self, cmd: Command) -> Result<()> {
        self.flush().await?;
        let bytes = self.with_core(|c| c.send(cmd))?;
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

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

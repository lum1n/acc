//! Harnesses in terminal multiplexer panes on a host (tmux, zellij, screen,
//! Rex), driven over SSH with the host's own multiplexer binary.
//!
//! The commands and parsers are [`acc_core::multiplexer`]'s; this runs them.
//! [`Pane`] co-drives a harness TUI the user may also be looking at: ACC
//! commands become keystrokes, and events come from the pane's OSC 7501
//! reports and from reading its screen (Claude's and pi's dialogs and state).

use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use acc_core::Event;
use acc_core::command::Command;
use acc_core::harness::Harness;
use acc_core::multiplexer::{
    self, Backend, Exec, Input, PaneInfo, PaneObserver, PaneRef, Program, StreamKind, Target, rex,
    screen_match, tmux, tui, zellij,
};
use acc_core::session::DEFAULT_STALE_AFTER_MS;
use acc_core::state::{SessionState, StateTracker};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep, timeout_at};

use crate::host::{Host, Output, Stream, StreamMsg};
use crate::session::now_ms;
use crate::{Error, Result};

/// Runs a multiplexer command on the host; fails unless it exits 0.
pub async fn run(host: &Host, exec: &Exec) -> Result<Output> {
    host.exec(&exec.script, &exec.stdin).await?.ok()
}

/// The panes of `target` (every session when it names none).
pub async fn list_panes(host: &Host, target: &Target) -> Result<Vec<PaneInfo>> {
    let b = multiplexer::backend(target.multiplexer);
    let out = run(host, &b.list_panes(target)).await?;
    b.parse_panes(target, &out.stdout_str()).map_err(mux)
}

/// The pane process `pid` runs in, read from its environment and checked
/// against the live pane list. `None` when it isn't in a multiplexer.
pub async fn locate(host: &Host, pid: u32) -> Result<Option<PaneInfo>> {
    let environ = run(host, &multiplexer::environ_script(pid)).await?.stdout_str();
    let Some(located) = multiplexer::locate(multiplexer::parse_environ(&environ)) else {
        return Ok(None);
    };
    let panes = list_panes(host, &located.target()).await?;
    Ok(multiplexer::match_pane(&panes, Some(&located), None).cloned())
}

/// Starts `program` in a new background pane of `target` in `cwd`, without
/// taking focus from attached clients. `name` labels it and should be unique.
pub async fn spawn(
    host: &Host,
    target: &Target,
    program: &Program,
    cwd: &str,
    name: &str,
) -> Result<PaneRef> {
    let b = multiplexer::backend(target.multiplexer);
    let out = run(host, &b.spawn(target, program, cwd, name)).await?;
    b.parse_spawned(target, name, &out.stdout_str()).map_err(mux)
}

fn mux(e: multiplexer::MuxError) -> Error {
    Error::Mux(e.to_string())
}

/// How [`Pane::next_events`] paces screen reads.
#[derive(Debug, Clone, Copy)]
pub struct Pacing {
    /// Read the screen at most this often while the pane is drawing.
    pub min_gap: Duration,
    /// Read the screen at least this often, even when the watch stream is
    /// quiet or unavailable.
    pub idle: Duration,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            min_gap: Duration::from_millis(250),
            idle: Duration::from_secs(3),
        }
    }
}

/// Decodes the watch stream of one multiplexer.
#[derive(Debug)]
enum Decoder {
    Tmux(tmux::ControlParser),
    /// One JSON object per line (zellij, Rex).
    Lines(Vec<u8>),
    Raw,
}

#[derive(Debug)]
struct Watcher {
    stream: Option<Stream>,
    decoder: Decoder,
    osc: PaneObserver,
    screen: screen_match::ScreenObserver,
    /// When the screen was last read, and when it should be read next.
    last_read: Option<Instant>,
    read_at: Option<Instant>,
    /// The watch command failed to start; until `retry_at` only read the screen.
    retry_at: Option<Instant>,
    exited: bool,
}

/// A harness TUI in a multiplexer pane, co-driven over SSH.
///
/// The user can keep using the pane; ACC types into it like they would.
/// Methods take `&self`, so one task can wait on [`Pane::next_events`] while
/// others [`Pane::send`] commands.
#[derive(Debug)]
pub struct Pane {
    host: Arc<Host>,
    pane: PaneRef,
    harness: Harness,
    pacing: Pacing,
    watcher: Mutex<Watcher>,
    state: StdMutex<StateTracker>,
}

impl Pane {
    pub fn new(host: Arc<Host>, pane: PaneRef, harness: Harness) -> Self {
        let decoder = new_decoder(&pane);
        Self {
            host,
            harness,
            pacing: Pacing::default(),
            watcher: Mutex::new(Watcher {
                stream: None,
                decoder,
                osc: PaneObserver::new(),
                screen: screen_match::ScreenObserver::new(harness),
                last_read: None,
                read_at: None,
                retry_at: None,
                exited: false,
            }),
            state: StdMutex::new(StateTracker::new(DEFAULT_STALE_AFTER_MS)),
            pane,
        }
    }

    /// Starts `harness`'s TUI (resuming its session id, where the TUI can)
    /// in a new pane of `target`.
    pub async fn spawn(
        host: Arc<Host>,
        target: &Target,
        harness: Harness,
        cwd: &str,
        name: &str,
        resume: Option<&str>,
    ) -> Result<Self> {
        let program = tui::launch(harness, resume)?;
        let pane = spawn(&host, target, &program, cwd, name).await?;
        Ok(Self::new(host, pane, harness))
    }

    pub fn with_pacing(mut self, pacing: Pacing) -> Self {
        self.pacing = pacing;
        self
    }

    pub fn pane(&self) -> &PaneRef {
        &self.pane
    }

    pub fn harness(&self) -> Harness {
        self.harness
    }

    /// The current state, merged across OSC 7501 and the screen by fidelity.
    pub fn state(&self) -> Option<SessionState> {
        self.tracker().current().cloned()
    }

    /// The pane's rendered text, with scrollback when asked.
    pub async fn capture(&self, scrollback: bool) -> Result<String> {
        let b = self.backend();
        Ok(run(&self.host, &b.capture(&self.pane, scrollback))
            .await?
            .stdout_str())
    }

    /// Delivers raw input to the pane, in order.
    pub async fn send_input(&self, inputs: &[Input]) -> Result<()> {
        let b = self.backend();
        for input in inputs {
            run(&self.host, &b.send(&self.pane, input)).await?;
        }
        Ok(())
    }

    /// Carries out `cmd` by typing into the TUI. Permission answers are
    /// keyed from the dialog on screen now, and fail with
    /// [`acc_core::driver::DriverError::UnknownRequest`] if that dialog is
    /// gone.
    pub async fn send(&self, cmd: Command) -> Result<()> {
        let inputs = match &cmd {
            Command::AnswerPermission {
                request_id,
                option_id,
            } => {
                let screen = self.capture(false).await?;
                screen_match::answer(self.harness, &screen, request_id, option_id)?
            }
            cmd => tui::inputs(self.harness, cmd)?,
        };
        self.send_input(&inputs).await
    }

    /// Asks the TUI to exit the way a user would.
    pub async fn exit(&self) -> Result<()> {
        self.send_input(&tui::exit_inputs(self.harness)?).await
    }

    /// Closes the pane, ending its program.
    pub async fn kill(&self) -> Result<()> {
        let exec = self.backend().kill(&self.pane).map_err(mux)?;
        run(&self.host, &exec).await.map(drop)
    }

    /// Reads the screen once and returns what changed.
    pub async fn poll(&self) -> Result<Vec<Event>> {
        let mut w = self.watcher.lock().await;
        self.read_screen(&mut w).await
    }

    /// Waits for the next events: state from OSC 7501 reports (tmux, screen,
    /// Rex) and from the screen (dialogs opening and closing, idle/working).
    /// Streams the multiplexer's watch output, reads the screen when the pane
    /// draws, and at least every [`Pacing::idle`]. Survives dropped
    /// connections by reopening the watch. Returns `[Exited]` once the pane
    /// is gone, and on every call after.
    pub async fn next_events(&self) -> Result<Vec<Event>> {
        let mut w = self.watcher.lock().await;
        if w.exited {
            return Ok(vec![Event::Exited { code: None }]);
        }
        loop {
            let now = Instant::now();
            if w.stream.is_none() && w.retry_at.is_none_or(|t| now >= t) {
                match self.open_watch().await {
                    Ok(stream) => {
                        w.stream = Some(stream);
                        w.retry_at = None;
                        w.decoder = new_decoder(&self.pane);
                    }
                    // Fall back to reading the screen until the next try.
                    Err(e) if e.is_transient() || matches!(e, Error::Remote { .. }) => {
                        w.retry_at = Some(now + self.pacing.idle);
                    }
                    Err(e) => return Err(e),
                }
            }
            let read_at = w
                .read_at
                .unwrap_or_else(|| w.last_read.map_or(now, |t| t + self.pacing.idle));
            let mut events = Vec::new();
            let msg = match w.stream.as_mut() {
                Some(stream) => timeout_at(read_at, stream.recv()).await.ok(),
                None => {
                    tokio::time::sleep_until(read_at).await;
                    None
                }
            };
            match msg {
                Some(StreamMsg::Stdout(bytes)) => {
                    let drew = self.decode(&mut w, &bytes, &mut events);
                    if drew && w.read_at.is_none() {
                        let earliest = w.last_read.map_or(now, |t| t + self.pacing.min_gap);
                        w.read_at = Some(earliest.max(now));
                    }
                }
                Some(StreamMsg::Stderr(_)) => {}
                Some(StreamMsg::Exited(_) | StreamMsg::Lost) => {
                    w.stream = None;
                    if !self.is_alive().await? {
                        w.exited = true;
                        events.push(Event::Exited { code: None });
                    } else {
                        // Don't spin if the watch keeps ending right away.
                        sleep(self.pacing.min_gap).await;
                    }
                }
                // Time to read the screen.
                None => match self.read_screen(&mut w).await {
                    Ok(read) => events.extend(read),
                    Err(Error::Remote { .. }) if !self.is_alive().await? => {
                        w.exited = true;
                        events.push(Event::Exited { code: None });
                    }
                    Err(e) => return Err(e),
                },
            }
            if !events.is_empty() {
                return Ok(events);
            }
        }
    }

    /// Stops watching and undoes what the watch set up on the host (screen's
    /// window log). The pane and its program keep running.
    pub async fn close(&self) -> Result<()> {
        let mut w = self.watcher.lock().await;
        if let Some(stream) = w.stream.take() {
            let _ = stream.finish().await;
        }
        match self.backend().watch(&self.pane).cleanup {
            Some(cleanup) => run(&self.host, &cleanup).await.map(drop),
            None => Ok(()),
        }
    }
}

impl Pane {
    fn backend(&self) -> &'static dyn Backend {
        multiplexer::backend(self.pane.multiplexer)
    }

    fn tracker(&self) -> std::sync::MutexGuard<'_, StateTracker> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn open_watch(&self) -> Result<Stream> {
        let watch = self.backend().watch(&self.pane);
        let stream = self.host.open(&watch.exec.script).await?;
        if !watch.exec.stdin.is_empty() {
            stream.write(&watch.exec.stdin).await?;
        }
        // tmux control mode detaches when its stdin closes.
        if watch.stream != StreamKind::TmuxControl {
            stream.finish().await?;
        }
        Ok(stream)
    }

    async fn is_alive(&self) -> Result<bool> {
        match list_panes(&self.host, &self.pane.target()).await {
            Ok(panes) => Ok(panes.iter().any(|p| p.pane.pane == self.pane.pane && !p.exited)),
            // The server or session is gone with it.
            Err(Error::Remote { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn read_screen(&self, w: &mut Watcher) -> Result<Vec<Event>> {
        let screen = self.capture(false).await?;
        w.last_read = Some(Instant::now());
        w.read_at = None;
        let mut events = w.screen.feed(&screen, now_ms());
        self.note(&mut events);
        Ok(events)
    }

    /// Decodes watch output into `events`; true if the pane drew something
    /// that calls for reading the screen.
    fn decode(&self, w: &mut Watcher, bytes: &[u8], events: &mut Vec<Event>) -> bool {
        let now = now_ms();
        let mut drew = false;
        let Watcher {
            decoder, osc, screen, ..
        } = w;
        let mut states = |raw: &[u8], events: &mut Vec<Event>| {
            for update in osc.feed(raw, now) {
                // Child records (`agents/3`) are the harness's subagents.
                if let (None, Some(state)) = (update.id, update.state) {
                    events.push(Event::StateChanged(state));
                }
            }
        };
        match decoder {
            Decoder::Tmux(parser) => {
                for ev in parser.feed(bytes) {
                    if let tmux::ControlEvent::Output { pane, bytes } = ev
                        && pane == self.pane.pane
                    {
                        drew = true;
                        states(&bytes, events);
                    }
                }
            }
            Decoder::Raw => {
                drew = true;
                states(bytes, events);
            }
            Decoder::Lines(buf) => {
                buf.extend_from_slice(bytes);
                while let Some(end) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=end).collect();
                    let line = String::from_utf8_lossy(&line);
                    match self.pane.multiplexer {
                        multiplexer::Multiplexer::Zellij => {
                            // Rendered lines: no need to capture.
                            if let Some(update) = zellij::parse_update(line.trim_end()) {
                                events.extend(screen.feed(&update.viewport.join("\n"), now));
                            }
                        }
                        _ => match rex::parse_event(line.trim_end(), now) {
                            Some(rex::RexEvent::ProgramStatus { block, update })
                                if block == self.pane.pane && update.id.is_none() =>
                            {
                                events.extend(update.state.map(Event::StateChanged));
                                drew = true;
                            }
                            Some(rex::RexEvent::ChildExited { block, code }) if block == self.pane.pane => {
                                events.push(Event::Exited {
                                    code: code.and_then(|c| i32::try_from(c).ok()),
                                });
                                w.exited = true;
                            }
                            Some(ev) if ev.block() == Some(self.pane.pane.as_str()) => drew = true,
                            _ => {}
                        },
                    }
                }
            }
        }
        self.note(events);
        drew
    }

    fn note(&self, events: &mut [Event]) {
        let mut tracker = self.tracker();
        for e in events.iter() {
            if let Event::StateChanged(s) = e {
                tracker.observe(s.clone());
            }
        }
    }
}

fn new_decoder(pane: &PaneRef) -> Decoder {
    match multiplexer::backend(pane.multiplexer).watch(pane).stream {
        StreamKind::TmuxControl => Decoder::Tmux(tmux::ControlParser::new()),
        StreamKind::ZellijSubscribe | StreamKind::RexEvents => Decoder::Lines(Vec::new()),
        StreamKind::RawBytes => Decoder::Raw,
    }
}

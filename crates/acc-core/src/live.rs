//! One harness session as an embedder sees it: a driver chosen by harness,
//! with timestamps and merged state, still sans-IO.
//!
//! This is the surface the language bindings wrap. The embedder owns the
//! transport (local pipe, SSH exec channel, detached FIFO/log, multiplexer
//! pane): it starts the process described by [`Session::launch_spec`], writes
//! [`Session::start`], and then shuttles bytes through [`Session::receive`]
//! and [`Session::send`]. Nothing here knows how the bytes travel.

use crate::acp::{AcpConfig, AcpDriver, Start};
use crate::claude::{self, ClaudeConfig, ClaudeDriver};
use crate::codex::{self, CodexConfig, CodexDriver};
use crate::command::{Command, CommandEntry, OptionDef, OptionId};
use crate::driver::{Driver, DriverError};
use crate::event::Event;
use crate::harness::{Harness, LaunchSpec};
use crate::pi::{self, PiDriver};
use crate::state::{SessionState, StateTracker};

/// How long a higher-fidelity state wins before a lower one may replace it.
pub const DEFAULT_STALE_AFTER_MS: u64 = 30_000;

/// How a session begins.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Opening {
    /// A fresh session.
    #[default]
    New,
    /// Resume the harness's own session id (for pi, the session file path).
    Resume { native_id: String },
    /// Join a process another client already started and initialized, such
    /// as a detached session being re-attached from its log. Nothing is
    /// written at start; the driver adopts the session from the traffic it
    /// reads (replay the log from offset 0 to rebuild commands and options).
    Attach,
}

/// Everything needed to create a [`Session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    pub harness: Harness,
    /// Absolute working directory on the host.
    pub cwd: String,
    pub opening: Opening,
    /// Model to start with, where the harness takes one at launch.
    pub model: Option<String>,
    /// Tag for this client's request ids, unique among clients sharing one
    /// harness process (`[A-Za-z0-9_-]`). Used by ACP and pi.
    pub client_tag: String,
}

impl SessionConfig {
    pub fn new(harness: Harness, cwd: impl Into<String>) -> Self {
        Self {
            harness,
            cwd: cwd.into(),
            opening: Opening::New,
            model: None,
            client_tag: "acc".into(),
        }
    }
}

/// What one chunk of harness output produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Received {
    pub events: Vec<Event>,
    /// Bytes to write back to the harness now (replies, follow-up queries).
    pub transmit: Vec<u8>,
}

/// A driver plus the bookkeeping every embedder would otherwise repeat.
#[derive(Debug)]
pub struct Session {
    driver: AnyDriver,
    opening: Opening,
    tracker: StateTracker,
    native_id: Option<String>,
    launch: LaunchSpec,
    /// A model to select over the protocol at start, for harnesses whose
    /// launch command doesn't take one (pi).
    start_model: Option<String>,
}

impl Session {
    pub fn new(config: SessionConfig) -> Self {
        let SessionConfig {
            harness,
            cwd,
            opening,
            model,
            client_tag,
        } = config;
        let resume = match &opening {
            Opening::Resume { native_id } => Some(native_id.clone()),
            _ => None,
        };
        let mut start_model = None;
        let (driver, launch) = match harness {
            Harness::Claude => {
                let d = ClaudeDriver::new(ClaudeConfig {
                    model,
                    ..ClaudeConfig::default()
                });
                let spec = d.launch(resume.as_deref());
                (AnyDriver::Claude(d), spec)
            }
            Harness::Codex => {
                let d = CodexDriver::new(CodexConfig {
                    resume: resume.clone(),
                    cwd: Some(cwd),
                    model,
                    ..CodexConfig::default()
                });
                let spec = d.launch(None);
                (AnyDriver::Codex(d), spec)
            }
            Harness::Pi => {
                let d = PiDriver::with_id_prefix(&client_tag);
                let spec = d.launch(resume.as_deref());
                start_model = model;
                (AnyDriver::Pi(d), spec)
            }
            Harness::Cursor | Harness::Copilot | Harness::Opencode => {
                let start = match &opening {
                    Opening::New => Start::New,
                    Opening::Resume { native_id } => Start::Resume {
                        session_id: native_id.clone(),
                    },
                    Opening::Attach => Start::Attach,
                };
                let mut cfg = AcpConfig::new(cwd, start);
                cfg.model = model;
                cfg.client_tag = client_tag;
                let d = AcpDriver::new(harness, cfg);
                let spec = d.launch(None);
                (AnyDriver::Acp(d), spec)
            }
        };
        Self {
            driver,
            native_id: resume,
            opening,
            tracker: StateTracker::new(DEFAULT_STALE_AFTER_MS),
            launch,
            start_model,
        }
    }

    pub fn harness(&self) -> Harness {
        self.driver.get().harness()
    }

    /// The process to start on the host for this session.
    pub fn launch_spec(&self) -> &LaunchSpec {
        &self.launch
    }

    /// Bytes to write once the process is running (handshake, session open
    /// and, for pi, the configured model). Empty when attaching. Call once.
    pub fn start(&mut self) -> Vec<u8> {
        if self.opening == Opening::Attach {
            return Vec::new();
        }
        let d = self.driver.get_mut();
        let mut out = d.handshake();
        out.extend(d.poll_transmit());
        if let Some(model) = self.start_model.take() {
            // pi takes `provider/model`; anything else stays on pi's default.
            let set = Command::SetOption {
                id: OptionId::Model,
                value: model,
            };
            if let Ok(bytes) = d.command(set) {
                out.extend(bytes);
            }
        }
        out
    }

    /// Feeds harness output. `now_ms` (milliseconds since the Unix epoch)
    /// stamps state changes, since the core has no clock.
    pub fn receive(&mut self, bytes: &[u8], now_ms: u64) -> Received {
        let d = self.driver.get_mut();
        let mut events = d.on_output(bytes);
        let transmit = d.poll_transmit();
        for event in &mut events {
            self.note(event, now_ms);
        }
        Received { events, transmit }
    }

    /// Turns a command into bytes to write to the harness.
    pub fn send(&mut self, command: Command) -> Result<Vec<u8>, DriverError> {
        let d = self.driver.get_mut();
        let mut out = d.command(command)?;
        out.extend(d.poll_transmit());
        Ok(out)
    }

    /// Records that the harness process ended (the transport saw EOF or an
    /// exit status). Returns the events to show for it.
    pub fn exited(&mut self, code: Option<i32>, now_ms: u64) -> Vec<Event> {
        let mut state = Event::StateChanged(SessionState {
            state: crate::state::RunState::Exited,
            message: code.map(|c| format!("exit code {c}")),
            source: crate::state::StateSource::Native,
            observed_at_ms: 0,
        });
        self.note(&mut state, now_ms);
        vec![state, Event::Exited { code }]
    }

    /// Merges a state seen elsewhere (OSC 7501 in a pane, a transcript, a
    /// screen match). Returns `true` if the visible state changed.
    pub fn observe(&mut self, state: SessionState) -> bool {
        self.tracker.observe(state)
    }

    /// The current state, merged across every source by fidelity.
    pub fn state(&self) -> Option<&SessionState> {
        self.tracker.current()
    }

    /// The harness's own session id, for resume and takeover, once known.
    pub fn native_id(&self) -> Option<&str> {
        self.native_id.as_deref()
    }

    pub fn commands(&self) -> &[CommandEntry] {
        self.driver.get().commands()
    }

    pub fn options(&self) -> &[OptionDef] {
        self.driver.get().options()
    }

    fn note(&mut self, event: &mut Event, now_ms: u64) {
        match event {
            Event::StateChanged(s) => {
                if s.observed_at_ms == 0 {
                    s.observed_at_ms = now_ms;
                }
                self.tracker.observe(s.clone());
            }
            Event::SessionStarted { native_id, .. } => {
                self.native_id = Some(native_id.clone());
            }
            _ => {}
        }
    }
}

/// Concrete drivers, so a `Session` is `Debug` and needs no allocation per call.
#[derive(Debug)]
enum AnyDriver {
    Claude(ClaudeDriver),
    Codex(CodexDriver),
    Pi(PiDriver),
    Acp(AcpDriver),
}

impl AnyDriver {
    fn get(&self) -> &dyn Driver {
        match self {
            AnyDriver::Claude(d) => d,
            AnyDriver::Codex(d) => d,
            AnyDriver::Pi(d) => d,
            AnyDriver::Acp(d) => d,
        }
    }

    fn get_mut(&mut self) -> &mut dyn Driver {
        match self {
            AnyDriver::Claude(d) => d,
            AnyDriver::Codex(d) => d,
            AnyDriver::Pi(d) => d,
            AnyDriver::Acp(d) => d,
        }
    }
}

/// Reads a harness's own transcript file to follow a session ACC didn't
/// start (state, usage), before or instead of taking it over.
#[derive(Debug)]
pub enum TranscriptObserver {
    Claude(claude::transcript::Observer),
    Codex(codex::rollout::Observer),
    Pi(pi::Transcript),
}

impl TranscriptObserver {
    /// `None` for harnesses whose transcripts ACC doesn't read yet.
    pub fn new(harness: Harness) -> Option<Self> {
        match harness {
            Harness::Claude => Some(Self::Claude(claude::transcript::Observer::new())),
            Harness::Codex => Some(Self::Codex(codex::rollout::Observer::new())),
            Harness::Pi => Some(Self::Pi(pi::Transcript::new())),
            Harness::Cursor | Harness::Copilot | Harness::Opencode => None,
        }
    }

    /// Feeds bytes appended to the transcript; stamps state changes with `now_ms`.
    pub fn feed(&mut self, bytes: &[u8], now_ms: u64) -> Vec<Event> {
        let mut events = match self {
            Self::Claude(o) => o.feed(bytes),
            Self::Codex(o) => o.feed(bytes),
            Self::Pi(o) => o.feed(bytes),
        };
        for event in &mut events {
            if let Event::StateChanged(s) = event
                && s.observed_at_ms == 0
            {
                s.observed_at_ms = now_ms;
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RunState;

    #[test]
    fn every_harness_builds_a_session() {
        for h in Harness::ALL {
            let s = Session::new(SessionConfig::new(h, "/tmp"));
            assert_eq!(s.harness(), h);
            assert_eq!(s.launch_spec().program, h.program());
        }
    }

    #[test]
    fn attach_writes_nothing_at_start() {
        let mut cfg = SessionConfig::new(Harness::Copilot, "/tmp");
        cfg.opening = Opening::Attach;
        assert!(Session::new(cfg).start().is_empty());
        assert!(
            !Session::new(SessionConfig::new(Harness::Copilot, "/tmp"))
                .start()
                .is_empty()
        );
    }

    #[test]
    fn resume_id_is_known_before_the_harness_reports_it() {
        let mut cfg = SessionConfig::new(Harness::Claude, "/tmp");
        cfg.opening = Opening::Resume {
            native_id: "abc".into(),
        };
        let s = Session::new(cfg);
        assert_eq!(s.native_id(), Some("abc"));
        assert!(s.launch_spec().args.ends_with(&["--resume".into(), "abc".into()]));
    }

    #[test]
    fn receive_stamps_state_and_learns_the_session_id() {
        let mut s = Session::new(SessionConfig::new(Harness::Claude, "/tmp"));
        let init = br#"{"type":"system","subtype":"init","session_id":"s-1","model":"m","tools":[],"slash_commands":[]}
"#;
        let got = s.receive(init, 42);
        assert!(
            got.events
                .iter()
                .any(|e| matches!(e, Event::SessionStarted { native_id, .. } if native_id == "s-1"))
        );
        assert_eq!(s.native_id(), Some("s-1"));
        if let Some(state) = s.state() {
            assert_eq!(state.observed_at_ms, 42);
        }
    }

    #[test]
    fn exit_sets_exited_state() {
        let mut s = Session::new(SessionConfig::new(Harness::Pi, "/tmp"));
        let events = s.exited(Some(1), 7);
        assert!(matches!(events.last(), Some(Event::Exited { code: Some(1) })));
        assert_eq!(s.state().map(|st| &st.state), Some(&RunState::Exited));
        assert_eq!(s.state().map(|st| st.observed_at_ms), Some(7));
    }

    #[test]
    fn pi_selects_the_configured_model_at_start() {
        let mut cfg = SessionConfig::new(Harness::Pi, "/tmp");
        cfg.model = Some("anthropic/claude-x".into());
        let start = String::from_utf8(Session::new(cfg).start()).expect("utf8");
        assert!(start.contains(r#""type":"set_model""#), "{start}");
        assert!(start.contains(r#""modelId":"claude-x""#), "{start}");
    }

    #[test]
    fn transcript_observer_exists_for_file_backed_harnesses() {
        assert!(TranscriptObserver::new(Harness::Claude).is_some());
        assert!(TranscriptObserver::new(Harness::Copilot).is_none());
    }
}

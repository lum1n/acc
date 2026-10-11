//! Following a harness ACC didn't start, from its transcript.
//!
//! [`Observed`] wraps the harness's transcript reader (Claude, Codex or pi)
//! with the same [`Transcript`], [`StateTracker`] and usage folding a driven
//! [`crate::session::Session`] has, so a client renders both the same way.
//! Commands go to the TUI as keystrokes through its multiplexer pane, where
//! it has one ([`crate::multiplexer::tui`]); full control comes from taking
//! the session over ([`crate::takeover`]).

use crate::claude;
use crate::codex;
use crate::command::Command;
use crate::detached::{FeedEntry, FeedPath};
use crate::discover::ForeignAgent;
use crate::driver::DriverError;
use crate::event::{Event, Quota, Usage};
use crate::harness::Harness;
use crate::multiplexer::{self, Exec, Input, tui};
use crate::pi;
use crate::session::Update;
use crate::state::{BlockKind, RunState, SessionState, StateSource, StateTracker};
use crate::transcript::Transcript;

/// After this long without news, a lower-fidelity state source may override.
const STALE_MS: u64 = 30_000;
/// An outstanding tool call this quiet in a live TUI is most likely waiting
/// on the user's approval.
const PROMPT_QUIET_MS: u64 = 4_000;

#[derive(Debug)]
enum Reader {
    Claude(claude::transcript::Observer),
    Codex(codex::rollout::Observer),
    Pi(pi::Transcript),
}

impl Reader {
    fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        match self {
            Reader::Claude(o) => o.feed(bytes),
            Reader::Codex(o) => o.feed(bytes),
            Reader::Pi(t) => t.feed(bytes),
        }
    }

    fn pending_tools(&self) -> usize {
        match self {
            Reader::Claude(o) => o.pending_tools(),
            Reader::Pi(t) => t.pending_tools(),
            Reader::Codex(_) => 0,
        }
    }
}

/// A foreign harness, followed through its transcript.
#[derive(Debug)]
pub struct Observed {
    agent: ForeignAgent,
    reader: Option<Reader>,
    offset: u64,
    replay_until: u64,
    tracker: StateTracker,
    transcript: Transcript,
    usage: Usage,
    quota: Option<Quota>,
    title: Option<String>,
    native_id: Option<String>,
    alive: bool,
    last_news_ms: u64,
}

impl Observed {
    pub fn new(agent: ForeignAgent) -> Self {
        let reader = agent.transcript.as_ref().and_then(|_| match agent.harness {
            Harness::Claude => Some(Reader::Claude(claude::transcript::Observer::new())),
            Harness::Codex => Some(Reader::Codex(codex::rollout::Observer::new())),
            Harness::Pi => Some(Reader::Pi(pi::Transcript::new())),
            _ => None,
        });
        Self {
            replay_until: agent.transcript.as_ref().map_or(0, |t| t.size),
            native_id: agent.native_id.clone(),
            reader,
            offset: 0,
            tracker: StateTracker::new(STALE_MS),
            transcript: Transcript::new(),
            usage: Usage::default(),
            quota: None,
            title: None,
            alive: true,
            last_news_ms: 0,
            agent,
        }
    }

    pub fn agent(&self) -> &ForeignAgent {
        &self.agent
    }

    /// Whether its transcript can be followed (Claude, Codex, pi).
    pub fn is_readable(&self) -> bool {
        self.reader.is_some()
    }

    /// The transcript to follow, from what has been read so far.
    pub fn feed_entry(&self) -> Option<FeedEntry> {
        let t = self.agent.transcript.as_ref()?;
        self.reader.as_ref()?;
        Some(FeedEntry {
            key: self.agent.key(),
            path: FeedPath::File(t.path.clone()),
            offset: self.offset,
        })
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn is_replaying(&self) -> bool {
        self.offset < self.replay_until
    }

    pub fn is_alive(&self) -> bool {
        self.alive
    }

    pub fn state(&self) -> Option<&SessionState> {
        self.tracker.current()
    }

    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    pub fn quota(&self) -> Option<&Quota> {
        self.quota.as_ref()
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The harness's session id, needed to take it over.
    pub fn native_id(&self) -> Option<&str> {
        self.native_id.as_deref()
    }

    /// The turn has ended and no tool call is outstanding: closing the TUI
    /// and resuming headless loses nothing.
    pub fn at_safe_point(&self) -> bool {
        let pending = self.reader.as_ref().map_or(0, Reader::pending_tools);
        pending == 0
            && matches!(
                self.tracker.current().map(|s| &s.state),
                Some(RunState::Done | RunState::Idle | RunState::Error) | None
            )
    }

    /// Bytes of the transcript, starting at [`Observed::offset`].
    pub fn feed(&mut self, bytes: &[u8], now_ms: u64) -> Vec<Update> {
        let Some(reader) = self.reader.as_mut() else {
            return Vec::new();
        };
        let replay = self.offset < self.replay_until;
        self.offset += bytes.len() as u64;
        let events = reader.feed(bytes);
        if !events.is_empty() {
            self.last_news_ms = now_ms;
        }
        let mut out = Vec::new();
        for mut event in events {
            match &mut event {
                Event::StateChanged(s) => {
                    s.observed_at_ms = now_ms;
                    if !self.tracker.observe(s.clone()) {
                        continue;
                    }
                }
                Event::SessionStarted { native_id, .. } => {
                    self.native_id.get_or_insert_with(|| native_id.clone());
                }
                Event::Usage(u) => {
                    if u.context_used.is_some() {
                        self.usage.context_used = u.context_used;
                    }
                    if u.context_window.is_some() {
                        self.usage.context_window = u.context_window;
                    }
                    if u.output_tokens.is_some() {
                        self.usage.output_tokens = u.output_tokens;
                        self.usage.input_tokens = u.input_tokens;
                    }
                }
                Event::Quota(q) => self.quota = Some(q.clone()),
                Event::Native { kind, payload } if kind == "title" => {
                    self.title = payload.get("title").and_then(|t| t.as_str()).map(str::to_owned);
                }
                _ => {}
            }
            let item = self.transcript.apply(&event);
            out.push(Update { event, item, replay });
        }
        out
    }

    /// Periodic check with whether the process still runs. Infers a waiting
    /// approval from a quiet outstanding tool call, and the end of the
    /// session from the process going away.
    pub fn tick(&mut self, now_ms: u64, process_alive: bool) -> Vec<Update> {
        let mut events = Vec::new();
        if !process_alive && self.alive {
            self.alive = false;
            events.push(Event::Exited { code: None });
            events.push(Event::StateChanged(SessionState {
                state: RunState::Exited,
                message: None,
                source: StateSource::Native,
                observed_at_ms: now_ms,
            }));
        } else if process_alive
            && self.reader.as_ref().is_some_and(|r| r.pending_tools() > 0)
            && now_ms.saturating_sub(self.last_news_ms) >= PROMPT_QUIET_MS
            && !matches!(
                self.tracker.current().map(|s| &s.state),
                Some(RunState::Blocked { .. })
            )
        {
            events.push(Event::StateChanged(SessionState {
                state: RunState::Blocked {
                    kind: BlockKind::Permission,
                },
                message: Some("Waiting in the terminal".into()),
                // Inferred from the transcript (an unanswered tool call) and
                // its silence, so it ranks with transcript observations.
                source: StateSource::Transcript,
                observed_at_ms: now_ms,
            }));
        }
        let mut out = Vec::new();
        for event in events {
            if let Event::StateChanged(s) = &event
                && !self.tracker.observe(s.clone())
            {
                continue;
            }
            if matches!(event, Event::Exited { .. }) {
                for change in self.transcript.close_running() {
                    out.push(Update {
                        event: Event::ItemCompleted {
                            id: change.id().to_owned(),
                            extensions: serde_json::Value::Null,
                        },
                        item: Some(change),
                        replay: false,
                    });
                }
            }
            out.push(Update {
                event,
                item: None,
                replay: false,
            });
        }
        out
    }

    /// Execs that type `cmd` into the TUI's pane: prompts, interrupt, and
    /// the slash commands each TUI takes. Approvals need a takeover.
    pub fn pane_command(&self, cmd: &Command) -> Result<Vec<Exec>, DriverError> {
        let pane = self.agent.pane.as_ref().ok_or(DriverError::Unsupported {
            harness: self.agent.harness,
            what: "commands outside a multiplexer pane",
        })?;
        let backend = multiplexer::backend(pane.multiplexer);
        Ok(tui::inputs(self.agent.harness, cmd)?
            .iter()
            .map(|i: &Input| backend.send(pane, i))
            .collect())
    }
}

/// Script printing `alive` or `gone` for a process.
pub fn alive_script(pid: u32) -> String {
    format!("kill -0 {pid} 2>/dev/null && echo alive || echo gone\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::discover::TranscriptRef;
    use crate::multiplexer::{Multiplexer, PaneRef};

    const CLAUDE: &str = include_str!("../tests/fixtures/claude/transcript.jsonl");

    fn agent(size: u64) -> ForeignAgent {
        ForeignAgent {
            harness: Harness::Claude,
            pid: 42,
            args: "claude".into(),
            cwd: Some("/home/v/proj".into()),
            elapsed_secs: Some(10),
            pane: Some(PaneRef {
                multiplexer: Multiplexer::Tmux,
                server: String::new(),
                session: None,
                pane: "%3".into(),
            }),
            pane_label: Some("work:1".into()),
            transcript: Some(TranscriptRef {
                path: "/home/v/.claude/projects/-home-v-proj/x.jsonl".into(),
                size,
                exact: true,
            }),
            native_id: None,
        }
    }

    #[test]
    fn replays_history_then_follows_live() {
        let mut o = Observed::new(agent(CLAUDE.len() as u64));
        let ups = o.feed(CLAUDE.as_bytes(), 1);
        assert!(ups.iter().all(|u| u.replay));
        assert!(!o.is_replaying());
        assert_eq!(o.native_id(), Some("5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11"));
        assert_eq!(o.title(), Some("Greeting reply"));
        assert!(o.at_safe_point());
        assert_eq!(o.transcript().len(), 5);
        assert_eq!(o.feed_entry().unwrap().offset, CLAUDE.len() as u64);
    }

    #[test]
    fn quiet_tool_call_reads_as_waiting_and_exit_ends_it() {
        let cut: String = CLAUDE
            .lines()
            .take_while(|l| !l.contains("\"tool_result\""))
            .map(|l| format!("{l}\n"))
            .collect();
        let mut o = Observed::new(agent(0));
        o.feed(cut.as_bytes(), 1_000);
        assert!(!o.at_safe_point());
        assert!(o.tick(2_000, true).is_empty());
        let ups = o.tick(6_000, true);
        assert!(matches!(&ups[0].event, Event::StateChanged(s)
            if s.state == RunState::Blocked { kind: BlockKind::Permission } && s.source == StateSource::Transcript));
        let ups = o.tick(7_000, false);
        assert!(ups.iter().any(|u| matches!(u.event, Event::Exited { .. })));
        assert!(!o.is_alive());
    }

    #[test]
    fn pane_commands_are_tmux_keystrokes() {
        let o = Observed::new(agent(0));
        let execs = o.pane_command(&Command::Prompt { text: "go on".into() }).unwrap();
        assert!(execs[0].script.contains("send-keys") && execs[0].script.contains("'go on'"));
        assert!(
            o.pane_command(&Command::AnswerPermission {
                request_id: "x".into(),
                option_id: "y".into()
            })
            .is_err()
        );
    }
}

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
/// An outstanding tool call this quiet in a live TUI may be waiting on the
/// user: the transcript doesn't record approval dialogs.
const PROMPT_QUIET_MS: u64 = 4_000;
/// Claude tools that never ask for approval, so a quiet one is just running.
const CLAUDE_UNPROMPTED: &[&str] = &[
    "Agent",
    "Task",
    "Read",
    "Glob",
    "Grep",
    "LS",
    "TodoWrite",
    "TodoRead",
    "NotebookRead",
    "BashOutput",
    "KillShell",
    "KillBash",
    "TaskOutput",
    "TaskStop",
    "ListMcpResourcesTool",
    "ReadMcpResourceTool",
];

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

    /// What the outstanding tool calls may be waiting on, and whether that is
    /// certain from the transcript alone (`false`: only the screen can tell
    /// a dialog from a slow tool).
    fn waiting_on(&self, bypass: bool) -> Option<(BlockKind, bool)> {
        // pi has no approvals; Codex records its own.
        let Reader::Claude(o) = self else { return None };
        let mut maybe = false;
        for name in o.pending_tool_names() {
            match name {
                "AskUserQuestion" => return Some((BlockKind::Question, true)),
                "ExitPlanMode" => return Some((BlockKind::Permission, true)),
                n if bypass || CLAUDE_UNPROMPTED.contains(&n) => {}
                _ => maybe = true,
            }
        }
        maybe.then_some((BlockKind::Permission, false))
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
    /// The current block was inferred rather than read, so news from the
    /// transcript ends it.
    inferred_block: bool,
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
            inferred_block: false,
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
        let mut events = reader.feed(bytes);
        if !events.is_empty() {
            self.last_news_ms = now_ms;
        }
        // The transcript moved on (a result, more output), so whatever the
        // inferred block was waiting on was answered. The reader doesn't
        // repeat a state it already reported, so say it here.
        if self.inferred_block
            && events.iter().any(|e| {
                matches!(
                    e,
                    Event::ItemStarted { .. } | Event::ItemCompleted { .. } | Event::StateChanged(_)
                )
            })
        {
            self.inferred_block = false;
            events.insert(0, Event::StateChanged(working(now_ms)));
        }
        let mut out = Vec::new();
        for mut event in events {
            match &mut event {
                Event::StateChanged(s) => {
                    s.observed_at_ms = now_ms;
                    self.inferred_block = false;
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

    /// Whether the process runs with Claude's permission checks off.
    fn bypasses_permissions(&self) -> bool {
        let args = &self.agent.args;
        args.contains("--dangerously-skip-permissions")
            || args.contains("--permission-mode bypassPermissions")
            || args.contains("--permission-mode=bypassPermissions")
    }

    /// A quiet outstanding tool call and what it may be waiting on.
    fn quiet_wait(&self, now_ms: u64) -> Option<(BlockKind, bool)> {
        if now_ms.saturating_sub(self.last_news_ms) < PROMPT_QUIET_MS {
            return None;
        }
        self.reader.as_ref()?.waiting_on(self.bypasses_permissions())
    }

    /// A capture of the TUI's pane for the next [`Observed::tick`], when only
    /// the screen can tell whether it waits on the user: a quiet tool call
    /// that may be behind an approval dialog, or an inferred block that may
    /// have been answered.
    pub fn screen_exec(&self, now_ms: u64) -> Option<Exec> {
        let pane = self.agent.pane.as_ref()?;
        let check = self.inferred_block || matches!(self.quiet_wait(now_ms), Some((_, false)));
        (self.alive && check).then(|| multiplexer::backend(pane.multiplexer).capture(pane, false))
    }

    /// Periodic check with whether the process still runs and, when
    /// [`Observed::screen_exec`] asked for one, the captured screen. Infers
    /// a waiting approval or question from a quiet outstanding tool call
    /// (confirmed on screen where there is one), and the end of the session
    /// from the process going away.
    pub fn tick(&mut self, now_ms: u64, process_alive: bool, screen: Option<&str>) -> Vec<Update> {
        let mut events = Vec::new();
        if !process_alive && self.alive {
            self.alive = false;
            self.inferred_block = false;
            events.push(Event::Exited { code: None });
            events.push(Event::StateChanged(SessionState {
                state: RunState::Exited,
                message: None,
                source: StateSource::Native,
                observed_at_ms: now_ms,
            }));
        } else if process_alive {
            let on_screen = |kind| match (&self.agent.pane, screen) {
                (Some(_), Some(text)) => Some(tui::shows_approval(self.agent.harness, text).then_some(kind)),
                // No capture this time: no news either way.
                (Some(_), None) => None,
                // Nothing to look at; the quiet call is the best guess.
                (None, _) => Some(Some(kind)),
            };
            let waiting = match self.quiet_wait(now_ms) {
                Some((kind, true)) => Some(Some(kind)),
                Some((kind, false)) => on_screen(kind),
                None => Some(None),
            };
            let blocked = matches!(
                self.tracker.current().map(|s| &s.state),
                Some(RunState::Blocked { .. })
            );
            match waiting {
                Some(Some(kind)) if !blocked => {
                    self.inferred_block = true;
                    events.push(Event::StateChanged(SessionState {
                        state: RunState::Blocked { kind },
                        message: Some("Waiting in the terminal".into()),
                        // Inferred from the transcript (an unanswered tool call)
                        // and its silence, so it ranks with transcript observations.
                        source: StateSource::Transcript,
                        observed_at_ms: now_ms,
                    }));
                }
                // The dialog went away (approved a slow command, say).
                Some(None) if self.inferred_block => {
                    self.inferred_block = false;
                    events.push(Event::StateChanged(working(now_ms)));
                }
                _ => {}
            }
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

fn working(now_ms: u64) -> SessionState {
    SessionState {
        state: RunState::Working { progress: None },
        message: None,
        source: StateSource::Transcript,
        observed_at_ms: now_ms,
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
            launcher: None,
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

    /// The fixture up to its Bash call's result: a call left outstanding.
    fn until_tool_result() -> String {
        CLAUDE
            .lines()
            .take_while(|l| !l.contains("\"tool_result\""))
            .map(|l| format!("{l}\n"))
            .collect()
    }

    fn tool_result() -> String {
        CLAUDE
            .lines()
            .find(|l| l.contains("\"tool_result\""))
            .map(|l| format!("{l}\n"))
            .unwrap()
    }

    fn blocked(ups: &[Update]) -> bool {
        ups.iter().any(|u| {
            matches!(&u.event, Event::StateChanged(s)
                if s.state == RunState::Blocked { kind: BlockKind::Permission } && s.source == StateSource::Transcript)
        })
    }

    fn working(ups: &[Update]) -> bool {
        ups.iter().any(
            |u| matches!(&u.event, Event::StateChanged(s) if s.state == RunState::Working { progress: None }),
        )
    }

    const DIALOG: &str = " Bash command\n   ls\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n";
    const RUNNING: &str = "● Bash(ls)\n  ⎿  Running…\n\n> \n";

    #[test]
    fn slow_tool_in_a_pane_is_not_an_approval() {
        let mut o = Observed::new(agent(0));
        o.feed(until_tool_result().as_bytes(), 1_000);
        assert!(!o.at_safe_point());
        assert!(o.screen_exec(2_000).is_none());
        assert!(o.tick(2_000, true, None).is_empty());
        let exec = o.screen_exec(6_000).unwrap();
        assert!(exec.script.contains("capture-pane"));
        assert!(o.tick(6_000, true, Some(RUNNING)).is_empty());
        // No capture is no news.
        assert!(o.tick(7_000, true, None).is_empty());
        let ups = o.tick(8_000, true, Some(DIALOG));
        assert!(blocked(&ups));
        // Approved: the command runs on with the dialog gone.
        assert!(o.screen_exec(9_000).is_some());
        assert!(working(&o.tick(9_000, true, Some(RUNNING))));
        let ups = o.tick(10_000, false, None);
        assert!(ups.iter().any(|u| matches!(u.event, Event::Exited { .. })));
        assert!(!o.is_alive());
    }

    #[test]
    fn transcript_news_ends_an_inferred_block() {
        let mut a = agent(0);
        a.pane = None;
        let mut o = Observed::new(a);
        o.feed(until_tool_result().as_bytes(), 1_000);
        assert!(o.screen_exec(6_000).is_none());
        assert!(blocked(&o.tick(6_000, true, None)));
        let ups = o.feed(tool_result().as_bytes(), 7_000);
        assert!(working(&ups));
        assert_eq!(
            o.state().map(|s| &s.state),
            Some(&RunState::Working { progress: None })
        );
        // Quiet again but nothing outstanding: no block.
        assert!(o.tick(20_000, true, None).is_empty());
    }

    #[test]
    fn bypassed_permissions_never_wait_on_approval() {
        let mut a = agent(0);
        a.pane = None;
        a.args = "claude --dangerously-skip-permissions".into();
        let mut o = Observed::new(a);
        o.feed(until_tool_result().as_bytes(), 1_000);
        assert!(o.tick(6_000, true, None).is_empty());
    }

    #[test]
    fn questions_need_no_screen() {
        let mut o = Observed::new(agent(0));
        let ask = r#"{"type":"assistant","uuid":"a1","sessionId":"s","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}}]}}"#;
        o.feed(format!("{ask}\n").as_bytes(), 1_000);
        assert!(o.screen_exec(6_000).is_none());
        let ups = o.tick(6_000, true, None);
        assert!(ups.iter().any(|u| matches!(&u.event, Event::StateChanged(s)
            if s.state == RunState::Blocked { kind: BlockKind::Question })));
    }

    #[test]
    fn subagents_and_reads_are_just_running() {
        let mut a = agent(0);
        a.pane = None;
        let mut o = Observed::new(a);
        let task = r#"{"type":"assistant","uuid":"a1","sessionId":"s","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Task","input":{}},{"type":"tool_use","id":"t2","name":"Read","input":{}},{"type":"server_tool_use","id":"w1","name":"web_search","input":{}}]}}"#;
        o.feed(format!("{task}\n").as_bytes(), 1_000);
        assert!(o.tick(60_000, true, None).is_empty());
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

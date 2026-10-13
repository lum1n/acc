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
use crate::session::{Update, completions};
use crate::state::{BlockKind, RunState, SessionState, StateSource, StateTracker};
use crate::transcript::Transcript;

/// After this long without news, a lower-fidelity state source may override.
const STALE_MS: u64 = 30_000;
/// How often to look for subagent transcripts while a subagent runs.
const CHILD_POLL_MS: u64 = 2_000;
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
    /// A reader for a subagent's transcript, under the item `parent`.
    fn for_child(&self, parent: String) -> Option<Reader> {
        match self {
            Reader::Claude(_) => Some(Reader::Claude(claude::transcript::Observer::for_subagent(parent))),
            Reader::Codex(_) => Some(Reader::Codex(codex::rollout::Observer::for_child(parent))),
            Reader::Pi(_) => None,
        }
    }

    /// Subagent ids learned since the last call, with their parent item.
    fn take_new_children(&mut self) -> Vec<(String, String)> {
        match self {
            Reader::Claude(o) => o.take_new_agents(),
            Reader::Codex(o) => o.take_new_children(),
            Reader::Pi(_) => Vec::new(),
        }
    }

    /// Whether a subagent is running whose transcript may not be known yet.
    fn subagent_running(&self) -> bool {
        match self {
            Reader::Claude(o) => {
                o.background_agents() > 0 || o.pending_tool_names().any(|n| matches!(n, "Agent" | "Task"))
            }
            _ => false,
        }
    }

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

    /// How the last turn ended (done, or waiting on background agents), as
    /// far as the transcript tells.
    fn turn_ended(&self) -> Option<RunState> {
        match self {
            Reader::Claude(o) if o.pending_tools() == 0 => match o.state() {
                Some(s @ (RunState::Done | RunState::Waiting)) => Some(s.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    fn asks_question(&self) -> bool {
        match self {
            Reader::Claude(o) => o.pending_tool_names().any(|n| n == "AskUserQuestion"),
            _ => false,
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

/// A subagent's transcript followed next to the session's.
#[derive(Debug)]
struct Child {
    key: String,
    path: String,
    /// The harness's id for it (Claude agent id, Codex thread id).
    id: String,
    offset: u64,
    reader: Reader,
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
    /// The harness reports its own state (Claude's `sessions/<pid>.json`),
    /// so nothing is inferred.
    reports_status: bool,
    children: Vec<Child>,
    /// Subagent ids whose transcript hasn't been found yet, with the item
    /// they belong to.
    unlocated: Vec<(String, String)>,
    last_child_check_ms: Option<u64>,
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
            reports_status: false,
            children: Vec::new(),
            unlocated: Vec::new(),
            last_child_check_ms: None,
            agent,
        }
    }

    // ---- subagents -----------------------------------------------------------

    /// A script that finds subagent transcripts not followed yet, while one
    /// may exist (a subagent is running, or one was just reported). Pass its
    /// output to [`Observed::on_children`]. Claude keeps them next to the
    /// session (`<session>/subagents/agent-<id>.jsonl`), Codex as rollouts of
    /// their own; pi and the ACP harnesses keep none.
    pub fn children_exec(&mut self, now_ms: u64) -> Option<Exec> {
        let reader = self.reader.as_mut()?;
        self.unlocated.extend(reader.take_new_children());
        let followed = |id: &str, children: &[Child]| children.iter().any(|c| c.id == id);
        self.unlocated.retain(|(id, _)| !followed(id, &self.children));
        let due = self
            .last_child_check_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= CHILD_POLL_MS);
        let wanted = !self.unlocated.is_empty() || reader.subagent_running();
        if !(due && wanted) {
            return None;
        }
        self.last_child_check_ms = Some(now_ms);
        match reader {
            Reader::Claude(_) => {
                let path = &self.agent.transcript.as_ref()?.path;
                Some(Exec::new(claude::transcript::subagents_script(path)))
            }
            Reader::Codex(_) => {
                let script: String = self
                    .unlocated
                    .iter()
                    .map(|(thread, _)| codex::rollout::locate_script(thread))
                    .collect();
                (!script.is_empty()).then(|| Exec::new(script))
            }
            Reader::Pi(_) => None,
        }
    }

    /// Takes the output of [`Observed::children_exec`]. Returns true when
    /// new transcripts are to be followed: restart the fan-in with
    /// [`Observed::feed_entry`] and [`Observed::child_feed_entries`].
    pub fn on_children(&mut self, stdout: &str) -> bool {
        let Some(reader) = self.reader.as_ref() else {
            return false;
        };
        let found: Vec<(String, String, Option<String>)> = match reader {
            Reader::Claude(_) => claude::transcript::parse_subagents(stdout)
                .into_iter()
                .map(|f| (f.agent_id, f.path, f.tool_use_id))
                .collect(),
            Reader::Codex(_) => stdout
                .lines()
                .map(str::trim)
                .filter_map(|p| {
                    Some((
                        codex::rollout::thread_id_from_path(p)?.to_owned(),
                        p.to_owned(),
                        None,
                    ))
                })
                .collect(),
            Reader::Pi(_) => Vec::new(),
        };
        let mut added = false;
        for (id, path, tool) in found {
            if self.children.iter().any(|c| c.id == id) {
                continue;
            }
            // The meta file names the call (Claude), or the parent's tool
            // result did.
            let parent = tool.or_else(|| {
                self.unlocated
                    .iter()
                    .find(|(u, _)| *u == id)
                    .map(|(_, p)| p.clone())
            });
            let Some(parent) = parent else { continue };
            let Some(child_reader) = reader.for_child(parent) else {
                continue;
            };
            self.unlocated.retain(|(u, _)| *u != id);
            self.children.push(Child {
                key: format!("{}.sub.{}", self.agent.key(), sanitize_key(&id)),
                path,
                id,
                offset: 0,
                reader: child_reader,
            });
            added = true;
        }
        added
    }

    /// The subagent transcripts to follow alongside [`Observed::feed_entry`].
    pub fn child_feed_entries(&self) -> Vec<FeedEntry> {
        self.children
            .iter()
            .map(|c| FeedEntry {
                key: c.key.clone(),
                path: FeedPath::File(c.path.clone()),
                offset: c.offset,
            })
            .collect()
    }

    /// Bytes of the subagent transcript followed under fan-in `key`.
    /// Returns nothing for a key that isn't one of this agent's children.
    pub fn feed_child(&mut self, key: &str, bytes: &[u8], now_ms: u64) -> Vec<Update> {
        let replay = self.is_replaying();
        let Some(child) = self.children.iter_mut().find(|c| c.key == key) else {
            return Vec::new();
        };
        child.offset += bytes.len() as u64;
        let events = child.reader.feed(bytes);
        if !events.is_empty() {
            self.last_news_ms = now_ms;
        }
        let mut out = Vec::new();
        for event in events {
            // A child's state and usage aren't the session's.
            if matches!(
                event,
                Event::StateChanged(_) | Event::Usage(_) | Event::Quota(_) | Event::SessionStarted { .. }
            ) {
                continue;
            }
            let item = self.transcript.apply(&event);
            out.push(Update { event, item, replay });
        }
        out
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
            out.extend(completions(self.transcript.settle(&event), replay));
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
        if self.reports_status {
            return None;
        }
        let check = self.inferred_block || matches!(self.quiet_wait(now_ms), Some((_, false)));
        (self.alive && check).then(|| multiplexer::backend(pane.multiplexer).capture(pane, false))
    }

    /// The state the harness reports itself, from the probe's output after
    /// its first line. Claude Code (2.1+) keeps `status` (busy, idle,
    /// waiting) and `waitingFor` in `~/.claude/sessions/<pid>.json`.
    fn reported_state(&self, report: &str, now_ms: u64) -> Option<SessionState> {
        if self.agent.harness != Harness::Claude {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(report.trim()).ok()?;
        let reader = self.reader.as_ref();
        let waiting_for = v.get("waitingFor").and_then(|w| w.as_str());
        let idle = || reader.and_then(Reader::turn_ended).unwrap_or(RunState::Idle);
        let state = match v.get("status")?.as_str()? {
            "busy" => RunState::Working { progress: None },
            "idle" => idle(),
            "waiting" => match waiting_for {
                Some("input needed") => RunState::Blocked {
                    kind: BlockKind::Question,
                },
                // The user opened a dialog in the TUI (/mcp, /model): they
                // are there already.
                Some("dialog open" | "goal proposal") => idle(),
                _ if reader.is_some_and(Reader::asks_question) => RunState::Blocked {
                    kind: BlockKind::Question,
                },
                // "permission prompt", "sandbox request", "worker request".
                _ => RunState::Blocked {
                    kind: BlockKind::Permission,
                },
            },
            _ => return None,
        };
        let message = matches!(state, RunState::Blocked { .. }).then(|| "Waiting in the terminal".to_owned());
        Some(SessionState {
            state,
            message,
            source: StateSource::Native,
            observed_at_ms: now_ms,
        })
    }

    /// Periodic check with the output of [`Observed::probe_exec`] and, when
    /// [`Observed::screen_exec`] asked for one, the captured screen. Takes
    /// the state the harness reports where it does; otherwise infers a
    /// waiting approval or question from a quiet outstanding tool call
    /// (confirmed on screen where there is one). Ends the session when the
    /// process goes away.
    pub fn tick(&mut self, now_ms: u64, probe: &str, screen: Option<&str>) -> Vec<Update> {
        let (status, report) = probe.split_once('\n').unwrap_or((probe, ""));
        let process_alive = status.trim() == "alive";
        let reported = process_alive
            .then(|| self.reported_state(report, now_ms))
            .flatten();
        self.reports_status = reported.is_some();
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
        } else if let Some(state) = reported {
            self.inferred_block = false;
            // Sent every tick to keep it fresh; unchanged is not news.
            events.push(Event::StateChanged(state));
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
            out.extend(completions(self.transcript.settle(&event), false));
            out.push(Update {
                event,
                item: None,
                replay: false,
            });
        }
        out
    }

    /// Script for [`Observed::tick`]: whether the process runs, and what
    /// the harness reports about itself.
    pub fn probe_exec(&self) -> Exec {
        Exec::new(probe_script(self.agent.harness, self.agent.pid))
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

/// A fan-in key part from a harness id (`[A-Za-z0-9_.:-]`).
fn sanitize_key(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.:-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect()
}

fn working(now_ms: u64) -> SessionState {
    SessionState {
        state: RunState::Working { progress: None },
        message: None,
        source: StateSource::Transcript,
        observed_at_ms: now_ms,
    }
}

/// Script printing `alive` or `gone` for a process, then for Claude its
/// session record. Pass the output to [`Observed::tick`].
pub fn probe_script(harness: Harness, pid: u32) -> String {
    match harness {
        Harness::Claude => format!(
            "if kill -0 {pid} 2>/dev/null; then echo alive; cat \"$HOME/.claude/sessions/{pid}.json\" 2>/dev/null; \
             echo; else echo gone; fi\n"
        ),
        _ => alive_script(pid),
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
        assert!(o.tick(2_000, "alive", None).is_empty());
        let exec = o.screen_exec(6_000).unwrap();
        assert!(exec.script.contains("capture-pane"));
        assert!(o.tick(6_000, "alive", Some(RUNNING)).is_empty());
        // No capture is no news.
        assert!(o.tick(7_000, "alive", None).is_empty());
        let ups = o.tick(8_000, "alive", Some(DIALOG));
        assert!(blocked(&ups));
        // Approved: the command runs on with the dialog gone.
        assert!(o.screen_exec(9_000).is_some());
        assert!(working(&o.tick(9_000, "alive", Some(RUNNING))));
        let ups = o.tick(10_000, "gone", None);
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
        assert!(blocked(&o.tick(6_000, "alive", None)));
        let ups = o.feed(tool_result().as_bytes(), 7_000);
        assert!(working(&ups));
        assert_eq!(
            o.state().map(|s| &s.state),
            Some(&RunState::Working { progress: None })
        );
        // Quiet again but nothing outstanding: no block.
        assert!(o.tick(20_000, "alive", None).is_empty());
    }

    #[test]
    fn bypassed_permissions_never_wait_on_approval() {
        let mut a = agent(0);
        a.pane = None;
        a.args = "claude --dangerously-skip-permissions".into();
        let mut o = Observed::new(a);
        o.feed(until_tool_result().as_bytes(), 1_000);
        assert!(o.tick(6_000, "alive", None).is_empty());
    }

    #[test]
    fn questions_need_no_screen() {
        let mut o = Observed::new(agent(0));
        let ask = r#"{"type":"assistant","uuid":"a1","sessionId":"s","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}}]}}"#;
        o.feed(format!("{ask}\n").as_bytes(), 1_000);
        assert!(o.screen_exec(6_000).is_none());
        let ups = o.tick(6_000, "alive", None);
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
        assert!(o.tick(60_000, "alive", None).is_empty());
    }

    #[test]
    fn pane_commands_are_tmux_keystrokes() {
        let o = Observed::new(agent(0));
        let execs = o.pane_command(&Command::prompt("go on")).unwrap();
        assert!(execs[0].script.contains("send-keys") && execs[0].script.contains("'go on'"));
        assert!(
            o.pane_command(&Command::AnswerPermission {
                request_id: "x".into(),
                option_id: "y".into()
            })
            .is_err()
        );
    }

    const SUB_PARENT: &str =
        include_str!("../tests/fixtures/claude/subagents/71b44a93-af51-4778-825f-a5a38dbf869a.jsonl");
    const SUB_FG: &str = include_str!(
        "../tests/fixtures/claude/subagents/71b44a93-af51-4778-825f-a5a38dbf869a/subagents/agent-adc9fb778705a136d.jsonl"
    );
    const SUB_FG_META: &str = include_str!(
        "../tests/fixtures/claude/subagents/71b44a93-af51-4778-825f-a5a38dbf869a/subagents/agent-adc9fb778705a136d.meta.json"
    );

    #[test]
    fn subagent_transcripts_are_found_and_followed() {
        let mut a = agent(0);
        a.pane = None;
        a.transcript.as_mut().unwrap().path = "/h/.claude/projects/-tmp-acc-sub-spike/s.jsonl".into();
        let mut o = Observed::new(a);
        // Nothing to look for before any subagent runs.
        assert!(o.children_exec(0).is_none());
        // The foreground Agent call starts: its transcript is looked for at once.
        let start = SUB_PARENT
            .lines()
            .position(|l| l.contains("\"name\":\"Agent\""))
            .unwrap();
        let head: String = SUB_PARENT
            .lines()
            .take(start + 1)
            .map(|l| format!("{l}\n"))
            .collect();
        o.feed(head.as_bytes(), 1_000);
        let exec = o.children_exec(1_000).expect("look for the subagent's file");
        assert!(
            exec.script
                .contains("/h/.claude/projects/-tmp-acc-sub-spike/s/subagents")
        );
        assert!(o.children_exec(1_500).is_none(), "throttled");
        let listing = format!(
            "/h/.claude/projects/-tmp-acc-sub-spike/s/subagents/agent-adc9fb778705a136d.jsonl\t{}\t{}\n",
            SUB_FG.len(),
            SUB_FG_META.trim()
        );
        assert!(o.on_children(&listing));
        assert!(!o.on_children(&listing), "already followed");
        let entries = o.child_feed_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "tui-claude-42.sub.adc9fb778705a136d");
        assert!(crate::detached::fan_in_script(&entries).contains("agent-adc9fb778705a136d.jsonl"));
        let ups = o.feed_child(&entries[0].key, SUB_FG.as_bytes(), 2_000);
        assert!(ups.iter().all(|u| !matches!(u.event, Event::StateChanged(_))));
        let parent = "toolu_01WqrBFboSUXpcf1Bb8qoLzE";
        assert!(o.transcript().children(parent).count() >= 3);
        assert_eq!(o.child_feed_entries()[0].offset, SUB_FG.len() as u64);
        assert!(o.feed_child("other", b"x\n", 2_000).is_empty());
    }

    fn claude_report(status: &str, waiting_for: Option<&str>) -> String {
        let w = waiting_for.map_or(String::new(), |w| format!(r#","waitingFor":"{w}""#));
        format!("alive\n{{\"pid\":42,\"sessionId\":\"s\",\"status\":\"{status}\"{w}}}\n")
    }

    fn state_after(o: &mut Observed, at: u64, probe: &str) -> Option<RunState> {
        o.tick(at, probe, None);
        o.state().map(|s| s.state.clone())
    }

    #[test]
    fn claude_reported_status_wins_over_guesses() {
        let mut o = Observed::new(agent(0));
        assert!(o.probe_exec().script.contains(".claude/sessions/42.json"));
        o.feed(until_tool_result().as_bytes(), 1_000);
        // A slow tool, reported busy: no screen needed, no block.
        assert_eq!(
            state_after(&mut o, 6_000, &claude_report("busy", None)),
            Some(RunState::Working { progress: None })
        );
        assert!(o.screen_exec(6_000).is_none());
        assert_eq!(
            state_after(
                &mut o,
                9_000,
                &claude_report("waiting", Some("permission prompt"))
            ),
            Some(RunState::Blocked {
                kind: BlockKind::Permission
            })
        );
        assert_eq!(
            state_after(&mut o, 12_000, &claude_report("waiting", Some("input needed"))),
            Some(RunState::Blocked {
                kind: BlockKind::Question
            })
        );
        // The rest of the turn arrives; the transcript ranks below the report.
        o.feed(tool_result().as_bytes(), 13_000);
        let rest: String = CLAUDE
            .lines()
            .skip_while(|l| !l.contains("\"tool_result\""))
            .skip(1)
            .map(|l| format!("{l}\n"))
            .collect();
        o.feed(rest.as_bytes(), 14_000);
        assert_eq!(
            state_after(&mut o, 15_000, &claude_report("idle", None)),
            Some(RunState::Done)
        );
        // A dialog the user opened in the TUI is not waiting on them.
        assert_eq!(
            state_after(&mut o, 18_000, &claude_report("waiting", Some("dialog open"))),
            Some(RunState::Done)
        );
        // No report (an older Claude): back to the transcript and guesses.
        assert_eq!(state_after(&mut o, 60_000, "alive\n"), Some(RunState::Done));
        assert!(!o.reports_status);
        assert!(
            o.tick(61_000, "gone\n", None)
                .iter()
                .any(|u| matches!(u.event, Event::Exited { .. }))
        );
    }
}

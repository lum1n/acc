//! Observing Claude Code sessions ACC didn't start, from their transcripts.
//!
//! Every Claude Code front end (terminal, desktop app, IDE extensions) appends
//! the conversation to `~/.claude/projects/<project>/<session>.jsonl`. Tailing
//! it over SSH (`tail -c +OFFSET -f`) gives the conversation (messages,
//! reasoning, tool calls and their results), state and usage with nothing
//! installed on the host, and tells ACC when a session is at a safe point to
//! take over (the turn has ended and no tool call is outstanding).

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::event::{Event, ItemKind, Usage};
use crate::harness::Harness;
use crate::ndjson::{Frame, LineFramer};
use crate::shell::quote;
use crate::state::{RunState, SessionState, StateSource};
use crate::wire::{self, owned, str_at};

/// Directory name Claude Code uses for a working directory under
/// `~/.claude/projects`: every character other than an ASCII letter, digit
/// or `-` becomes `-`. (verify for very long paths, which newer CLIs may
/// shorten)
pub fn project_dir_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Transcript path relative to `$HOME`.
pub fn transcript_path(cwd: &str, session_id: &str) -> String {
    format!(".claude/projects/{}/{session_id}.jsonl", project_dir_name(cwd))
}

/// Script printing the transcript path of `session_id` whatever its project
/// directory, or nothing. Useful when the session's cwd isn't known.
pub fn locate_script(session_id: &str) -> String {
    format!(
        "for f in \"$HOME\"/.claude/projects/*/{}.jsonl; do [ -f \"$f\" ] && echo \"$f\"; done; true\n",
        quote(session_id)
    )
}

/// Script listing up to `limit` transcripts, most recently modified first,
/// as paths relative to `~/.claude/projects`.
pub fn recent_script(limit: usize) -> String {
    format!(
        "cd \"$HOME\"/.claude/projects 2>/dev/null && ls -t -- */*.jsonl 2>/dev/null | head -n {limit}; true\n"
    )
}

/// Path of a subagent's transcript, next to its session's: the session
/// directory holds `subagents/agent-<agent id>.jsonl` and a `.meta.json`
/// naming the `Agent` call it ran for (2.1+).
pub fn subagent_dir(transcript_path: &str) -> String {
    format!(
        "{}/subagents",
        transcript_path.strip_suffix(".jsonl").unwrap_or(transcript_path)
    )
}

/// Script listing a session's subagent transcripts as
/// `<path>\t<size>\t<meta json>` lines (the meta may be empty).
pub fn subagents_script(transcript_path: &str) -> String {
    format!(
        "for f in {}/agent-*.jsonl; do [ -f \"$f\" ] || continue; printf '%s\\t%s\\t' \"$f\" \"$(wc -c <\"$f\" | tr -d ' ')\"; \
         cat \"${{f%.jsonl}}.meta.json\" 2>/dev/null | tr -d '\\n'; echo; done; true\n",
        quote(&subagent_dir(transcript_path))
    )
}

/// One subagent transcript found by [`subagents_script`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentFile {
    pub path: String,
    pub size: u64,
    /// `agent-<id>` from the file name.
    pub agent_id: String,
    /// The `Agent` tool call it ran for, from the meta file.
    pub tool_use_id: Option<String>,
    pub agent_type: Option<String>,
    pub description: Option<String>,
}

/// Parses the output of [`subagents_script`].
pub fn parse_subagents(stdout: &str) -> Vec<SubagentFile> {
    stdout
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let path = parts.next()?.to_owned();
            let size = parts.next()?.trim().parse().ok()?;
            let meta: Value = parts
                .next()
                .and_then(|m| serde_json::from_str(m).ok())
                .unwrap_or(Value::Null);
            let agent_id = path
                .rsplit('/')
                .next()?
                .strip_prefix("agent-")?
                .strip_suffix(".jsonl")?
                .to_owned();
            Some(SubagentFile {
                path,
                size,
                agent_id,
                tool_use_id: owned(&meta, "toolUseId"),
                agent_type: owned(&meta, "agentType"),
                description: owned(&meta, "description"),
            })
        })
        .collect()
}

/// Turns transcript lines into transcript-sourced state and usage events.
///
/// [`Observer::for_subagent`] reads a subagent's transcript instead: its
/// items carry the `Agent` call as `parent`, and it reports no state or
/// usage of its own, only the parent item's completion once its turn ends.
#[derive(Debug)]
pub struct Observer {
    framer: LineFramer,
    session_id: Option<String>,
    state: Option<RunState>,
    /// Outstanding tool calls by id, with the tool's name.
    pending_tools: HashMap<String, String>,
    /// The `Agent` item a subagent transcript's lines belong to.
    parent: Option<String>,
    /// Subagents this transcript launched: agent id → `Agent` tool call id.
    agents: HashMap<String, String>,
    /// Agent ids whose transcript has been handed out by [`Observer::take_new_agents`].
    announced: usize,
    /// Agent ids in launch order, for [`Observer::take_new_agents`].
    agent_order: Vec<String>,
    /// `Agent` calls launched in the background and not yet reported back.
    /// They die with the TUI, so the session isn't at a safe point.
    background: Vec<String>,
}

impl Default for Observer {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer {
    pub fn new() -> Self {
        Self {
            framer: LineFramer::new(wire::MAX_FRAME),
            session_id: None,
            state: None,
            pending_tools: HashMap::new(),
            parent: None,
            agents: HashMap::new(),
            announced: 0,
            agent_order: Vec::new(),
            background: Vec::new(),
        }
    }

    /// Background subagents still running.
    pub fn background_agents(&self) -> usize {
        self.background.len()
    }

    /// A reader for a subagent's transcript, whose items belong to the
    /// `Agent` call `parent`.
    pub fn for_subagent(parent: impl Into<String>) -> Self {
        Self {
            parent: Some(parent.into()),
            ..Self::new()
        }
    }

    /// The `Agent` call a subagent id was launched by, from its tool result.
    pub fn tool_for_agent(&self, agent_id: &str) -> Option<&str> {
        self.agents.get(agent_id).map(String::as_str)
    }

    /// Subagents launched since the last call: `(agent id, tool call id)`.
    pub fn take_new_agents(&mut self) -> Vec<(String, String)> {
        let new: Vec<(String, String)> = self.agent_order[self.announced..]
            .iter()
            .filter_map(|a| Some((a.clone(), self.agents.get(a)?.clone())))
            .collect();
        self.announced = self.agent_order.len();
        new
    }

    /// The last state the transcript implies, if any line has been seen.
    pub fn state(&self) -> Option<&RunState> {
        self.state.as_ref()
    }

    /// Tool calls with no result yet. A live process with an outstanding
    /// call that stays quiet is most likely waiting on a permission prompt.
    pub fn pending_tools(&self) -> usize {
        self.pending_tools.len()
    }

    /// Names of the tools with no result yet.
    pub fn pending_tool_names(&self) -> impl Iterator<Item = &str> {
        self.pending_tools.values().map(String::as_str)
    }

    /// True when the last turn ended and nothing is outstanding, so closing
    /// the TUI and resuming headless loses nothing.
    pub fn at_safe_point(&self) -> bool {
        matches!(self.state, Some(RunState::Done))
            && self.pending_tools.is_empty()
            && self.background.is_empty()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        for frame in self.framer.feed(bytes) {
            if let Frame::Line(line) = frame
                && let Ok(v) = serde_json::from_str::<Value>(&line)
            {
                self.line(&v, &mut out);
            }
        }
        out
    }

    fn line(&mut self, v: &Value, out: &mut Vec<Event>) {
        let sidechain = v.get("isSidechain").and_then(Value::as_bool) == Some(true);
        if self.parent.is_none() {
            if str_at(v, "type") == Some("ai-title")
                && let Some(title) = owned(v, "aiTitle")
            {
                out.push(Event::Native {
                    kind: "title".into(),
                    payload: json!({ "title": title }),
                });
            }
            if self.session_id.is_none()
                && !sidechain
                && let Some(id) = owned(v, "sessionId")
            {
                self.session_id = Some(id.clone());
                out.push(Event::SessionStarted {
                    harness: Harness::Claude,
                    native_id: id,
                    model: None,
                });
            }
        }
        // Older CLIs wrote subagent conversations into the session's own
        // file: they belong to the `Agent` call that launched them, once its
        // result has named the agent, and never to the main turn's state.
        let parent = match (&self.parent, sidechain) {
            (Some(p), _) => Some(p.clone()),
            (None, true) => match owned(v, "agentId").and_then(|a| self.agents.get(&a).cloned()) {
                Some(p) => Some(p),
                None => return,
            },
            (None, false) => None,
        };
        // In a subagent's own file every line is a sidechain line; in the
        // main file, a sidechain line is someone else's turn.
        let child = parent.is_some();
        let tracks_turn = self.parent.is_some() || !sidechain;
        // A background agent finished: the TUI queues its report for the
        // model (`queue-operation`), then delivers it (`queued_command`).
        let queued = match str_at(v, "type") {
            Some("queue-operation") if str_at(v, "operation") == Some("enqueue") => str_at(v, "content"),
            Some("attachment")
                if v.pointer("/attachment/type").and_then(Value::as_str) == Some("queued_command") =>
            {
                v.pointer("/attachment/prompt").and_then(Value::as_str)
            }
            _ => None,
        };
        if let Some(text) = queued {
            self.task_report(text, out);
            return;
        }
        let Some(message) = v.get("message") else { return };
        match str_at(v, "type") {
            Some("user") => {
                let mut prompt = false;
                let mut text = String::new();
                match message.get("content") {
                    Some(Value::String(s)) if s.trim_start().starts_with("<task-notification>") => {
                        // The model hears of it as a user message, but it is
                        // the agent's report.
                        self.task_report(s, out);
                    }
                    Some(Value::String(s)) => {
                        prompt = !local_input(s);
                        text.push_str(s);
                    }
                    Some(Value::Array(blocks)) => {
                        for b in blocks {
                            match str_at(b, "type") {
                                Some("tool_result") => {
                                    if let Some(id) = str_at(b, "tool_use_id") {
                                        self.tool_result(id, b, v, tracks_turn, out);
                                    }
                                }
                                Some("text") => {
                                    prompt = true;
                                    text.push_str(str_at(b, "text").unwrap_or_default());
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                let meta = v.get("isMeta").and_then(Value::as_bool) == Some(true);
                if prompt && !meta {
                    if tracks_turn {
                        self.pending_tools.clear();
                    }
                    // Esc in the TUI: the turn is over, nothing follows.
                    let state = if text.starts_with("[Request interrupted by user") {
                        RunState::Idle
                    } else {
                        RunState::Working { progress: None }
                    };
                    if !child {
                        self.set(state, out);
                    }
                    let id = owned(v, "uuid").unwrap_or_else(|| format!("user-{}", text.len()));
                    whole_item(out, id, ItemKind::UserMessage, parent.clone(), text, Value::Null);
                }
            }
            Some("assistant") => {
                let uuid = owned(v, "uuid").unwrap_or_default();
                let mut last_text = None;
                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for (i, b) in blocks.iter().enumerate() {
                        match str_at(b, "type") {
                            Some(kind @ ("tool_use" | "server_tool_use")) => {
                                let Some(id) = owned(b, "id") else { continue };
                                let name = str_at(b, "name").unwrap_or_default();
                                // Server tools run in the API; their result comes in
                                // the same response, never as a user tool_result.
                                if kind == "tool_use" && tracks_turn {
                                    self.pending_tools.insert(id.clone(), name.to_owned());
                                }
                                out.push(Event::ItemStarted {
                                    id,
                                    kind: super::tool_kind(name),
                                    parent: parent.clone(),
                                    extensions: json!({
                                        "name": name,
                                        "input": b.get("input").cloned().unwrap_or(Value::Null),
                                    }),
                                });
                            }
                            // A server tool's result (`web_search_tool_result`, ...).
                            Some(kind) if kind.ends_with("_tool_result") => {
                                let Some(id) = owned(b, "tool_use_id") else {
                                    continue;
                                };
                                out.push(Event::ItemCompleted {
                                    id,
                                    extensions: json!({
                                        "content": b.get("content").cloned().unwrap_or(Value::Null),
                                    }),
                                });
                            }
                            Some("text") => {
                                let text = str_at(b, "text").unwrap_or_default().to_owned();
                                last_text = Some(text.clone());
                                whole_item(
                                    out,
                                    format!("{uuid}:{i}"),
                                    ItemKind::AgentMessage,
                                    parent.clone(),
                                    text,
                                    Value::Null,
                                );
                            }
                            Some("thinking") => whole_item(
                                out,
                                format!("{uuid}:{i}"),
                                ItemKind::Reasoning,
                                parent.clone(),
                                str_at(b, "thinking").unwrap_or_default().to_owned(),
                                Value::Null,
                            ),
                            _ => {}
                        }
                    }
                }
                if !child && let Some(usage) = message.get("usage") {
                    out.push(Event::Usage(Usage {
                        context_used: wire::sum(
                            usage,
                            &[
                                "input_tokens",
                                "cache_read_input_tokens",
                                "cache_creation_input_tokens",
                                "output_tokens",
                            ],
                        ),
                        ..Usage::default()
                    }));
                }
                if !tracks_turn {
                    return;
                }
                let ended = matches!(
                    str_at(message, "stop_reason"),
                    Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal")
                );
                let done = ended && self.pending_tools.is_empty();
                match &self.parent {
                    // A subagent's final message is its report to the parent
                    // call, which a background launch's tool result left open.
                    Some(p) if done => out.push(Event::ItemCompleted {
                        id: p.clone(),
                        extensions: json!({"task": {"status": "completed", "summary": last_text}}),
                    }),
                    Some(_) => {}
                    None if done && !self.background.is_empty() => self.set(RunState::Waiting, out),
                    None if done => self.set(RunState::Done, out),
                    None => self.set(RunState::Working { progress: None }, out),
                }
            }
            _ => {}
        }
    }

    /// A background agent's `<task-notification>`: its `Agent` call is done.
    fn task_report(&mut self, text: &str, out: &mut Vec<Event>) {
        let Some((tool, task)) = super::task_notification(text) else {
            return;
        };
        let was_running = self.background.contains(&tool) || self.pending_tools.contains_key(&tool);
        self.pending_tools.remove(&tool);
        self.background.retain(|b| *b != tool);
        if !was_running {
            // Already reported (the enqueue, then the delivery).
            return;
        }
        let failed = matches!(str_at(&task, "status"), Some("failed" | "stopped" | "killed"));
        out.push(Event::ItemCompleted {
            id: tool,
            extensions: json!({"task": task, "is_error": failed}),
        });
        // The report wakes the model for another turn.
        if self.background.is_empty() && self.state == Some(RunState::Waiting) {
            self.set(RunState::Working { progress: None }, out);
        }
    }

    /// A tool's result. An `Agent` call's structured result (`toolUseResult`)
    /// names the agent whose transcript continues the story; a background
    /// launch only acknowledges, so its item stays running until that
    /// transcript ends (or the TUI's own notification does).
    fn tool_result(
        &mut self,
        id: &str,
        block: &Value,
        line: &Value,
        tracks_turn: bool,
        out: &mut Vec<Event>,
    ) {
        let name = if tracks_turn {
            self.pending_tools.remove(id)
        } else {
            None
        };
        let mut ext = json!({
            "is_error": block.get("is_error").cloned().unwrap_or(Value::Bool(false)),
            "content": block.get("content").cloned().unwrap_or(Value::Null),
        });
        let subagent = name
            .as_deref()
            .is_some_and(|n| super::tool_kind(n) == ItemKind::Subagent);
        let result = line.get("toolUseResult").filter(|r| r.is_object());
        if subagent && let Some(result) = result {
            if let Some(agent) = owned(result, "agentId")
                && !self.agents.contains_key(&agent)
            {
                self.agents.insert(agent.clone(), id.to_owned());
                self.agent_order.push(agent);
            }
            ext["result"] = result.clone();
            if result.get("isAsync").and_then(Value::as_bool) == Some(true) {
                self.background.push(id.to_owned());
                out.push(Event::ItemUpdated {
                    id: id.to_owned(),
                    extensions: ext,
                });
                return;
            }
        }
        out.push(Event::ItemCompleted {
            id: id.to_owned(),
            extensions: ext,
        });
    }

    fn set(&mut self, state: RunState, out: &mut Vec<Event>) {
        if self.state.as_ref() == Some(&state) {
            return;
        }
        self.state = Some(state.clone());
        out.push(Event::StateChanged(SessionState {
            state,
            message: None,
            source: StateSource::Transcript,
            observed_at_ms: 0,
        }));
    }
}

/// Input the TUI handles itself, recorded without starting a turn: slash
/// commands (`/model`, `/mcp`; a prompt command's turn shows in the
/// assistant lines that follow), their output, and `!` shell commands.
fn local_input(s: &str) -> bool {
    let s = s.trim_start();
    ["<local-command", "<command-name>", "<command-message>", "<bash-"]
        .iter()
        .any(|p| s.starts_with(p))
}

/// Announces an item that is complete when first seen.
pub(crate) fn whole_item(
    out: &mut Vec<Event>,
    id: String,
    kind: ItemKind,
    parent: Option<String>,
    text: String,
    ext: Value,
) {
    out.push(Event::ItemStarted {
        id: id.clone(),
        kind,
        parent,
        extensions: Value::Null,
    });
    if !text.is_empty() {
        out.push(Event::ItemDelta { id: id.clone(), text });
    }
    out.push(Event::ItemCompleted { id, extensions: ext });
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    const TRANSCRIPT: &str = include_str!("../../tests/fixtures/claude/transcript.jsonl");
    const SUB_PARENT: &str =
        include_str!("../../tests/fixtures/claude/subagents/71b44a93-af51-4778-825f-a5a38dbf869a.jsonl");
    const SUB_FG: &str = include_str!(
        "../../tests/fixtures/claude/subagents/71b44a93-af51-4778-825f-a5a38dbf869a/subagents/agent-adc9fb778705a136d.jsonl"
    );
    const SUB_BG: &str = include_str!(
        "../../tests/fixtures/claude/subagents/71b44a93-af51-4778-825f-a5a38dbf869a/subagents/agent-a8183a85ad355b426.jsonl"
    );
    const SUB_FG_META: &str = include_str!(
        "../../tests/fixtures/claude/subagents/71b44a93-af51-4778-825f-a5a38dbf869a/subagents/agent-adc9fb778705a136d.meta.json"
    );

    #[test]
    fn subagent_transcripts_attach_to_their_agent_calls() {
        use crate::item::ItemDetail;
        use crate::transcript::{ItemStatus, Transcript};
        let mut main = Observer::new();
        let mut t = Transcript::new();
        // The parent's lines up to the background launch: one agent running.
        let launch = SUB_PARENT.find("Async agent launched").unwrap();
        let cut = SUB_PARENT[launch..].find('\n').unwrap() + launch + 1;
        for e in main.feed(&SUB_PARENT.as_bytes()[..cut]) {
            t.apply(&e);
        }
        assert_eq!(main.background_agents(), 1);
        for e in main.feed(&SUB_PARENT.as_bytes()[cut..]) {
            t.apply(&e);
        }
        assert_eq!(main.background_agents(), 0, "the notification reports it back");
        assert!(main.at_safe_point());
        let agents = main.take_new_agents();
        assert_eq!(agents.len(), 2, "{agents:?}");
        assert!(main.take_new_agents().is_empty());
        assert_eq!(
            main.tool_for_agent("adc9fb778705a136d"),
            Some("toolu_01WqrBFboSUXpcf1Bb8qoLzE")
        );
        // Only the user's prompt is a user message; the notification isn't.
        let prompts = t.top_level().filter(|i| i.kind == ItemKind::UserMessage).count();
        assert_eq!(prompts, 1);

        for (agent, tool) in &agents {
            let file = if agent == "adc9fb778705a136d" {
                SUB_FG
            } else {
                SUB_BG
            };
            let mut child = Observer::for_subagent(tool.clone());
            for e in child.feed(file.as_bytes()) {
                assert!(
                    !matches!(e, Event::SessionStarted { .. } | Event::StateChanged(_)),
                    "{e:?}"
                );
                t.apply(&e);
            }
        }
        for sub in t.subagents() {
            assert_eq!(sub.status, ItemStatus::Completed, "{}", sub.id);
            let ItemDetail::Subagent {
                agent, child, output, ..
            } = sub.detail(Harness::Claude)
            else {
                panic!()
            };
            assert_eq!(agent.as_deref(), Some("Explore"));
            assert!(child.is_some() && output.is_some(), "{sub:?}");
            let kinds: Vec<_> = t.children(&sub.id).map(|i| i.kind).collect();
            assert!(
                kinds.contains(&ItemKind::UserMessage) && kinds.contains(&ItemKind::AgentMessage),
                "{kinds:?}"
            );
        }
    }

    #[test]
    fn subagent_files_are_listed_with_their_meta() {
        let script = subagents_script("/h/.claude/projects/-p/s1.jsonl");
        assert!(
            script.contains("/h/.claude/projects/-p/s1/subagents/agent-*.jsonl"),
            "{script}"
        );
        assert!(subagents_script("/h/a b.jsonl").contains("'/h/a b/subagents'/agent-*.jsonl"));
        let out = format!(
            "/h/.claude/projects/-p/s1/subagents/agent-adc9fb778705a136d.jsonl\t8373\t{}\n/h/x/agent-b.jsonl\t5\t\n",
            SUB_FG_META.trim()
        );
        let files = parse_subagents(&out);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].agent_id, "adc9fb778705a136d");
        assert_eq!(files[0].size, 8373);
        assert_eq!(
            files[0].tool_use_id.as_deref(),
            Some("toolu_01WqrBFboSUXpcf1Bb8qoLzE")
        );
        assert_eq!(files[0].agent_type.as_deref(), Some("Explore"));
        assert_eq!(files[1].tool_use_id, None);
    }

    #[test]
    fn project_dir_matches_claude() {
        assert_eq!(project_dir_name("/home/v/my_proj.x"), "-home-v-my-proj-x");
        assert_eq!(
            transcript_path("/home/v/proj", "abc"),
            ".claude/projects/-home-v-proj/abc.jsonl"
        );
    }

    #[test]
    fn locate_script_quotes_id() {
        assert!(locate_script("a b").contains("'a b'.jsonl"));
    }

    #[test]
    fn recorded_transcript_ends_at_safe_point() {
        let mut o = Observer::new();
        let events = o.feed(TRANSCRIPT.as_bytes());
        assert!(matches!(events.first(), Some(Event::SessionStarted { .. })));
        assert!(o.at_safe_point());
        assert_eq!(o.pending_tools(), 0);
        let states: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::StateChanged(s) => Some(s.state.clone()),
                _ => None,
            })
            .collect();
        // hi -> done, then the denied Bash call: working -> done.
        assert_eq!(
            states,
            [
                RunState::Working { progress: None },
                RunState::Done,
                RunState::Working { progress: None },
                RunState::Done
            ]
        );
    }

    #[test]
    fn transcript_becomes_a_conversation() {
        let mut o = Observer::new();
        let mut t = crate::transcript::Transcript::new();
        for e in o.feed(TRANSCRIPT.as_bytes()) {
            t.apply(&e);
        }
        let kinds: Vec<_> = t.items().iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            [
                ItemKind::UserMessage,
                ItemKind::AgentMessage,
                ItemKind::UserMessage,
                ItemKind::CommandExecution,
                ItemKind::AgentMessage
            ]
        );
        assert_eq!(t.items()[1].text, "hi");
        let bash = &t.items()[3];
        assert_eq!(bash.status, crate::transcript::ItemStatus::Failed);
        assert!(matches!(
            bash.detail(Harness::Claude),
            crate::item::ItemDetail::Command { ref command, .. } if command.contains("acc-probe")
        ));
    }

    #[test]
    fn outstanding_tool_call_is_not_safe() {
        let mut o = Observer::new();
        let cut = TRANSCRIPT
            .lines()
            .take_while(|l| !l.contains("\"tool_result\""))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        o.feed(cut.as_bytes());
        assert_eq!(o.pending_tools(), 1);
        assert!(!o.at_safe_point());
    }

    #[test]
    fn local_commands_and_interrupts_end_no_turn_in_working() {
        let mut o = Observer::new();
        o.feed(TRANSCRIPT.as_bytes());
        assert_eq!(o.state(), Some(&RunState::Done));
        for line in [
            r#"{"type":"user","uuid":"c1","message":{"role":"user","content":"<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>"}}"#,
            r#"{"type":"user","uuid":"c2","message":{"role":"user","content":"<local-command-stdout>Set model to opus</local-command-stdout>"}}"#,
            r#"{"type":"user","uuid":"c3","message":{"role":"user","content":"<bash-input>ls</bash-input>"}}"#,
        ] {
            o.feed(format!("{line}\n").as_bytes());
        }
        assert_eq!(o.state(), Some(&RunState::Done));
        o.feed(b"{\"type\":\"user\",\"uuid\":\"p\",\"message\":{\"role\":\"user\",\"content\":\"go on\"}}\n");
        assert_eq!(o.state(), Some(&RunState::Working { progress: None }));
        o.feed(b"{\"type\":\"user\",\"uuid\":\"i\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"[Request interrupted by user]\"}]}}\n");
        assert_eq!(o.state(), Some(&RunState::Idle));
    }
}

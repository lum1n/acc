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

/// Turns transcript lines into transcript-sourced state and usage events.
#[derive(Debug)]
pub struct Observer {
    framer: LineFramer,
    session_id: Option<String>,
    state: Option<RunState>,
    /// Outstanding tool calls by id, with the tool's name.
    pending_tools: HashMap<String, String>,
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
        }
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
        matches!(self.state, Some(RunState::Done)) && self.pending_tools.is_empty()
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
        if str_at(v, "type") == Some("ai-title")
            && let Some(title) = owned(v, "aiTitle")
        {
            out.push(Event::Native {
                kind: "title".into(),
                payload: json!({ "title": title }),
            });
        }
        if self.session_id.is_none()
            && let Some(id) = owned(v, "sessionId")
        {
            self.session_id = Some(id.clone());
            out.push(Event::SessionStarted {
                harness: Harness::Claude,
                native_id: id,
                model: None,
            });
        }
        // Subagent conversations share the file but not the main turn's state.
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let Some(message) = v.get("message") else { return };
        match str_at(v, "type") {
            Some("user") => {
                let mut prompt = false;
                let mut text = String::new();
                match message.get("content") {
                    Some(Value::String(s)) => {
                        prompt = !local_input(s);
                        text.push_str(s);
                    }
                    Some(Value::Array(blocks)) => {
                        for b in blocks {
                            match str_at(b, "type") {
                                Some("tool_result") => {
                                    if let Some(id) = str_at(b, "tool_use_id") {
                                        self.pending_tools.remove(id);
                                        out.push(Event::ItemCompleted {
                                            id: id.to_owned(),
                                            extensions: json!({
                                                "is_error": b.get("is_error").cloned().unwrap_or(Value::Bool(false)),
                                                "content": b.get("content").cloned().unwrap_or(Value::Null),
                                            }),
                                        });
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
                    self.pending_tools.clear();
                    // Esc in the TUI: the turn is over, nothing follows.
                    let state = if text.starts_with("[Request interrupted by user") {
                        RunState::Idle
                    } else {
                        RunState::Working { progress: None }
                    };
                    self.set(state, out);
                    let id = owned(v, "uuid").unwrap_or_else(|| format!("user-{}", text.len()));
                    whole_item(out, id, ItemKind::UserMessage, text, Value::Null);
                }
            }
            Some("assistant") => {
                let uuid = owned(v, "uuid").unwrap_or_default();
                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for (i, b) in blocks.iter().enumerate() {
                        match str_at(b, "type") {
                            Some(kind @ ("tool_use" | "server_tool_use")) => {
                                let Some(id) = owned(b, "id") else { continue };
                                let name = str_at(b, "name").unwrap_or_default();
                                // Server tools run in the API; their result comes in
                                // the same response, never as a user tool_result.
                                if kind == "tool_use" {
                                    self.pending_tools.insert(id.clone(), name.to_owned());
                                }
                                out.push(Event::ItemStarted {
                                    id,
                                    kind: super::tool_kind(name),
                                    extensions: json!({
                                        "name": name,
                                        "input": b.get("input").cloned().unwrap_or(Value::Null),
                                    }),
                                });
                            }
                            Some("text") => whole_item(
                                out,
                                format!("{uuid}:{i}"),
                                ItemKind::AgentMessage,
                                str_at(b, "text").unwrap_or_default().to_owned(),
                                Value::Null,
                            ),
                            Some("thinking") => whole_item(
                                out,
                                format!("{uuid}:{i}"),
                                ItemKind::Reasoning,
                                str_at(b, "thinking").unwrap_or_default().to_owned(),
                                Value::Null,
                            ),
                            _ => {}
                        }
                    }
                }
                if let Some(usage) = message.get("usage") {
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
                let ended = matches!(
                    str_at(message, "stop_reason"),
                    Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal")
                );
                if ended && self.pending_tools.is_empty() {
                    self.set(RunState::Done, out);
                } else {
                    self.set(RunState::Working { progress: None }, out);
                }
            }
            _ => {}
        }
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
pub(crate) fn whole_item(out: &mut Vec<Event>, id: String, kind: ItemKind, text: String, ext: Value) {
    out.push(Event::ItemStarted {
        id: id.clone(),
        kind,
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

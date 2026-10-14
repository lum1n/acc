//! Observing Codex sessions ACC didn't start, from their rollout files.
//!
//! Every Codex front end (TUI, `exec`, IDE extension, app) records a thread
//! to `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`
//! (`CODEX_HOME` defaults to `~/.codex`). Tailing it over SSH gives state and
//! usage with nothing installed on the host, and tells ACC when the thread is
//! at a safe point to take over with `thread/resume`.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::claude::transcript::whole_item;
use crate::event::{Event, ItemKind, Quota, QuotaWindow, Usage};
use crate::harness::Harness;
use crate::ndjson::{Frame, LineFramer};
use crate::shell::quote;
use crate::state::{BlockKind, RunState, SessionState, StateSource};
use crate::wire::{self, owned, str_at, u64_at};

/// Script printing the rollout path of `thread_id`, or nothing.
pub fn locate_script(thread_id: &str) -> String {
    format!(
        "find \"${{CODEX_HOME:-$HOME/.codex}}\"/sessions -name {} -print 2>/dev/null | head -n 1\n",
        quote(&format!("rollout-*-{thread_id}.jsonl"))
    )
}

/// Script listing up to `limit` rollout files, most recently modified first.
pub fn recent_script(limit: usize) -> String {
    format!(
        "cd \"${{CODEX_HOME:-$HOME/.codex}}\"/sessions 2>/dev/null && ls -t -- */*/*/rollout-*.jsonl 2>/dev/null | head -n {limit}; true\n"
    )
}

/// The thread id embedded in a rollout file name (the trailing UUID).
pub fn thread_id_from_path(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?.strip_suffix(".jsonl")?;
    let start = name.len().checked_sub(36)?;
    let id = name.get(start..)?;
    (id.bytes().filter(|b| *b == b'-').count() == 4).then_some(id)
}

/// Turns rollout lines into transcript-sourced state, usage and quota events.
#[derive(Debug)]
pub struct Observer {
    framer: LineFramer,
    started: bool,
    state: Option<RunState>,
    /// The spawning item a child thread's rollout belongs to.
    parent: Option<String>,
    /// Child threads this rollout spawned: thread id → `spawn_agent` call id.
    children: HashMap<String, String>,
    child_order: Vec<String>,
    announced: usize,
    /// `spawn_agent` calls waiting for their output (which names the thread).
    spawning: HashMap<String, Value>,
    /// 0.162 agent paths (`/root/pong`) → `spawn_agent` call id.
    paths: HashMap<String, String>,
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
            started: false,
            state: None,
            parent: None,
            children: HashMap::new(),
            child_order: Vec::new(),
            announced: 0,
            spawning: HashMap::new(),
            paths: HashMap::new(),
        }
    }

    /// A reader for a child thread's rollout, whose items belong to the
    /// `spawn_agent` call `parent` in the parent's transcript.
    pub fn for_child(parent: impl Into<String>) -> Self {
        Self {
            parent: Some(parent.into()),
            ..Self::new()
        }
    }

    /// Child threads spawned since the last call: `(thread id, call id)`.
    /// Locate their rollouts with [`locate_script`].
    pub fn take_new_children(&mut self) -> Vec<(String, String)> {
        let new = self.child_order[self.announced..]
            .iter()
            .filter_map(|t| Some((t.clone(), self.children.get(t)?.clone())))
            .collect();
        self.announced = self.child_order.len();
        new
    }

    pub fn state(&self) -> Option<&RunState> {
        self.state.as_ref()
    }

    /// True between turns: closing the TUI and resuming loses nothing.
    pub fn at_safe_point(&self) -> bool {
        matches!(
            self.state,
            Some(RunState::Done | RunState::Idle | RunState::Error)
        )
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
        let p = v.get("payload").cloned().unwrap_or(Value::Null);
        match str_at(v, "type") {
            Some("response_item") => self.item(&p, v, out),
            Some("session_meta") if !self.started && self.parent.is_none() => {
                if let Some(id) = owned(&p, "id") {
                    self.started = true;
                    out.push(Event::SessionStarted {
                        harness: Harness::Codex,
                        native_id: id,
                        model: None,
                    });
                }
            }
            Some("event_msg") => match str_at(&p, "type") {
                Some("task_started" | "user_message" | "exec_command_begin" | "patch_apply_begin") => {
                    self.set(RunState::Working { progress: None }, out);
                }
                Some("task_complete") => self.set(RunState::Done, out),
                Some("turn_aborted") => self.set(RunState::Idle, out),
                Some("error") => self.set(RunState::Error, out),
                Some("exec_approval_request" | "apply_patch_approval_request" | "request_permissions") => {
                    self.set(
                        RunState::Blocked {
                            kind: BlockKind::Permission,
                        },
                        out,
                    );
                }
                Some("request_user_input" | "elicitation_request") => {
                    self.set(
                        RunState::Blocked {
                            kind: BlockKind::Question,
                        },
                        out,
                    );
                }
                Some("token_count") if self.parent.is_none() => self.token_count(&p, out),
                Some("item_completed") => self.activity(&p, out),
                _ => {}
            },
            _ => {}
        }
    }

    /// Conversation items, shaped like the app-server's so the same
    /// item rendering applies to observed and driven sessions.
    fn item(&mut self, p: &Value, line: &Value, out: &mut Vec<Event>) {
        let fallback = || format!("ord-{}", u64_at(line, "ordinal").unwrap_or(0));
        let id = owned(p, "id").unwrap_or_else(fallback);
        let text_of = |key: &str| {
            p.get(key)
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| str_at(b, "text"))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        };
        match str_at(p, "type") {
            Some("message") => {
                let text = text_of("content");
                let kind = match str_at(p, "role") {
                    Some("user") => ItemKind::UserMessage,
                    Some("assistant") => ItemKind::AgentMessage,
                    _ => return,
                };
                // Context Codex injects as user messages isn't the user's.
                let injected = text.trim_start().starts_with('<') || text.starts_with("# AGENTS.md");
                if !text.is_empty() && !(kind == ItemKind::UserMessage && injected) {
                    whole_item(out, id, kind, self.parent.clone(), text, Value::Null);
                }
            }
            // 0.162 messages between agents; a child's final answer is
            // the report of the call that spawned it.
            Some("agent_message") => {
                let text = text_of("content");
                let field = |name: &str| text.lines().find_map(|l| l.strip_prefix(name)).map(str::trim);
                if field("Message Type:") == Some("FINAL_ANSWER")
                    && let Some(call) = field("Sender:").and_then(|s| self.paths.get(s))
                    && let Some((_, report)) = text.split_once("Payload:\n")
                {
                    out.push(Event::ItemUpdated {
                        id: call.clone(),
                        extensions: json!({"report": report}),
                    });
                }
            }
            Some("reasoning") => {
                let text = text_of("summary");
                if !text.is_empty() {
                    whole_item(
                        out,
                        id,
                        ItemKind::Reasoning,
                        self.parent.clone(),
                        text,
                        Value::Null,
                    );
                }
            }
            Some("function_call" | "custom_tool_call" | "local_shell_call") => {
                let Some(call) = owned(p, "call_id") else { return };
                let name = str_at(p, "name").unwrap_or("shell");
                let args: Value = str_at(p, "arguments")
                    .and_then(|a| serde_json::from_str(a).ok())
                    .unwrap_or(Value::Null);
                let (kind, ext) = if str_at(p, "type") == Some("local_shell_call") {
                    let action = p.get("action").cloned().unwrap_or(Value::Null);
                    (
                        ItemKind::CommandExecution,
                        json!({"type": "commandExecution", "command": action.get("command"),
                               "cwd": action.get("working_directory")}),
                    )
                } else if matches!(
                    name,
                    "shell" | "exec_command" | "shell_command" | "container.exec"
                ) {
                    let command = args.get("command").or_else(|| args.get("cmd")).cloned();
                    (
                        ItemKind::CommandExecution,
                        json!({"type": "commandExecution", "command": command, "cwd": args.get("workdir")}),
                    )
                } else if let Some(tool) = collab_tool(name) {
                    if tool == "spawnAgent" {
                        self.spawning.insert(call.clone(), args.clone());
                    }
                    let target = args
                        .get("id")
                        .or_else(|| args.get("agent_id"))
                        .or_else(|| args.get("target"))
                        .and_then(Value::as_str)
                        .map(|t| vec![Value::String(t.to_owned())])
                        .unwrap_or_default();
                    (
                        ItemKind::Subagent,
                        json!({"type": "collabAgentToolCall", "tool": tool,
                               "prompt": args.get("message").or_else(|| args.get("prompt")).or_else(|| args.get("task"))
                                   .filter(|m| !m.as_str().is_some_and(encrypted)),
                               "agent": args.get("agent_type").or_else(|| args.get("nickname")).or_else(|| args.get("task_name")),
                               "model": args.get("model"),
                               "receiverThreadIds": target}),
                    )
                } else if name == "apply_patch" {
                    let patch = str_at(p, "input")
                        .or_else(|| str_at(&args, "input"))
                        .unwrap_or_default();
                    (
                        ItemKind::FileChange,
                        json!({"type": "fileChange", "changes": crate::item::apply_patch_changes(patch)}),
                    )
                } else {
                    let arguments = if args.is_null() {
                        p.get("input").cloned()
                    } else {
                        Some(args)
                    };
                    (
                        ItemKind::ToolCall,
                        json!({"type": "dynamicToolCall", "tool": name, "arguments": arguments}),
                    )
                };
                out.push(Event::ItemStarted {
                    id: call,
                    kind,
                    parent: None,
                    extensions: ext,
                });
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                let Some(call) = owned(p, "call_id") else { return };
                let raw = p.get("output").cloned().unwrap_or(Value::Null);
                // Shell outputs are JSON text with the output and exit code.
                let parsed: Value = raw
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(Value::Null);
                let output = owned(&parsed, "output")
                    .or_else(|| raw.as_str().map(str::to_owned))
                    .unwrap_or_default();
                let exit = parsed
                    .pointer("/metadata/exit_code")
                    .cloned()
                    .unwrap_or(Value::Null);
                let failed = exit.as_i64().is_some_and(|c| c != 0);
                if let Some(_args) = self.spawning.remove(&call) {
                    // `spawn_agent` answers with the new agent's thread id.
                    let thread = owned(&parsed, "agent_id")
                        .or_else(|| owned(&parsed, "id"))
                        .or_else(|| owned(&parsed, "thread_id"))
                        .or_else(|| thread_id_in(&output));
                    if let Some(t) = &thread
                        && !self.children.contains_key(t)
                    {
                        self.children.insert(t.clone(), call.clone());
                        self.child_order.push(t.clone());
                    }
                    // 0.162 answers with the task name only; its
                    // `SubAgentActivity` already named the thread.
                    let mut ext = json!({"status": "completed"});
                    if let Some(t) = thread {
                        ext["receiverThreadIds"] = json!([t]);
                    }
                    out.push(Event::ItemCompleted {
                        id: call,
                        extensions: ext,
                    });
                    return;
                }
                out.push(Event::ItemCompleted {
                    id: call,
                    extensions: json!({
                        "aggregatedOutput": output,
                        "contentItems": output,
                        "exitCode": exit,
                        "status": if failed { "failed" } else { "completed" },
                    }),
                });
            }
            _ => {}
        }
    }

    /// A 0.162 `SubAgentActivity`: `started` names the spawn's thread,
    /// anything else says that subagent stopped.
    fn activity(&mut self, p: &Value, out: &mut Vec<Event>) {
        let item = p.get("item").cloned().unwrap_or(Value::Null);
        if str_at(&item, "type") != Some("SubAgentActivity") {
            return;
        }
        let Some(thread) = owned(&item, "agent_thread_id") else {
            return;
        };
        match (str_at(&item, "kind"), owned(&item, "id")) {
            (Some("started"), Some(call)) => {
                if let Some(path) = owned(&item, "agent_path") {
                    self.paths.insert(path, call.clone());
                }
                if !self.children.contains_key(&thread) {
                    self.children.insert(thread.clone(), call.clone());
                    self.child_order.push(thread.clone());
                }
                out.push(Event::ItemUpdated {
                    id: call,
                    extensions: json!({"receiverThreadIds": [thread], "model": item.get("model"),
                                       "childStatus": "running"}),
                });
            }
            (kind, _) => {
                if let Some(call) = self.children.get(&thread) {
                    out.push(Event::ItemUpdated {
                        id: call.clone(),
                        extensions: json!({"childStatus": kind.unwrap_or("completed")}),
                    });
                }
            }
        }
    }

    fn token_count(&mut self, p: &Value, out: &mut Vec<Event>) {
        if let Some(info) = p.get("info").filter(|i| !i.is_null()) {
            let total = info.get("total_token_usage").cloned().unwrap_or(Value::Null);
            out.push(Event::Usage(Usage {
                input_tokens: u64_at(&total, "input_tokens"),
                output_tokens: u64_at(&total, "output_tokens"),
                cache_read_tokens: u64_at(&total, "cached_input_tokens"),
                cache_write_tokens: None,
                cost_usd: None,
                context_used: info
                    .pointer("/last_token_usage/total_tokens")
                    .and_then(Value::as_u64),
                context_window: u64_at(info, "model_context_window"),
            }));
        }
        if let Some(limits) = p.get("rate_limits").filter(|r| !r.is_null()) {
            let windows = ["primary", "secondary"]
                .iter()
                .filter_map(|id| {
                    let w = limits.get(*id).filter(|w| !w.is_null())?;
                    Some(QuotaWindow {
                        id: (*id).to_owned(),
                        used_percent: w.get("used_percent").and_then(Value::as_f64),
                        window_minutes: u64_at(w, "window_minutes"),
                        resets_at: u64_at(w, "resets_at"),
                        status: None,
                    })
                })
                .collect();
            out.push(Event::Quota(Quota {
                account: None,
                plan: owned(limits, "plan_type"),
                windows,
            }));
        }
    }

    fn set(&mut self, state: RunState, out: &mut Vec<Event>) {
        if let Some(parent) = &self.parent {
            // A child thread's turn ending is its parent call's report.
            if matches!(state, RunState::Done | RunState::Error) {
                out.push(Event::ItemUpdated {
                    id: parent.clone(),
                    extensions: json!({"childStatus": if state == RunState::Done { "completed" } else { "errored" }}),
                });
            }
            self.state = Some(state);
            return;
        }
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

/// The app-server name of a collab tool the model calls by its function name.
fn collab_tool(name: &str) -> Option<&'static str> {
    Some(match name {
        "spawn_agent" => "spawnAgent",
        "send_input" | "send_message" | "assign_agent_task" => "sendInput",
        "wait_agent" | "wait" => "wait",
        "close_agent" => "closeAgent",
        "resume_agent" => "resumeAgent",
        _ => return None,
    })
}

/// Codex 0.162 stores a spawned agent's task as a Fernet token, not text.
fn encrypted(text: &str) -> bool {
    text.starts_with("gAAAAA") && !text.contains(char::is_whitespace)
}

/// The first UUID-shaped word in a tool output (a thread id).
fn thread_id_in(text: &str) -> Option<String> {
    text.split(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
        .find(|w| w.len() == 36 && w.bytes().filter(|b| *b == b'-').count() == 4)
        .map(str::to_owned)
}

/// The thread a child rollout's `session_meta` says spawned it.
pub fn parent_thread(session_meta: &Value) -> Option<&str> {
    session_meta
        .pointer("/payload/source/subagent/thread_spawn/parent_thread_id")
        .and_then(Value::as_str)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn rollout_items_render_like_app_server_items() {
        let lines = [
            json!({"type": "response_item", "ordinal": 1, "payload": {"type": "message", "role": "user", "id": "u1",
                "content": [{"type": "input_text", "text": "<environment_context>x</environment_context>"}]}}),
            json!({"type": "response_item", "ordinal": 2, "payload": {"type": "message", "role": "user", "id": "u2",
                "content": [{"type": "input_text", "text": "run tests"}]}}),
            json!({"type": "response_item", "ordinal": 3, "payload": {"type": "function_call", "name": "shell",
                "call_id": "c1", "arguments": "{\"command\":[\"cargo\",\"test\"],\"workdir\":\"/w\"}"}}),
            json!({"type": "response_item", "ordinal": 4, "payload": {"type": "function_call_output", "call_id": "c1",
                "output": "{\"output\":\"ok\",\"metadata\":{\"exit_code\":101}}"}}),
            json!({"type": "response_item", "ordinal": 5, "payload": {"type": "message", "role": "assistant", "id": "a1",
                "content": [{"type": "output_text", "text": "tests fail"}]}}),
        ];
        let mut bytes = Vec::new();
        for l in &lines {
            bytes.extend(serde_json::to_vec(l).unwrap());
            bytes.push(b'\n');
        }
        let mut o = Observer::new();
        let mut t = crate::transcript::Transcript::new();
        for e in o.feed(&bytes) {
            t.apply(&e);
        }
        let kinds: Vec<_> = t.items().iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            [
                ItemKind::UserMessage,
                ItemKind::CommandExecution,
                ItemKind::AgentMessage
            ]
        );
        let cmd = t.get("c1").unwrap();
        assert_eq!(cmd.status, crate::transcript::ItemStatus::Failed);
        assert_eq!(
            cmd.detail(Harness::Codex),
            crate::item::ItemDetail::Command {
                command: "cargo test".into(),
                cwd: Some("/w".into()),
                output: "ok".into(),
                exit_code: Some(101)
            }
        );
    }

    const ROLLOUT: &str = include_str!("../../tests/fixtures/codex/rollout.jsonl");

    #[test]
    fn recorded_rollout_interrupted_turn_is_safe() {
        let mut o = Observer::new();
        let events = o.feed(ROLLOUT.as_bytes());
        assert!(matches!(&events[0],
            Event::SessionStarted { native_id, .. } if native_id == "01a125bb-467b-7971-9643-70534831e6ee"));
        assert_eq!(o.state(), Some(&RunState::Idle));
        assert!(o.at_safe_point());
    }

    #[test]
    fn running_turn_is_not_safe() {
        let mut o = Observer::new();
        o.feed(
            br#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t"}}
"#,
        );
        assert!(!o.at_safe_point());
        o.feed(
            br#"{"type":"event_msg","payload":{"type":"exec_approval_request","call_id":"c"}}
"#,
        );
        assert_eq!(
            o.state(),
            Some(&RunState::Blocked {
                kind: BlockKind::Permission
            })
        );
        o.feed(
            br#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"ok"}}
"#,
        );
        assert!(o.at_safe_point());
    }

    #[test]
    fn token_count_maps_usage_and_quota() {
        let mut o = Observer::new();
        let events = o.feed(br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":7,"reasoning_output_tokens":0,"total_tokens":107},"last_token_usage":{"input_tokens":50,"cached_input_tokens":0,"output_tokens":3,"reasoning_output_tokens":0,"total_tokens":53},"model_context_window":258400},"rate_limits":{"primary":{"used_percent":12.5,"window_minutes":300,"resets_at":1791652200},"secondary":null,"plan_type":"pro"}}}
"#);
        assert_eq!(
            events[0],
            Event::Usage(Usage {
                input_tokens: Some(100),
                output_tokens: Some(7),
                cache_read_tokens: Some(40),
                cache_write_tokens: None,
                cost_usd: None,
                context_used: Some(53),
                context_window: Some(258_400),
            })
        );
        let Event::Quota(q) = &events[1] else {
            unreachable!("quota")
        };
        assert_eq!(q.plan.as_deref(), Some("pro"));
        assert_eq!(q.windows.len(), 1);
        assert_eq!(q.windows[0].used_percent, Some(12.5));
    }

    /// Shaped from the 0.128 binary's tool names; not recorded.
    #[test]
    fn spawn_agent_is_a_subagent_and_names_its_thread() {
        let child = "019a0000-0000-7000-8000-00000000c41d";
        let lines = [
            json!({"type": "response_item", "payload": {"type": "function_call", "name": "spawn_agent",
                "call_id": "c9", "arguments": "{\"message\":\"Count lines\",\"agent_type\":\"explorer\"}"}}),
            json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "c9",
                "output": format!("{{\"agent_id\":\"{child}\",\"nickname\":\"Lovelace\"}}")}}),
        ];
        let mut o = Observer::new();
        let mut t = crate::transcript::Transcript::new();
        for l in &lines {
            for e in o.feed(format!("{l}\n").as_bytes()) {
                t.apply(&e);
            }
        }
        let item = t.get("c9").unwrap();
        assert_eq!(item.kind, ItemKind::Subagent);
        let crate::item::ItemDetail::Subagent {
            agent,
            child: c,
            prompt,
            ..
        } = item.detail(Harness::Codex)
        else {
            panic!()
        };
        assert_eq!(
            (agent.as_deref(), prompt.as_deref()),
            (Some("explorer"), Some("Count lines"))
        );
        assert_eq!(c.as_deref(), Some(child));
        assert_eq!(o.take_new_children(), [(child.to_owned(), "c9".to_owned())]);
        assert!(o.take_new_children().is_empty());

        // The child's rollout: its items hang off the spawn call.
        let mut c = Observer::for_child("c9");
        let child_lines = [
            json!({"type": "session_meta", "payload": {"id": child, "source": {"subagent": {"thread_spawn": {"parent_thread_id": "p", "depth": 1}}}}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "id": "m1",
                "content": [{"type": "output_text", "text": "3 lines"}]}}),
            json!({"type": "event_msg", "payload": {"type": "task_complete"}}),
        ];
        assert_eq!(parent_thread(&child_lines[0]), Some("p"));
        let mut events = Vec::new();
        for l in &child_lines {
            events.extend(c.feed(format!("{l}\n").as_bytes()));
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::SessionStarted { .. } | Event::StateChanged(_)))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ItemStarted { parent: Some(p), .. } if p == "c9"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ItemUpdated { id, .. } if id == "c9"))
        );
    }

    const PARENT_0162: &str =
        include_str!("../../tests/fixtures/codex/subagent-0.162.1-parent.rollout.jsonl");
    const CHILD_0162: &str = include_str!("../../tests/fixtures/codex/subagent-0.162.1-child.rollout.jsonl");

    /// Recorded 0.162.1 rollouts: the spawn's task is encrypted, the thread
    /// is named by a `SubAgentActivity`, and the child's `FINAL_ANSWER`
    /// reaches the parent as an inter-agent message.
    #[test]
    fn recorded_0162_spawn_names_its_thread_and_report() {
        let child = "01a13c40-d0f6-7b91-8fb1-48b77d83bdfb";
        let spawn = "call_732b7e56cb3a4dba89c9ef3b2bb1e7b9";
        let mut o = Observer::new();
        let mut t = crate::transcript::Transcript::new();
        for e in o.feed(PARENT_0162.as_bytes()) {
            t.apply(&e);
        }
        let crate::item::ItemDetail::Subagent {
            agent,
            child: c,
            prompt,
            status,
            output,
            model,
            ..
        } = t.get(spawn).unwrap().detail(Harness::Codex)
        else {
            panic!()
        };
        assert_eq!((agent.as_deref(), prompt), (Some("pong"), None));
        assert_eq!(c.as_deref(), Some(child));
        assert_eq!(status.as_deref(), Some("completed"));
        assert_eq!(output.as_deref(), Some("pong"));
        assert_eq!(model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(o.take_new_children(), [(child.to_owned(), spawn.to_owned())]);

        let mut c = Observer::for_child(spawn);
        for e in c.feed(CHILD_0162.as_bytes()) {
            t.apply(&e);
        }
        let replies: Vec<_> = t.children(spawn).map(|i| i.text.as_str()).collect();
        assert_eq!(replies, ["pong"]);
    }

    #[test]
    fn paths_and_scripts() {
        assert_eq!(
            thread_id_from_path(
                "2026/10/10/rollout-2026-10-10T12-13-12-01a125bb-467b-7971-9643-70534831e6ee.jsonl"
            ),
            Some("01a125bb-467b-7971-9643-70534831e6ee")
        );
        assert_eq!(thread_id_from_path("notes.jsonl"), None);
        assert!(locate_script("x'y").contains(r"'rollout-*-x'\''y.jsonl'"));
        assert!(recent_script(5).contains("head -n 5"));
    }
}

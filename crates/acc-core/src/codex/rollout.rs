//! Observing Codex sessions ACC didn't start, from their rollout files.
//!
//! Every Codex front end (TUI, `exec`, IDE extension, app) records a thread
//! to `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`
//! (`CODEX_HOME` defaults to `~/.codex`). Tailing it over SSH gives state and
//! usage with nothing installed on the host, and tells ACC when the thread is
//! at a safe point to take over with `thread/resume`.

use serde_json::Value;

use crate::event::{Event, Quota, QuotaWindow, Usage};
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
        }
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
            Some("session_meta") if !self.started => {
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
                Some("token_count") => self.token_count(&p, out),
                _ => {}
            },
            _ => {}
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

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

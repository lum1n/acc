//! Taking over a harness ACC didn't start, and handing it back.
//!
//! [`Takeover`] is a state machine a client steps with what it observes:
//! wait until the TUI is at a safe point (or interrupt it when forced),
//! close it the way a user would, make sure the process is gone, then
//! resume the same conversation headless as a detached ACC session. The
//! client runs each [`Exec`] it is handed and reports back; nothing here
//! does I/O or keeps time.

use serde::{Deserialize, Serialize};

use crate::command::Command;
use crate::discover::ForeignAgent;
use crate::harness::Harness;
use crate::multiplexer::{self, Exec, Multiplexer, PaneRef, tui};
use crate::session::{LaunchRequest, Origin};
use crate::shell::quote;

/// How long to wait for a busy TUI to reach a safe point.
const SAFE_POINT_TIMEOUT_MS: u64 = 120_000;
/// How long a closed TUI gets to exit before it is terminated.
const EXIT_TIMEOUT_MS: u64 = 8_000;
const POLL_MS: u64 = 500;

/// What the client observed since the last step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TakeoverInput {
    /// Time passed; `safe` is [`crate::observe::Observed::at_safe_point`].
    Tick { now_ms: u64, safe: bool },
    /// The last exec finished with this stdout.
    Ran { now_ms: u64, stdout: String },
}

/// What the client should do next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TakeoverAction {
    /// Run this on the host, then report `Ran`.
    Run {
        exec: Exec,
        step: TakeoverStep,
    },
    /// Report a `Tick` after about this long.
    Wait {
        ms: u64,
        step: TakeoverStep,
    },
    /// The TUI is gone: start this session (with [`crate::session::Session::launch`]).
    Launch {
        request: LaunchRequest,
    },
    Failed {
        reason: String,
    },
}

/// Where a takeover is, for progress displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeoverStep {
    WaitingForSafePoint,
    Interrupting,
    ClosingTui,
    WaitingForExit,
    Terminating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Start,
    WaitSafe { since: u64 },
    Interrupted { at: u64 },
    Closed { at: u64 },
    Probing { since: u64 },
    Terminated,
    Done,
}

/// Steps one takeover. See the module docs.
#[derive(Debug, Clone)]
pub struct Takeover {
    agent: ForeignAgent,
    native_id: String,
    cwd: String,
    force: bool,
    phase: Phase,
}

/// Why a foreign agent can't be taken over.
pub fn takeover_blocker(agent: &ForeignAgent, native_id: Option<&str>) -> Option<String> {
    if !matches!(agent.harness, Harness::Claude | Harness::Codex | Harness::Pi) {
        return Some(format!(
            "{} can't resume a TUI session headless yet",
            agent.harness.display_name()
        ));
    }
    if native_id.is_none() {
        return Some("its session id isn't known yet".into());
    }
    if agent.cwd.as_deref().is_none_or(str::is_empty) {
        return Some("its working directory isn't known".into());
    }
    None
}

fn join_execs(execs: Vec<Exec>) -> Exec {
    let mut script = String::new();
    let mut stdin = Vec::new();
    for e in execs {
        if !e.stdin.is_empty() {
            // Only pastes carry stdin; one per step at most.
            stdin = e.stdin;
        }
        script.push_str(e.script.trim_end());
        script.push('\n');
        script.push_str("sleep 0.2\n");
    }
    Exec::with_stdin(script, stdin)
}

impl Takeover {
    /// `native_id` is the harness session to resume (from discovery or the
    /// transcript). With `force`, a busy TUI is interrupted instead of
    /// waited for.
    pub fn new(agent: ForeignAgent, native_id: Option<String>, force: bool) -> Result<Self, String> {
        if let Some(reason) = takeover_blocker(&agent, native_id.as_deref()) {
            return Err(reason);
        }
        Ok(Self {
            native_id: native_id.unwrap_or_default(),
            cwd: agent.cwd.clone().unwrap_or_default(),
            agent,
            force,
            phase: Phase::Start,
        })
    }

    fn pane_inputs(&self, inputs: Vec<multiplexer::Input>) -> Option<Exec> {
        let pane = self.agent.pane.as_ref()?;
        let backend = multiplexer::backend(pane.multiplexer);
        Some(join_execs(inputs.iter().map(|i| backend.send(pane, i)).collect()))
    }

    fn close_exec(&self) -> Exec {
        tui::exit_inputs(self.agent.harness)
            .ok()
            .and_then(|i| self.pane_inputs(i))
            .unwrap_or_else(|| Exec::new(format!("kill -TERM {} 2>/dev/null; true\n", self.agent.pid)))
    }

    fn probe(&self) -> Exec {
        Exec::new(crate::observe::alive_script(self.agent.pid))
    }

    fn launch(&mut self) -> TakeoverAction {
        self.phase = Phase::Done;
        let pane = self.agent.pane.as_ref().map(pane_label);
        TakeoverAction::Launch {
            request: LaunchRequest {
                harness: Some(self.agent.harness),
                cwd: self.cwd.clone(),
                model: None,
                permission: None,
                resume: Some(self.native_id.clone()),
                fork: false,
                title: None,
                prompt: None,
                parent: None,
                replaces_parent: false,
                origin: Origin::TakenOver {
                    pane,
                    transcript: self.agent.transcript.as_ref().map(|t| t.path.clone()),
                },
            },
        }
    }

    pub fn next(&mut self, input: TakeoverInput) -> TakeoverAction {
        let (now, safe, stdout) = match &input {
            TakeoverInput::Tick { now_ms, safe } => (*now_ms, *safe, None),
            TakeoverInput::Ran { now_ms, stdout } => (*now_ms, false, Some(stdout.as_str())),
        };
        match self.phase {
            Phase::Start | Phase::WaitSafe { .. } if safe => {
                self.phase = Phase::Closed { at: now };
                TakeoverAction::Run {
                    exec: self.close_exec(),
                    step: TakeoverStep::ClosingTui,
                }
            }
            Phase::Start if self.force => {
                let interrupt = tui::inputs(self.agent.harness, &Command::Interrupt)
                    .ok()
                    .and_then(|i| self.pane_inputs(i));
                match interrupt {
                    Some(exec) => {
                        self.phase = Phase::Interrupted { at: now };
                        TakeoverAction::Run {
                            exec,
                            step: TakeoverStep::Interrupting,
                        }
                    }
                    // No pane to type into: closing ends the turn too.
                    None => {
                        self.phase = Phase::Closed { at: now };
                        TakeoverAction::Run {
                            exec: self.close_exec(),
                            step: TakeoverStep::ClosingTui,
                        }
                    }
                }
            }
            Phase::Start => {
                self.phase = Phase::WaitSafe { since: now };
                TakeoverAction::Wait {
                    ms: POLL_MS,
                    step: TakeoverStep::WaitingForSafePoint,
                }
            }
            Phase::WaitSafe { since } => {
                if now.saturating_sub(since) > SAFE_POINT_TIMEOUT_MS {
                    TakeoverAction::Failed {
                        reason: "the agent stayed busy; try again, or force it".into(),
                    }
                } else {
                    TakeoverAction::Wait {
                        ms: POLL_MS,
                        step: TakeoverStep::WaitingForSafePoint,
                    }
                }
            }
            Phase::Interrupted { at } => {
                if stdout.is_some() || now.saturating_sub(at) < 1_500 {
                    // Give the TUI a moment to settle after Escape.
                    TakeoverAction::Wait {
                        ms: 1_500,
                        step: TakeoverStep::Interrupting,
                    }
                } else {
                    self.phase = Phase::Closed { at: now };
                    TakeoverAction::Run {
                        exec: self.close_exec(),
                        step: TakeoverStep::ClosingTui,
                    }
                }
            }
            Phase::Closed { at } => {
                self.phase = Phase::Probing { since: at };
                TakeoverAction::Run {
                    exec: self.probe(),
                    step: TakeoverStep::WaitingForExit,
                }
            }
            Phase::Probing { since } => match stdout.map(str::trim) {
                Some("gone") => self.launch(),
                Some(_) if now.saturating_sub(since) > EXIT_TIMEOUT_MS => {
                    self.phase = Phase::Terminated;
                    let pid = self.agent.pid;
                    TakeoverAction::Run {
                        exec: Exec::new(format!(
                            "kill -TERM {pid} 2>/dev/null; sleep 1; kill -KILL {pid} 2>/dev/null; true\n"
                        )),
                        step: TakeoverStep::Terminating,
                    }
                }
                Some(_) => TakeoverAction::Wait {
                    ms: POLL_MS,
                    step: TakeoverStep::WaitingForExit,
                },
                None => TakeoverAction::Run {
                    exec: self.probe(),
                    step: TakeoverStep::WaitingForExit,
                },
            },
            Phase::Terminated => self.launch(),
            Phase::Done => TakeoverAction::Failed {
                reason: "takeover already finished".into(),
            },
        }
    }
}

/// `tmux:%3` style label for a pane, stored in the session's origin.
pub fn pane_label(p: &PaneRef) -> String {
    let kind = match p.multiplexer {
        Multiplexer::Tmux => "tmux",
        Multiplexer::Zellij => "zellij",
        Multiplexer::Screen => "screen",
        Multiplexer::Rex => "rex",
    };
    if p.server.is_empty() {
        format!("{kind}:{}", p.pane)
    } else {
        format!("{kind}:{}@{}", p.pane, p.server)
    }
}

fn parse_pane_label(label: &str) -> Option<PaneRef> {
    let (kind, rest) = label.split_once(':')?;
    let multiplexer = match kind {
        "tmux" => Multiplexer::Tmux,
        "zellij" => Multiplexer::Zellij,
        "screen" => Multiplexer::Screen,
        "rex" => Multiplexer::Rex,
        _ => return None,
    };
    let (pane, server) = rest.split_once('@').unwrap_or((rest, ""));
    Some(PaneRef {
        multiplexer,
        server: server.to_owned(),
        session: None,
        pane: pane.to_owned(),
    })
}

/// Script handing a session back to its TUI in the pane it was taken from:
/// types the resume command into the shell left there. Stop the headless
/// session first (its `kill_exec`). `None` when there's no pane to go back to.
pub fn hand_back_exec(harness: Harness, native_id: &str, pane_label: &str, cwd: &str) -> Option<Exec> {
    let pane = parse_pane_label(pane_label)?;
    let backend = multiplexer::backend(pane.multiplexer);
    let mut inputs = vec![
        multiplexer::Input::Text(format!("cd {}", quote(cwd))),
        multiplexer::Input::Keys(vec![multiplexer::Key::Enter]),
    ];
    inputs.extend(tui::resume_inputs(harness, native_id).ok()?);
    Some(join_execs(
        inputs.iter().map(|i| backend.send(&pane, i)).collect(),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::discover::TranscriptRef;

    fn agent(pane: bool) -> ForeignAgent {
        ForeignAgent {
            harness: Harness::Claude,
            pid: 4242,
            args: "claude".into(),
            cwd: Some("/w".into()),
            elapsed_secs: None,
            pane: pane.then(|| PaneRef {
                multiplexer: Multiplexer::Tmux,
                server: String::new(),
                session: None,
                pane: "%3".into(),
            }),
            pane_label: None,
            transcript: Some(TranscriptRef {
                path: "/t.jsonl".into(),
                size: 0,
                exact: true,
            }),
            native_id: Some("sid".into()),
        }
    }

    fn ran(now: u64, out: &str) -> TakeoverInput {
        TakeoverInput::Ran {
            now_ms: now,
            stdout: out.into(),
        }
    }

    #[test]
    fn waits_for_safe_point_then_closes_and_resumes() {
        let mut t = Takeover::new(agent(true), Some("sid".into()), false).unwrap();
        assert!(matches!(
            t.next(TakeoverInput::Tick {
                now_ms: 0,
                safe: false
            }),
            TakeoverAction::Wait {
                step: TakeoverStep::WaitingForSafePoint,
                ..
            }
        ));
        let TakeoverAction::Run { exec, step } = t.next(TakeoverInput::Tick {
            now_ms: 500,
            safe: true,
        }) else {
            panic!("close")
        };
        assert_eq!(step, TakeoverStep::ClosingTui);
        assert!(exec.script.contains("send-keys") && exec.script.contains("/exit"));
        assert!(matches!(
            t.next(ran(600, "")),
            TakeoverAction::Run {
                step: TakeoverStep::WaitingForExit,
                ..
            }
        ));
        assert!(matches!(t.next(ran(700, "alive\n")), TakeoverAction::Wait { .. }));
        let TakeoverAction::Launch { request } = t.next(ran(1_200, "gone\n")) else {
            panic!("launch")
        };
        assert_eq!(request.resume.as_deref(), Some("sid"));
        assert_eq!(
            request.origin,
            Origin::TakenOver {
                pane: Some("tmux:%3".into()),
                transcript: Some("/t.jsonl".into())
            }
        );
    }

    #[test]
    fn forced_takeover_interrupts_and_stubborn_process_is_killed() {
        let mut t = Takeover::new(agent(true), Some("sid".into()), true).unwrap();
        let TakeoverAction::Run { exec, step } = t.next(TakeoverInput::Tick {
            now_ms: 0,
            safe: false,
        }) else {
            panic!("interrupt")
        };
        assert_eq!(step, TakeoverStep::Interrupting);
        assert!(exec.script.contains("Escape"));
        assert!(matches!(t.next(ran(10, "")), TakeoverAction::Wait { .. }));
        assert!(matches!(
            t.next(TakeoverInput::Tick {
                now_ms: 2_000,
                safe: false
            }),
            TakeoverAction::Run {
                step: TakeoverStep::ClosingTui,
                ..
            }
        ));
        t.next(ran(2_100, ""));
        assert!(matches!(
            t.next(ran(20_000, "alive")),
            TakeoverAction::Run {
                step: TakeoverStep::Terminating,
                ..
            }
        ));
        assert!(matches!(t.next(ran(21_000, "")), TakeoverAction::Launch { .. }));
    }

    #[test]
    fn no_pane_closes_by_signal_and_blockers_explain() {
        let mut t = Takeover::new(agent(false), Some("sid".into()), false).unwrap();
        let TakeoverAction::Run { exec, .. } = t.next(TakeoverInput::Tick {
            now_ms: 0,
            safe: true,
        }) else {
            panic!("close")
        };
        assert!(exec.script.starts_with("kill -TERM 4242"));
        let mut cursor = agent(true);
        cursor.harness = Harness::Cursor;
        assert!(Takeover::new(cursor, Some("x".into()), false).is_err());
        assert!(Takeover::new(agent(true), None, false).is_err());
    }

    #[test]
    fn hand_back_types_resume_into_the_pane() {
        let exec = hand_back_exec(Harness::Claude, "sid", "tmux:%3", "/w").unwrap();
        assert!(exec.script.contains("send-keys") && exec.script.contains("claude --resume sid"));
        assert!(hand_back_exec(Harness::Claude, "sid", "bogus", "/w").is_none());
    }
}

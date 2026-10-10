//! Replays real ACP sessions recorded from Copilot CLI 1.0.95 and opencode
//! 1.18.35 (`acc acp` against a local mock model, one shell tool call that
//! needs approval). `*.out.ndjson` is what the agent wrote, `*.in.ndjson` what
//! ACC wrote back. Paths and request-id tags were normalized after recording.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use acc_core::Harness;
use acc_core::acp::{AcpConfig, AcpDriver, Start};
use acc_core::command::{Command, OptionId};
use acc_core::driver::Driver;
use acc_core::event::{Event, ItemKind};
use acc_core::state::{BlockKind, RunState};
use serde_json::Value;

const COPILOT_OUT: &str = include_str!("fixtures/acp/copilot-1.0.95.out.ndjson");
const COPILOT_IN: &str = include_str!("fixtures/acp/copilot-1.0.95.in.ndjson");
const OPENCODE_OUT: &str = include_str!("fixtures/acp/opencode-1.18.35.out.ndjson");
const OPENCODE_IN: &str = include_str!("fixtures/acp/opencode-1.18.35.in.ndjson");

const PROMPT: &str = "Run echo acc-live-test";

fn json_lines(s: &str) -> Vec<Value> {
    s.lines()
        .map(|l| {
            let mut v: Value = serde_json::from_str(l).unwrap();
            // The crate version changes; the recording's doesn't matter.
            if let Some(info) = v.pointer_mut("/params/clientInfo/version") {
                *info = Value::Null;
            }
            v
        })
        .collect()
}

fn states(events: &[Event]) -> Vec<RunState> {
    let mut out: Vec<RunState> = Vec::new();
    for e in events {
        if let Event::StateChanged(s) = e {
            out.push(s.state.clone());
        }
    }
    out
}

/// Drives the agent output line by line, issuing the same commands the
/// recording client did, and returns the events and the bytes written.
fn replay_as_launcher(harness: Harness, out_log: &str) -> (Vec<Event>, Vec<u8>) {
    let mut d = AcpDriver::new(harness, AcpConfig::new("/home/v/proj", Start::New));
    let mut written = d.handshake();
    let mut events = Vec::new();
    for line in out_log.split_inclusive('\n') {
        // Feed in small pieces, as a slow SSH channel would deliver it.
        for piece in line.as_bytes().chunks(97) {
            for ev in d.on_output(piece) {
                match &ev {
                    Event::SessionStarted { .. } => {
                        written.extend(d.take_outgoing());
                        written.extend(d.command(Command::Prompt { text: PROMPT.into() }).unwrap());
                    }
                    Event::PermissionRequested {
                        request_id, options, ..
                    } => {
                        written.extend(d.take_outgoing());
                        written.extend(
                            d.command(Command::AnswerPermission {
                                request_id: request_id.clone(),
                                option_id: options[0].id.clone(),
                            })
                            .unwrap(),
                        );
                    }
                    _ => {}
                }
                events.push(ev);
            }
            written.extend(d.take_outgoing());
        }
    }
    (events, written)
}

fn check_turn(harness: Harness, out_log: &str, in_log: &str, approve: &str) {
    let (events, written) = replay_as_launcher(harness, out_log);
    assert_eq!(
        json_lines(std::str::from_utf8(&written).unwrap()),
        json_lines(in_log),
        "ACC must write exactly what the agent accepted"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Unrecognized { .. })),
        "{events:#?}"
    );
    assert_eq!(
        states(&events),
        [
            RunState::Idle,
            RunState::Working { progress: None },
            RunState::Blocked {
                kind: BlockKind::Permission
            },
            RunState::Working { progress: None },
            RunState::Done,
        ]
    );
    let perm = events.iter().find_map(|e| match e {
        Event::PermissionRequested { options, .. } => Some(options[0].id.as_str()),
        _ => None,
    });
    assert_eq!(perm, Some(approve));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::ItemStarted {
            kind: ItemKind::CommandExecution,
            ..
        }
    )));
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemCompleted { extensions, .. } if extensions["status"] == "completed")));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ItemDelta { text, .. } if text == "All done."))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Usage(u) if u.output_tokens.is_some()))
    );
    assert_eq!(events.last(), Some(&Event::TurnCompleted));
}

#[test]
fn copilot_turn_with_approval() {
    check_turn(Harness::Copilot, COPILOT_OUT, COPILOT_IN, "allow_once");
}

#[test]
fn opencode_turn_with_approval() {
    check_turn(Harness::Opencode, OPENCODE_OUT, OPENCODE_IN, "once");
}

#[test]
fn recorded_options_and_commands() {
    let mut d = AcpDriver::new(Harness::Copilot, AcpConfig::new("/home/v/proj", Start::New));
    d.handshake();
    d.on_output(COPILOT_OUT.as_bytes());
    let perm = d.options().iter().find(|o| o.id == OptionId::Permission).unwrap();
    assert_eq!(perm.values, ["on", "off"]);
    assert_eq!(perm.current.as_deref(), Some("off"));
    let mode = d.options().iter().find(|o| o.id == OptionId::Mode).unwrap();
    assert_eq!(mode.values.len(), 3);
    assert!(d.commands().iter().any(|c| c.name == "compact"));
    assert_eq!(d.auth_methods()[0].0, "copilot-login");

    let mut d = AcpDriver::new(Harness::Opencode, AcpConfig::new("/home/v/proj", Start::New));
    d.handshake();
    d.on_output(OPENCODE_OUT.as_bytes());
    let model = d.options().iter().find(|o| o.id == OptionId::Model).unwrap();
    assert_eq!(model.current.as_deref(), Some("mock/mock-1"));
    let mode = d.options().iter().find(|o| o.id == OptionId::Mode).unwrap();
    assert_eq!(mode.values, ["build", "plan"]);
    assert!(d.commands().iter().any(|c| c.name == "review"));
}

/// A phone re-attaching to a detached agent replays the whole log written
/// while another client drove it, and ends up in the same place.
#[test]
fn attach_replays_another_clients_log() {
    for (harness, log) in [(Harness::Copilot, COPILOT_OUT), (Harness::Opencode, OPENCODE_OUT)] {
        let mut cfg = AcpConfig::new("/home/v/proj", Start::Attach);
        cfg.client_tag = "phone-1".into();
        let mut d = AcpDriver::new(harness, cfg);
        assert!(d.handshake().is_empty());
        let events = d.on_output(log.as_bytes());
        assert!(d.session_id().is_some(), "{harness:?}");
        assert_eq!(states(&events).last(), Some(&RunState::Done), "{harness:?}");
        assert_eq!(events.last(), Some(&Event::TurnCompleted), "{harness:?}");
        assert!(
            d.take_outgoing().is_empty(),
            "an attached client only listens until asked"
        );
        // The permission was answered by the other client; nothing is pending here.
        assert!(
            d.command(Command::AnswerPermission {
                request_id: "0".into(),
                option_id: "once".into()
            })
            .is_err()
        );
        let next: Value =
            serde_json::from_slice(&d.command(Command::Prompt { text: "next".into() }).unwrap()).unwrap();
        assert_eq!(next["id"], "phone-1-0");
    }
}

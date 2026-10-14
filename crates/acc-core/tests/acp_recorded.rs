//! Replays real ACP sessions recorded from Copilot CLI 1.0.95 and opencode
//! 1.18.35 (`acc acp` against a local mock model) and Cursor Agent 2026.10.01
//! (its own default model), each one shell tool call that needs approval. `*.out.ndjson` is what the agent wrote, `*.in.ndjson` what
//! ACC wrote back. Paths and request-id tags were normalized after recording.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use acc_core::Harness;
use acc_core::acp::{AcpConfig, AcpDriver, Start};
use acc_core::command::{Attachment, Command, OptionId};
use acc_core::driver::Driver;
use acc_core::event::{Event, ItemKind};
use acc_core::state::{BlockKind, RunState};
use serde_json::Value;

const COPILOT_OUT: &str = include_str!("fixtures/acp/copilot-1.0.95.out.ndjson");
const COPILOT_IN: &str = include_str!("fixtures/acp/copilot-1.0.95.in.ndjson");
const OPENCODE_OUT: &str = include_str!("fixtures/acp/opencode-1.18.35.out.ndjson");
const OPENCODE_IN: &str = include_str!("fixtures/acp/opencode-1.18.35.in.ndjson");
const CURSOR_OUT: &str = include_str!("fixtures/acp/cursor-2026.10.01.out.ndjson");
const CURSOR_IN: &str = include_str!("fixtures/acp/cursor-2026.10.01.in.ndjson");

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
                        written.extend(d.poll_transmit());
                        written.extend(d.command(Command::prompt(PROMPT)).unwrap());
                    }
                    Event::PermissionRequested {
                        request_id, options, ..
                    } => {
                        written.extend(d.poll_transmit());
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
            written.extend(d.poll_transmit());
        }
    }
    (events, written)
}

fn check_turn(harness: Harness, out_log: &str, in_log: &str, approve: &str) -> Vec<Event> {
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
    assert_eq!(events.last(), Some(&Event::TurnCompleted));
    events
}

/// The mock model's reply and usage report.
fn check_mock_reply(events: &[Event]) {
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
}

#[test]
fn copilot_turn_with_approval() {
    check_mock_reply(&check_turn(
        Harness::Copilot,
        COPILOT_OUT,
        COPILOT_IN,
        "allow_once",
    ));
}

#[test]
fn opencode_turn_with_approval() {
    check_mock_reply(&check_turn(Harness::Opencode, OPENCODE_OUT, OPENCODE_IN, "once"));
}

#[test]
fn cursor_turn_with_approval() {
    let events = check_turn(Harness::Cursor, CURSOR_OUT, CURSOR_IN, "allow-once");
    let reply: String = events
        .iter()
        .filter_map(|e| match e {
            Event::ItemDelta { id, text } if id == "item-4" => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reply, "`acc-live-test`");
    // Cursor reports no usage in ACP.
    assert!(!events.iter().any(|e| matches!(e, Event::Usage(_))));
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
    // Copilot's mode values are URLs; show their names.
    assert!(mode.current.as_deref().unwrap().starts_with("https://"));
    assert_eq!(mode.current_label(), Some("Agent"));
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
    for (harness, log) in [
        (Harness::Copilot, COPILOT_OUT),
        (Harness::Opencode, OPENCODE_OUT),
        (Harness::Cursor, CURSOR_OUT),
    ] {
        let mut cfg = AcpConfig::new("/home/v/proj", Start::Attach);
        cfg.client_tag = "phone-1".into();
        let mut d = AcpDriver::new(harness, cfg);
        assert!(d.handshake().is_empty());
        let events = d.on_output(log.as_bytes());
        assert!(d.session_id().is_some(), "{harness:?}");
        assert_eq!(states(&events).last(), Some(&RunState::Done), "{harness:?}");
        assert_eq!(events.last(), Some(&Event::TurnCompleted), "{harness:?}");
        assert!(
            d.poll_transmit().is_empty(),
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
        let next: Value = serde_json::from_slice(&d.command(Command::prompt("next")).unwrap()).unwrap();
        assert_eq!(next["id"], "phone-1-0");
        // Both recorded agents advertise `promptCapabilities.image`, which the
        // attaching client learned from the replayed `initialize` reply.
        let with_image = d
            .command(Command::Prompt {
                text: "and this?".into(),
                attachments: vec![Attachment::from_bytes("image/png", b"PNG")],
            })
            .unwrap();
        let with_image: Value = serde_json::from_slice(&with_image).unwrap();
        assert_eq!(with_image["params"]["prompt"][1]["type"], "image");
        assert_eq!(with_image["params"]["prompt"][1]["mimeType"], "image/png");
    }
}

/// opencode 2.0.14 against a real model: one `subagent` call to the
/// `explore` agent, whose child session streams under the parent's session
/// id with `_meta["opencode/child-session"]`.
const OPENCODE_SUBAGENT: &str = include_str!("fixtures/acp/opencode-2.0.14-subagent.out.ndjson");

#[test]
fn opencode_subagent_and_its_child_session() {
    use acc_core::item::ItemDetail;
    use acc_core::transcript::Transcript;
    let mut d = AcpDriver::new(
        Harness::Opencode,
        AcpConfig::new("/tmp/acc-sub-spike", Start::Attach),
    );
    let mut t = Transcript::new();
    for line in OPENCODE_SUBAGENT.split_inclusive('\n') {
        for e in d.on_output(line.as_bytes()) {
            t.settle(&e);
            t.apply(&e);
        }
    }
    let subs: Vec<_> = t.subagents().collect();
    assert_eq!(subs.len(), 1, "{:?}", t.items());
    let sub = subs[0];
    let ItemDetail::Subagent {
        agent,
        description,
        prompt,
        child,
        status,
        output,
        ..
    } = sub.detail(Harness::Opencode)
    else {
        panic!("subagent detail")
    };
    assert_eq!(agent.as_deref(), Some("explore"));
    assert_eq!(description, "List directory files");
    assert!(prompt.unwrap().starts_with("List the files"));
    assert_eq!(child.as_deref(), Some("ses_ed041c0e2ffeOlPhTtPjxjVhMz"));
    assert_eq!(status.as_deref(), Some("completed"));
    assert!(output.unwrap().starts_with("Files in /tmp/acc-sub-spike"));

    let children: Vec<_> = t.children(&sub.id).map(|i| (i.kind, i.text.as_str())).collect();
    assert_eq!(children[0].0, ItemKind::ToolCall);
    assert_eq!(
        children[1],
        (
            ItemKind::AgentMessage,
            "Files in /tmp/acc-sub-spike:\n- main.rs\n- README.txt"
        )
    );
    // The main conversation's answer isn't the child's.
    assert!(
        t.top_level()
            .any(|i| i.kind == ItemKind::AgentMessage && i.text.starts_with("The subagent found"))
    );
    assert!(
        t.items()
            .iter()
            .all(|i| i.status != acc_core::transcript::ItemStatus::Running)
    );
}

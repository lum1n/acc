//! Replays recorded Claude Code 2.1.296 sessions through the driver.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

const HI: &str = include_str!("../../tests/fixtures/claude/hi-and-controls.ndjson");
const PERMISSION: &str = include_str!("../../tests/fixtures/claude/permission-deny.ndjson");

/// Feeds a recording in uneven chunks, as an SSH channel would deliver it.
fn replay(d: &mut ClaudeDriver, recording: &str) -> Vec<Event> {
    let mut out = Vec::new();
    for chunk in recording.as_bytes().chunks(997) {
        out.extend(d.on_output(chunk));
    }
    out
}

/// The permission recording up to and including the `can_use_tool` request,
/// so the prompt is still open (the full recording goes on to deny it).
fn until_prompt(recording: &str) -> String {
    let mut out = String::new();
    for line in recording.lines() {
        out.push_str(line);
        out.push('\n');
        if line.contains("\"can_use_tool\"") {
            break;
        }
    }
    out
}

fn sent(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn states(events: &[Event]) -> Vec<RunState> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::StateChanged(s) => Some(s.state.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn launch_spec_requests_control_protocol() {
    let d = ClaudeDriver::new(ClaudeConfig {
        model: Some("opus".into()),
        permission_mode: None,
        fork: true,
        client_tag: None,
    });
    let spec = d.launch(Some("abc"));
    for flag in [
        "--permission-prompt-tool",
        "--include-partial-messages",
        "--replay-user-messages",
    ] {
        assert!(spec.args.iter().any(|a| a == flag), "missing {flag}");
    }
    assert!(spec.args.windows(2).any(|w| w == ["--resume", "abc"]));
    assert!(spec.args.windows(2).any(|w| w == ["--model", "opus"]));
    assert_eq!(spec.args.last().map(String::as_str), Some("--fork-session"));
    assert!(
        spec.env
            .contains(&("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS".into(), "1".into()))
    );
    // No fork without a session to fork from.
    assert!(!d.launch(None).args.iter().any(|a| a == "--fork-session"));
}

#[test]
fn handshake_is_initialize_with_id_matching_fixture() {
    let mut d = ClaudeDriver::default();
    let msgs = sent(&d.handshake());
    assert_eq!(msgs[0]["type"], "control_request");
    assert_eq!(msgs[0]["request_id"], "acc-1");
    assert_eq!(msgs[0]["request"]["subtype"], "initialize");
}

#[test]
fn recorded_turn_produces_session_text_usage_and_done() {
    let mut d = ClaudeDriver::default();
    d.handshake();
    let events = replay(&mut d, HI);

    assert!(events.iter().any(|e| matches!(e,
        Event::SessionStarted { harness: Harness::Claude, native_id, model: Some(m) }
            if native_id == "5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11" && m == "claude-sonnet-5-5")));
    assert_eq!(d.session_id(), Some("5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11"));

    // The streamed text item: started, delta "hi", completed exactly once.
    let item = "msg_011CftY7bUGFRBMVYZDXqu3y:0";
    let started = events
        .iter()
        .filter(|e| matches!(e, Event::ItemStarted { id, kind: ItemKind::AgentMessage, .. } if id == item))
        .count();
    let completed = events
        .iter()
        .filter(|e| matches!(e, Event::ItemCompleted { id, .. } if id == item))
        .count();
    assert_eq!((started, completed), (1, 1));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ItemDelta { id, text } if id == item && text == "hi"))
    );

    let usage = events.iter().find_map(|e| match e {
        Event::Usage(u) if u.cost_usd.is_some() => Some(u.clone()),
        _ => None,
    });
    let usage = usage.expect("result usage");
    assert_eq!(usage.output_tokens, Some(4));
    assert_eq!(usage.cache_write_tokens, Some(35312));
    assert_eq!(usage.context_window, Some(1_000_000));
    assert_eq!(usage.context_used, Some(2 + 35312 + 4));

    assert!(events.contains(&Event::TurnCompleted));
    assert!(states(&events).contains(&RunState::Done));

    // get_context_usage reply, then the set_model echo as a notice.
    assert!(events.iter().any(
        |e| matches!(e, Event::Usage(u) if u.context_used == Some(35314) && u.context_window == Some(500_000))
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Notice { message } if message.contains("Set model to")))
    );
}

#[test]
fn rate_limit_event_becomes_quota_percentages() {
    let mut d = ClaudeDriver::default();
    let events = replay(&mut d, HI);
    let quota = events.iter().find_map(|e| match e {
        Event::Quota(q) => Some(q.clone()),
        _ => None,
    });
    let quota = quota.expect("quota");
    let five = quota
        .windows
        .iter()
        .find(|w| w.id == "five_hour")
        .expect("five hour");
    assert_eq!(five.used_percent, Some(0.0));
    assert_eq!(five.window_minutes, Some(300));
    assert_eq!(five.status.as_deref(), Some("allowed"));
    let week = quota
        .windows
        .iter()
        .find(|w| w.id == "seven_day")
        .expect("seven day");
    assert!((week.used_percent.unwrap() - 23.0).abs() < 1e-9);
    assert_eq!(week.resets_at, Some(1_791_853_200));
}

#[test]
fn initialize_reply_fills_palette_and_options() {
    let mut d = ClaudeDriver::default();
    d.handshake();
    replay(&mut d, HI);
    let names: Vec<_> = d.commands().iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"compact"));
    assert!(names.contains(&"interrupt"));
    let compact = d.commands().iter().find(|c| c.name == "compact").unwrap();
    assert_eq!(compact.exec, Exec::Text);

    let model = d.options().iter().find(|o| o.id == OptionId::Model).unwrap();
    assert!(model.values.iter().any(|v| v == "sonnet"));
    // The resolved id maps back to its alias rather than "default".
    assert_eq!(model.current.as_deref(), Some("sonnet"));
    let effort = d.options().iter().find(|o| o.id == OptionId::Effort).unwrap();
    assert!(effort.values.iter().any(|v| v == "max"));
    let perm = d.options().iter().find(|o| o.id == OptionId::Permission).unwrap();
    assert_eq!(perm.current.as_deref(), Some("default"));
}

#[test]
fn permission_round_trip_matches_recording() {
    let mut d = ClaudeDriver::default();
    d.handshake();
    let events = replay(&mut d, PERMISSION);

    let (request_id, title, options) = events
        .iter()
        .find_map(|e| match e {
            Event::PermissionRequested {
                request_id,
                title,
                options,
                ..
            } => Some((request_id.clone(), title.clone(), options.clone())),
            _ => None,
        })
        .expect("permission request");
    assert_eq!(request_id, "ef3cff2f-5523-4403-874b-dfeb45c76fb7");
    assert_eq!(title, "Bash: touch acc-probe.txt");
    let ids: Vec<_> = options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["allow", "allow_always", "deny"]);

    // Idle from the initialize reply, then native session state:
    // running -> blocked -> running -> done (idle after done is ignored).
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

    // The Bash tool item opened with its input and closed by the denied result.
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemStarted { id, kind: ItemKind::CommandExecution, extensions }
            if id == "toolu_01ADx3kHAMjJRN8Qvy9H8avv" && extensions["input"]["command"] == "touch acc-probe.txt")));
    assert!(events.iter().any(|e| matches!(e,
        Event::ItemCompleted { id, extensions } if id == "toolu_01ADx3kHAMjJRN8Qvy9H8avv" && extensions["is_error"] == true)));
}

#[test]
fn answers_are_control_responses() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, &until_prompt(PERMISSION));
    let rid = "ef3cff2f-5523-4403-874b-dfeb45c76fb7";

    let bytes = d
        .command(Command::AnswerPermission {
            request_id: rid.into(),
            option_id: "allow_always".into(),
        })
        .unwrap();
    let msg = &sent(&bytes)[0];
    assert_eq!(msg["type"], "control_response");
    assert_eq!(msg["response"]["request_id"], rid);
    let result = &msg["response"]["response"];
    assert_eq!(result["behavior"], "allow");
    assert_eq!(result["toolUseID"], "toolu_01ADx3kHAMjJRN8Qvy9H8avv");
    assert_eq!(result["updatedInput"]["command"], "touch acc-probe.txt");
    assert_eq!(result["updatedPermissions"][0]["type"], "addRules");

    // Answered once; a second answer is an error, not a duplicate write.
    assert_eq!(
        d.command(Command::AnswerPermission {
            request_id: rid.into(),
            option_id: "deny".into()
        }),
        Err(DriverError::UnknownRequest(rid.into()))
    );
}

#[test]
fn ask_user_question_is_a_question_answered_by_allowing() {
    let mut d = ClaudeDriver::default();
    let req = json!({"type":"control_request","request_id":"q1","request":{
        "subtype":"can_use_tool","tool_name":"AskUserQuestion","tool_use_id":"toolu_q",
        "input":{"questions":[{"question":"Which DB?","header":"DB","multiSelect":false,
            "options":[{"label":"Postgres","description":""},{"label":"SQLite","description":""}]}]}}});
    let events = d.on_output(&wire::line(&req));
    assert!(events.iter().any(|e| matches!(e,
        Event::PermissionRequested { title, options, .. } if title == "Which DB?" && options.len() == 2)));
    assert!(states(&events).contains(&RunState::Blocked {
        kind: BlockKind::Question
    }));

    let bytes = d
        .command(Command::AnswerPermission {
            request_id: "q1".into(),
            option_id: "SQLite".into(),
        })
        .unwrap();
    let result = &sent(&bytes)[0]["response"]["response"];
    assert_eq!(result["behavior"], "allow");
    assert_eq!(result["updatedInput"]["answers"]["Which DB?"], "SQLite");
}

#[test]
fn cancel_request_resolves_pending_permission() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, &until_prompt(PERMISSION));
    let events = d.on_output(
        b"{\"type\":\"control_cancel_request\",\"request_id\":\"ef3cff2f-5523-4403-874b-dfeb45c76fb7\"}\n",
    );
    assert_eq!(
        events,
        [Event::PermissionResolved {
            request_id: "ef3cff2f-5523-4403-874b-dfeb45c76fb7".into()
        }]
    );
}

#[test]
fn unknown_control_requests_get_an_error_reply() {
    let mut d = ClaudeDriver::default();
    d.on_output(
        b"{\"type\":\"control_request\",\"request_id\":\"h1\",\"request\":{\"subtype\":\"hook_callback\"}}\n",
    );
    let reply = sent(&d.poll_transmit());
    assert_eq!(reply[0]["response"]["subtype"], "error");
    assert_eq!(reply[0]["response"]["request_id"], "h1");
    assert!(d.poll_transmit().is_empty());
}

#[test]
fn commands_map_to_text_or_control_requests() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, HI);

    let prompt = sent(&d.command(Command::Prompt { text: "hello".into() }).unwrap());
    assert_eq!(prompt[0]["type"], "user");
    assert_eq!(prompt[0]["message"]["content"], "hello");

    let compact = sent(
        &d.command(Command::Compact {
            hint: Some("keep tests".into()),
        })
        .unwrap(),
    );
    assert_eq!(compact[0]["message"]["content"], "/compact keep tests");
    let invoke = sent(
        &d.command(Command::Invoke {
            name: "code-review".into(),
            args: String::new(),
        })
        .unwrap(),
    );
    assert_eq!(invoke[0]["message"]["content"], "/code-review");

    let interrupt = sent(&d.command(Command::Interrupt).unwrap());
    assert_eq!(interrupt[0]["request"]["subtype"], "interrupt");

    let model = sent(
        &d.command(Command::SetOption {
            id: OptionId::Model,
            value: "haiku".into(),
        })
        .unwrap(),
    );
    assert_eq!(
        model[0]["request"],
        json!({"subtype": "set_model", "model": "haiku"})
    );
    let effort = sent(
        &d.command(Command::SetOption {
            id: OptionId::Effort,
            value: "high".into(),
        })
        .unwrap(),
    );
    assert_eq!(effort[0]["request"]["settings"]["effortLevel"], "high");
    assert_eq!(
        d.command(Command::SetOption {
            id: OptionId::Permission,
            value: "yolo".into()
        }),
        Err(DriverError::InvalidOption("yolo".into()))
    );
    let rename = sent(
        &d.command(Command::Rename {
            name: "auth fix".into(),
        })
        .unwrap(),
    );
    assert_eq!(
        rename[0]["request"],
        json!({"subtype": "rename_session", "title": "auth fix"})
    );
    assert!(matches!(
        d.command(Command::Fork),
        Err(DriverError::Unsupported { .. })
    ));
}

#[test]
fn set_model_reply_updates_current_option() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, HI);
    let req = sent(
        &d.command(Command::SetOption {
            id: OptionId::Model,
            value: "haiku".into(),
        })
        .unwrap(),
    );
    let id = req[0]["request_id"].as_str().unwrap().to_owned();
    let events = d.on_output(&wire::line(&json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": id},
    })));
    assert!(events.contains(&Event::OptionsChanged));
    let model = d.options().iter().find(|o| o.id == OptionId::Model).unwrap();
    assert_eq!(model.current.as_deref(), Some("haiku"));
}

#[test]
fn unstreamed_assistant_text_is_announced_whole() {
    let mut d = ClaudeDriver::default();
    let events = d.on_output(&wire::line(&json!({
        "type": "assistant",
        "message": {"id": "m1", "content": [{"type": "text", "text": "hello"}]},
        "parent_tool_use_id": null,
    })));
    assert_eq!(
        events,
        [
            Event::ItemStarted {
                id: "m1:0".into(),
                kind: ItemKind::AgentMessage,
                extensions: Value::Null
            },
            Event::ItemDelta {
                id: "m1:0".into(),
                text: "hello".into()
            },
            Event::ItemCompleted {
                id: "m1:0".into(),
                extensions: Value::Null
            },
        ]
    );
}

#[test]
fn garbage_and_unknown_frames_are_kept_not_fatal() {
    let mut d = ClaudeDriver::default();
    let events = d.on_output(b"not json\n{\"type\":\"future_thing\"}\n");
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| matches!(e, Event::Unrecognized { .. })));
}

#[test]
fn tool_result_resolves_the_prompt_for_every_reader_of_the_log() {
    let mut d = ClaudeDriver::default();
    let events = replay(&mut d, PERMISSION);
    let rid = "ef3cff2f-5523-4403-874b-dfeb45c76fb7";
    let asked = events
        .iter()
        .position(|e| matches!(e, Event::PermissionRequested { request_id, .. } if request_id == rid));
    let resolved = events
        .iter()
        .position(|e| matches!(e, Event::PermissionResolved { request_id } if request_id == rid));
    assert!(matches!((asked, resolved), (Some(a), Some(r)) if a < r));
    assert_eq!(
        d.command(Command::AnswerPermission {
            request_id: rid.into(),
            option_id: "allow".into()
        }),
        Err(DriverError::UnknownRequest(rid.into()))
    );
}

#[test]
fn client_tag_prefixes_ids_and_foreign_errors_are_ignored() {
    let mut d = ClaudeDriver::new(ClaudeConfig {
        client_tag: Some("phone7".into()),
        ..ClaudeConfig::default()
    });
    let hs = sent(&d.handshake());
    assert_eq!(hs[0]["request_id"], "phone7-1");
    let events = d.on_output(
        b"{\"type\":\"control_response\",\"response\":{\"subtype\":\"error\",\"request_id\":\"desk-3\",\"error\":\"x\"}}\n",
    );
    assert!(events.is_empty());
}

#[test]
fn attaching_client_adopts_another_clients_initialize_reply() {
    let mut first = ClaudeDriver::default();
    let _ = first.handshake();
    let mut late = ClaudeDriver::new(ClaudeConfig {
        client_tag: Some("late".into()),
        ..ClaudeConfig::default()
    });
    let events = replay(&mut late, HI);
    assert!(events.contains(&Event::CommandsChanged));
    assert!(late.commands().iter().any(|c| c.source != CommandSource::Acc));
    let attach = sent(&late.attach_handshake());
    assert!(attach.iter().all(|m| m["request"]["subtype"] != "initialize"));
    assert!(
        attach
            .iter()
            .all(|m| m["request_id"].as_str().unwrap().starts_with("late-"))
    );
}

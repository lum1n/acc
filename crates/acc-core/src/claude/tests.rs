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
        Event::ItemStarted { id, kind: ItemKind::CommandExecution, parent: None, extensions }
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
fn ask_user_question_answers_every_question() {
    let mut d = ClaudeDriver::default();
    let req = json!({"type":"control_request","request_id":"q2","request":{
        "subtype":"can_use_tool","tool_name":"AskUserQuestion","tool_use_id":"toolu_q",
        "input":{"questions":[
            {"question":"Which DB?","header":"DB","multiSelect":false,
                "options":[{"label":"Postgres","description":""},{"label":"SQLite","description":""}]},
            {"question":"Which extras?","header":"Extras","multiSelect":true,
                "options":[{"label":"Cache","description":""},{"label":"Search","description":""}]}]}}});
    d.on_output(&wire::line(&req));
    let answers = vec![
        vec!["Postgres".to_owned()],
        vec!["Cache".to_owned(), "Search".to_owned()],
    ];
    let bytes = d
        .command(Command::AnswerPermission {
            request_id: "q2".into(),
            option_id: crate::question::encode(&answers),
        })
        .unwrap();
    let result = &sent(&bytes)[0]["response"]["response"];
    assert_eq!(
        result["updatedInput"]["answers"],
        json!({"Which DB?": "Postgres", "Which extras?": "Cache, Search"})
    );
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

    let prompt = sent(&d.command(Command::prompt("hello")).unwrap());
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
                parent: None,
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

/// Claude Code names the message only in `message_start`; its other stream
/// events carry no `api_message_id`.
#[test]
fn stream_events_without_a_message_id_stream_into_their_own_blocks() {
    let mut d = ClaudeDriver::default();
    let stream = |event: Value| json!({"type": "stream_event", "event": event, "parent_tool_use_id": null});
    let lines = [
        stream(json!({"type": "message_start", "message": {"id": "m1", "content": []}})),
        stream(
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        ),
        stream(
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
        ),
        json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "thinking", "thinking": "hmm"}]}, "parent_tool_use_id": null}),
        stream(json!({"type": "content_block_stop", "index": 0})),
        stream(
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        ),
        stream(
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "he"}}),
        ),
        stream(
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "llo"}}),
        ),
        json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "text", "text": "hello"}]}, "parent_tool_use_id": null}),
        stream(json!({"type": "content_block_stop", "index": 1})),
        stream(json!({"type": "message_stop"})),
    ];
    let mut t = crate::transcript::Transcript::default();
    for l in &lines {
        for e in d.on_output(&wire::line(l)) {
            t.apply(&e);
        }
    }
    let items: Vec<_> = t
        .items()
        .iter()
        .map(|i| (i.id.as_str(), i.kind, i.text.as_str()))
        .collect();
    assert_eq!(
        items,
        [
            ("m1:0", ItemKind::Reasoning, "hmm"),
            ("m1:1", ItemKind::AgentMessage, "hello"),
        ]
    );
}

#[test]
fn unstreamed_blocks_of_one_message_stay_apart() {
    let mut d = ClaudeDriver::default();
    let mut t = crate::transcript::Transcript::default();
    for block in [
        json!({"type": "thinking", "thinking": "hmm"}),
        json!({"type": "text", "text": "hello"}),
    ] {
        let frame = json!({"type": "assistant", "message": {"id": "m1", "content": [block]}, "parent_tool_use_id": null});
        for e in d.on_output(&wire::line(&frame)) {
            t.apply(&e);
        }
    }
    let items: Vec<_> = t
        .items()
        .iter()
        .map(|i| (i.id.as_str(), i.text.as_str()))
        .collect();
    assert_eq!(items, [("m1:0", "hmm"), ("m1:1", "hello")]);
}

#[test]
fn failed_turn_reports_error_before_turn_completed() {
    let mut d = ClaudeDriver::default();
    let events = d.on_output(
        b"{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"errors\":[\"boom\"]}\n",
    );
    assert_eq!(events.last(), Some(&Event::TurnCompleted));
    assert_eq!(states(&events), [RunState::Error]);
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

fn control_reply(id: &Value, body: Value) -> String {
    format!(
        "{}\n",
        json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": body}})
    )
}

fn invoke(d: &mut ClaudeDriver, name: &str, args: &str) -> Result<Vec<Value>, DriverError> {
    d.command(Command::Invoke {
        name: name.into(),
        args: args.into(),
    })
    .map(|b| sent(&b))
}

#[test]
fn option_commands_open_pickers_and_set_options() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, HI);
    let input = |name: &str| {
        d.commands()
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.input.clone())
    };
    assert_eq!(input("model"), Some(CommandInput::Option { id: OptionId::Model }));
    assert_eq!(
        input("effort"),
        Some(CommandInput::Option { id: OptionId::Effort })
    );
    assert_eq!(
        input("mode"),
        Some(CommandInput::Option {
            id: OptionId::Permission
        })
    );

    let model = invoke(&mut d, "model", "opus").unwrap();
    assert_eq!(model[0]["request"]["subtype"], "set_model");
    assert_eq!(model[0]["request"]["model"], "opus");
    let mode = invoke(&mut d, "mode", "plan").unwrap();
    assert_eq!(mode[0]["request"]["subtype"], "set_permission_mode");
    assert_eq!(mode[0]["request"]["mode"], "plan");
    assert!(matches!(
        invoke(&mut d, "mode", ""),
        Err(DriverError::MissingArgument(_))
    ));
    // Without a value Claude prints the current model and the choices.
    assert_eq!(
        invoke(&mut d, "model", "").unwrap()[0]["message"]["content"],
        "/model"
    );
}

#[test]
fn mcp_opens_a_panel_whose_actions_refresh_it() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, HI);
    let mcp = d.commands().iter().find(|c| c.name == "mcp").unwrap();
    assert_eq!(
        mcp.exec,
        Exec::Rpc {
            method: "mcp_status".into()
        }
    );

    let req = invoke(&mut d, "mcp", "").unwrap();
    assert_eq!(req[0]["request"]["subtype"], "mcp_status");
    let events = d.on_output(
        control_reply(
            &req[0]["request_id"],
            json!({"mcpServers": [
                {"name": "sentry", "status": "failed", "error": "401", "scope": "user", "config": {"type": "http"}},
                {"name": "repclue", "status": "disabled", "scope": "project", "config": {"type": "stdio"}},
            ]}),
        )
        .as_bytes(),
    );
    let Some(Event::Panel(panel)) = events.first() else {
        panic!("no panel in {events:?}");
    };
    assert_eq!(panel.id, "mcp");
    let rows = &panel.sections[0].rows;
    assert_eq!(rows[0].label, "sentry");
    assert_eq!(rows[0].value.as_deref(), Some("failed"));
    assert_eq!(rows[0].detail.as_deref(), Some("user · http · 401"));
    assert_eq!(rows[0].actions[0].args, "reconnect sentry");
    assert_eq!(rows[0].actions[1].args, "disable sentry");
    assert_eq!(rows[1].actions[1].args, "enable repclue");

    let toggle = invoke(&mut d, "mcp", "enable repclue").unwrap();
    assert_eq!(toggle[0]["request"]["subtype"], "mcp_toggle");
    assert_eq!(toggle[0]["request"]["serverName"], "repclue");
    assert_eq!(toggle[0]["request"]["enabled"], true);
    d.on_output(control_reply(&toggle[0]["request_id"], json!({})).as_bytes());
    let again = sent(&d.poll_transmit());
    assert_eq!(again[0]["request"]["subtype"], "mcp_status");
    assert!(matches!(
        invoke(&mut d, "mcp", "reconnect"),
        Err(DriverError::MissingArgument(_))
    ));
}

#[test]
fn status_and_permissions_panels() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, HI);
    let req = invoke(&mut d, "status", "").unwrap();
    assert_eq!(req[0]["request"]["subtype"], "get_status");
    let events = d.on_output(
        control_reply(
            &req[0]["request_id"],
            json!({"sections": [{"title": "Session", "rows": [{"label": "Version", "value": "2.1.296"}]}]}),
        )
        .as_bytes(),
    );
    let Some(Event::Panel(status)) = events.first() else {
        panic!("no panel");
    };
    assert_eq!(status.sections[0].title.as_deref(), Some("Session"));
    assert_eq!(status.sections[0].rows[0].value.as_deref(), Some("2.1.296"));

    // Recorded from Claude Code 2.1.280 with a local settings file.
    let req = invoke(&mut d, "permissions", "").unwrap();
    let events = d.on_output(
        control_reply(
            &req[0]["request_id"],
            json!({"state": {"rules": [
                {"behavior": "allow", "source": "localSettings", "rule": "Bash(npm test:*)",
                 "description": {"prefix": "Any Bash command starting with ", "emphasis": "npm test"}, "editability": "persistent"},
                {"behavior": "deny", "source": "localSettings", "rule": "Read(./.env)", "editability": "persistent"},
            ], "workspaceDirectories": [], "originalCwd": "/p", "managedOnly": false}}),
        )
        .as_bytes(),
    );
    let Some(Event::Panel(rules)) = events.first() else {
        panic!("no panel");
    };
    let titles: Vec<_> = rules
        .sections
        .iter()
        .map(|s| s.title.as_deref().unwrap())
        .collect();
    assert_eq!(titles, ["Allow", "Deny"]);
    assert_eq!(
        rules.sections[0].rows[0].detail.as_deref(),
        Some("Any Bash command starting with npm test")
    );
}

#[test]
fn terminal_only_commands_are_listed_but_not_sent() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, HI);
    let doctor = d.commands().iter().find(|c| c.name == "doctor").unwrap();
    assert_eq!(doctor.exec, Exec::Terminal);
    assert!(matches!(
        invoke(&mut d, "doctor", ""),
        Err(DriverError::Unsupported { .. })
    ));
    // reload-plugins is terminal-only as text but has a control request.
    let reload = invoke(&mut d, "reload-plugins", "").unwrap();
    assert_eq!(reload[0]["request"]["subtype"], "reload_plugins");
}

#[test]
fn panels_answer_only_the_client_that_asked() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, HI);
    let events = d.on_output(control_reply(&json!("other-7"), json!({"mcpServers": []})).as_bytes());
    assert!(!events.iter().any(|e| matches!(e, Event::Panel(_))));
}

#[test]
fn terminal_commands_are_known_before_the_first_turn() {
    let mut d = ClaudeDriver::default();
    let init = d.handshake();
    let id = sent(&init)[0]["request_id"].clone();
    d.on_output(
        control_reply(
            &id,
            json!({"commands": [
                {"name": "doctor", "description": "Diagnose", "builtin": true},
                {"name": "output-style", "description": "Switch style", "argumentHint": "[style]", "builtin": true},
            ], "models": [], "available_output_styles": ["default", "Concise"]}),
        )
        .as_bytes(),
    );
    let doctor = d.commands().iter().find(|c| c.name == "doctor").unwrap();
    assert_eq!(doctor.exec, Exec::Terminal);
    let style = d.commands().iter().find(|c| c.name == "output-style").unwrap();
    let CommandInput::Choice { choices, required } = &style.input else {
        panic!("no choices: {style:?}");
    };
    assert_eq!(choices[1].value, "Concise");
    assert!(!required);
}

#[test]
fn plan_enters_plan_mode_then_sends_the_description() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, HI);
    // Claude never lists /plan headless; ACC offers it anyway.
    let plan = d.commands().iter().find(|c| c.name == "plan").unwrap();
    assert_eq!(plan.arg_hint.as_deref(), Some("[<description>]"));
    assert_ne!(plan.exec, Exec::Terminal);

    let req = invoke(&mut d, "plan", "add caching to the feed").unwrap();
    assert_eq!(req.len(), 1, "the prompt waits for plan mode: {req:?}");
    assert_eq!(
        req[0]["request"],
        json!({"subtype": "set_permission_mode", "mode": "plan"})
    );
    let events = d.on_output(control_reply(&req[0]["request_id"], json!({})).as_bytes());
    assert!(events.contains(&Event::OptionsChanged));
    let perm = d.options().iter().find(|o| o.id == OptionId::Permission).unwrap();
    assert_eq!(perm.current.as_deref(), Some("plan"));
    let prompt = sent(&d.poll_transmit());
    assert_eq!(prompt[0]["type"], "user");
    assert_eq!(prompt[0]["message"]["content"], "add caching to the feed");

    // Already planning: a description is just the next prompt.
    let again = invoke(&mut d, "plan", "also cover invalidation").unwrap();
    assert_eq!(again[0]["message"]["content"], "also cover invalidation");
    assert!(matches!(
        invoke(&mut d, "plan", "share"),
        Err(DriverError::Unsupported { .. })
    ));
}

#[test]
fn plan_shows_the_plan_once_planning() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, HI);
    // Bare /plan outside plan mode only enters it.
    let enter = invoke(&mut d, "plan", "").unwrap();
    assert_eq!(enter[0]["request"]["subtype"], "set_permission_mode");
    d.on_output(control_reply(&enter[0]["request_id"], json!({})).as_bytes());
    assert!(d.poll_transmit().is_empty());

    let req = invoke(&mut d, "plan", "").unwrap();
    assert_eq!(req[0]["request"], json!({"subtype": "get_plan"}));
    let events = d.on_output(
        control_reply(
            &req[0]["request_id"],
            json!({"exists": true, "path": "/home/v/.claude/plans/feed-cache.md",
                   "content": "Cache the feed.\n\n# Context\nThe feed is slow.\n\n## Steps\n1. Add a cache\n```\n# not a heading\n```\n"}),
        )
        .as_bytes(),
    );
    let Some(Event::Panel(panel)) = events.first() else {
        panic!("no panel in {events:?}");
    };
    assert_eq!(panel.id, "plan");
    let section = &panel.sections[0];
    assert_eq!(
        section.title.as_deref(),
        Some("/home/v/.claude/plans/feed-cache.md")
    );
    let labels: Vec<_> = section.rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["Plan", "Context", "Steps"]);
    assert_eq!(section.rows[0].detail.as_deref(), Some("Cache the feed."));
    assert_eq!(
        section.rows[2].detail.as_deref(),
        Some("1. Add a cache\n```\n# not a heading\n```")
    );

    // `open` shows the plan too; no plan yet says so.
    let req = invoke(&mut d, "plan", "open").unwrap();
    assert_eq!(req[0]["request"]["subtype"], "get_plan");
    let events = d.on_output(control_reply(&req[0]["request_id"], json!({"exists": false})).as_bytes());
    let Some(Event::Panel(panel)) = events.first() else {
        panic!("no panel in {events:?}");
    };
    assert_eq!(panel.sections[0].rows[0].label, "No plan written yet");
}

#[test]
fn plan_open_outside_plan_mode_enters_it_then_shows_the_plan() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, HI);
    let enter = invoke(&mut d, "plan", "open").unwrap();
    assert_eq!(enter[0]["request"]["subtype"], "set_permission_mode");
    d.on_output(control_reply(&enter[0]["request_id"], json!({})).as_bytes());
    let next = sent(&d.poll_transmit());
    assert_eq!(next.len(), 1);
    assert_eq!(next[0]["request"]["subtype"], "get_plan");
}

#[test]
fn prompts_with_images_are_content_blocks() {
    let mut d = ClaudeDriver::default();
    replay(&mut d, HI);
    let png = Attachment::from_bytes("image/png", b"PNG");
    let prompt = sent(
        &d.command(Command::Prompt {
            text: "what is this".into(),
            attachments: vec![png.clone()],
        })
        .unwrap(),
    );
    assert_eq!(
        prompt[0]["message"]["content"],
        json!([
            {"type": "text", "text": "what is this"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "UE5H"}},
        ])
    );
    // The API refuses an empty text block, so an image-only prompt has none.
    let only = sent(
        &d.command(Command::Prompt {
            text: String::new(),
            attachments: vec![png],
        })
        .unwrap(),
    );
    assert_eq!(only[0]["message"]["content"].as_array().unwrap().len(), 1);
    assert!(matches!(
        d.command(Command::Prompt {
            text: String::new(),
            attachments: vec![Attachment::from_bytes(
                "image/png",
                &vec![0; Attachment::MAX_BYTES + 1]
            )],
        }),
        Err(DriverError::AttachmentRejected(_))
    ));

    // Replayed from the log, an image-only prompt still shows as a user item.
    let echoed = json!({"type": "user", "uuid": "img-1", "session_id": "s", "message": {"role": "user",
        "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "UE5H"}}]}});
    let events = d.on_output(format!("{echoed}\n").as_bytes());
    assert!(events.contains(&Event::ItemDelta {
        id: "img-1".into(),
        text: "[image]".into()
    }));
}

/// Claude Code 2.1.280 with `--forward-subagent-text`: a foreground and a
/// background `Agent` call to Explore, the background one finishing first.
const SUBAGENTS: &str = include_str!("../../tests/fixtures/claude/subagents.ndjson");

#[test]
fn subagents_are_items_with_their_conversations() {
    use crate::item::ItemDetail;
    use crate::transcript::{ItemStatus, Transcript};
    let mut d = ClaudeDriver::default();
    let mut t = Transcript::new();
    let mut background_running_after_launch = false;
    for line in SUBAGENTS.split_inclusive('\n') {
        for e in d.on_output(line.as_bytes()) {
            t.settle(&e);
            t.apply(&e);
        }
        if line.contains("Async agent launched") {
            background_running_after_launch =
                t.get("toolu_01EJj1wwkNi2uz6whj1RpML9").map(|i| i.status) == Some(ItemStatus::Running);
        }
    }
    assert!(
        background_running_after_launch,
        "a background launch isn't the agent's result"
    );
    let subs: Vec<_> = t.subagents().collect();
    assert_eq!(subs.len(), 2);
    for sub in &subs {
        assert_eq!(sub.status, ItemStatus::Completed);
        let ItemDetail::Subagent {
            agent,
            child,
            status,
            output,
            prompt,
            ..
        } = sub.detail(Harness::Claude)
        else {
            panic!("subagent detail")
        };
        assert_eq!(agent.as_deref(), Some("Explore"));
        assert_eq!(status.as_deref(), Some("completed"));
        assert!(prompt.is_some());
        assert!(child.is_some_and(|c| c.starts_with('a')));
        let output = output.unwrap();
        assert!(!output.contains("Subagent hand-back"), "{output}");
        // Each subagent's own conversation: its tool call and its answer.
        // (Claude forwards the prompt frame only for foreground agents; the
        // detail carries it either way.)
        let kinds: Vec<_> = t.children(&sub.id).map(|i| i.kind).collect();
        assert!(kinds.contains(&ItemKind::CommandExecution), "{kinds:?}");
        assert!(kinds.contains(&ItemKind::AgentMessage), "{kinds:?}");
    }
    let fg = t.get("toolu_01WqrBFboSUXpcf1Bb8qoLzE").unwrap();
    let ItemDetail::Subagent { output, child, .. } = fg.detail(Harness::Claude) else {
        panic!()
    };
    assert_eq!(child.as_deref(), Some("adc9fb778705a136d"));
    assert!(output.unwrap().starts_with("Here are all the files"));
    assert!(t.children(&fg.id).any(|i| i.kind == ItemKind::UserMessage));
    let bg = t.get("toolu_01EJj1wwkNi2uz6whj1RpML9").unwrap();
    let ItemDetail::Subagent { output, .. } = bg.detail(Harness::Claude) else {
        panic!()
    };
    assert!(output.unwrap().contains("3 lines"));
    // Subagent frames neither start turns nor count towards the main context.
    assert!(t.top_level().all(|i| i.parent.is_none()));
    assert_eq!(
        t.top_level().filter(|i| i.kind == ItemKind::UserMessage).count(),
        1
    );
}

#[test]
fn launch_forwards_subagent_text() {
    let spec = ClaudeDriver::default().launch(None);
    assert!(spec.args.iter().any(|a| a == "--forward-subagent-text"));
}

/// Claude Code 2.1.280: a turn launches a background `Agent` call and ends
/// before it reports back; the report then wakes the model for a second turn
/// (no `running` state of its own), and only after that is the session idle.
const BACKGROUND_WAKES: &str = include_str!("../../tests/fixtures/claude/background-agent-wakes.ndjson");

#[test]
fn a_turn_left_waiting_on_a_background_agent_is_not_done() {
    use crate::transcript::{ItemStatus, Transcript};
    let mut d = ClaudeDriver::default();
    let mut t = Transcript::new();
    let mut states = Vec::new();
    let mut agent_while_waiting = None;
    for line in BACKGROUND_WAKES.split_inclusive('\n') {
        for e in d.on_output(line.as_bytes()) {
            t.settle(&e);
            t.apply(&e);
            if let Event::StateChanged(s) = &e {
                states.push(s.state.clone());
                if s.state == RunState::Waiting {
                    agent_while_waiting = t.subagents().next().map(|i| i.status);
                }
            }
        }
    }
    assert_eq!(
        states,
        [
            RunState::Working { progress: None },
            RunState::Waiting,
            RunState::Working { progress: None },
            RunState::Done,
        ]
    );
    assert_eq!(agent_while_waiting, Some(ItemStatus::Running));
    assert!(t.subagents().all(|i| i.status == ItemStatus::Completed));
}

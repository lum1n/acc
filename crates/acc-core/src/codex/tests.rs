//! Replays recorded `codex app-server` 0.162.1 sessions through the driver.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

const HANDSHAKE: &str = include_str!("../../tests/fixtures/codex/handshake.ndjson");
const TURN: &str = include_str!("../../tests/fixtures/codex/turn-network-error.ndjson");
const THREAD: &str = "01a125ba-bc10-7a62-8401-c7b0088728d3";

fn replay(d: &mut CodexDriver, recording: &str) -> Vec<Event> {
    let mut out = Vec::new();
    for chunk in recording.as_bytes().chunks(733) {
        out.extend(d.on_output(chunk));
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

fn started() -> CodexDriver {
    let mut d = CodexDriver::default();
    d.handshake();
    replay(&mut d, HANDSHAKE);
    d
}

fn feed(d: &mut CodexDriver, v: Value) -> Vec<Event> {
    d.on_output(&wire::line(&v))
}

fn option(d: &CodexDriver, id: OptionId) -> &OptionDef {
    d.options().iter().find(|o| o.id == id).unwrap()
}

#[test]
fn handshake_pipelines_initialize_thread_and_discovery() {
    let mut d = CodexDriver::new(CodexConfig {
        cwd: Some("/home/v/proj".into()),
        ..CodexConfig::default()
    });
    let msgs = sent(&d.handshake());
    let methods: Vec<_> = msgs.iter().map(|m| m["method"].as_str().unwrap()).collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "initialized",
            "thread/start",
            "model/list",
            "permissionProfile/list",
            "collaborationMode/list",
            "account/rateLimits/read",
            "skills/list"
        ]
    );
    assert!(msgs.iter().all(|m| m.get("jsonrpc").is_none()));
    assert_eq!(msgs[0]["params"]["capabilities"]["experimentalApi"], true);
    assert!(msgs[1].get("id").is_none());
    assert_eq!(msgs[2]["params"]["cwd"], "/home/v/proj");
    // Parameterless methods omit `params`.
    assert!(msgs[6].get("params").is_none());
}

#[test]
fn resume_uses_thread_resume() {
    let mut d = CodexDriver::new(CodexConfig {
        resume: Some(THREAD.into()),
        ..CodexConfig::default()
    });
    let msgs = sent(&d.handshake());
    assert_eq!(msgs[2]["method"], "thread/resume");
    assert_eq!(msgs[2]["params"]["threadId"], THREAD);
}

#[test]
fn recorded_handshake_starts_session_and_fills_options() {
    let mut d = CodexDriver::default();
    d.handshake();
    let events = replay(&mut d, HANDSHAKE);

    assert!(events.iter().any(|e| matches!(e,
        Event::SessionStarted { harness: Harness::Codex, native_id, model: Some(m) }
            if native_id == THREAD && m == "gpt-6.1-sol")));
    assert_eq!(d.thread_id(), Some(THREAD));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Notice { message } if message.contains("bubblewrap")))
    );
    // Missing ChatGPT auth for rate limits is not surfaced as an error.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Notice { message } if message.contains("rate limits")))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::StateChanged(s) if s.state == RunState::Idle))
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Unrecognized { .. })),
        "{events:#?}"
    );

    let model = option(&d, OptionId::Model);
    assert_eq!(model.current.as_deref(), Some("gpt-6.1-sol"));
    assert!(model.values.iter().any(|v| v == "gpt-6.1-sol"));
    let effort = option(&d, OptionId::Effort);
    assert!(effort.values.iter().any(|v| v == "high"));
    assert!(effort.current.is_some(), "falls back to the model default");
    assert_eq!(option(&d, OptionId::Mode).values, ["plan", "default"]);
    let perm = option(&d, OptionId::Permission);
    assert_eq!(perm.values, [":read-only", ":workspace", ":danger-full-access"]);
    assert_eq!(perm.current.as_deref(), Some(":read-only"));
}

#[test]
fn recorded_turn_streams_user_item_and_retry_state() {
    let mut d = CodexDriver::default();
    d.handshake();
    let events = replay(&mut d, TURN);
    let user = "01a125bb-51f4-75c3-a8d1-cd794f5996fe";
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ItemStarted { id, kind: ItemKind::UserMessage, .. } if id == user))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ItemDelta { id, text } if id == user && text == "hi"))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ItemCompleted { id, .. } if id == user))
    );
    // Retries keep the turn working, with the retry message.
    assert!(events.iter().any(|e| matches!(e,
        Event::StateChanged(s) if matches!(s.state, RunState::Working { .. }) && s.message.as_deref() == Some("Reconnecting... 5/5"))));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Notice { message } if message.starts_with("Falling back")))
    );

    // A prompt during a running turn steers it.
    let steer = sent(
        &d.command(Command::Prompt {
            text: "also this".into(),
        })
        .unwrap(),
    );
    assert_eq!(steer[0]["method"], "turn/steer");
    assert_eq!(
        steer[0]["params"]["expectedTurnId"],
        "01a125bb-51ae-7f72-9283-b6d03498333d"
    );
    let interrupt = sent(&d.command(Command::Interrupt).unwrap());
    assert_eq!(interrupt[0]["method"], "turn/interrupt");
}

#[test]
fn prompt_sends_turn_start_with_pending_option_changes_once() {
    let mut d = started();
    assert!(
        d.command(Command::SetOption {
            id: OptionId::Effort,
            value: "high".into()
        })
        .unwrap()
        .is_empty()
    );
    d.command(Command::SetOption {
        id: OptionId::Mode,
        value: "plan".into(),
    })
    .unwrap();
    d.command(Command::SetOption {
        id: OptionId::Permission,
        value: ":workspace".into(),
    })
    .unwrap();
    assert_eq!(option(&d, OptionId::Effort).current.as_deref(), Some("high"));
    assert_eq!(
        d.command(Command::SetOption {
            id: OptionId::Permission,
            value: ":root".into()
        }),
        Err(DriverError::InvalidOption(":root".into()))
    );

    let first = sent(&d.command(Command::Prompt { text: "go".into() }).unwrap());
    let p = &first[0]["params"];
    assert_eq!(first[0]["method"], "turn/start");
    assert_eq!(p["threadId"], THREAD);
    assert_eq!(
        p["input"][0],
        json!({"type": "text", "text": "go", "text_elements": []})
    );
    assert_eq!(p["effort"], "high");
    assert_eq!(p["permissions"], ":workspace");
    assert_eq!(p["collaborationMode"]["mode"], "plan");
    assert_eq!(p["collaborationMode"]["settings"]["model"], "gpt-6.1-sol");

    // Codex keeps them sticky, so the next turn doesn't resend them.
    feed(
        &mut d,
        json!({"method": "turn/completed", "params": {"threadId": THREAD, "turn": {"id": "t1", "status": "completed"}}}),
    );
    let second = sent(&d.command(Command::Prompt { text: "again".into() }).unwrap());
    assert!(second[0]["params"].get("effort").is_none());
    assert_eq!(option(&d, OptionId::Mode).current.as_deref(), Some("plan"));
}

#[test]
fn turn_lifecycle_and_sticky_done() {
    let mut d = started();
    let mut events = feed(
        &mut d,
        json!({"method": "turn/started", "params": {"threadId": THREAD, "turn": {"id": "t1"}}}),
    );
    events.extend(feed(
        &mut d,
        json!({"method": "item/started", "params": {"threadId": THREAD, "turnId": "t1",
        "item": {"type": "agentMessage", "id": "a1", "text": ""}}}),
    ));
    events.extend(feed(&mut d, json!({"method": "item/agentMessage/delta", "params": {"threadId": THREAD, "turnId": "t1", "itemId": "a1", "delta": "Hel"}})));
    events.extend(feed(
        &mut d,
        json!({"method": "item/completed", "params": {"threadId": THREAD, "turnId": "t1",
        "item": {"type": "agentMessage", "id": "a1", "text": "Hello"}}}),
    ));
    events.extend(feed(&mut d, json!({"method": "thread/tokenUsage/updated", "params": {"threadId": THREAD, "turnId": "t1", "tokenUsage": {
        "total": {"totalTokens": 120, "inputTokens": 100, "cachedInputTokens": 60, "cacheWriteInputTokens": 0, "outputTokens": 20, "reasoningOutputTokens": 5},
        "last": {"totalTokens": 90, "inputTokens": 80, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 10, "reasoningOutputTokens": 0},
        "modelContextWindow": 258400}}})));
    events.extend(feed(&mut d, json!({"method": "turn/completed", "params": {"threadId": THREAD, "turn": {"id": "t1", "status": "completed"}}})));
    events.extend(feed(&mut d, json!({"method": "thread/status/changed", "params": {"threadId": THREAD, "status": {"type": "idle"}}})));

    assert!(events.contains(&Event::ItemStarted {
        id: "a1".into(),
        kind: ItemKind::AgentMessage,
        extensions: json!({"type": "agentMessage", "id": "a1", "text": ""})
    }));
    assert!(events.contains(&Event::ItemDelta {
        id: "a1".into(),
        text: "Hel".into()
    }));
    assert!(events.contains(&Event::Usage(Usage {
        input_tokens: Some(100),
        output_tokens: Some(20),
        cache_read_tokens: Some(60),
        cache_write_tokens: Some(0),
        cost_usd: None,
        context_used: Some(90),
        context_window: Some(258_400),
    })));
    assert!(events.contains(&Event::TurnCompleted));
    let last_state = events.iter().rev().find_map(|e| match e {
        Event::StateChanged(s) => Some(s.state.clone()),
        _ => None,
    });
    assert_eq!(last_state, Some(RunState::Done));
    // No active turn: interrupt is a no-op.
    assert!(d.command(Command::Interrupt).unwrap().is_empty());
}

#[test]
fn failed_turn_is_error_with_message() {
    let mut d = started();
    let events = feed(
        &mut d,
        json!({"method": "turn/completed", "params": {"threadId": THREAD,
        "turn": {"id": "t1", "status": "failed", "error": {"message": "stream disconnected\nmore"}}}}),
    );
    assert!(events.iter().any(|e| matches!(e,
        Event::StateChanged(s) if s.state == RunState::Error && s.message.as_deref() == Some("stream disconnected"))));
}

#[test]
fn command_approval_round_trip() {
    let mut d = started();
    let events = feed(
        &mut d,
        json!({"id": 0, "method": "item/commandExecution/requestApproval", "params": {
        "kind": "command", "threadId": THREAD, "turnId": "t1", "itemId": "c1", "startedAtMs": 1,
        "environmentId": null, "command": "cargo test", "availableDecisions": ["accept", "decline", {"acceptWithExecpolicyAmendment": {}}]}}),
    );
    let (rid, title, options) = events
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
        .unwrap();
    assert_eq!(rid, "0");
    assert_eq!(title, "Run: cargo test");
    assert_eq!(
        options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        ["accept", "decline"]
    );
    assert!(events.iter().any(|e| matches!(e, Event::StateChanged(s) if s.state == RunState::Blocked { kind: BlockKind::Permission })));

    assert_eq!(
        d.command(Command::AnswerPermission {
            request_id: rid.clone(),
            option_id: "maybe".into()
        }),
        Err(DriverError::InvalidOption("maybe".into()))
    );
    let reply = sent(
        &d.command(Command::AnswerPermission {
            request_id: rid.clone(),
            option_id: "accept".into(),
        })
        .unwrap(),
    );
    assert_eq!(reply[0], json!({"id": 0, "result": {"decision": "accept"}}));

    let events = feed(
        &mut d,
        json!({"method": "serverRequest/resolved", "params": {"threadId": THREAD, "requestId": 0}}),
    );
    assert!(events.is_empty(), "already answered here");
}

#[test]
fn request_resolved_elsewhere_is_reported() {
    let mut d = started();
    feed(
        &mut d,
        json!({"id": "r9", "method": "item/fileChange/requestApproval", "params": {
        "threadId": THREAD, "turnId": "t1", "itemId": "f1", "startedAtMs": 1}}),
    );
    let events = feed(
        &mut d,
        json!({"method": "serverRequest/resolved", "params": {"threadId": THREAD, "requestId": "r9"}}),
    );
    assert_eq!(
        events,
        [Event::PermissionResolved {
            request_id: "r9".into()
        }]
    );
}

#[test]
fn user_input_and_permission_grants() {
    let mut d = started();
    feed(
        &mut d,
        json!({"id": 5, "method": "item/tool/requestUserInput", "params": {
        "threadId": THREAD, "turnId": "t1", "itemId": "u1", "isBlocking": true, "autoResolutionMs": null,
        "questions": [{"id": "q1", "header": "DB", "question": "Which DB?", "isOther": false, "isSecret": false,
            "options": [{"label": "Postgres", "description": ""}]}]}}),
    );
    let reply = sent(
        &d.command(Command::AnswerPermission {
            request_id: "5".into(),
            option_id: "Postgres".into(),
        })
        .unwrap(),
    );
    assert_eq!(
        reply[0]["result"],
        json!({"answers": {"q1": {"answers": ["Postgres"]}}})
    );

    feed(
        &mut d,
        json!({"id": 6, "method": "item/permissions/requestApproval", "params": {
        "threadId": THREAD, "turnId": "t1", "itemId": "p1", "environmentId": null, "startedAtMs": 1, "cwd": "/w",
        "reason": "needs network", "permissions": {"network": {"enabled": true}, "fileSystem": null}}}),
    );
    let reply = sent(
        &d.command(Command::AnswerPermission {
            request_id: "6".into(),
            option_id: "session".into(),
        })
        .unwrap(),
    );
    assert_eq!(
        reply[0]["result"],
        json!({"permissions": {"network": {"enabled": true}}, "scope": "session"})
    );
}

#[test]
fn unknown_server_requests_are_refused_not_ignored() {
    let mut d = started();
    let events = feed(
        &mut d,
        json!({"id": 11, "method": "item/tool/call", "params": {}}),
    );
    assert!(events.is_empty());
    let reply = sent(&d.poll_transmit());
    assert_eq!(reply[0]["id"], 11);
    assert_eq!(reply[0]["error"]["code"], -32601);
}

#[test]
fn canonical_commands_map_to_rpcs() {
    let mut d = started();
    let method = |d: &mut CodexDriver, c: Command| {
        sent(&d.command(c).unwrap())[0]["method"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(
        method(&mut d, Command::Compact { hint: None }),
        "thread/compact/start"
    );
    assert_eq!(method(&mut d, Command::Fork), "thread/fork");
    assert_eq!(method(&mut d, Command::New), "thread/start");
    assert_eq!(
        method(&mut d, Command::Rename { name: "x".into() }),
        "thread/name/set"
    );
    assert_eq!(
        method(&mut d, Command::Shell { command: "ls".into() }),
        "thread/shellCommand"
    );
    let review = sent(
        &d.command(Command::Invoke {
            name: "review".into(),
            args: "focus on auth".into(),
        })
        .unwrap(),
    );
    assert_eq!(
        review[0]["params"]["target"],
        json!({"type": "custom", "instructions": "focus on auth"})
    );
    let review = sent(&d.command(Command::Review).unwrap());
    assert_eq!(review[0]["params"]["target"]["type"], "uncommittedChanges");
    let refresh = sent(&d.command(Command::Refresh).unwrap());
    assert_eq!(refresh.len(), 3);
}

#[test]
fn fork_switches_session() {
    let mut d = started();
    let req = sent(&d.command(Command::Fork).unwrap());
    let id = req[0]["id"].clone();
    let events = feed(
        &mut d,
        json!({"id": id, "result": {"thread": {"id": "new-thread", "turns": []}, "model": "gpt-6.1-sol"}}),
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::SessionStarted { native_id, .. } if native_id == "new-thread"))
    );
    assert_eq!(d.thread_id(), Some("new-thread"));
}

#[test]
fn skills_become_palette_entries_and_skill_items() {
    let mut d = started();
    // skills/list is request 7 of the handshake.
    let events = feed(
        &mut d,
        json!({"id": 7, "result": {"data": [{"cwd": "/w", "errors": [], "skills": [
        {"name": "deploy", "description": "Deploy the app", "path": "/w/.agents/skills/deploy/SKILL.md", "scope": "repo", "enabled": true, "pluginId": null},
        {"name": "off", "description": "", "path": "/x", "scope": "user", "enabled": false, "pluginId": null}]}]}}),
    );
    assert!(events.contains(&Event::CommandsChanged));
    let deploy = d.commands().iter().find(|c| c.name == "deploy").unwrap();
    assert_eq!(
        (deploy.source, &deploy.exec),
        (CommandSource::Skill, &Exec::SkillItem)
    );
    assert!(!d.commands().iter().any(|c| c.name == "off"));

    let turn = sent(
        &d.command(Command::Invoke {
            name: "deploy".into(),
            args: "staging".into(),
        })
        .unwrap(),
    );
    assert_eq!(
        turn[0]["params"]["input"][0],
        json!({"type": "skill", "name": "deploy", "path": "/w/.agents/skills/deploy/SKILL.md"})
    );
    assert_eq!(turn[0]["params"]["input"][1]["text"], "$deploy staging");
    assert!(matches!(
        d.command(Command::Invoke {
            name: "nope".into(),
            args: String::new()
        }),
        Err(DriverError::Unsupported { .. })
    ));

    // skills/changed triggers a refetch on the transmit queue.
    feed(&mut d, json!({"method": "skills/changed", "params": {}}));
    assert_eq!(sent(&d.poll_transmit())[0]["method"], "skills/list");
}

#[test]
fn rate_limits_become_quota() {
    let mut d = started();
    let events = feed(
        &mut d,
        json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {
        "limitId": null, "limitName": null, "normalModelSlug": null,
        "primary": {"usedPercent": 42.0, "windowDurationMins": 300, "resetsAt": 1791652200},
        "secondary": {"usedPercent": 7.5, "windowDurationMins": 10080, "resetsAt": 1791853200},
        "credits": null, "individualLimit": null, "spendControlReached": null, "planType": "plus", "rateLimitReachedType": null}}}),
    );
    let Event::Quota(q) = &events[0] else {
        panic!("quota")
    };
    assert_eq!(q.plan.as_deref(), Some("plus"));
    assert_eq!(q.windows[0].used_percent, Some(42.0));
    assert_eq!(q.windows[1].window_minutes, Some(10080));
}

#[test]
fn resumed_history_is_replayed_as_items() {
    let mut d = CodexDriver::new(CodexConfig {
        resume: Some("t".into()),
        ..CodexConfig::default()
    });
    d.handshake();
    let events = feed(
        &mut d,
        json!({"id": 2, "result": {"model": "gpt-6.1-sol", "thread": {"id": "t", "turns": [{"id": "u", "status": "completed", "items": [
        {"type": "userMessage", "id": "i1", "content": [{"type": "text", "text": "fix it", "text_elements": []}]},
        {"type": "agentMessage", "id": "i2", "text": "Fixed."}]}]}}}),
    );
    assert!(events.contains(&Event::ItemDelta {
        id: "i1".into(),
        text: "fix it".into()
    }));
    assert!(events.contains(&Event::ItemDelta {
        id: "i2".into(),
        text: "Fixed.".into()
    }));
}

#[test]
fn prompt_before_thread_is_not_started() {
    let mut d = CodexDriver::default();
    assert_eq!(
        d.command(Command::Prompt { text: "x".into() }),
        Err(DriverError::NotStarted)
    );
}

#[test]
fn client_tag_makes_string_ids_and_attach_skips_initialize() {
    let mut d = CodexDriver::new(CodexConfig {
        client_tag: Some("ph".into()),
        ..CodexConfig::default()
    });
    let msgs = sent(&d.handshake());
    assert_eq!(msgs[0]["id"], "ph-1");
    let attach = sent(&d.attach_handshake());
    assert!(
        attach
            .iter()
            .all(|m| m["id"].as_str().is_some_and(|i| i.starts_with("ph-")))
    );
    let methods: Vec<_> = attach.iter().map(|m| m["method"].as_str().unwrap()).collect();
    assert!(
        !methods
            .iter()
            .any(|m| *m == "initialize" || m.starts_with("thread/"))
    );
    assert!(methods.contains(&"model/list"));
}

#[test]
fn late_client_adopts_thread_from_shared_log_and_ignores_foreign_errors() {
    let mut late = CodexDriver::new(CodexConfig {
        client_tag: Some("late".into()),
        ..CodexConfig::default()
    });
    let events = replay(&mut late, HANDSHAKE);
    assert_eq!(late.thread_id(), Some(THREAD));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::SessionStarted { native_id, .. } if native_id == THREAD))
    );
    assert!(feed(&mut late, json!({"id": 9, "error": {"message": "nope"}})).is_empty());
}

#[test]
fn turn_diff_is_passed_through() {
    let mut d = started();
    let events = feed(
        &mut d,
        json!({"method": "turn/diff/updated", "params": {"threadId": THREAD, "turnId": "t1", "diff": "diff --git a/x b/x\n"}}),
    );
    assert!(events.iter().any(|e| matches!(e, Event::Native { kind, payload } if kind == "turn_diff" && payload["diff"].as_str().is_some())));
}

fn invoke(d: &mut CodexDriver, name: &str, args: &str) -> Result<Vec<Value>, DriverError> {
    d.command(Command::Invoke {
        name: name.into(),
        args: args.into(),
    })
    .map(|b| sent(&b))
}

fn panel(events: &[Event]) -> &Panel {
    events
        .iter()
        .find_map(|e| match e {
            Event::Panel(p) => Some(p),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no panel in {events:?}"))
}

#[test]
fn option_commands_become_pickers() {
    let mut d = started();
    for (name, id) in [("model", OptionId::Model), ("effort", OptionId::Effort)] {
        let entry = d.commands().iter().find(|c| c.name == name).unwrap();
        assert_eq!(entry.input, CommandInput::Option { id });
    }
    let model = option(&d, OptionId::Model).values[0].clone();
    assert_eq!(invoke(&mut d, "model", &model).unwrap(), Vec::<Value>::new());
    assert_eq!(
        option(&d, OptionId::Model).current.as_deref(),
        Some(model.as_str())
    );
    assert!(matches!(
        invoke(&mut d, "effort", ""),
        Err(DriverError::MissingArgument(_))
    ));
}

#[test]
fn mcp_panel_lists_servers_and_logs_in() {
    let mut d = started();
    let req = invoke(&mut d, "mcp", "").unwrap();
    assert_eq!(req[0]["method"], "mcpServerStatus/list");
    assert_eq!(req[0]["params"]["detail"], "toolsAndAuthOnly");
    // Shapes from `codex app-server generate-ts` (ListMcpServerStatusResponse).
    let events = feed(
        &mut d,
        json!({"id": req[0]["id"], "result": {"data": [
            {"name": "linear", "tools": {"a": {}, "b": {}}, "resources": [], "resourceTemplates": [], "authStatus": "notLoggedIn"},
            {"name": "fs", "tools": {}, "resources": [], "resourceTemplates": [], "authStatus": "unsupported"},
        ], "nextCursor": null}}),
    );
    let p = panel(&events);
    assert_eq!(p.sections[0].rows[0].value.as_deref(), Some("2 tools"));
    assert_eq!(p.sections[0].rows[0].actions[0].args, "login linear");
    assert!(p.sections[0].rows[1].actions.is_empty());
    assert_eq!(p.sections[0].actions[0].args, "reload");

    let login = invoke(&mut d, "mcp", "login linear").unwrap();
    assert_eq!(login[0]["method"], "mcpServer/oauth/login");
    assert_eq!(login[0]["params"]["name"], "linear");
    let events = feed(
        &mut d,
        json!({"id": login[0]["id"], "result": {"authorizationUrl": "https://auth.example/x"}}),
    );
    assert!(events.contains(&Event::Notice {
        message: "Open to log in: https://auth.example/x".into()
    }));

    let reload = invoke(&mut d, "mcp", "reload").unwrap();
    assert_eq!(reload[0]["method"], "config/mcpServer/reload");
    feed(&mut d, json!({"id": reload[0]["id"], "result": {}}));
    assert_eq!(sent(&d.poll_transmit())[0]["method"], "mcpServerStatus/list");
}

#[test]
fn experimental_panel_toggles_features() {
    let mut d = started();
    let req = invoke(&mut d, "experimental", "").unwrap();
    let events = feed(
        &mut d,
        json!({"id": req[0]["id"], "result": {"data": [
            {"name": "web_search", "stage": "beta", "displayName": "Web search", "description": "Search the web",
             "announcement": null, "enabled": false, "defaultEnabled": false},
            {"name": "internal", "stage": "underDevelopment", "displayName": null, "description": null,
             "announcement": null, "enabled": false, "defaultEnabled": false},
        ], "nextCursor": null}}),
    );
    let rows = &panel(&events).sections[0].rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "Web search");
    assert_eq!(rows[0].actions[0].args, "enable web_search");

    let set = invoke(&mut d, "experimental", "enable web_search").unwrap();
    assert_eq!(set[0]["method"], "experimentalFeature/enablement/set");
    assert_eq!(set[0]["params"]["enablement"]["web_search"], true);
}

#[test]
fn status_panel_shows_account_and_options() {
    let mut d = started();
    let req = invoke(&mut d, "status", "").unwrap();
    assert_eq!(req[0]["method"], "account/read");
    let events = feed(
        &mut d,
        json!({"id": req[0]["id"], "result": {"account": {"type": "chatgpt", "email": "v@example.com", "planType": "pro"}, "requiresOpenaiAuth": true}}),
    );
    let rows = &panel(&events).sections[0].rows;
    assert_eq!(rows[0].value.as_deref(), Some("ChatGPT (v@example.com)"));
    assert_eq!(rows[1].value.as_deref(), Some("pro"));
    assert!(rows.iter().any(|r| r.label == "Model"));
}

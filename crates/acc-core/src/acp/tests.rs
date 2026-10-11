//! Replay tests: scripted agent traffic in, events and wire bytes out. The
//! message shapes follow the ACP 1.8.0 schema and what Copilot 1.0.95 sent
//! when probed (see `research/commands-and-options/cursor-copilot.md`).

use serde_json::{Value, json};

use super::*;
use crate::command::Apply;

fn driver(harness: Harness, start: Start) -> AcpDriver {
    AcpDriver::new(harness, AcpConfig::new("/home/v/proj", start))
}

/// Parses NDJSON the driver wrote.
fn sent(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect()
}

fn one(bytes: &[u8]) -> Value {
    let mut v = sent(bytes);
    assert_eq!(v.len(), 1, "expected one message, got {v:?}");
    v.remove(0)
}

fn feed(d: &mut AcpDriver, msgs: &[Value]) -> Vec<Event> {
    let mut bytes = Vec::new();
    for m in msgs {
        bytes.extend(serde_json::to_vec(m).expect("json"));
        bytes.push(b'\n');
    }
    d.on_output(&bytes)
}

fn update(session: &str, update: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": session, "update": update } })
}

fn result(id: impl Into<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.into(), "result": result })
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

fn working() -> RunState {
    RunState::Working { progress: None }
}

fn text_chunk(kind: &str, text: &str) -> Value {
    json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } })
}

fn init_result(caps: Value) -> Value {
    json!({
        "protocolVersion": 1,
        "agentCapabilities": caps,
        "authMethods": [{ "id": "copilot-login", "name": "Log in with Copilot" }],
        "agentInfo": { "name": "copilot", "version": "1.0.95" },
    })
}

fn copilot_session(session: &str) -> Value {
    json!({
        "sessionId": session,
        "configOptions": [
            { "id": "mode", "name": "Mode", "category": "mode", "type": "select",
              "currentValue": "https://agentclientprotocol.com/protocol/session-modes#agent",
              "options": [
                { "value": "https://agentclientprotocol.com/protocol/session-modes#agent", "name": "Agent" },
                { "value": "https://agentclientprotocol.com/protocol/session-modes#plan", "name": "Plan" } ] },
            { "id": "model", "name": "Model", "category": "model", "type": "select",
              "currentValue": "gpt-5.4",
              "options": [ { "value": "gpt-5.4", "name": "GPT-5.4" }, { "value": "claude-sonnet-5", "name": "Sonnet" } ] },
            { "id": "allow_all", "name": "Allow all", "category": "permissions", "type": "boolean", "currentValue": false }
        ]
    })
}

/// Runs `initialize` and `session/new` for a Copilot-like agent.
fn ready_copilot() -> AcpDriver {
    let mut d = driver(Harness::Copilot, Start::New);
    let init = one(&d.handshake());
    feed(
        &mut d,
        &[result(
            init["id"].clone(),
            init_result(json!({ "loadSession": true })),
        )],
    );
    let new = one(&d.poll_transmit());
    feed(
        &mut d,
        &[
            update(
                "s1",
                json!({ "sessionUpdate": "available_commands_update", "availableCommands": [
                    { "name": "compact", "description": "Summarize the conversation" },
                    { "name": "rename", "description": "Rename the session", "input": { "hint": "name" } },
                    { "name": "review", "description": "Review changes" } ] }),
            ),
            result(new["id"].clone(), copilot_session("s1")),
        ],
    );
    d
}

#[test]
fn handshake_opens_a_new_session() {
    let mut d = driver(Harness::Copilot, Start::New);
    let init = one(&d.handshake());
    assert_eq!(init["method"], "initialize");
    assert_eq!(init["id"], "acc-0");
    assert_eq!(init["params"]["protocolVersion"], 1);
    assert_eq!(init["params"]["clientCapabilities"]["fs"]["readTextFile"], false);
    assert!(d.handshake().is_empty(), "initialize is sent once");

    let events = feed(
        &mut d,
        &[result("acc-0", init_result(json!({ "loadSession": true })))],
    );
    assert!(events.is_empty());
    let new = one(&d.poll_transmit());
    assert_eq!(new["method"], "session/new");
    assert_eq!(new["params"]["cwd"], "/home/v/proj");
    assert_eq!(new["params"]["mcpServers"], json!([]));

    // Commands for the new session can arrive before its response.
    let events = feed(
        &mut d,
        &[
            update(
                "s1",
                json!({ "sessionUpdate": "available_commands_update",
                        "availableCommands": [{ "name": "compact", "description": "Compact" }] }),
            ),
            result(new["id"].clone(), copilot_session("s1")),
        ],
    );
    assert_eq!(events[0], Event::CommandsChanged);
    assert!(events.contains(&Event::SessionStarted {
        harness: Harness::Copilot,
        native_id: "s1".into(),
        model: Some("gpt-5.4".into()),
    }));
    assert_eq!(states(&events), [RunState::Idle]);
    assert_eq!(d.session_id(), Some("s1"));
    assert_eq!(d.auth_methods()[0].0, "copilot-login");

    let names: Vec<_> = d.commands().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["compact", "interrupt", "new", "model", "mode", "permissions"]
    );
    assert_eq!(d.commands()[0].exec, Exec::Text);
    assert_eq!(d.commands()[1].source, CommandSource::Acc);
    // Copilot reports model, mode and allow-all as options: each gets a picker.
    assert_eq!(
        d.commands()[3].input,
        CommandInput::Option { id: OptionId::Model }
    );

    let ids: Vec<_> = d.options().iter().map(|o| o.id).collect();
    assert_eq!(ids, [OptionId::Mode, OptionId::Model, OptionId::Permission]);
}

#[test]
fn a_full_turn_with_a_permission_prompt() {
    let mut d = ready_copilot();
    let prompt = one(&d
        .command(Command::Prompt {
            text: "fix the build".into(),
        })
        .expect("prompt"));
    assert_eq!(prompt["method"], "session/prompt");
    assert_eq!(prompt["params"]["sessionId"], "s1");
    assert_eq!(
        prompt["params"]["prompt"][0],
        json!({ "type": "text", "text": "fix the build" })
    );

    let events = feed(
        &mut d,
        &[
            update("s1", text_chunk("agent_thought_chunk", "Looking")),
            update("s1", text_chunk("agent_message_chunk", "I'll run ")),
            update("s1", text_chunk("agent_message_chunk", "cargo.")),
            update(
                "s1",
                json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "cargo build",
                        "kind": "execute", "status": "pending", "rawInput": { "command": "cargo build" } }),
            ),
            json!({ "jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {
                "sessionId": "s1",
                "toolCall": { "toolCallId": "t1", "title": "cargo build" },
                "options": [
                    { "optionId": "allow-once", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" } ] } }),
        ],
    );
    assert_eq!(
        states(&events),
        [
            working(),
            RunState::Blocked {
                kind: BlockKind::Permission
            }
        ]
    );
    let Some(Event::StateChanged(s)) = events.iter().find(|e| matches!(e, Event::StateChanged(_))) else {
        unreachable!()
    };
    assert_eq!(s.source, StateSource::Native);
    assert_eq!(s.observed_at_ms, 0);

    let kinds: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::ItemStarted { id, kind, .. } => Some((id.as_str(), *kind)),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        [
            ("item-1", ItemKind::Reasoning),
            ("item-2", ItemKind::AgentMessage),
            ("t1", ItemKind::CommandExecution)
        ]
    );
    assert!(events.contains(&Event::ItemDelta {
        id: "item-2".into(),
        text: "cargo.".into()
    }));
    assert!(events.contains(&Event::ItemCompleted {
        id: "item-2".into(),
        extensions: Value::Null
    }));
    assert!(events.iter().any(|e| matches!(e, Event::PermissionRequested {
        request_id, title, options, kind: BlockKind::Permission, item_id, ..
    } if request_id == "7" && title == "cargo build" && item_id.is_some() && options == &vec![
            PermissionOption {
                id: "allow-once".into(),
                label: "Allow".into()
            },
            PermissionOption {
                id: "reject-once".into(),
                label: "Reject".into()
            },
        ])));

    // The answer echoes the agent's numeric id.
    let answer = one(&d
        .command(Command::AnswerPermission {
            request_id: "7".into(),
            option_id: "allow-once".into(),
        })
        .expect("answer"));
    assert_eq!(
        answer,
        json!({ "jsonrpc": "2.0", "id": 7, "result": { "outcome": { "outcome": "selected", "optionId": "allow-once" } } })
    );
    assert!(matches!(
        d.command(Command::AnswerPermission {
            request_id: "7".into(),
            option_id: "allow-once".into()
        }),
        Err(DriverError::UnknownRequest(_))
    ));

    let events = feed(
        &mut d,
        &[
            update(
                "s1",
                json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress" }),
            ),
            update(
                "s1",
                json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                        "content": [{ "type": "content", "content": { "type": "text", "text": "ok" } }] }),
            ),
            update("s1", text_chunk("agent_message_chunk", "Fixed.")),
            update(
                "s1",
                json!({ "sessionUpdate": "usage_update", "used": 53000, "size": 200000,
                                 "cost": { "amount": 0.12, "currency": "USD" } }),
            ),
            result(
                prompt["id"].clone(),
                json!({ "stopReason": "end_turn", "usage": { "totalTokens": 1500, "inputTokens": 1200,
                        "outputTokens": 300, "cachedReadTokens": 1000 } }),
            ),
        ],
    );
    assert_eq!(states(&events), [working(), RunState::Done]);
    assert!(events.contains(&Event::ItemUpdated {
        id: "t1".into(),
        extensions: json!({ "toolCallId": "t1", "status": "in_progress" }),
    }));
    assert!(
        matches!(&events[2], Event::ItemCompleted { id, extensions } if id == "t1" && extensions["status"] == "completed")
    );
    assert!(events.contains(&Event::Usage(Usage {
        context_used: Some(53_000),
        context_window: Some(200_000),
        cost_usd: Some(0.12),
        ..Usage::default()
    })));
    assert!(events.contains(&Event::Usage(Usage {
        input_tokens: Some(1200),
        output_tokens: Some(300),
        cache_read_tokens: Some(1000),
        ..Usage::default()
    })));
    assert_eq!(events.last(), Some(&Event::TurnCompleted));
}

#[test]
fn split_reads_give_the_same_events() {
    let msgs = [
        update("s1", text_chunk("agent_message_chunk", "héllo ✓")),
        update("s1", json!({ "sessionUpdate": "plan", "entries": [] })),
        result("acc-1", json!({ "stopReason": "end_turn" })),
    ];
    let mut whole = ready_copilot();
    whole
        .command(Command::Prompt { text: "x".into() })
        .expect("prompt");
    let expected = feed(&mut whole, &msgs);

    let mut bytes = Vec::new();
    for m in &msgs {
        bytes.extend(serde_json::to_vec(m).expect("json"));
        bytes.push(b'\n');
    }
    for size in [1, 2, 7, 64] {
        let mut d = ready_copilot();
        d.command(Command::Prompt { text: "x".into() }).expect("prompt");
        let got: Vec<_> = bytes.chunks(size).flat_map(|c| d.on_output(c)).collect();
        assert_eq!(got, expected, "chunk size {size}");
    }
    assert!(expected.contains(&Event::Native {
        kind: "plan".into(),
        payload: json!({ "entries": [] })
    }));
}

#[test]
fn resume_loads_and_replays_history_quietly() {
    let mut d = driver(
        Harness::Cursor,
        Start::Resume {
            session_id: "s9".into(),
        },
    );
    d.handshake();
    feed(
        &mut d,
        &[result("acc-0", init_result(json!({ "loadSession": true })))],
    );
    let load = one(&d.poll_transmit());
    assert_eq!(load["method"], "session/load");
    assert_eq!(load["params"]["sessionId"], "s9");

    let events = feed(
        &mut d,
        &[
            update("s9", text_chunk("user_message_chunk", "hi")),
            update("s9", text_chunk("agent_message_chunk", "hello")),
            result(
                "acc-1",
                json!({ "modes": { "currentModeId": "agent",
                "availableModes": [{ "id": "agent", "name": "Agent" }, { "id": "plan", "name": "Plan" }] } }),
            ),
        ],
    );
    assert_eq!(states(&events), [RunState::Idle], "replay is not live work");
    assert!(events.contains(&Event::ItemDelta {
        id: "item-1".into(),
        text: "hi".into()
    }));
    assert!(events.contains(&Event::SessionStarted {
        harness: Harness::Cursor,
        native_id: "s9".into(),
        model: None,
    }));

    // Cursor: modes over legacy set_mode, model only at spawn.
    let set = one(&d
        .command(Command::SetOption {
            id: OptionId::Mode,
            value: "plan".into(),
        })
        .expect("mode"));
    assert_eq!(set["method"], "session/set_mode");
    assert_eq!(set["params"]["modeId"], "plan");
    let events = feed(&mut d, &[result(set["id"].clone(), json!({}))]);
    assert_eq!(events, [Event::OptionsChanged]);
    let mode = d.options().iter().find(|o| o.id == OptionId::Mode).expect("mode");
    assert_eq!(mode.current.as_deref(), Some("plan"));

    let bytes = d
        .command(Command::SetOption {
            id: OptionId::Model,
            value: "composer-2".into(),
        })
        .expect("model");
    assert!(bytes.is_empty(), "Cursor can't switch models live");
    let model = d
        .options()
        .iter()
        .find(|o| o.id == OptionId::Model)
        .expect("model");
    assert_eq!(
        (model.apply, model.current.as_deref()),
        (Apply::Restart, Some("composer-2"))
    );
    assert_eq!(d.launch(Some("s9")).args, ["--model", "composer-2", "acp"]);
}

#[test]
fn resume_falls_back_to_session_resume_or_fails() {
    let mut d = driver(
        Harness::Opencode,
        Start::Resume {
            session_id: "s2".into(),
        },
    );
    d.handshake();
    feed(
        &mut d,
        &[result(
            "acc-0",
            init_result(json!({ "sessionCapabilities": { "resume": {} } })),
        )],
    );
    assert_eq!(one(&d.poll_transmit())["method"], "session/resume");

    let mut d = driver(
        Harness::Opencode,
        Start::Resume {
            session_id: "s2".into(),
        },
    );
    d.handshake();
    let events = feed(&mut d, &[result("acc-0", init_result(json!({})))]);
    assert_eq!(states(&events), [RunState::Error]);
    assert!(d.poll_transmit().is_empty());
}

#[test]
fn version_mismatch_is_an_error() {
    let mut d = driver(Harness::Copilot, Start::New);
    d.handshake();
    let events = feed(&mut d, &[result("acc-0", json!({ "protocolVersion": 2 }))]);
    let Some(Event::StateChanged(s)) = events.first() else {
        unreachable!()
    };
    assert_eq!(s.state, RunState::Error);
    assert!(s.message.as_deref().is_some_and(|m| m.contains("version 2")));
    assert!(matches!(d.command(Command::New), Err(DriverError::NotStarted)));
}

#[test]
fn auth_required_blocks_until_authenticated() {
    let mut d = driver(Harness::Copilot, Start::New);
    d.handshake();
    feed(&mut d, &[result("acc-0", init_result(json!({})))]);
    let new = one(&d.poll_transmit());
    let events = feed(
        &mut d,
        &[
            json!({ "jsonrpc": "2.0", "id": new["id"], "error": { "code": -32000, "message": "Authentication required" } }),
        ],
    );
    let Some(Event::StateChanged(s)) = events.first() else {
        unreachable!()
    };
    assert_eq!(
        s.state,
        RunState::Blocked {
            kind: BlockKind::Auth
        }
    );
    assert_eq!(
        s.message.as_deref(),
        Some("Authentication required (methods: copilot-login)")
    );

    let auth = one(&d.authenticate("copilot-login"));
    assert_eq!(auth["params"]["methodId"], "copilot-login");
    feed(&mut d, &[result(auth["id"].clone(), json!({}))]);
    assert_eq!(one(&d.poll_transmit())["method"], "session/new");
}

#[test]
fn interrupt_cancels_open_permission_requests() {
    let mut d = ready_copilot();
    d.command(Command::Prompt { text: "go".into() }).expect("prompt");
    feed(
        &mut d,
        &[
            json!({ "jsonrpc": "2.0", "id": "p1", "method": "session/request_permission", "params": {
            "sessionId": "s1", "toolCall": { "toolCallId": "t9", "title": "rm -rf build", "kind": "execute" },
            "options": [{ "optionId": "allow-once", "name": "Allow", "kind": "allow_once" }] } }),
        ],
    );
    let msgs = sent(&d.command(Command::Interrupt).expect("interrupt"));
    assert_eq!(
        msgs[0],
        json!({ "jsonrpc": "2.0", "id": "p1", "result": { "outcome": { "outcome": "cancelled" } } })
    );
    assert_eq!(
        msgs[1],
        json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": "s1" } })
    );

    let events = feed(&mut d, &[result("acc-2", json!({ "stopReason": "cancelled" }))]);
    assert!(events.contains(&Event::ItemCompleted {
        id: "t9".into(),
        extensions: Value::Null
    }));
    assert_eq!(states(&events), [RunState::Idle]);
}

#[test]
fn unserved_agent_requests_get_an_error_reply() {
    let mut d = ready_copilot();
    let events = feed(
        &mut d,
        &[json!({ "jsonrpc": "2.0", "id": 3, "method": "fs/read_text_file",
                  "params": { "sessionId": "s1", "path": "/etc/passwd" } })],
    );
    assert!(matches!(&events[0], Event::Native { kind, .. } if kind == "fs/read_text_file"));
    let reply = one(&d.poll_transmit());
    assert_eq!(reply["id"], 3);
    assert_eq!(reply["error"]["code"], -32601);
}

#[test]
fn options_route_to_config_options() {
    let mut d = ready_copilot();
    let set = one(&d
        .command(Command::SetOption {
            id: OptionId::Permission,
            value: "true".into(),
        })
        .expect("set"));
    assert_eq!(set["method"], "session/set_config_option");
    assert_eq!(
        set["params"],
        json!({ "sessionId": "s1", "configId": "allow_all", "type": "boolean", "value": true })
    );
    let mut after = copilot_session("s1");
    after["configOptions"][2]["currentValue"] = json!(true);
    let events = feed(
        &mut d,
        &[result(
            set["id"].clone(),
            json!({ "configOptions": after["configOptions"] }),
        )],
    );
    assert_eq!(events, [Event::OptionsChanged]);
    let perm = d
        .options()
        .iter()
        .find(|o| o.id == OptionId::Permission)
        .expect("perm");
    assert_eq!(perm.current.as_deref(), Some("true"));

    // Agent-pushed changes land the same way.
    let mut pushed = copilot_session("s1");
    pushed["configOptions"][1]["currentValue"] = json!("claude-sonnet-5");
    feed(
        &mut d,
        &[update(
            "s1",
            json!({ "sessionUpdate": "config_option_update", "configOptions": pushed["configOptions"] }),
        )],
    );
    let model = d
        .options()
        .iter()
        .find(|o| o.id == OptionId::Model)
        .expect("model");
    assert_eq!(model.current.as_deref(), Some("claude-sonnet-5"));

    // A rejected change is reported, not fatal.
    let set = one(&d
        .command(Command::SetOption {
            id: OptionId::Model,
            value: "nope".into(),
        })
        .expect("set"));
    let events = feed(
        &mut d,
        &[json!({ "jsonrpc": "2.0", "id": set["id"], "error": { "code": -32602, "message": "bad model" } })],
    );
    assert!(matches!(&events[0], Event::Native { kind, .. } if kind == "request_failed"));
    assert!(matches!(
        d.command(Command::SetOption {
            id: OptionId::Effort,
            value: "high".into()
        }),
        Err(DriverError::Unsupported { .. })
    ));
    assert_eq!(
        one(&d.set_config("_agent", "reviewer").expect("set"))["params"]["configId"],
        "_agent"
    );
}

#[test]
fn canonical_commands_use_advertised_slash_commands() {
    let mut d = ready_copilot();
    let c = one(&d
        .command(Command::Compact {
            hint: Some("keep the API notes".into()),
        })
        .expect("compact"));
    assert_eq!(c["params"]["prompt"][0]["text"], "/compact keep the API notes");
    let r = one(&d
        .command(Command::Rename {
            name: "build fix".into(),
        })
        .expect("rename"));
    assert_eq!(r["params"]["prompt"][0]["text"], "/rename build fix");
    assert!(matches!(
        d.command(Command::Shell { command: "ls".into() }),
        Err(DriverError::Unsupported { .. })
    ));
    assert!(matches!(
        d.command(Command::Fork),
        Err(DriverError::Unsupported { .. })
    ));

    // Without the agent advertising it, there's nothing to send...
    let mut bare = driver(Harness::Opencode, Start::Attach);
    feed(&mut bare, &[update("s1", text_chunk("agent_message_chunk", "x"))]);
    assert!(matches!(
        bare.command(Command::Review),
        Err(DriverError::Unsupported { .. })
    ));
    // ...except opencode's unlisted /compact.
    let c = one(&bare.command(Command::Compact { hint: None }).expect("compact"));
    assert_eq!(c["params"]["prompt"][0]["text"], "/compact");
}

#[test]
fn new_session_switches_and_drops_stale_updates() {
    let mut d = ready_copilot();
    let new = one(&d.command(Command::New).expect("new"));
    assert_eq!(new["method"], "session/new");
    let events = feed(
        &mut d,
        &[
            update(
                "s2",
                json!({ "sessionUpdate": "available_commands_update", "availableCommands": [] }),
            ),
            update("s1", text_chunk("agent_message_chunk", "late")),
            result(new["id"].clone(), copilot_session("s2")),
            update("s1", text_chunk("agent_message_chunk", "stale")),
        ],
    );
    assert_eq!(d.session_id(), Some("s2"));
    assert!(events.contains(&Event::CommandsChanged), "early update replayed");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ItemDelta { text, .. } if text == "stale"))
    );
    assert!(d.commands().iter().all(|c| c.source == CommandSource::Acc));
}

#[test]
fn attach_follows_another_clients_session() {
    let mut cfg = AcpConfig::new("/home/v/proj", Start::Attach);
    cfg.client_tag = "phone-3f2a".into();
    let mut d = AcpDriver::new(Harness::Copilot, cfg);
    assert!(d.handshake().is_empty());

    // The log as the desktop client left it.
    let events = feed(
        &mut d,
        &[
            result(
                "desk-0",
                init_result(json!({ "sessionCapabilities": { "fork": {} } })),
            ),
            result("desk-1", copilot_session("s1")),
            update("s1", text_chunk("agent_message_chunk", "working on it")),
        ],
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::SessionStarted { native_id, .. } if native_id == "s1"))
    );
    assert_eq!(states(&events), [RunState::Idle, working()]);
    assert!(d.commands().iter().any(|c| c.name == "fork"));

    // The desktop answers a permission prompt; progress clears the block here too.
    let events = feed(
        &mut d,
        &[
            json!({ "jsonrpc": "2.0", "id": 4, "method": "session/request_permission", "params": {
                "sessionId": "s1", "toolCall": { "toolCallId": "t1", "title": "edit" }, "options": [] } }),
            update(
                "s1",
                json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed" }),
            ),
            result("desk-2", json!({ "stopReason": "end_turn" })),
        ],
    );
    assert_eq!(
        states(&events),
        [
            RunState::Blocked {
                kind: BlockKind::Permission
            },
            working(),
            RunState::Done
        ]
    );
    assert_eq!(events.last(), Some(&Event::TurnCompleted));

    let p = one(&d
        .command(Command::Prompt { text: "next".into() })
        .expect("prompt"));
    assert_eq!(p["id"], "phone-3f2a-0");
}

#[test]
fn garbage_is_kept_not_fatal() {
    let mut d = ready_copilot();
    let events = d.on_output(b"not json\n[1,2]\n\xff\xfe\n");
    assert_eq!(events.len(), 3);
    assert!(events.iter().all(|e| matches!(e, Event::Unrecognized { .. })));
    let events = feed(
        &mut d,
        &[update("s1", text_chunk("agent_message_chunk", "still fine"))],
    );
    assert!(events.contains(&Event::ItemDelta {
        id: "item-1".into(),
        text: "still fine".into()
    }));
}

#[test]
fn launch_specs() {
    assert_eq!(launch_spec(Harness::Cursor, None).args, ["acp"]);
    assert_eq!(
        launch_spec(Harness::Copilot, Some("gpt-5.4")).args,
        ["--acp", "--model", "gpt-5.4"]
    );
    let oc = launch_spec(Harness::Opencode, Some("ignored"));
    assert_eq!(
        (oc.program.as_str(), oc.args.as_slice()),
        ("opencode", ["acp".to_owned()].as_slice())
    );
    assert_eq!(oc.protocol, Protocol::Acp);
}

#[test]
fn invoke_notice_and_resolved_permissions() {
    let mut d = ready_copilot();
    let r = one(&d
        .command(Command::Invoke {
            name: "/review".into(),
            args: "main".into(),
        })
        .expect("invoke"));
    assert_eq!(r["params"]["prompt"][0]["text"], "/review main");
    assert_eq!(
        one(&d
            .command(Command::Invoke {
                name: "interrupt".into(),
                args: String::new()
            })
            .expect("interrupt"))["method"],
        "session/cancel"
    );
    assert!(matches!(
        d.command(Command::Invoke {
            name: "nope".into(),
            args: String::new()
        }),
        Err(DriverError::InvalidOption(_))
    ));
    assert_eq!(d.command(Command::Refresh).expect("refresh"), Vec::<u8>::new());

    let events = feed(
        &mut d,
        &[
            update(
                "s1",
                json!({ "sessionUpdate": "notice", "severity": "warning", "title": "Rate limited" }),
            ),
            json!({ "jsonrpc": "2.0", "id": 9, "method": "session/request_permission", "params": {
                "sessionId": "s1", "toolCall": { "toolCallId": "t2", "title": "edit" },
                "options": [{ "optionId": "allow_once", "name": "Allow", "kind": "allow_once" }] } }),
            update(
                "s1",
                json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "in_progress" }),
            ),
        ],
    );
    assert_eq!(
        events[0],
        Event::Notice {
            message: "Rate limited".into()
        }
    );
    assert!(events.contains(&Event::PermissionResolved {
        request_id: "9".into()
    }));
    assert!(matches!(
        d.command(Command::AnswerPermission {
            request_id: "9".into(),
            option_id: "allow_once".into()
        }),
        Err(DriverError::UnknownRequest(_))
    ));
}

#[test]
fn option_pickers_set_config_options() {
    let mut d = ready_copilot();
    let mode = d
        .commands()
        .iter()
        .find(|c| c.name == "mode")
        .expect("mode picker");
    assert_eq!(mode.input, CommandInput::Option { id: OptionId::Mode });
    let set = one(&d
        .command(Command::Invoke {
            name: "mode".into(),
            args: "plan".into(),
        })
        .expect("mode"));
    assert_eq!(set["method"], "session/set_config_option");
    assert_eq!(set["params"]["value"], "plan");
    assert!(matches!(
        d.command(Command::Invoke {
            name: "model".into(),
            args: String::new(),
        }),
        Err(DriverError::MissingArgument(_))
    ));
    // No effort option, no effort picker.
    assert!(!d.commands().iter().any(|c| c.name == "effort"));
}

#[test]
fn opencode_lists_its_unadvertised_compact() {
    let mut d = driver(Harness::Opencode, Start::Attach);
    feed(&mut d, &[update("s1", text_chunk("agent_message_chunk", "x"))]);
    assert!(d.commands().iter().any(|c| c.name == "compact"));
    let c = one(&d
        .command(Command::Invoke {
            name: "compact".into(),
            args: "keep notes".into(),
        })
        .expect("compact"));
    assert_eq!(c["params"]["prompt"][0]["text"], "/compact keep notes");
}

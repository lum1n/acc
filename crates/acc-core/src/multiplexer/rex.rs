//! Rex (Superlogical, invite beta since 2026-10-05) through its `rex` CLI.
//!
//! Rex answers the OSC 7501 probe and keeps each terminal's program-status
//! records, which `rex events --json` reports as `program_status_changed`. So
//! inside Rex, Claude Code and pi report state natively even when ACC didn't
//! start them. The CLI shapes here follow Rex's public docs (CLI, shell
//! scripting, events, server API); only the macOS app ships so far, so they
//! are **unverified against a real server**. Parsing is lenient where the docs
//! leave fields out (`session.view`, the program-status `record`).

use base64::Engine as _;
use serde_json::Value;

use super::{
    Backend, Exec, Input, Key, Multiplexer, MuxError, PaneInfo, PaneRef, Program, StatusUpdate, StreamKind,
    Target, Watch, status_update,
};
use crate::osc7501::{self, Osc7501};
use crate::shell::join;

/// The Rex backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rex;

const SESSION_MARKER: &str = "#acc-session ";

fn rex(server: &str, session: Option<&str>, args: &[&str]) -> String {
    let mut argv = vec!["rex"];
    if !server.is_empty() {
        argv.extend(["-S", server]);
    }
    if let Some(s) = session {
        argv.extend(["-s", s]);
    }
    argv.extend_from_slice(args);
    join(argv)
}

fn key_name(key: Key) -> String {
    match key {
        Key::Enter => "enter".into(),
        Key::Escape => "escape".into(),
        Key::Tab => "tab".into(),
        Key::Backspace => "backspace".into(),
        Key::Up => "up".into(),
        Key::Down => "down".into(),
        Key::Left => "left".into(),
        Key::Right => "right".into(),
        Key::Ctrl(c) => format!("ctrl+{}", c.to_ascii_lowercase()),
        Key::Char(c) => c.to_string(),
    }
}

/// Collects every object with a `block_id` from a JSON tree.
fn blocks<'a>(v: &'a Value, out: &mut Vec<&'a serde_json::Map<String, Value>>) {
    match v {
        Value::Object(m) => {
            if m.get("block_id").and_then(Value::as_str).is_some() {
                out.push(m);
            }
            m.values().for_each(|c| blocks(c, out));
        }
        Value::Array(a) => a.iter().for_each(|c| blocks(c, out)),
        _ => {}
    }
}

impl Backend for Rex {
    fn multiplexer(&self) -> Multiplexer {
        Multiplexer::Rex
    }

    fn list_panes(&self, target: &Target) -> Exec {
        let server = target.server.as_str();
        let view = |s: &str| rex(server, Some(s), &["api", "call", "session.view"]);
        Exec::new(match &target.session {
            Some(s) => format!(
                "printf '%s\\n' {}\n{}\n",
                crate::shell::quote(&format!("{SESSION_MARKER}{s}")),
                view(s)
            ),
            None => format!(
                "{} | tr ',{{' '\\n\\n' | sed -n 's/.*\"session_id\" *: *\"\\([^\"]*\\)\".*/\\1/p' | \
                 while IFS= read -r s; do printf '{SESSION_MARKER}%s\\n' \"$s\"; {} -s \"$s\" api call session.view </dev/null; done\n",
                rex(server, None, &["ls", "--json"]),
                rex(server, None, &[]),
            ),
        })
    }

    fn parse_panes(&self, target: &Target, stdout: &str) -> Result<Vec<PaneInfo>, MuxError> {
        let mut panes = Vec::new();
        let mut session = target.session.clone();
        let mut json = String::new();
        let mut flush = |session: &Option<String>, json: &mut String| -> Result<(), MuxError> {
            if json.trim().is_empty() {
                return Ok(());
            }
            let v: Value =
                serde_json::from_str(json).map_err(|e| MuxError::Parse("rex session.view", e.to_string()))?;
            json.clear();
            let mut found = Vec::new();
            blocks(&v, &mut found);
            for b in found {
                let id = b.get("block_id").and_then(Value::as_str).unwrap_or_default();
                if panes.iter().any(|p: &PaneInfo| p.pane.pane == id) {
                    continue;
                }
                let s = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_owned);
                let fg = b.get("foreground");
                panes.push(PaneInfo {
                    pane: PaneRef {
                        multiplexer: Multiplexer::Rex,
                        server: target.server.clone(),
                        session: session.clone(),
                        pane: id.to_owned(),
                    },
                    pid: fg
                        .and_then(|f| f.get("pid"))
                        .and_then(Value::as_u64)
                        .and_then(|p| u32::try_from(p).ok()),
                    tty: None,
                    command: fg
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    cwd: s("pwd"),
                    title: s("title").or_else(|| s("label")),
                    active: b.get("focused").and_then(Value::as_bool).unwrap_or(false),
                    exited: false,
                });
            }
            Ok(())
        };
        for line in stdout.lines() {
            if let Some(s) = line.strip_prefix(SESSION_MARKER) {
                flush(&session, &mut json)?;
                json.clear();
                session = Some(s.to_owned());
            } else {
                json.push_str(line);
                json.push('\n');
            }
        }
        flush(&session, &mut json)?;
        Ok(panes)
    }

    /// Rex's CLI documents no scrollback option, so `scrollback` is ignored.
    fn capture(&self, pane: &PaneRef, _scrollback: bool) -> Exec {
        Exec::new(rex(
            &pane.server,
            pane.session.as_deref(),
            &["capture", "-b", &pane.pane, "--trim", "--unwrap"],
        ))
    }

    fn send(&self, pane: &PaneRef, input: &Input) -> Exec {
        let x = |args: &[&str]| rex(&pane.server, pane.session.as_deref(), args);
        let b = pane.pane.as_str();
        match input {
            // `rex send` reads stdin when given no text and adds no Enter.
            Input::Text(t) => Exec::with_stdin(x(&["send", "-b", b]), t.as_bytes()),
            Input::Paste(t) => Exec::with_stdin(
                x(&["send", "-b", b]),
                [b"\x1b[200~", t.as_bytes(), b"\x1b[201~"].concat(),
            ),
            Input::Keys(keys) => Exec::new(
                keys.iter()
                    .map(|k| x(&["send-key", "-b", b, &key_name(*k)]))
                    .collect::<Vec<_>>()
                    .join(" && "),
            ),
        }
    }

    fn spawn(&self, target: &Target, program: &Program, cwd: &str, name: &str) -> Exec {
        let argv = program.argv();
        let mut args = match &target.session {
            Some(_) => vec![
                "run",
                "--json",
                "--label",
                name,
                "--focus=false",
                "--cwd",
                cwd,
                "--",
            ],
            None => vec!["new", "--json", name, "--cwd", cwd, "--"],
        };
        args.extend(argv.iter().map(String::as_str));
        Exec::new(rex(&target.server, target.session.as_deref(), &args))
    }

    fn parse_spawned(&self, target: &Target, name: &str, stdout: &str) -> Result<PaneRef, MuxError> {
        let v: Value = serde_json::from_str(stdout.trim())
            .map_err(|e| MuxError::Parse("rex run --json", e.to_string()))?;
        let block = v
            .pointer("/block_ids/0")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                let mut found = Vec::new();
                blocks(&v, &mut found);
                found
                    .first()
                    .and_then(|b| b.get("block_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .ok_or_else(|| MuxError::Parse("rex run --json", stdout.to_owned()))?;
        let session = target.session.clone().or_else(|| {
            v.get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| Some(name.to_owned()))
        });
        Ok(PaneRef {
            multiplexer: Multiplexer::Rex,
            server: target.server.clone(),
            session,
            pane: block,
        })
    }

    fn kill(&self, _pane: &PaneRef) -> Result<Exec, MuxError> {
        Err(MuxError::Unsupported(
            Multiplexer::Rex,
            "close a block from its documented CLI",
        ))
    }

    /// Streams the session's events; filter by block with [`parse_event`].
    fn watch(&self, pane: &PaneRef) -> Watch {
        Watch {
            exec: Exec::new(rex(
                &pane.server,
                None,
                &match &pane.session {
                    Some(s) => vec!["events", "--json", "-s", s.as_str()],
                    None => vec!["events", "--json", "--all"],
                },
            )),
            stream: StreamKind::RexEvents,
            cleanup: None,
        }
    }
}

/// A terminal event from `rex events --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RexEvent {
    /// A program-status (OSC 7501) record was set or cleared.
    ProgramStatus {
        block: String,
        update: StatusUpdate,
    },
    /// Records were dropped, e.g. because the program exited.
    ProgramStatusRemoved {
        block: String,
        ids: Vec<String>,
    },
    Title {
        block: String,
        title: String,
    },
    Bell {
        block: String,
    },
    /// The foreground process changed; `None` when Rex reports none.
    Process {
        block: String,
        foreground: Option<String>,
    },
    ChildExited {
        block: String,
        code: Option<i64>,
    },
    BlockClosed {
        block: String,
    },
    /// Any other event.
    Other {
        name: String,
    },
}

impl RexEvent {
    /// The block the event is about, if any.
    pub fn block(&self) -> Option<&str> {
        match self {
            RexEvent::ProgramStatus { block, .. }
            | RexEvent::ProgramStatusRemoved { block, .. }
            | RexEvent::Title { block, .. }
            | RexEvent::Bell { block }
            | RexEvent::Process { block, .. }
            | RexEvent::ChildExited { block, .. }
            | RexEvent::BlockClosed { block } => Some(block),
            RexEvent::Other { .. } => None,
        }
    }
}

/// Parses one line of `rex events --json`. `now_ms` stamps state updates.
pub fn parse_event(line: &str, now_ms: u64) -> Option<RexEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    let ty = v.get("type")?.as_str()?;
    if ty != "session_event" {
        return Some(RexEvent::Other { name: ty.to_owned() });
    }
    let ev = v.pointer("/payload/event")?;
    let ety = ev.get("type")?.as_str()?;
    let p = ev.get("payload")?;
    if ety == "block_closed" {
        return Some(RexEvent::BlockClosed {
            block: p.get("block_id")?.as_str()?.to_owned(),
        });
    }
    if ety != "block_event" {
        return Some(RexEvent::Other { name: ety.to_owned() });
    }
    let block = p.get("block_id")?.as_str()?.to_owned();
    let name = p.get("name")?.as_str()?;
    let data = p.get("payload").unwrap_or(&Value::Null);
    let s = |k: &str| data.get(k).and_then(Value::as_str).map(str::to_owned);
    Some(match name {
        "program_status_changed" => RexEvent::ProgramStatus {
            update: record_update(s("id"), data.get("record").unwrap_or(&Value::Null), now_ms)?,
            block,
        },
        "program_status_removed" => RexEvent::ProgramStatusRemoved {
            ids: data
                .get("ids")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default(),
            block,
        },
        "title_changed" => RexEvent::Title {
            title: s("title").unwrap_or_default(),
            block,
        },
        "bell" => RexEvent::Bell { block },
        "process_changed" => RexEvent::Process {
            foreground: data
                .pointer("/foreground/name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            block,
        },
        "child_exited" => RexEvent::ChildExited {
            code: data.get("exit_code").and_then(Value::as_i64),
            block,
        },
        other => RexEvent::Other {
            name: other.to_owned(),
        },
    })
}

/// Maps a program-status record. Rex doesn't document the record's fields;
/// this accepts the OSC 7501 keys (`state`, `kind`, `progress`, `app`,
/// `title`, `msg`) and a decoded `message`. A null record is a clear.
fn record_update(id: Option<String>, record: &Value, now_ms: u64) -> Option<StatusUpdate> {
    let id = id.filter(|i| !i.is_empty());
    let get = |k: &str| record.get(k).and_then(Value::as_str);
    let state = get("state").unwrap_or("clear");
    let mut payload = format!("state={state}");
    if let Some(kind) = get("kind") {
        payload.push_str(&format!(":kind={kind}"));
    }
    if let Some(p) = record.get("progress").and_then(Value::as_u64) {
        payload.push_str(&format!(":progress={p}"));
    }
    let Some(Osc7501::Report(mut report)) = osc7501::parse_payload(&payload) else {
        return None;
    };
    let text = |k: &str| {
        get(k).map(|t| {
            // OSC 7501 carries base64; accept either form.
            base64::engine::general_purpose::STANDARD
                .decode(t)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .filter(|_| k == "msg")
                .unwrap_or_else(|| t.to_owned())
        })
    };
    report.id = id;
    report.app = get("app").map(str::to_owned);
    report.title = text("title");
    report.message = get("message").map(str::to_owned).or_else(|| text("msg"));
    Some(status_update(report, now_ms))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::state::{BlockKind, RunState, StateSource};

    fn wrap(name: &str, payload: &str) -> String {
        format!(
            r#"{{"type":"session_event","payload":{{"session_id":"session:01a1","event":{{"type":"block_event","payload":{{"block_id":"block:01b2","creator_name":"com.superlogical.terminal","name":"{name}","payload":{payload}}}}}}}}}"#
        )
    }

    #[test]
    fn parses_documented_child_exited() {
        let line = wrap("child_exited", r#"{"exit_code":2,"error":"exit status 2"}"#);
        assert_eq!(
            parse_event(&line, 0),
            Some(RexEvent::ChildExited {
                block: "block:01b2".into(),
                code: Some(2)
            })
        );
        let created = r#"{"type":"session_created","payload":{"session":{"session_id":"session:01a1","label":"work"}}}"#;
        assert_eq!(
            parse_event(created, 0),
            Some(RexEvent::Other {
                name: "session_created".into()
            })
        );
    }

    #[test]
    fn program_status_record_maps_to_state() {
        let line = wrap(
            "program_status_changed",
            r#"{"id":"","record":{"state":"blocked","kind":"question","app":"claude-code","msg":"V2hpY2ggZmlsZT8="},"reason":"report"}"#,
        );
        let Some(RexEvent::ProgramStatus { block, update }) = parse_event(&line, 7) else {
            panic!("expected program status")
        };
        assert_eq!(block, "block:01b2");
        assert_eq!(update.id, None);
        let s = update.state.unwrap();
        assert_eq!(
            s.state,
            RunState::Blocked {
                kind: BlockKind::Question
            }
        );
        assert_eq!(s.message.as_deref(), Some("Which file?"));
        assert_eq!(s.source, StateSource::Osc7501);

        let cleared = wrap(
            "program_status_changed",
            r#"{"id":"agents/1","record":null,"reason":"clear"}"#,
        );
        let Some(RexEvent::ProgramStatus { update, .. }) = parse_event(&cleared, 0) else {
            panic!("expected program status")
        };
        assert_eq!(update.id.as_deref(), Some("agents/1"));
        assert!(update.state.is_none());
    }

    #[test]
    fn spawn_and_send() {
        let t = Target {
            multiplexer: Multiplexer::Rex,
            server: String::new(),
            session: Some("work".into()),
        };
        let prog = Program {
            program: "pi".into(),
            args: vec![],
            env: vec![],
        };
        assert_eq!(
            Rex.spawn(&t, &prog, "/src", "acc-1").script,
            "rex -s work run --json --label acc-1 --focus=false --cwd /src -- pi"
        );
        let p = Rex
            .parse_spawned(&t, "acc-1", r#"{"window_id":"window:1","block_ids":["block:9"]}"#)
            .unwrap();
        assert_eq!(p.pane, "block:9");
        let keys = Rex
            .send(&p, &Input::Keys(vec![Key::Escape, Key::Ctrl('c')]))
            .script;
        assert_eq!(
            keys,
            "rex -s work send-key -b block:9 escape && rex -s work send-key -b block:9 ctrl+c"
        );
        assert_eq!(Rex.send(&p, &Input::Text("hi".into())).stdin, b"hi");
    }

    #[test]
    fn panes_from_session_view() {
        let t = Target {
            multiplexer: Multiplexer::Rex,
            server: "studio".into(),
            session: Some("work".into()),
        };
        let out = r#"#acc-session work
{"windows":[{"window_id":"window:1","blocks":[{"block_id":"block:1","title":"pi","pwd":"/src","foreground":{"name":"pi","pid":42}}]}]}
"#;
        let panes = Rex.parse_panes(&t, out).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].command.as_deref(), Some("pi"));
        assert_eq!(panes[0].pid, Some(42));
        assert_eq!(panes[0].pane.server, "studio");
    }
}

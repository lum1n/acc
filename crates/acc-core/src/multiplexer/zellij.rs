//! zellij (0.44+): pane-targeted `zellij action` commands and `zellij subscribe`.
//!
//! zellij is a full terminal emulator between the harness and ACC: it neither
//! answers nor forwards OSC 7501, and `subscribe` streams rendered lines only.
//! State therefore comes from transcripts and the rendered screen. Older
//! zellij (0.43 and before) can't target panes and isn't supported. Tested
//! against zellij 0.45.1.

use serde::Deserialize;

use super::{
    Backend, Exec, Input, Key, Multiplexer, MuxError, PaneInfo, PaneRef, Program, StreamKind, Target, Watch,
};
use crate::shell::join;

/// The zellij backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct Zellij;

/// Line printed before each session's pane list when listing all sessions.
const SESSION_MARKER: &str = "#acc-session ";

fn session_of(target: &Target) -> &str {
    target.session.as_deref().unwrap_or(&target.server)
}

fn zellij(session: &str, args: &[&str]) -> String {
    let mut argv = vec!["zellij", "-s", session];
    argv.extend_from_slice(args);
    join(argv)
}

fn key_name(key: Key) -> String {
    match key {
        Key::Enter => "Enter".into(),
        Key::Escape => "Esc".into(),
        Key::Tab => "Tab".into(),
        Key::Backspace => "Backspace".into(),
        Key::Up => "Up".into(),
        Key::Down => "Down".into(),
        Key::Left => "Left".into(),
        Key::Right => "Right".into(),
        Key::Ctrl(c) => format!("Ctrl {}", c.to_ascii_lowercase()),
        Key::Char(c) => c.to_string(),
    }
}

#[derive(Deserialize)]
struct RawPane {
    id: u64,
    #[serde(default)]
    is_plugin: bool,
    #[serde(default)]
    is_focused: bool,
    #[serde(default)]
    exited: bool,
    #[serde(default)]
    is_held: bool,
    title: Option<String>,
    pane_command: Option<String>,
    terminal_command: Option<String>,
    pane_cwd: Option<String>,
}

impl Backend for Zellij {
    fn multiplexer(&self) -> Multiplexer {
        Multiplexer::Zellij
    }

    fn list_panes(&self, target: &Target) -> Exec {
        let list = |s: &str| zellij(s, &["action", "list-panes", "-a", "-j"]);
        let script = match &target.session {
            Some(s) => format!(
                "printf '%s\\n' {}\n{}",
                crate::shell::quote(&format!("{SESSION_MARKER}{s}")),
                list(s)
            ),
            None => format!(
                "zellij list-sessions -s -n 2>/dev/null | while IFS= read -r s; do \
                 printf '{SESSION_MARKER}%s\\n' \"$s\"; zellij -s \"$s\" action list-panes -a -j </dev/null; done\n"
            ),
        };
        Exec::new(script)
    }

    fn parse_panes(&self, target: &Target, stdout: &str) -> Result<Vec<PaneInfo>, MuxError> {
        let mut panes = Vec::new();
        let mut session = target.session.clone();
        let mut json = String::new();
        let mut flush = |session: &Option<String>, json: &mut String| -> Result<(), MuxError> {
            if json.trim().is_empty() {
                json.clear();
                return Ok(());
            }
            let raw: Vec<RawPane> = serde_json::from_str(json)
                .map_err(|e| MuxError::Parse("zellij list-panes", e.to_string()))?;
            json.clear();
            let session = session.clone().unwrap_or_else(|| target.server.clone());
            for p in raw.into_iter().filter(|p| !p.is_plugin) {
                panes.push(PaneInfo {
                    pane: PaneRef {
                        multiplexer: Multiplexer::Zellij,
                        server: session.clone(),
                        session: Some(session.clone()),
                        pane: format!("terminal_{}", p.id),
                    },
                    pid: None,
                    tty: None,
                    command: p
                        .pane_command
                        .or(p.terminal_command)
                        .map(|c| c.rsplit('/').next().unwrap_or(&c).to_owned()),
                    cwd: p.pane_cwd,
                    title: p.title,
                    active: p.is_focused,
                    exited: p.exited || p.is_held,
                });
            }
            Ok(())
        };
        for line in stdout.lines() {
            if let Some(name) = line.strip_prefix(SESSION_MARKER) {
                flush(&session, &mut json)?;
                session = Some(name.to_owned());
            } else {
                json.push_str(line);
                json.push('\n');
            }
        }
        flush(&session, &mut json)?;
        Ok(panes)
    }

    fn capture(&self, pane: &PaneRef, scrollback: bool) -> Exec {
        let mut args = vec!["action", "dump-screen", "-p", &pane.pane];
        if scrollback {
            args.push("-f");
        }
        Exec::new(zellij(session_of(&pane.target()), &args))
    }

    fn send(&self, pane: &PaneRef, input: &Input) -> Exec {
        let s = pane.target();
        let s = session_of(&s);
        let p = pane.pane.as_str();
        Exec::new(match input {
            Input::Text(text) => zellij(s, &["action", "write-chars", "-p", p, "--", text]),
            Input::Paste(text) => zellij(s, &["action", "paste", "-p", p, "--", text]),
            Input::Keys(keys) => {
                let names: Vec<String> = keys.iter().map(|k| key_name(*k)).collect();
                let mut args = vec!["action", "send-keys", "-p", p, "--"];
                args.extend(names.iter().map(String::as_str));
                zellij(s, &args)
            }
        })
    }

    fn spawn(&self, target: &Target, program: &Program, cwd: &str, name: &str) -> Exec {
        let argv = program.argv();
        let mut args = vec!["run", "--name", name, "--cwd", cwd, "--"];
        args.extend(argv.iter().map(String::as_str));
        Exec::new(zellij(session_of(target), &args))
    }

    fn parse_spawned(&self, target: &Target, _name: &str, stdout: &str) -> Result<PaneRef, MuxError> {
        let id = stdout
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("terminal_"))
            .ok_or_else(|| MuxError::Parse("zellij run", stdout.to_owned()))?;
        let session = session_of(target).to_owned();
        Ok(PaneRef {
            multiplexer: Multiplexer::Zellij,
            server: session.clone(),
            session: Some(session),
            pane: id.to_owned(),
        })
    }

    fn kill(&self, pane: &PaneRef) -> Result<Exec, MuxError> {
        Ok(Exec::new(zellij(
            session_of(&pane.target()),
            &["action", "close-pane", "-p", &pane.pane],
        )))
    }

    fn watch(&self, pane: &PaneRef) -> Watch {
        Watch {
            exec: Exec::new(zellij(
                session_of(&pane.target()),
                &["subscribe", "-p", &pane.pane, "-f", "json"],
            )),
            stream: StreamKind::ZellijSubscribe,
            cleanup: None,
        }
    }
}

/// A rendered update of a subscribed pane.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PaneUpdate {
    pub pane_id: String,
    /// The visible screen, top to bottom.
    pub viewport: Vec<String>,
    #[serde(default)]
    pub scrollback: Option<Vec<String>>,
    #[serde(default)]
    pub is_initial: bool,
}

/// Parses one line of `zellij subscribe -f json`. Other event kinds (and
/// anything malformed) give `None`.
pub fn parse_update(line: &str) -> Option<PaneUpdate> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("event")?.as_str()? != "pane_update" {
        return None;
    }
    serde_json::from_value(v).ok()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            multiplexer: Multiplexer::Zellij,
            server: "dev".into(),
            session: Some("dev".into()),
        }
    }

    #[test]
    fn parses_recorded_list_panes() {
        // Trimmed from zellij 0.45.1 `action list-panes -a -j`.
        let out = r#"#acc-session dev
[
 {"id":0,"is_plugin":true,"is_focused":false,"title":"tab-bar","exited":false,"is_held":false},
 {"id":1,"is_plugin":false,"is_focused":true,"title":"agent","exited":false,"is_held":false,
  "terminal_command":"sh -c echo hello; exec cat","pane_command":"cat","pane_cwd":"/tmp","tab_name":"Tab #1"}
]
#acc-session other
[{"id":4,"is_plugin":false,"is_focused":false,"title":"Pane #1","exited":true,"is_held":true,"pane_command":"/bin/bash"}]
"#;
        let panes = Zellij.parse_panes(&target(), out).unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane.pane, "terminal_1");
        assert_eq!(panes[0].command.as_deref(), Some("cat"));
        assert!(panes[0].active);
        assert_eq!(panes[1].pane.session.as_deref(), Some("other"));
        assert_eq!(panes[1].command.as_deref(), Some("bash"));
        assert!(panes[1].exited);
    }

    #[test]
    fn parses_recorded_subscribe_update() {
        let line = r#"{"event":"pane_update","is_initial":false,"pane_id":"terminal_1","scrollback":null,"viewport":["hello","abc",""]}"#;
        let u = parse_update(line).unwrap();
        assert_eq!(u.pane_id, "terminal_1");
        assert_eq!(u.viewport[1], "abc");
        assert!(parse_update(r#"{"event":"pane_closed","pane_id":"terminal_1"}"#).is_none());
        assert!(parse_update("not json").is_none());
    }

    #[test]
    fn builds_commands() {
        let p = PaneRef {
            multiplexer: Multiplexer::Zellij,
            server: "dev".into(),
            session: Some("dev".into()),
            pane: "terminal_2".into(),
        };
        assert_eq!(
            Zellij.send(&p, &Input::Text("-rf $x".into())).script,
            "zellij -s dev action write-chars -p terminal_2 -- '-rf $x'"
        );
        assert_eq!(
            Zellij
                .send(&p, &Input::Keys(vec![Key::Ctrl('c'), Key::Enter]))
                .script,
            "zellij -s dev action send-keys -p terminal_2 -- 'Ctrl c' Enter"
        );
        let spawned = Zellij.parse_spawned(&target(), "x", "terminal_9\n").unwrap();
        assert_eq!(spawned.pane, "terminal_9");
        assert!(Zellij.parse_spawned(&target(), "x", "").is_err());
    }
}

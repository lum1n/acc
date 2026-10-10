//! GNU screen (4.x), best effort: `screen -X` commands and `-Q` queries.
//!
//! screen has no streaming API. ACC observes a window by turning on its log
//! (which records the raw bytes, OSC 7501 included) and tailing the file, and
//! reads the rendered screen with `hardcopy`. Both write a file under
//! `$HOME/.acc/screen/` that ACC removes afterwards; nothing is installed.
//! Tested against screen 4.09.

use super::{
    Backend, Exec, Input, Key, Multiplexer, MuxError, PaneInfo, PaneRef, Program, StreamKind, Target, Watch,
    safe_name,
};
use crate::shell::{join, quote};

/// The screen backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct Screen;

const SESSION_MARKER: &str = "#acc-session ";
/// `-Q windows` format: number, a space, then the title; one window per line.
const WINDOWS_FORMAT: &str = "%n %t\n";
const DIR: &str = "\"$HOME\"/.acc/screen";

fn session_of(target: &Target) -> &str {
    target.session.as_deref().unwrap_or(&target.server)
}

fn screen(session: &str, window: Option<&str>, args: &[&str]) -> String {
    let mut argv = vec!["screen", "-S", session];
    if let Some(w) = window {
        argv.extend(["-p", w]);
    }
    argv.extend_from_slice(args);
    join(argv)
}

/// Escapes bytes for `stuff`. screen expands `$VAR`, `^X` and `\` escapes in
/// command arguments, so everything except letters, digits and spaces is
/// sent as a `\ooo` octal escape, which screen turns back into the literal byte.
fn stuff_escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b == b' ' {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("\\{b:03o}"));
        }
    }
    out
}

fn key_bytes(key: Key) -> Vec<u8> {
    match key {
        Key::Enter => b"\r".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Backspace => b"\x7f".to_vec(),
        Key::Up => b"\x1b[A".to_vec(),
        Key::Down => b"\x1b[B".to_vec(),
        Key::Right => b"\x1b[C".to_vec(),
        Key::Left => b"\x1b[D".to_vec(),
        Key::Ctrl(c) => vec![(c.to_ascii_lowercase() as u8) & 0x1f],
        Key::Char(c) => c.to_string().into_bytes(),
    }
}

fn log_file(pane: &PaneRef) -> String {
    let name = safe_name("", &format!("{}-{}", session_of(&pane.target()), pane.pane));
    format!("{DIR}/{name}.log")
}

impl Backend for Screen {
    fn multiplexer(&self) -> Multiplexer {
        Multiplexer::Screen
    }

    fn list_panes(&self, target: &Target) -> Exec {
        let script = match &target.session {
            Some(s) => format!(
                "printf '%s\\n' {}\n{}\n",
                quote(&format!("{SESSION_MARKER}{s}")),
                screen(s, None, &["-Q", "windows", WINDOWS_FORMAT])
            ),
            // `screen -ls` lines look like "\t12345.name\t(Detached)".
            None => format!(
                "screen -ls 2>/dev/null | awk -F'\\t' '/^\\t[0-9]+\\./ {{print $2}}' | while IFS= read -r s; do \
                 printf '{SESSION_MARKER}%s\\n' \"$s\"; screen -S \"$s\" -Q windows {} </dev/null; done\n",
                quote(WINDOWS_FORMAT)
            ),
        };
        Exec::new(script)
    }

    fn parse_panes(&self, target: &Target, stdout: &str) -> Result<Vec<PaneInfo>, MuxError> {
        let mut session = target.session.clone();
        let mut panes = Vec::new();
        for line in stdout.lines().filter(|l| !l.is_empty()) {
            if let Some(s) = line.strip_prefix(SESSION_MARKER) {
                session = Some(s.to_owned());
                continue;
            }
            let (num, title) = line.split_once(' ').unwrap_or((line, ""));
            if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
                return Err(MuxError::Parse("screen -Q windows", line.to_owned()));
            }
            let s = session.clone().unwrap_or_else(|| target.server.clone());
            panes.push(PaneInfo {
                pane: PaneRef {
                    multiplexer: Multiplexer::Screen,
                    server: s.clone(),
                    session: Some(s),
                    pane: num.to_owned(),
                },
                pid: None,
                tty: None,
                command: None,
                cwd: None,
                title: Some(title.to_owned()).filter(|t| !t.is_empty()),
                active: false,
                exited: false,
            });
        }
        Ok(panes)
    }

    fn capture(&self, pane: &PaneRef, scrollback: bool) -> Exec {
        let s = pane.target();
        let mut args = vec!["-X", "hardcopy"];
        if scrollback {
            args.push("-h");
        }
        // `-X` returns before screen has written the file, so wait for it.
        Exec::new(format!(
            "f=$(mktemp) || exit 1\n\
             {} \"$f\" || {{ rm -f \"$f\"; exit 1; }}\n\
             i=0; while [ ! -s \"$f\" ] && [ $i -lt 100 ]; do sleep 0.02; i=$((i+1)); done\n\
             cat \"$f\"; rm -f \"$f\"\n",
            screen(session_of(&s), Some(&pane.pane), &args)
        ))
    }

    fn send(&self, pane: &PaneRef, input: &Input) -> Exec {
        let bytes = match input {
            Input::Text(t) => t.as_bytes().to_vec(),
            // Bracketed paste markers, as a terminal sends them, so a
            // multi-line prompt doesn't submit at its first newline.
            Input::Paste(t) => [b"\x1b[200~", t.as_bytes(), b"\x1b[201~"].concat(),
            Input::Keys(keys) => keys.iter().flat_map(|k| key_bytes(*k)).collect(),
        };
        let s = pane.target();
        Exec::new(screen(
            session_of(&s),
            Some(&pane.pane),
            &["-X", "stuff", &stuff_escape(&bytes)],
        ))
    }

    fn spawn(&self, target: &Target, program: &Program, cwd: &str, name: &str) -> Exec {
        let mut argv = vec!["sh", "-c", "cd \"$1\" && shift && exec \"$@\"", "sh", cwd];
        let prog = program.argv();
        argv.extend(prog.iter().map(String::as_str));
        let cmd = join(argv);
        Exec::new(match &target.session {
            Some(s) => format!(
                "{} {cmd} && {}\n",
                screen(s, None, &["-X", "screen", "-t", name]),
                screen(s, None, &["-Q", "windows", WINDOWS_FORMAT])
            ),
            None => format!("{} {cmd} && echo 0\n", join(["screen", "-dmS", name, "-t", name])),
        })
    }

    fn parse_spawned(&self, target: &Target, name: &str, stdout: &str) -> Result<PaneRef, MuxError> {
        let session = target.session.clone().unwrap_or_else(|| name.to_owned());
        let window = if target.session.is_none() {
            Some(stdout.trim().to_owned()).filter(|w| w == "0")
        } else {
            stdout
                .lines()
                .rev()
                .filter_map(|l| l.split_once(' '))
                .find(|(_, t)| *t == name)
                .map(|(n, _)| n.to_owned())
        };
        let window = window.ok_or_else(|| MuxError::Parse("screen windows", stdout.to_owned()))?;
        Ok(PaneRef {
            multiplexer: Multiplexer::Screen,
            server: session.clone(),
            session: Some(session),
            pane: window,
        })
    }

    fn kill(&self, pane: &PaneRef) -> Result<Exec, MuxError> {
        let s = pane.target();
        Ok(Exec::new(screen(
            session_of(&s),
            Some(&pane.pane),
            &["-X", "kill"],
        )))
    }

    /// Logs the window to a file and tails it. screen's `logfile` name is a
    /// session-wide setting, so this changes where any later `log on` in the
    /// session writes; [`Watch::cleanup`] turns logging off and removes the file.
    fn watch(&self, pane: &PaneRef) -> Watch {
        let s = pane.target();
        let s = session_of(&s);
        let w = Some(pane.pane.as_str());
        let f = log_file(pane);
        let x = |args: &[&str]| screen(s, w, args);
        Watch {
            exec: Exec::new(format!(
                "mkdir -p {DIR} && f={f} && : >\"$f\" || exit 1\n\
                 {} \"$f\" && {} && {} || exit 1\n\
                 exec tail -c +1 -F \"$f\"\n",
                x(&["-X", "logfile"]),
                x(&["-X", "logfile", "flush", "0"]),
                x(&["-X", "log", "on"]),
            )),
            stream: StreamKind::RawBytes,
            cleanup: Some(Exec::new(format!("{}; rm -f {f}\n", x(&["-X", "log", "off"])))),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn pane() -> PaneRef {
        PaneRef {
            multiplexer: Multiplexer::Screen,
            server: "123.work".into(),
            session: Some("123.work".into()),
            pane: "2".into(),
        }
    }

    #[test]
    fn stuff_escapes_everything_screen_would_expand() {
        assert_eq!(stuff_escape(b"echo $HOME ^M\\"), "echo \\044HOME \\136M\\134");
        let e = Screen
            .send(&pane(), &Input::Keys(vec![Key::Ctrl('c'), Key::Up]))
            .script;
        assert_eq!(e, "screen -S 123.work -p 2 -X stuff '\\003\\033\\133A'");
    }

    #[test]
    fn paste_is_bracketed() {
        let e = Screen.send(&pane(), &Input::Paste("a\nb".into())).script;
        assert!(e.contains("\\033\\133200\\176a\\012b\\033\\133201\\176"), "{e}");
    }

    #[test]
    fn parses_windows() {
        let t = Target {
            multiplexer: Multiplexer::Screen,
            server: "123.work".into(),
            session: None,
        };
        let out = "#acc-session 123.work\n0 sh\n1 my win\n#acc-session 9.x\n3 \n";
        let panes = Screen.parse_panes(&t, out).unwrap();
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[1].title.as_deref(), Some("my win"));
        assert_eq!(panes[2].pane.session.as_deref(), Some("9.x"));
        assert_eq!(panes[2].title, None);
        let s = Target {
            session: Some("123.work".into()),
            ..t
        };
        let p = Screen.parse_spawned(&s, "acc-1", "0 sh\n4 acc-1\n").unwrap();
        assert_eq!(p.pane, "4");
        assert!(Screen.parse_spawned(&s, "acc-1", "0 sh\n").is_err());
    }

    #[test]
    fn watch_logs_and_cleans_up() {
        let w = Screen.watch(&pane());
        assert!(w.exec.script.contains("log on"));
        assert!(w.exec.script.contains("tail -c +1 -F"));
        assert!(w.cleanup.unwrap().script.contains("log off"));
    }
}

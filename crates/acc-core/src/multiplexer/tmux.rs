//! tmux (3.2+): CLI commands and the control-mode protocol.
//!
//! Observation uses control mode (`tmux -C attach`) over an SSH exec channel:
//! no PTY is needed, and `%output` carries every pane's raw bytes, OSC 7501
//! reports included. Input and listing use plain CLI calls. Tested against
//! tmux 3.4.

use super::{
    Backend, Exec, Input, Key, Multiplexer, MuxError, PaneInfo, PaneRef, Program, StreamKind, Target, Watch,
    safe_name,
};
use crate::shell::{join, quote};

/// The tmux backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tmux;

/// Fields after the pane id are `#{q:}`-escaped and space-separated; the
/// title is last and taken verbatim. tmux replaces tabs and non-ASCII bytes
/// in format output, so no other separator is reliable.
const LIST_FORMAT: &str = "#{pane_id} #{pane_pid} #{pane_active} #{pane_dead} #{q:pane_tty} \
#{q:session_name} #{q:pane_current_command} #{q:pane_current_path} #{pane_title}";

fn tmux(server: &str, args: &[&str]) -> String {
    let mut argv = vec!["tmux"];
    if !server.is_empty() {
        argv.extend(["-S", server]);
    }
    argv.extend_from_slice(args);
    join(argv)
}

fn key_name(key: Key) -> String {
    match key {
        Key::Enter => "Enter".into(),
        Key::Escape => "Escape".into(),
        Key::Tab => "Tab".into(),
        Key::Backspace => "BSpace".into(),
        Key::Up => "Up".into(),
        Key::Down => "Down".into(),
        Key::Left => "Left".into(),
        Key::Right => "Right".into(),
        Key::Ctrl(c) => format!("C-{}", c.to_ascii_lowercase()),
        Key::Char(c) => c.to_string(),
    }
}

/// Splits one `#{q:}`-escaped field off `s`, returning it and the rest.
fn take_field(s: &str) -> (String, &str) {
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                if let Some((_, n)) = chars.next() {
                    out.push(n);
                }
            }
            ' ' => return (out, &s[i + 1..]),
            _ => out.push(c),
        }
    }
    (out, "")
}

impl Backend for Tmux {
    fn multiplexer(&self) -> Multiplexer {
        Multiplexer::Tmux
    }

    fn list_panes(&self, target: &Target) -> Exec {
        let script = match &target.session {
            Some(s) => {
                let t = format!("={s}");
                tmux(&target.server, &["list-panes", "-s", "-t", &t, "-F", LIST_FORMAT])
            }
            None => tmux(&target.server, &["list-panes", "-a", "-F", LIST_FORMAT]),
        };
        Exec::new(script)
    }

    fn parse_panes(&self, target: &Target, stdout: &str) -> Result<Vec<PaneInfo>, MuxError> {
        let mut panes = Vec::new();
        for line in stdout.lines().filter(|l| !l.is_empty()) {
            let bad = || MuxError::Parse("tmux list-panes", line.to_owned());
            let mut rest = line;
            let mut fields = Vec::with_capacity(8);
            for _ in 0..8 {
                if rest.is_empty() {
                    return Err(bad());
                }
                let (f, r) = take_field(rest);
                fields.push(f);
                rest = r;
            }
            if !fields[0].starts_with('%') {
                return Err(bad());
            }
            let opt = |s: &str| Some(s.to_owned()).filter(|s| !s.is_empty());
            panes.push(PaneInfo {
                pane: PaneRef {
                    multiplexer: Multiplexer::Tmux,
                    server: target.server.clone(),
                    session: opt(&fields[5]),
                    pane: fields[0].clone(),
                },
                pid: fields[1].parse().ok(),
                active: fields[2] == "1",
                exited: fields[3] == "1",
                tty: opt(&fields[4]),
                command: opt(&fields[6]),
                cwd: opt(&fields[7]),
                title: opt(rest),
            });
        }
        Ok(panes)
    }

    fn capture(&self, pane: &PaneRef, scrollback: bool) -> Exec {
        let mut args = vec!["capture-pane", "-p", "-J", "-t", &pane.pane];
        if scrollback {
            args.extend(["-S", "-"]);
        }
        Exec::new(tmux(&pane.server, &args))
    }

    fn send(&self, pane: &PaneRef, input: &Input) -> Exec {
        let p = pane.pane.as_str();
        match input {
            Input::Text(text) => Exec::new(tmux(&pane.server, &["send-keys", "-t", p, "-l", "--", text])),
            Input::Paste(text) => {
                // stdin avoids argv limits and tmux's command parser entirely;
                // `-p` pastes bracketed when the program asked for it.
                let buf = safe_name("acc-", p);
                Exec::with_stdin(
                    format!(
                        "{} && {}",
                        tmux(&pane.server, &["load-buffer", "-b", &buf, "-"]),
                        tmux(&pane.server, &["paste-buffer", "-p", "-d", "-b", &buf, "-t", p]),
                    ),
                    text.as_bytes(),
                )
            }
            Input::Keys(keys) => {
                let names: Vec<String> = keys.iter().map(|k| key_name(*k)).collect();
                let mut args: Vec<&str> = Vec::new();
                for (i, (key, name)) in keys.iter().zip(&names).enumerate() {
                    if i > 0 {
                        args.push(";");
                    }
                    args.extend(["send-keys", "-t", p]);
                    // A literal character could otherwise be read as a key name.
                    if matches!(key, Key::Char(_)) {
                        args.extend(["-l", "--"]);
                    }
                    args.push(name);
                }
                Exec::new(tmux(&pane.server, &args))
            }
        }
    }

    fn spawn(&self, target: &Target, program: &Program, cwd: &str, name: &str) -> Exec {
        let argv = program.argv();
        let mut args: Vec<&str> = Vec::new();
        let session_target;
        match &target.session {
            Some(s) => {
                session_target = format!("={s}:");
                args.extend(["new-window", "-d", "-t", &session_target, "-n", name]);
            }
            None => args.extend(["new-session", "-d", "-s", name]),
        }
        args.extend(["-P", "-F", "#{pane_id}", "-c", cwd, "--"]);
        args.extend(argv.iter().map(String::as_str));
        Exec::new(tmux(&target.server, &args))
    }

    fn parse_spawned(&self, target: &Target, name: &str, stdout: &str) -> Result<PaneRef, MuxError> {
        let id = stdout.trim();
        if !id.starts_with('%') {
            return Err(MuxError::Parse("tmux new-window", stdout.to_owned()));
        }
        Ok(PaneRef {
            multiplexer: Multiplexer::Tmux,
            server: target.server.clone(),
            session: target.session.clone().or_else(|| Some(name.to_owned())),
            pane: id.to_owned(),
        })
    }

    fn kill(&self, pane: &PaneRef) -> Result<Exec, MuxError> {
        Ok(Exec::new(tmux(&pane.server, &["kill-pane", "-t", &pane.pane])))
    }

    /// Attaches a control client to the pane's session. The client ignores
    /// its size, so it never resizes windows attached elsewhere. stdin holds
    /// subscriptions for the pane's title and foreground command, and must
    /// then stay open (closing it detaches); send further control commands
    /// built with [`control_command`] on it.
    fn watch(&self, pane: &PaneRef) -> Watch {
        let p = &pane.pane;
        let mut stdin = control_command(&["refresh-client", "-B", &format!("acc-title:{p}:#{{pane_title}}")]);
        stdin.push_str(&control_command(&[
            "refresh-client",
            "-B",
            &format!("acc-command:{p}:#{{pane_current_command}}"),
        ]));
        Watch {
            exec: Exec::with_stdin(
                tmux(&pane.server, &["-C", "attach", "-f", "ignore-size", "-t", p]),
                stdin,
            ),
            stream: StreamKind::TmuxControl,
            cleanup: None,
        }
    }
}

/// Restarts the pane with `program`, killing what runs there. For handing a
/// session back to its TUI in a pane ACC created; in the user's own pane,
/// type the command into their shell instead ([`super::tui::resume_inputs`]).
pub fn respawn(pane: &PaneRef, program: &Program, cwd: &str) -> Exec {
    let argv = program.argv();
    let mut args = vec!["respawn-pane", "-k", "-t", &pane.pane, "-c", cwd, "--"];
    args.extend(argv.iter().map(String::as_str));
    Exec::new(tmux(&pane.server, &args))
}

/// Prints the tmux server sockets of the current user, one per line.
pub fn list_servers() -> Exec {
    Exec::new(
        "for s in \"${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)\"/*; do [ -S \"$s\" ] && printf '%s\\n' \"$s\"; done; true\n"
            .to_owned(),
    )
}

/// One control-mode command line. tmux's parser accepts POSIX single quoting.
pub fn control_command(args: &[&str]) -> String {
    let mut line = args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    line.push('\n');
    line
}

/// Control command resuming a pane tmux paused for flow control
/// (`refresh-client -f pause-after=N`). Output during a pause is lost, so
/// recapture the screen afterwards.
pub fn continue_pane(pane: &str) -> String {
    control_command(&["refresh-client", "-A", &format!("{pane}:continue")])
}

/// A control-mode message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    /// Raw bytes a pane wrote (`%output`, `%extended-output`).
    Output {
        pane: String,
        bytes: Vec<u8>,
    },
    /// The reply to a command, in the order commands were sent.
    Reply {
        ok: bool,
        lines: Vec<String>,
    },
    /// A `refresh-client -B` format subscription changed.
    Subscription {
        name: String,
        pane: Option<String>,
        value: String,
    },
    Pause {
        pane: String,
    },
    Continue {
        pane: String,
    },
    /// The control client is detaching; the stream ends.
    Exit {
        reason: Option<String>,
    },
    /// Any other notification (`%window-add @1`, `%session-changed ...`).
    Notification {
        name: String,
        args: String,
    },
}

/// Incremental parser for tmux control-mode output.
#[derive(Debug)]
pub struct ControlParser {
    line: Vec<u8>,
    block: Option<(String, Vec<String>)>,
    max_line: usize,
    overflow: bool,
}

impl Default for ControlParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlParser {
    pub fn new() -> Self {
        Self {
            line: Vec::new(),
            block: None,
            max_line: 16 * 1024 * 1024,
            overflow: false,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<ControlEvent> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while let Some(pos) = rest.iter().position(|&b| b == b'\n') {
            let (head, tail) = rest.split_at(pos);
            rest = &tail[1..];
            if self.overflow {
                self.overflow = false;
                self.line.clear();
                continue;
            }
            self.line.extend_from_slice(head);
            let line = std::mem::take(&mut self.line);
            self.line_done(&line, &mut out);
        }
        if !self.overflow {
            self.line.extend_from_slice(rest);
            if self.line.len() > self.max_line {
                self.line.clear();
                self.overflow = true;
            }
        }
        out
    }

    fn line_done(&mut self, line: &[u8], out: &mut Vec<ControlEvent>) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some((number, lines)) = &mut self.block {
            // Reply lines may themselves start with '%'; only the matching
            // %end/%error closes the block.
            let text = String::from_utf8_lossy(line);
            let mut words = text.split(' ');
            let tag = words.next();
            let num = words.nth(1);
            if matches!(tag, Some("%end") | Some("%error")) && num == Some(number.as_str()) {
                let ok = tag == Some("%end");
                let lines = std::mem::take(lines);
                self.block = None;
                out.push(ControlEvent::Reply { ok, lines });
            } else {
                lines.push(text.into_owned());
            }
            return;
        }
        if let Some(rest) = line.strip_prefix(b"%output ") {
            if let Some(sp) = rest.iter().position(|&b| b == b' ') {
                out.push(ControlEvent::Output {
                    pane: String::from_utf8_lossy(&rest[..sp]).into_owned(),
                    bytes: unescape(&rest[sp + 1..]),
                });
            }
            return;
        }
        if let Some(rest) = line.strip_prefix(b"%extended-output ") {
            // %extended-output %pane age ... : data
            if let (Some(sp), Some(sep)) = (
                rest.iter().position(|&b| b == b' '),
                rest.windows(3).position(|w| w == b" : "),
            ) {
                out.push(ControlEvent::Output {
                    pane: String::from_utf8_lossy(&rest[..sp]).into_owned(),
                    bytes: unescape(&rest[sep + 3..]),
                });
            }
            return;
        }
        let text = String::from_utf8_lossy(line);
        let (name, args) = text.split_once(' ').unwrap_or((&text, ""));
        match name {
            "%begin" => {
                let number = args.split(' ').nth(1).unwrap_or_default().to_owned();
                self.block = Some((number, Vec::new()));
            }
            "%exit" => out.push(ControlEvent::Exit {
                reason: Some(args.to_owned()).filter(|a| !a.is_empty()),
            }),
            "%pause" => out.push(ControlEvent::Pause {
                pane: args.to_owned(),
            }),
            "%continue" => out.push(ControlEvent::Continue {
                pane: args.to_owned(),
            }),
            "%subscription-changed" => {
                // name $session @window index %pane ... : value
                let (head, value) = args.split_once(" : ").unwrap_or((args, ""));
                let mut words = head.split(' ');
                let name = words.next().unwrap_or_default().to_owned();
                let pane = words.nth(3).filter(|p| p.starts_with('%')).map(str::to_owned);
                out.push(ControlEvent::Subscription {
                    name,
                    pane,
                    value: value.to_owned(),
                });
            }
            _ if name.starts_with('%') => out.push(ControlEvent::Notification {
                name: name[1..].to_owned(),
                args: args.to_owned(),
            }),
            _ => {}
        }
    }
}

/// Decodes `%output` data: bytes below 0x20 and `\` arrive as `\ooo`.
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'\\'
            && let Some(oct) = data.get(i + 1..i + 4)
            && oct.iter().all(|b| (b'0'..=b'7').contains(b))
        {
            let v = oct.iter().fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
            out.push(u8::try_from(v).unwrap_or(b'?'));
            i += 4;
            continue;
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn pane() -> PaneRef {
        PaneRef {
            multiplexer: Multiplexer::Tmux,
            server: "/tmp/tmux-501/default".into(),
            session: None,
            pane: "%3".into(),
        }
    }

    #[test]
    fn parses_control_stream() {
        let mut p = ControlParser::new();
        // Recorded from tmux 3.4, split mid-line.
        let mut ev =
            p.feed(b"%begin 1791634500 276 1\n%0 990\n%end 1791634500 276 1\n%output %0 \\033]7501;st");
        ev.extend(p.feed(b"ate=idle\\007\\015\\012# \n%subscription-changed t $0 @0 0 %0 : vm\n%window-renamed @0 sh\n%exit\n"));
        assert_eq!(
            ev,
            [
                ControlEvent::Reply {
                    ok: true,
                    lines: vec!["%0 990".into()]
                },
                ControlEvent::Output {
                    pane: "%0".into(),
                    bytes: b"\x1b]7501;state=idle\x07\r\n# ".to_vec()
                },
                ControlEvent::Subscription {
                    name: "t".into(),
                    pane: Some("%0".into()),
                    value: "vm".into()
                },
                ControlEvent::Notification {
                    name: "window-renamed".into(),
                    args: "@0 sh".into()
                },
                ControlEvent::Exit { reason: None },
            ]
        );
    }

    #[test]
    fn error_reply_and_extended_output() {
        let mut p = ControlParser::new();
        let ev = p.feed(
            b"%begin 1 7 1\ncan't find pane: %9\n%error 1 7 1\n%extended-output %1 12 : a\\134b\xc3\xa9\n",
        );
        assert_eq!(
            ev[0],
            ControlEvent::Reply {
                ok: false,
                lines: vec!["can't find pane: %9".into()]
            }
        );
        assert_eq!(
            ev[1],
            ControlEvent::Output {
                pane: "%1".into(),
                bytes: "a\\bé".as_bytes().to_vec()
            }
        );
    }

    #[test]
    fn parses_list_panes() {
        let t = Target {
            multiplexer: Multiplexer::Tmux,
            server: String::new(),
            session: None,
        };
        let out = "%2 2257 1 0 /dev/pts/4 my\\ sess pi /home/u/my\\ proj π working: fix tests\n%0 990 0 1 /dev/pts/3 s1 sh /tmp \n";
        let panes = Tmux.parse_panes(&t, out).unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane.session.as_deref(), Some("my sess"));
        assert_eq!(panes[0].cwd.as_deref(), Some("/home/u/my proj"));
        assert_eq!(panes[0].title.as_deref(), Some("π working: fix tests"));
        assert_eq!(panes[0].pid, Some(2257));
        assert!(panes[0].active && !panes[0].exited);
        assert!(panes[1].exited);
        assert_eq!(panes[1].title, None);
        assert!(Tmux.parse_panes(&t, "garbage\n").is_err());
    }

    #[test]
    fn builds_quoted_commands() {
        let p = pane();
        assert_eq!(
            Tmux.send(&p, &Input::Text("it's $HOME".into())).script,
            "tmux -S /tmp/tmux-501/default send-keys -t %3 -l -- 'it'\\''s $HOME'"
        );
        assert_eq!(
            Tmux.send(
                &p,
                &Input::Keys(vec![Key::Escape, Key::Char('1'), Key::Ctrl('C')])
            )
            .script,
            "tmux -S /tmp/tmux-501/default send-keys -t %3 Escape ';' send-keys -t %3 -l -- 1 ';' send-keys -t %3 C-c"
        );
        let paste = Tmux.send(&p, &Input::Paste("a\nb".into()));
        assert_eq!(paste.stdin, b"a\nb");
        assert!(paste.script.contains("paste-buffer -p -d -b acc-_3 -t %3"));
        assert!(Tmux.capture(&p, true).script.ends_with("-S -"));
        assert_eq!(
            control_command(&["refresh-client", "-B", "t:%3:#{pane_title}"]),
            "refresh-client -B 't:%3:#{pane_title}'\n"
        );
    }

    #[test]
    fn spawn_targets_session_or_creates_one() {
        let prog = Program {
            program: "pi".into(),
            args: vec!["--session".into(), "/s/a b.jsonl".into()],
            env: vec![("PI_PROGRAM_STATUS".into(), "1".into())],
        };
        let t = Target {
            multiplexer: Multiplexer::Tmux,
            server: String::new(),
            session: Some("work".into()),
        };
        let s = Tmux.spawn(&t, &prog, "/home/u", "acc-1").script;
        assert_eq!(
            s,
            "tmux new-window -d -t =work: -n acc-1 -P -F '#{pane_id}' -c /home/u -- env PI_PROGRAM_STATUS=1 pi --session '/s/a b.jsonl'"
        );
        let p = Tmux.parse_spawned(&t, "acc-1", "%7\n").unwrap();
        assert_eq!(p.pane, "%7");
        let none = Target { session: None, ..t };
        assert!(
            Tmux.spawn(&none, &prog, "/", "acc-1")
                .script
                .starts_with("tmux new-session -d -s acc-1")
        );
    }
}

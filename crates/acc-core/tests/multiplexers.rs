//! Drives real tmux, zellij and screen servers with the commands the
//! multiplexer backends build, run through `sh` exactly as an SSH exec
//! channel would run them.
//!
//! A multiplexer that isn't installed is skipped, unless `ACC_REQUIRE_MUX=1`
//! (set in CI), which makes a missing one a failure.
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use acc_core::multiplexer::{
    Backend, Exec, Input, Key, Multiplexer, PaneObserver, PaneRef, Program, Target, screen_match, tmux,
    zellij,
};
use acc_core::state::{BlockKind, RunState};

const TIMEOUT: Duration = Duration::from_secs(15);

fn available(program: &str) -> bool {
    let found = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {program}"))
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !found {
        assert!(
            std::env::var("ACC_REQUIRE_MUX").is_err(),
            "{program} is required (ACC_REQUIRE_MUX is set) but not installed"
        );
        eprintln!("skipping: {program} not installed");
    }
    found
}

struct Env {
    home: PathBuf,
}

impl Env {
    fn new(name: &str) -> Self {
        let home = std::env::temp_dir().join(format!("acc-mux-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        Self { home }
    }

    fn sh(&self, script: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg(script)
            .env("HOME", &self.home)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("ZELLIJ")
            .env_remove("ZELLIJ_SESSION_NAME")
            .env_remove("ZELLIJ_PANE_ID")
            .env_remove("STY")
            .env_remove("WINDOW");
        c
    }

    /// Runs a one-shot command and returns its stdout; panics on failure.
    fn run(&self, exec: &Exec) -> String {
        let mut child = self
            .sh(&exec.script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&exec.stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "`{}` failed: {}",
            exec.script,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Starts a long-running watch; stdout chunks arrive on the receiver.
    fn watch(&self, exec: &Exec) -> (Child, ChildStdin, Receiver<Vec<u8>>) {
        let mut child = self
            .sh(&exec.script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(&exec.stdin).unwrap();
        stdin.flush().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        (child, stdin, rx)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Polls `check` until it returns true or the timeout passes.
fn eventually(what: &str, check: impl FnMut() -> bool) {
    eventually_within(TIMEOUT, what, check);
}

fn eventually_within(timeout: Duration, what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn shell() -> Program {
    Program {
        program: "sh".into(),
        args: vec![],
        env: vec![("PS1".into(), "$ ".into())],
    }
}

fn type_line(b: &dyn Backend, pane: &PaneRef, line: &str) -> [Exec; 2] {
    [
        b.send(pane, &Input::Text(line.into())),
        b.send(pane, &Input::Keys(vec![Key::Enter])),
    ]
}

/// Shell line that prints an OSC 7501 report. The typed text has no escape
/// bytes, so only printf's output carries the real sequence.
const REPORT: &str = r"printf '\033]7501;state=blocked:kind=question:app=pi\033\\'";

fn blocked(observer: &mut PaneObserver, bytes: &[u8]) -> bool {
    observer.feed(bytes, 0).iter().any(|u| {
        u.state.as_ref().map(|s| &s.state)
            == Some(&RunState::Blocked {
                kind: BlockKind::Question,
            })
    })
}

#[test]
fn tmux_end_to_end() {
    if !available("tmux") {
        return;
    }
    let env = Env::new("tmux");
    let sock = env.home.join("sock");
    let sock = sock.to_str().unwrap();
    let b = acc_core::multiplexer::backend(Multiplexer::Tmux);
    env.run(&Exec {
        script: format!("tmux -S {sock} new-session -d -s 'acc test' -x 80 -y 24 sh"),
        stdin: vec![],
    });
    let target = Target {
        multiplexer: Multiplexer::Tmux,
        server: sock.into(),
        session: Some("acc test".into()),
    };

    let panes = b.parse_panes(&target, &env.run(&b.list_panes(&target))).unwrap();
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].pane.session.as_deref(), Some("acc test"));
    assert!(panes[0].pid.is_some() && panes[0].tty.is_some());

    let out = env.run(&b.spawn(&target, &shell(), env.home.to_str().unwrap(), "acc-1"));
    let pane = b.parse_spawned(&target, "acc-1", &out).unwrap();
    let watch = b.watch(&pane);
    let (mut child, _stdin, rx) = env.watch(&watch.exec);

    // Control mode delivers the pane's raw bytes, OSC 7501 included.
    for e in type_line(b, &pane, REPORT) {
        env.run(&e);
    }
    let mut parser = tmux::ControlParser::new();
    let mut observer = PaneObserver::new();
    let (mut saw_report, mut saw_subscription) = (false, false);
    eventually("an OSC 7501 report in %output and a subscription update", || {
        while let Ok(chunk) = rx.try_recv() {
            for ev in parser.feed(&chunk) {
                match ev {
                    tmux::ControlEvent::Output { pane: p, bytes } if p == pane.pane => {
                        saw_report |= blocked(&mut observer, &bytes);
                    }
                    tmux::ControlEvent::Subscription { name, pane: p, .. } => {
                        saw_subscription |= name == "acc-command" && p.as_ref() == Some(&pane.pane);
                    }
                    _ => {}
                }
            }
        }
        saw_report && saw_subscription
    });

    // A multi-line paste arrives as one bracketed paste, not three commands.
    env.run(&b.send(&pane, &Input::Paste("echo one\necho two".into())));
    eventually("pasted text on screen", || {
        env.run(&b.capture(&pane, true)).contains("echo two")
    });

    env.run(&b.kill(&pane).unwrap());
    let _ = child.kill();
    let _ = child.wait();
    env.run(&Exec {
        script: format!("tmux -S {sock} kill-server"),
        stdin: vec![],
    });
}

#[test]
fn zellij_end_to_end() {
    if !available("zellij") {
        return;
    }
    let env = Env::new("zellij");
    let session = format!("acc-test-{}", std::process::id());
    let b = acc_core::multiplexer::backend(Multiplexer::Zellij);
    env.run(&Exec {
        script: format!("zellij attach --create-background {session}"),
        stdin: vec![],
    });
    let target = Target {
        multiplexer: Multiplexer::Zellij,
        server: session.clone(),
        session: Some(session.clone()),
    };
    eventually("zellij session to list panes", || {
        let mut c = env.sh(&b.list_panes(&target).script);
        c.stderr(Stdio::null());
        c.output()
            .ok()
            .and_then(|o| b.parse_panes(&target, &String::from_utf8_lossy(&o.stdout)).ok())
            .is_some_and(|p| !p.is_empty())
    });

    let out = env.run(&b.spawn(&target, &shell(), "/", "acc-1"));
    let pane = b.parse_spawned(&target, "acc-1", &out).unwrap();
    let panes = b.parse_panes(&target, &env.run(&b.list_panes(&target))).unwrap();
    assert!(panes.iter().any(|p| p.pane == pane), "{panes:?}");

    let watch = b.watch(&pane);
    let (mut child, _stdin, rx) = env.watch(&watch.exec);
    for e in type_line(b, &pane, "echo hi-$((40+2))") {
        env.run(&e);
    }
    let mut lines = String::new();
    eventually("rendered output in subscribe updates", || {
        while let Ok(chunk) = rx.try_recv() {
            lines.push_str(&String::from_utf8_lossy(&chunk));
        }
        lines
            .lines()
            .filter_map(zellij::parse_update)
            .any(|u| u.pane_id == pane.pane && u.viewport.iter().any(|l| l == "hi-42"))
    });
    env.run(&b.send(&pane, &Input::Paste("echo one\necho two".into())));
    eventually("pasted text on screen", || {
        env.run(&b.capture(&pane, false)).contains("echo two")
    });

    env.run(&b.kill(&pane).unwrap());
    let _ = child.kill();
    let _ = child.wait();
    let _ = env
        .sh(&format!(
            "zellij kill-session {session}; zellij delete-session --force {session}"
        ))
        .output();
}

#[test]
fn screen_end_to_end() {
    if !available("screen") {
        return;
    }
    // macOS still ships screen 4.00, which predates `-Q`; ACC needs 4.01+.
    let version = Command::new("screen").arg("-v").output().unwrap();
    if String::from_utf8_lossy(&version.stdout).contains(" 4.00") {
        eprintln!("skipping: screen 4.00 has no -Q");
        return;
    }
    let env = Env::new("screen");
    let session = format!("acc-test-{}", std::process::id());
    let b = acc_core::multiplexer::backend(Multiplexer::Screen);
    env.run(&Exec {
        script: format!("screen -dmS {session} sh"),
        stdin: vec![],
    });
    let target = Target {
        multiplexer: Multiplexer::Screen,
        server: session.clone(),
        session: Some(session.clone()),
    };
    let panes = b.parse_panes(&target, &env.run(&b.list_panes(&target))).unwrap();
    assert_eq!(panes.len(), 1);

    let out = env.run(&b.spawn(&target, &shell(), "/", "acc-1"));
    let pane = b.parse_spawned(&target, "acc-1", &out).unwrap();
    assert_eq!(pane.pane, "1");

    // The window log carries raw bytes, OSC 7501 included.
    let watch = b.watch(&pane);
    let (mut child, _stdin, rx) = env.watch(&watch.exec);
    std::thread::sleep(Duration::from_millis(300));
    for e in type_line(b, &pane, REPORT) {
        env.run(&e);
    }
    let mut observer = PaneObserver::new();
    eventually("an OSC 7501 report in the window log", || {
        let mut hit = false;
        while let Ok(chunk) = rx.try_recv() {
            hit |= blocked(&mut observer, &chunk);
        }
        hit
    });
    for e in type_line(b, &pane, "echo it\\'s $((40+2))") {
        env.run(&e);
    }
    eventually("typed text on screen", || {
        env.run(&b.capture(&pane, false)).contains("it's 42")
    });

    let _ = child.kill();
    let _ = child.wait();
    env.run(watch.cleanup.as_ref().unwrap());
    let log_dir = env.home.join(".acc/screen");
    assert_eq!(
        std::fs::read_dir(&log_dir).unwrap().count(),
        0,
        "log file left behind"
    );
    env.run(&b.kill(&pane).unwrap());
    let _ = env.sh(&format!("screen -S {session} -X quit")).output();
}

#[test]
fn tmux_lists_servers_script_runs() {
    let env = Env::new("servers");
    // No server for this HOME/uid may exist; the script must still succeed.
    let _ = env.run(&tmux::list_servers());
}

/// Co-drives a real pi TUI in tmux: launch it with the TUI launcher, follow
/// its state, prompt it with typed keys. pi 1.x reports OSC 7501 through
/// control mode; older pi (0.82) sends none, so the screen is read too and
/// either source passes.
///
/// Needs `pi` on PATH (or `ACC_PI` naming it) and `ACC_PI_HOME` naming a HOME
/// whose `.pi/agent/models.json` selects a working model (a local fake
/// OpenAI-compatible server is enough), so it is ignored by default:
/// `ACC_PI_HOME=... cargo test -p acc-core --test multiplexers -- --ignored pi`.
#[test]
#[ignore = "needs pi and a configured model"]
fn pi_tui_in_tmux() {
    use acc_core::Harness;
    use acc_core::command::Command as AccCommand;
    use acc_core::multiplexer::tui;
    use acc_core::state::StateSource;

    let pi_home = std::env::var("ACC_PI_HOME").expect("ACC_PI_HOME");
    let env = Env::new("pi");
    let sock = env.home.join("sock");
    let sock = sock.to_str().unwrap();
    let b = acc_core::multiplexer::backend(Multiplexer::Tmux);
    env.run(&Exec {
        script: format!("tmux -S {sock} new-session -d -s pi -x 120 -y 40 sh"),
        stdin: vec![],
    });
    let target = Target {
        multiplexer: Multiplexer::Tmux,
        server: sock.into(),
        session: Some("pi".into()),
    };
    let mut program = tui::launch(Harness::Pi, None).unwrap();
    if let Ok(pi) = std::env::var("ACC_PI") {
        program.program = pi;
    }
    program.env.push(("HOME".into(), pi_home));
    // Without network pi's startup waits on tool downloads (fd, rg) and
    // ignores Enter until they give up.
    program.env.push(("PI_OFFLINE".into(), "1".into()));
    let out = env.run(&b.spawn(&target, &program, env.home.to_str().unwrap(), "acc-pi"));
    let pane = b.parse_spawned(&target, "acc-pi", &out).unwrap();
    let (mut child, _stdin, rx) = env.watch(&b.watch(&pane).exec);

    let mut parser = tmux::ControlParser::new();
    let mut observer = PaneObserver::new();
    let mut screen = screen_match::ScreenObserver::new(Harness::Pi);
    let mut states: Vec<(StateSource, RunState)> = Vec::new();
    let mut drain = |states: &mut Vec<(StateSource, RunState)>| {
        let mut output = false;
        while let Ok(chunk) = rx.try_recv() {
            for ev in parser.feed(&chunk) {
                if let tmux::ControlEvent::Output { pane: p, bytes } = ev
                    && p == pane.pane
                {
                    output = true;
                    states.extend(
                        observer
                            .feed(&bytes, 0)
                            .into_iter()
                            .filter_map(|u| u.state.map(|s| (s.source, s.state))),
                    );
                }
            }
        }
        // Control mode has raw bytes, not the screen: capture after output.
        if output || states.is_empty() {
            for ev in screen.feed(&env.run(&b.capture(&pane, false)), 0) {
                if let acc_core::Event::StateChanged(s) = ev {
                    states.push((s.source, s.state));
                }
            }
        }
    };
    eventually("pi to show idle", || {
        drain(&mut states);
        states.iter().any(|(_, s)| *s == RunState::Idle)
    });
    for input in tui::inputs(
        Harness::Pi,
        &AccCommand::Prompt {
            text: "list files".into(),
        },
    )
    .unwrap()
    {
        env.run(&b.send(&pane, &input));
    }
    eventually("pi to work, then finish", || {
        drain(&mut states);
        let w = states
            .iter()
            .position(|(_, s)| matches!(s, RunState::Working { .. }));
        w.is_some_and(|w| {
            states[w..]
                .iter()
                .any(|(_, s)| matches!(s, RunState::Done | RunState::Idle))
        })
    });
    eprintln!("pi states: {states:?}");
    for input in tui::exit_inputs(Harness::Pi).unwrap() {
        env.run(&b.send(&pane, &input));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = env.sh(&format!("tmux -S {sock} kill-server")).output();
}

/// Starts a background multiplexer server for the live harness tests and
/// returns its target plus the script that stops it.
fn live_server(env: &Env, m: Multiplexer) -> (Target, String) {
    let b = acc_core::multiplexer::backend(m);
    let target = match m {
        Multiplexer::Tmux => {
            let sock = env.home.join("sock").to_str().unwrap().to_owned();
            env.run(&Exec {
                script: format!("tmux -S {sock} new-session -d -s live -x 120 -y 40 sh"),
                stdin: vec![],
            });
            Target {
                multiplexer: m,
                server: sock,
                session: Some("live".into()),
            }
        }
        Multiplexer::Zellij => {
            let session = format!("acc-live-{}", std::process::id());
            env.run(&Exec {
                script: format!("zellij attach --create-background {session}"),
                stdin: vec![],
            });
            let target = Target {
                multiplexer: m,
                server: session.clone(),
                session: Some(session),
            };
            eventually("zellij session to list panes", || {
                let mut c = env.sh(&b.list_panes(&target).script);
                c.stderr(Stdio::null());
                c.output()
                    .ok()
                    .and_then(|o| b.parse_panes(&target, &String::from_utf8_lossy(&o.stdout)).ok())
                    .is_some_and(|p| !p.is_empty())
            });
            target
        }
        _ => unreachable!("live tests cover tmux and zellij"),
    };
    let stop = match m {
        Multiplexer::Tmux => format!("tmux -S {} kill-server", target.server),
        _ => format!(
            "zellij kill-session {s}; zellij delete-session --force {s}",
            s = target.server
        ),
    };
    (target, stop)
}

/// Runs a real Claude Code TUI in a pane and answers its dialogs from the
/// screen alone: the folder trust dialog (unnumbered), then a Bash permission
/// dialog (numbered), as ACC must for a session it didn't start.
///
/// Needs `claude` on PATH and `ACC_CLAUDE_HOME` naming a throwaway HOME where
/// it is signed in (the test trusts a temp folder there), so it is ignored:
/// `ACC_CLAUDE_HOME=... cargo test -p acc-core --test multiplexers -- --ignored claude`.
fn claude_dialogs_in(m: Multiplexer) {
    use acc_core::Harness;
    use acc_core::command::Command as AccCommand;
    use acc_core::multiplexer::tui;
    use acc_core::state::BlockKind;

    let home = std::env::var("ACC_CLAUDE_HOME").expect("ACC_CLAUDE_HOME");
    let env = Env::new(&format!("claude-{m:?}"));
    let work = env.home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let (target, stop) = live_server(&env, m);
    let b = acc_core::multiplexer::backend(m);
    // A Claude Code parent's session variables would leak into the child.
    let program = Program {
        program: "env".into(),
        args: [
            "-u",
            "CLAUDE_CODE_SESSION_ID",
            "-u",
            "CLAUDE_CODE_REMOTE_SESSION_ID",
            "-u",
            "CLAUDECODE",
            "-u",
            "CLAUDE_CODE_ENTRYPOINT",
        ]
        .into_iter()
        .map(String::from)
        .chain([format!("HOME={home}")])
        .chain(["claude", "--permission-mode", "default"].map(String::from))
        .collect(),
        env: vec![],
    };
    let out = env.run(&b.spawn(&target, &program, work.to_str().unwrap(), "acc-claude"));
    let pane = b.parse_spawned(&target, "acc-claude", &out).unwrap();

    let mut observer = screen_match::ScreenObserver::new(Harness::Claude);
    let mut screen = String::new();
    let mut events = Vec::new();
    let mut poll = |screen: &mut String, events: &mut Vec<acc_core::Event>| {
        *screen = env.run(&b.capture(&pane, false));
        events.extend(observer.feed(screen, 0));
        observer.open_dialog().map(str::to_owned)
    };
    let answer = |screen: &str, id: &str, option: &str| {
        // A dialog drawn on the first frame ignores keys until Claude reads input.
        std::thread::sleep(Duration::from_secs(1));
        for input in screen_match::answer(Harness::Claude, screen, id, option).unwrap() {
            env.run(&b.send(&pane, &input));
        }
    };
    let long = Duration::from_secs(90);

    // A fresh HOME asks to trust the folder first (or the theme, onboarding).
    let mut dialog = None;
    eventually_within(long, "Claude's prompt or trust dialog", || {
        dialog = poll(&mut screen, &mut events);
        if dialog.is_none() && screen.contains("Press Enter to continue") {
            env.run(&b.send(&pane, &Input::Keys(vec![Key::Enter])));
        }
        dialog.is_some() || screen_match::read(Harness::Claude, &screen).is_some()
    });
    let trust = screen_match::read(Harness::Claude, &screen)
        .and_then(|r| r.dialog)
        .filter(|d| d.title.starts_with("Accessing workspace"));
    if let (Some(id), Some(_)) = (dialog, trust) {
        answer(&screen, &id, "2");
    }
    eventually_within(long, "Claude's idle prompt", || {
        poll(&mut screen, &mut events);
        screen_match::read(Harness::Claude, &screen).map(|r| r.state) == Some(RunState::Idle)
    });

    // Claude draws its prompt before it finishes starting up.
    std::thread::sleep(Duration::from_secs(2));
    let file = work.join("acc-made-this");
    let prompt = format!(
        "Run this shell command and nothing else: touch {}",
        file.display()
    );
    for input in tui::inputs(Harness::Claude, &AccCommand::Prompt { text: prompt }).unwrap() {
        env.run(&b.send(&pane, &input));
    }
    let mut id = None;
    eventually_within(long, "a Bash permission dialog", || {
        id = poll(&mut screen, &mut events);
        id.is_some()
    });
    let read = screen_match::read(Harness::Claude, &screen).unwrap();
    let dialog = read.dialog.unwrap();
    assert_eq!(dialog.kind, BlockKind::Permission, "{screen}");
    assert_eq!(dialog.options[0].label, "Yes", "{screen}");
    answer(&screen, &dialog.id, "1");
    eventually_within(long, "the command to run and Claude to go idle", || {
        poll(&mut screen, &mut events);
        file.exists() && screen_match::read(Harness::Claude, &screen).map(|r| r.state) == Some(RunState::Idle)
    });
    assert!(
        events.iter().any(
            |e| matches!(e, acc_core::Event::PermissionResolved { request_id } if *request_id == dialog.id)
        ),
        "{events:?}"
    );

    for input in tui::exit_inputs(Harness::Claude).unwrap() {
        env.run(&b.send(&pane, &input));
    }
    let _ = env.sh(&stop).output();
}

#[test]
#[ignore = "needs claude and a signed-in HOME"]
fn claude_dialogs_in_tmux() {
    if available("tmux") {
        claude_dialogs_in(Multiplexer::Tmux);
    }
}

#[test]
#[ignore = "needs claude and a signed-in HOME"]
fn claude_dialogs_in_zellij() {
    if available("zellij") {
        claude_dialogs_in(Multiplexer::Zellij);
    }
}

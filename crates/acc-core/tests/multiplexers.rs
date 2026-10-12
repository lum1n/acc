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
    Backend, Exec, Input, Key, Multiplexer, PaneObserver, PaneRef, Program, Target, tmux, zellij,
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
fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
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

/// Co-drives a real pi TUI in tmux: launch it with the TUI launcher, see its
/// OSC 7501 reports through control mode, prompt it with typed keys.
///
/// Needs `pi` on PATH and `ACC_PI_HOME` naming a HOME whose
/// `.pi/agent/models.json` selects a working model (a local fake
/// OpenAI-compatible server is enough), so it is ignored by default:
/// `ACC_PI_HOME=... cargo test -p acc-core --test multiplexers -- --ignored`.
#[test]
#[ignore = "needs pi and a configured model"]
fn pi_tui_in_tmux() {
    use acc_core::Harness;
    use acc_core::command::Command as AccCommand;
    use acc_core::multiplexer::tui;

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
    program.env.push(("HOME".into(), pi_home));
    // Without network pi's startup waits on tool downloads (fd, rg) and
    // ignores Enter until they give up.
    program.env.push(("PI_OFFLINE".into(), "1".into()));
    let out = env.run(&b.spawn(&target, &program, env.home.to_str().unwrap(), "acc-pi"));
    let pane = b.parse_spawned(&target, "acc-pi", &out).unwrap();
    let (mut child, _stdin, rx) = env.watch(&b.watch(&pane).exec);

    let mut parser = tmux::ControlParser::new();
    let mut observer = PaneObserver::new();
    let mut states = Vec::new();
    let mut drain = |states: &mut Vec<RunState>| {
        while let Ok(chunk) = rx.try_recv() {
            for ev in parser.feed(&chunk) {
                if let tmux::ControlEvent::Output { pane: p, bytes } = ev
                    && p == pane.pane
                {
                    states.extend(
                        observer
                            .feed(&bytes, 0)
                            .into_iter()
                            .filter_map(|u| u.state.map(|s| s.state)),
                    );
                }
            }
        }
    };
    eventually("pi to report idle", || {
        drain(&mut states);
        states.contains(&RunState::Idle)
    });
    for input in tui::inputs(Harness::Pi, &AccCommand::prompt("list files")).unwrap() {
        env.run(&b.send(&pane, &input));
    }
    eventually("pi to report working then done", || {
        drain(&mut states);
        let w = states.iter().position(|s| matches!(s, RunState::Working { .. }));
        w.is_some_and(|w| states[w..].contains(&RunState::Done))
    });
    eprintln!("pi states: {states:?}");
    for input in tui::exit_inputs(Harness::Pi).unwrap() {
        env.run(&b.send(&pane, &input));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = env.sh(&format!("tmux -S {sock} kill-server")).output();
}

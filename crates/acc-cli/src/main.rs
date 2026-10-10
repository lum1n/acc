//! `acc`: a small command-line front end to acc-core, mainly for spikes on
//! real hosts. Scripts it prints can be piped to `ssh host sh`, which is
//! exactly what the mobile client does over an SSH exec channel.

mod doctor;
mod run;

use std::io::{Read as _, Write as _};
use std::process::{Command, ExitCode};

use acc_core::Harness;
use acc_core::detached::SessionDir;
use acc_core::driver::Driver as _;
use acc_core::multiplexer::{self, Multiplexer, PaneRef, Target, tui};
use acc_core::osc7501::Scanner;
use acc_core::pi::{PiDriver, Transcript};

const USAGE: &str = "\
usage:
  acc doctor                            installed harnesses, multiplexers and session stores
  acc run <harness> [cwd] [--resume ID] [--model M] [--program PATH] [--json]
                                        drive a harness headlessly (claude, codex, pi, cursor,
                                        copilot, opencode); type prompts or :help on stdin
  acc sessions <claude|codex|pi> [N]    recent sessions on this machine, with resume commands
  acc harnesses                         detect installed harnesses and versions
  acc osc7501                           read terminal bytes on stdin, print OSC 7501 messages as JSON
  acc script launch <harness> <id> <cwd> [resume-id]
  acc script attach <id> [offset]
  acc script input <id>
  acc script status <id>                print a host-side shell script (pipe to `ssh host sh`)
  acc script mux <tmux|zellij|screen|rex> <server|-> <session|-> list
  acc script mux <mux> <server> <session|-> capture|watch <pane>
  acc script mux <mux> <server> <session|-> type <pane> <text>
                                        multiplexer commands (`type` submits text with Enter)
  acc decode pi|pi-transcript|claude-transcript|codex-rollout
                                        read harness output or a session file on stdin, print events
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["harnesses"] => harnesses(),
        ["osc7501"] => osc(),
        ["script", "mux", rest @ ..] => mux(rest),
        ["doctor"] => doctor::doctor(),
        ["run", harness, rest @ ..] => or_usage(parse_harness(harness).and_then(|h| run::run(h, rest))),
        // `acc acp` predates `acc run` and is kept for ACP harnesses.
        ["acp", harness, rest @ ..] => or_usage(parse_harness(harness).and_then(|h| {
            if h.native_acp() {
                run::run(h, rest)
            } else {
                Err(format!("{} has no native ACP server", h.display_name()))
            }
        })),
        ["sessions", harness, rest @ ..] => match (parse_harness(harness), rest) {
            (Ok(h), []) => doctor::sessions(h, 20),
            (Ok(h), [n]) if n.parse::<usize>().is_ok() => doctor::sessions(h, n.parse().unwrap_or(20)),
            (Err(e), _) => or_usage(Err(e)),
            _ => or_usage(Err(USAGE.to_owned())),
        },
        ["script", rest @ ..] => script(rest),
        ["decode", kind] => decode(kind),
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn or_usage(r: Result<ExitCode, String>) -> ExitCode {
    r.unwrap_or_else(|e| {
        eprintln!("acc: {e}");
        ExitCode::from(2)
    })
}

fn harnesses() -> ExitCode {
    for h in Harness::ALL {
        let version = Command::new(h.program())
            .args(h.version_args())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_owned()
            });
        match version {
            Some(v) => println!("{:<10} {:<9} {v}", h.id(), h.program()),
            None => println!("{:<10} {:<9} (not found)", h.id(), h.program()),
        }
    }
    ExitCode::SUCCESS
}

fn osc() -> ExitCode {
    let mut scanner = Scanner::new();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 8192];
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => return ExitCode::SUCCESS,
            Ok(n) => n,
            Err(e) => {
                eprintln!("acc: {e}");
                return ExitCode::FAILURE;
            }
        };
        for msg in scanner.feed(&buf[..n]) {
            let line = serde_json::to_string(&msg).unwrap_or_default();
            if writeln!(stdout, "{line}").is_err() {
                return ExitCode::FAILURE;
            }
        }
    }
}

fn script(args: &[&str]) -> ExitCode {
    let dir = |id: &str| SessionDir::new(id).ok_or_else(|| format!("invalid session id {id:?}"));
    let out = match args {
        ["launch", harness, id, cwd, resume @ ..] => parse_harness(harness)
            .and_then(|h| Ok(dir(id)?.launch_script(&h.launch(resume.first().copied()), cwd))),
        ["attach", id] => dir(id).map(|d| d.attach_script(0)),
        ["attach", id, offset] => offset
            .parse()
            .map_err(|_| format!("invalid offset {offset:?}"))
            .and_then(|o| Ok(dir(id)?.attach_script(o))),
        ["input", id] => dir(id).map(|d| d.input_script()),
        ["status", id] => dir(id).map(|d| d.status_script()),
        _ => Err(USAGE.to_owned()),
    };
    match out {
        Ok(s) => {
            print!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("acc: {e}");
            ExitCode::from(2)
        }
    }
}

fn parse_harness(s: &str) -> Result<Harness, String> {
    Harness::ALL
        .into_iter()
        .find(|h| h.id() == s || h.program() == s || format!("{h:?}").eq_ignore_ascii_case(s))
        .ok_or_else(|| format!("unknown harness {s:?}"))
}

fn mux(args: &[&str]) -> ExitCode {
    let [m, server, session, rest @ ..] = args else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let m = match *m {
        "tmux" => Multiplexer::Tmux,
        "zellij" => Multiplexer::Zellij,
        "screen" => Multiplexer::Screen,
        "rex" => Multiplexer::Rex,
        other => {
            eprintln!("acc: unknown multiplexer {other:?}");
            return ExitCode::from(2);
        }
    };
    let b = multiplexer::backend(m);
    let target = Target {
        multiplexer: m,
        server: Some((*server).to_owned())
            .filter(|s| s != "-")
            .unwrap_or_default(),
        session: Some((*session).to_owned()).filter(|s| s != "-"),
    };
    let pane = |p: &str| PaneRef {
        multiplexer: m,
        server: target.server.clone(),
        session: target.session.clone(),
        pane: p.to_owned(),
    };
    // Inputs carried on stdin are shown as a heredoc so the output stays one script.
    let print = |e: multiplexer::Exec| {
        if e.stdin.is_empty() {
            println!("{}", e.script);
        } else {
            println!("{} <<'ACC_EOF'\n{}\nACC_EOF", e.script, heredoc_body(&e.stdin));
        }
    };
    match rest {
        ["list"] => print(b.list_panes(&target)),
        ["capture", p] => print(b.capture(&pane(p), true)),
        ["watch", p] => {
            let e = b.watch(&pane(p)).exec;
            if e.stdin.is_empty() {
                print(e);
            } else {
                // Watches that read stdin (tmux control mode) end when it
                // closes, so feed the setup through a FIFO this script holds
                // open until the watch exits.
                println!(
                    "d=$(mktemp -d) && mkfifo \"$d/in\" || exit 1\n{} <\"$d/in\" &\nexec 3>\"$d/in\"\nrm -rf \"$d\"\ncat >&3 <<'ACC_EOF'\n{}\nACC_EOF\nwait $!",
                    e.script,
                    heredoc_body(&e.stdin)
                );
            }
        }
        ["type", p, text] => {
            for input in tui::submit(text) {
                print(b.send(&pane(p), &input));
            }
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

/// stdin bytes as a heredoc body. The heredoc adds the final newline itself;
/// a doubled one would be an empty line, which detaches a tmux control client.
fn heredoc_body(stdin: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(stdin.strip_suffix(b"\n").unwrap_or(stdin))
}

type Decoder = dyn FnMut(&[u8]) -> Vec<acc_core::Event>;

fn decode(kind: &str) -> ExitCode {
    let mut feed: Box<Decoder> = match kind {
        "pi" => {
            let mut d = PiDriver::new();
            Box::new(move |b| d.on_output(b))
        }
        "pi-transcript" => {
            let mut t = Transcript::new();
            Box::new(move |b| t.feed(b))
        }
        "claude-transcript" => {
            let mut t = acc_core::claude::transcript::Observer::new();
            Box::new(move |b| t.feed(b))
        }
        "codex-rollout" => {
            let mut t = acc_core::codex::rollout::Observer::new();
            Box::new(move |b| t.feed(b))
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 65536];
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => return ExitCode::SUCCESS,
            Ok(n) => n,
            Err(e) => {
                eprintln!("acc: {e}");
                return ExitCode::FAILURE;
            }
        };
        for ev in feed(&buf[..n]) {
            let line = serde_json::to_string(&ev).unwrap_or_default();
            if writeln!(stdout, "{line}").is_err() {
                return ExitCode::FAILURE;
            }
        }
    }
}

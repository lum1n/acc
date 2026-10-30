//! `acc`: a small command-line front end to acc-core, mainly for spikes on
//! real hosts. Scripts it prints can be piped to `ssh host sh`, which is
//! exactly what the mobile client does over an SSH exec channel.

mod doctor;
mod remote;
mod run;

use std::io::{Read as _, Write as _};
use std::process::{Command, ExitCode};

use acc_core::Harness;
use acc_core::detached::SessionDir;
use acc_core::driver::Driver as _;
use acc_core::pi::PiDriver;

const USAGE: &str = "\
usage:
  acc doctor                            installed harnesses, and whether sessions can be sandboxed
  acc run <harness> [cwd] [--resume ID] [--model M] [--program PATH] [--json]
                                        drive a harness headlessly (claude, codex, pi, cursor,
                                        copilot, opencode); type prompts or :help on stdin
      [--sandbox [workspace|read-only]] [--sandbox-write PATH]... [--sandbox-hide PATH]...
      [--sandbox-show-credentials]
                                        confine it (Seatbelt on macOS, bubblewrap on Linux): write
                                        only the workspace and its own state, credentials hidden
      [--ssh [user@]host[:port] [--identity KEY] [--insecure] [--attach ID]]
                                        run it detached on a host over SSH, nothing installed
                                        there; end stdin to detach, --attach to rejoin
  acc ssh-sessions [user@]host[:port] [--identity KEY] [--insecure] [--stop ID]... [--prune]
                                        detached sessions on a host; stop one, remove ended ones
  acc harnesses                         detect installed harnesses and versions
  acc script launch <harness> <id> <cwd> [resume-id] [--sandbox [preset]] [--sandbox-...]
  acc script attach <id> [offset]
  acc script input <id>
  acc script status <id>                print a host-side shell script (pipe to `ssh host sh`)
  acc decode pi                         read pi RPC output on stdin, print events
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["harnesses"] => harnesses(),
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
        ["ssh-sessions", dest, rest @ ..] => or_usage(remote::sessions(dest, rest)),
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
            Some(v) => println!(
                "{:<10} {:<9} {v}{}",
                h.id(),
                h.program(),
                doctor::compat_note(h, &v)
            ),
            None => println!("{:<10} {:<9} (not found)", h.id(), h.program()),
        }
    }
    ExitCode::SUCCESS
}

fn script(args: &[&str]) -> ExitCode {
    let dir = |id: &str| SessionDir::new(id).ok_or_else(|| format!("invalid session id {id:?}"));
    let out = match args {
        ["launch", harness, id, cwd, rest @ ..] => parse_harness(harness).and_then(|h| {
            let mut sandbox = run::SandboxFlags::default();
            let mut resume = None;
            let mut it = rest.iter().copied().peekable();
            while let Some(arg) = it.next() {
                if !sandbox.take(arg, &mut it)? {
                    if arg.starts_with("--") || resume.is_some() {
                        return Err(USAGE.to_owned());
                    }
                    resume = Some(arg);
                }
            }
            let mut spec = h.launch(resume);
            spec.sandbox = sandbox.get().map(|s| acc_core::sandbox::rules(h, &s));
            Ok(dir(id)?.launch_script(&spec, cwd))
        }),
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

type Decoder = dyn FnMut(&[u8]) -> Vec<acc_core::Event>;

fn decode(kind: &str) -> ExitCode {
    let mut feed: Box<Decoder> = match kind {
        "pi" => {
            let mut d = PiDriver::new();
            Box::new(move |b| d.on_output(b))
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

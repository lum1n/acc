//! `acc`: a small command-line front end to acc-core, mainly for spikes on
//! real hosts. Scripts it prints can be piped to `ssh host sh`, which is
//! exactly what the mobile client does over an SSH exec channel.

use std::io::{Read as _, Write as _};
use std::process::{Command, ExitCode};

use acc_core::Harness;
use acc_core::detached::SessionDir;
use acc_core::osc7501::Scanner;

const USAGE: &str = "\
usage:
  acc harnesses                         detect installed harnesses and versions
  acc osc7501                           read terminal bytes on stdin, print OSC 7501 messages as JSON
  acc script launch <harness> <id> <cwd> [resume-id]
  acc script attach <id> [offset]
  acc script input <id>
  acc script status <id>                print a host-side shell script (pipe to `ssh host sh`)
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["harnesses"] => harnesses(),
        ["osc7501"] => osc(),
        ["script", rest @ ..] => script(rest),
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
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

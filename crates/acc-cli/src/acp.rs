//! `acc acp`: drives an ACP harness as a local child process. Events go to
//! stdout as NDJSON; each stdin line is a JSON `Command`, or plain text sent
//! as a prompt. For spikes on real hosts, e.g. `ssh host acc acp copilot ~/p`.

use std::io::{BufRead as _, Read as _, Write as _};
use std::process::{Command as Proc, ExitCode, Stdio};
use std::sync::mpsc;

use acc_core::Harness;
use acc_core::acp::{AcpConfig, AcpDriver, Start};
use acc_core::command::Command;
use acc_core::driver::Driver as _;

enum Input {
    Agent(Vec<u8>),
    AgentClosed,
    User(String),
}

pub fn run(harness: Harness, args: &[&str]) -> Result<ExitCode, String> {
    let (cwd, flags) = args.split_first().ok_or("missing <cwd>")?;
    let mut start = Start::New;
    let mut model = None;
    let mut program = None;
    let mut it = flags.iter();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .map(|v| (*v).to_owned())
                .ok_or(format!("{flag} needs a value"))
        };
        match *flag {
            "--resume" => start = Start::Resume { session_id: value()? },
            "--model" => model = Some(value()?),
            "--program" => program = Some(value()?),
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    let cwd = std::fs::canonicalize(cwd).map_err(|e| format!("{cwd}: {e}"))?;
    let mut cfg = AcpConfig::new(cwd.to_string_lossy(), start);
    cfg.model = model;
    cfg.client_tag = format!("cli{}", std::process::id());
    let mut driver = AcpDriver::new(harness, cfg);

    let spec = driver.launch(None);
    let mut child = Proc::new(program.as_deref().unwrap_or(&spec.program))
        .args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", spec.program))?;
    let mut agent_in = child.stdin.take().ok_or("no stdin")?;
    let mut agent_out = child.stdout.take().ok_or("no stdout")?;

    let (tx, rx) = mpsc::channel();
    let agent_tx = tx.clone();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match agent_out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if agent_tx.send(Input::Agent(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = agent_tx.send(Input::AgentClosed);
    });
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(Input::User(line)).is_err() {
                break;
            }
        }
    });

    let mut stdout = std::io::stdout().lock();
    let mut write = |bytes: &[u8]| agent_in.write_all(bytes).and_then(|()| agent_in.flush());
    write(&driver.handshake()).map_err(|e| e.to_string())?;
    for input in rx {
        match input {
            Input::Agent(bytes) => {
                driver.set_now(now_ms());
                for event in driver.on_output(&bytes) {
                    let line = serde_json::to_string(&event).unwrap_or_default();
                    writeln!(stdout, "{line}").map_err(|e| e.to_string())?;
                }
                write(&driver.take_outgoing()).map_err(|e| e.to_string())?;
            }
            Input::AgentClosed => break,
            Input::User(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let cmd = serde_json::from_str::<Command>(line).unwrap_or_else(|_| Command::Prompt {
                    text: line.to_owned(),
                });
                match driver.command(cmd) {
                    Ok(bytes) => write(&bytes).map_err(|e| e.to_string())?,
                    Err(e) => eprintln!("acc: {e}"),
                }
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let line = serde_json::to_string(&acc_core::Event::Exited { code: status.code() }).unwrap_or_default();
    let _ = writeln!(stdout, "{line}");
    Ok(ExitCode::SUCCESS)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

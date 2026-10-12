//! Shared loop for driving a harness that speaks its protocol over plain
//! stdio: spawn the child, send the handshake, then pump bytes between it
//! and the user. Events go to stdout as NDJSON; each stdin line is a JSON
//! `Command` (a `prompt` may carry base64 `attachments`), or plain text sent
//! as a prompt. `acc acp` and `acc run` both use this for spikes on real
//! hosts, e.g. `ssh host acc run claude ~/p`.

use std::io::{BufRead as _, Read as _, Write as _};
use std::path::Path;
use std::process::{Command as Proc, ExitCode, Stdio};
use std::sync::mpsc;

use acc_core::Event;
use acc_core::command::Command;
use acc_core::driver::Driver;

enum Input {
    Agent(Vec<u8>),
    AgentClosed,
    User(String),
}

/// Spawns `driver`'s harness in `cwd`, performs the handshake and drives it
/// until the child exits. `resume` is the launch-time resume argument some
/// drivers take (Claude's `--resume`, pi's `--session`); others ignore it,
/// taking resume in-protocol instead. `program` overrides the harness's own
/// binary, for a wrapper or sandboxed copy.
pub fn drive(
    mut driver: Box<dyn Driver>,
    cwd: &Path,
    resume: Option<&str>,
    program: Option<&str>,
) -> Result<ExitCode, String> {
    let spec = driver.launch(resume);
    let mut child = Proc::new(program.unwrap_or(&spec.program))
        .args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
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
                for mut event in driver.on_output(&bytes) {
                    // The driver has no clock; stamp state on receipt.
                    if let Event::StateChanged(state) = &mut event {
                        state.observed_at_ms = now_ms();
                    }
                    let line = serde_json::to_string(&event).unwrap_or_default();
                    writeln!(stdout, "{line}").map_err(|e| e.to_string())?;
                }
                write(&driver.poll_transmit()).map_err(|e| e.to_string())?;
            }
            Input::AgentClosed => break,
            Input::User(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let cmd = serde_json::from_str::<Command>(line).unwrap_or_else(|_| Command::prompt(line));
                match driver.command(cmd) {
                    Ok(bytes) => write(&bytes).map_err(|e| e.to_string())?,
                    Err(e) => eprintln!("acc: {e}"),
                }
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let line = serde_json::to_string(&Event::Exited { code: status.code() }).unwrap_or_default();
    let _ = writeln!(stdout, "{line}");
    Ok(ExitCode::SUCCESS)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

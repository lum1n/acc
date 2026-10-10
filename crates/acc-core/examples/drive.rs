//! Drives a real Claude Code or Codex process with an ACC driver, for spikes.
//!
//! ```text
//! cargo run -p acc-core --example drive -- claude [session-id]
//! cargo run -p acc-core --example drive -- codex [thread-id]
//! ```
//!
//! Each stdin line is a prompt, or a command:
//! `:interrupt`, `:refresh`, `:compact`, `:new`, `:fork`,
//! `:answer <request-id> <option-id>`, `:set <model|effort|mode|permission> <value>`,
//! `:invoke <name> [args]`, `:commands`, `:options`.
//! Events print as JSON lines.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::process::{Command as Process, Stdio};
use std::sync::{Arc, Mutex};

use acc_core::claude::{ClaudeConfig, ClaudeDriver};
use acc_core::codex::{CodexConfig, CodexDriver};
use acc_core::command::{Command, OptionId};
use acc_core::driver::Driver;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let resume = args.get(1).cloned();
    let mut driver: Box<dyn Driver> = match args.first().map(String::as_str) {
        Some("claude") => Box::new(ClaudeDriver::new(ClaudeConfig::default())),
        Some("codex") => Box::new(CodexDriver::new(CodexConfig {
            resume: resume.clone(),
            ..CodexConfig::default()
        })),
        _ => {
            eprintln!("usage: drive <claude|codex> [resume-id]");
            std::process::exit(2);
        }
    };
    let spec = driver.launch(resume.as_deref());
    let mut child = Process::new(&spec.program)
        .args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn harness");
    let stdin = Arc::new(Mutex::new(child.stdin.take()));
    let mut stdout = child.stdout.take().expect("stdout");

    write(&stdin, &driver.handshake());
    let driver = Arc::new(Mutex::new(driver));

    let (reader_driver, reader_stdin) = (driver.clone(), stdin.clone());
    let reader = std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut d = reader_driver.lock().unwrap();
            for event in d.on_output(&buf[..n]) {
                println!("{}", serde_json::to_string(&event).unwrap());
            }
            let reply = d.poll_transmit();
            write(&reader_stdin, &reply);
        }
    });

    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin line");
        let mut d = driver.lock().unwrap();
        let mut words = line.splitn(3, ' ');
        let cmd = match (words.next().unwrap_or(""), words.next(), words.next()) {
            (":interrupt", ..) => Command::Interrupt,
            (":refresh", ..) => Command::Refresh,
            (":compact", ..) => Command::Compact { hint: None },
            (":new", ..) => Command::New,
            (":fork", ..) => Command::Fork,
            (":answer", Some(r), Some(o)) => Command::AnswerPermission {
                request_id: r.into(),
                option_id: o.into(),
            },
            (":invoke", Some(n), a) => Command::Invoke {
                name: n.into(),
                args: a.unwrap_or("").into(),
            },
            (":set", Some(id), Some(v)) => {
                let id = match id {
                    "model" => OptionId::Model,
                    "effort" => OptionId::Effort,
                    "mode" => OptionId::Mode,
                    _ => OptionId::Permission,
                };
                Command::SetOption { id, value: v.into() }
            }
            (":commands", ..) => {
                println!("{}", serde_json::to_string(d.commands()).unwrap());
                continue;
            }
            (":options", ..) => {
                println!("{}", serde_json::to_string(d.options()).unwrap());
                continue;
            }
            _ => Command::Prompt { text: line.clone() },
        };
        match d.command(cmd) {
            Ok(bytes) => write(&stdin, &bytes),
            Err(e) => eprintln!("error: {e}"),
        }
    }
    // Closing stdin ends the harness once its turn is done.
    stdin.lock().unwrap().take();
    let _ = child.wait();
    let _ = reader.join();
}

fn write(stdin: &Mutex<Option<std::process::ChildStdin>>, bytes: &[u8]) {
    if let Some(w) = stdin.lock().unwrap().as_mut()
        && !bytes.is_empty()
    {
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }
}

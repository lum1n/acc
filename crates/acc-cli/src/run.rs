//! `acc run`: drives any harness as a local child process through its ACC
//! driver, for trying ACC by hand and for spikes on real hosts.
//!
//! Each stdin line is a prompt, a `:command` (see [`HELP`]) or a JSON
//! `Command`. Events print as readable lines, or as NDJSON with `--json`.
//! When stdin ends, ACC waits for the running turn, closes the harness's
//! stdin and exits, so `echo hi | acc run claude` is a one-shot smoke test.

use std::collections::HashMap;
use std::io::{BufRead as _, Read as _, Write as _};
use std::process::{ChildStdin, Command as Proc, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use acc_core::acp::{AcpConfig, AcpDriver, Start};
use acc_core::claude::{ClaudeConfig, ClaudeDriver};
use acc_core::codex::{CodexConfig, CodexDriver};
use acc_core::command::{Command, OptionId};
use acc_core::driver::{Driver, DriverError};
use acc_core::event::ItemKind;
use acc_core::pi::PiDriver;
use acc_core::state::RunState;
use acc_core::{Event, Harness};

pub const HELP: &str = "\
type a prompt, or one of:
  :interrupt  :compact  :new  :fork  :refresh
  :answer [request-id] <option-id>   answer a permission request (default: the latest)
  :set <model|effort|mode|permission> <value>
  :invoke <name> [args]              run a slash command from :commands
  :commands  :options                print the palette / options as JSON
  {\"type\": ...}                      any acc_core Command as JSON
end input (Ctrl-D) to finish after the current turn";

enum Input {
    Agent(Vec<u8>),
    AgentClosed,
    User(String),
    UserClosed,
}

pub fn run(harness: Harness, args: &[&str]) -> Result<ExitCode, String> {
    let mut cwd = ".".to_owned();
    let mut resume = None;
    let mut model = None;
    let mut program = None;
    let mut json = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .map(|v| (*v).to_owned())
                .ok_or(format!("{arg} needs a value"))
        };
        match *arg {
            "--resume" => resume = Some(value()?),
            "--model" => model = Some(value()?),
            "--program" => program = Some(value()?),
            "--json" => json = true,
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag:?}")),
            dir => dir.clone_into(&mut cwd),
        }
    }
    let cwd = std::fs::canonicalize(&cwd).map_err(|e| format!("{cwd}: {e}"))?;
    let cwd_str = cwd.to_string_lossy().into_owned();

    let (mut driver, spec): (Box<dyn Driver>, _) = match harness {
        Harness::Claude => {
            let d = ClaudeDriver::new(ClaudeConfig {
                model: model.take(),
                ..ClaudeConfig::default()
            });
            let spec = d.launch(resume.as_deref());
            (Box::new(d), spec)
        }
        Harness::Codex => {
            let d = CodexDriver::new(CodexConfig {
                resume: resume.clone(),
                cwd: Some(cwd_str.clone()),
                model: model.take(),
                ..CodexConfig::default()
            });
            let spec = d.launch(None);
            (Box::new(d), spec)
        }
        // pi takes its model at runtime; set below once it is up.
        Harness::Pi => {
            let d = PiDriver::new();
            let spec = d.launch(resume.as_deref());
            (Box::new(d), spec)
        }
        Harness::Cursor | Harness::Copilot | Harness::Opencode => {
            let start = match resume.clone() {
                Some(session_id) => Start::Resume { session_id },
                None => Start::New,
            };
            let mut cfg = AcpConfig::new(&cwd_str, start);
            cfg.model = model.take();
            cfg.client_tag = format!("cli{}", std::process::id());
            let d = AcpDriver::new(harness, cfg);
            let spec = d.launch(None);
            (Box::new(d), spec)
        }
    };

    let program = program.unwrap_or(spec.program.clone());
    let mut proc = Proc::new(&program);
    // Started from inside Claude Code (its Bash tool, `!`), a child Claude
    // would otherwise adopt the parent's session id.
    for var in ["CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_REMOTE_SESSION_ID"] {
        proc.env_remove(var);
    }
    let mut child = proc
        .args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "spawn {program}: {e} (is {} installed? try `acc doctor`)",
                harness.display_name()
            )
        })?;
    let mut agent_in = Some(child.stdin.take().ok_or("no stdin")?);
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
                return;
            }
        }
        let _ = tx.send(Input::UserClosed);
    });

    let mut out = Printer::new(harness, json);
    if !json {
        eprintln!(
            "acc: started {} in {cwd_str}; :help for commands",
            harness.display_name()
        );
    }
    write(&mut agent_in, &driver.handshake())?;
    if let Some(m) = model.filter(|_| harness == Harness::Pi) {
        let cmd = Command::SetOption {
            id: OptionId::Model,
            value: m,
        };
        let bytes = driver.command(cmd).map_err(|e| e.to_string())?;
        write(&mut agent_in, &bytes)?;
    }

    // A prompt is in flight until its turn completes; stdin EOF waits for it.
    let mut busy = false;
    let mut eof = false;
    // Commands typed (or piped) before the harness has a session wait here
    // and are sent once `SessionStarted` arrives.
    let mut queued: Vec<Command> = Vec::new();
    loop {
        let input = if agent_in.is_some() {
            rx.recv().map_err(|e| e.to_string())?
        } else {
            // Stdin is closed; give the harness a moment to exit on its own.
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(input) => input,
                Err(_) => {
                    let _ = child.kill();
                    break;
                }
            }
        };
        match input {
            Input::Agent(bytes) => {
                let mut started = false;
                for mut event in driver.on_output(&bytes) {
                    started |= matches!(event, Event::SessionStarted { .. });
                    // The driver has no clock; stamp state on receipt.
                    if let Event::StateChanged(state) = &mut event {
                        state.observed_at_ms = now_ms();
                    }
                    if matches!(event, Event::TurnCompleted) {
                        busy = false;
                    }
                    out.event(&event, driver.as_ref());
                }
                write(&mut agent_in, &driver.poll_transmit())?;
                if started {
                    for cmd in std::mem::take(&mut queued) {
                        send(driver.as_mut(), cmd, &mut agent_in, &mut queued)?;
                    }
                }
            }
            Input::AgentClosed => break,
            Input::User(line) => {
                if let Some(cmd) = parse_line(&line, driver.as_ref(), &out) {
                    busy |= matches!(cmd, Command::Prompt { .. } | Command::Invoke { .. });
                    if queued.is_empty() {
                        send(driver.as_mut(), cmd, &mut agent_in, &mut queued)?;
                    } else {
                        queued.push(cmd);
                    }
                }
            }
            Input::UserClosed => eof = true,
        }
        if eof && !busy {
            agent_in = None;
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    out.event(&Event::Exited { code: status.code() }, driver.as_ref());
    Ok(ExitCode::SUCCESS)
}

/// Sends a command, or queues it when the harness has no session yet.
fn send(
    driver: &mut dyn Driver,
    cmd: Command,
    agent_in: &mut Option<ChildStdin>,
    queued: &mut Vec<Command>,
) -> Result<(), String> {
    match driver.command(cmd.clone()) {
        Ok(bytes) => write(agent_in, &bytes),
        Err(DriverError::NotStarted) => {
            queued.push(cmd);
            Ok(())
        }
        Err(e) => {
            eprintln!("acc: {e}");
            Ok(())
        }
    }
}

fn write(w: &mut Option<ChildStdin>, bytes: &[u8]) -> Result<(), String> {
    match w {
        Some(w) if !bytes.is_empty() => w
            .write_all(bytes)
            .and_then(|()| w.flush())
            .map_err(|e| format!("write to harness: {e}")),
        _ => Ok(()),
    }
}

/// Turns one input line into a command, or handles it locally.
fn parse_line(line: &str, driver: &dyn Driver, out: &Printer) -> Option<Command> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if line.starts_with('{') {
        return match serde_json::from_str::<Command>(line) {
            Ok(cmd) => Some(cmd),
            Err(e) => {
                eprintln!("acc: invalid command JSON: {e}");
                None
            }
        };
    }
    let Some(rest) = line.strip_prefix(':') else {
        return Some(Command::Prompt {
            text: line.to_owned(),
        });
    };
    let mut words = rest.splitn(3, ' ');
    let cmd = match (words.next().unwrap_or(""), words.next(), words.next()) {
        ("interrupt", ..) => Command::Interrupt,
        ("refresh", ..) => Command::Refresh,
        ("compact", ..) => Command::Compact { hint: None },
        ("new", ..) => Command::New,
        ("fork", ..) => Command::Fork,
        ("answer", Some(r), Some(o)) => Command::AnswerPermission {
            request_id: r.into(),
            option_id: o.into(),
        },
        ("answer", Some(o), None) => match &out.last_request {
            Some(r) => Command::AnswerPermission {
                request_id: r.clone(),
                option_id: o.into(),
            },
            None => {
                eprintln!("acc: no permission request is pending");
                return None;
            }
        },
        ("invoke", Some(n), a) => Command::Invoke {
            name: n.into(),
            args: a.unwrap_or("").into(),
        },
        ("set", Some(id), Some(v)) => {
            let id = match id {
                "model" => OptionId::Model,
                "effort" => OptionId::Effort,
                "mode" => OptionId::Mode,
                "permission" => OptionId::Permission,
                other => {
                    eprintln!("acc: unknown option {other:?}");
                    return None;
                }
            };
            Command::SetOption { id, value: v.into() }
        }
        ("commands", ..) => {
            println!(
                "{}",
                serde_json::to_string_pretty(driver.commands()).unwrap_or_default()
            );
            return None;
        }
        ("options", ..) => {
            println!(
                "{}",
                serde_json::to_string_pretty(driver.options()).unwrap_or_default()
            );
            return None;
        }
        _ => {
            eprintln!("{HELP}");
            return None;
        }
    };
    Some(cmd)
}

/// Prints events as readable lines (or NDJSON).
struct Printer {
    harness: Harness,
    json: bool,
    kinds: HashMap<String, ItemKind>,
    /// Streaming text is on an unfinished line.
    mid_line: bool,
    last_request: Option<String>,
}

impl Printer {
    fn new(harness: Harness, json: bool) -> Self {
        Self {
            harness,
            json,
            kinds: HashMap::new(),
            mid_line: false,
            last_request: None,
        }
    }

    fn line(&mut self, text: &str) {
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
        println!("{text}");
    }

    fn event(&mut self, event: &Event, driver: &dyn Driver) {
        if let Event::PermissionRequested { request_id, .. } = event {
            self.last_request = Some(request_id.clone());
        }
        if self.json {
            println!("{}", serde_json::to_string(event).unwrap_or_default());
            return;
        }
        match event {
            Event::SessionStarted { native_id, model, .. } => {
                let model = model.as_deref().map(|m| format!(" ({m})")).unwrap_or_default();
                self.line(&format!("● session {native_id}{model}"));
                self.line(&format!(
                    "  resume later with: acc run {} --resume {native_id}",
                    crate::doctor::name(self.harness)
                ));
            }
            Event::StateChanged(s) => {
                let state = match s.state {
                    RunState::Idle => "idle".to_owned(),
                    RunState::Working { progress: Some(p) } => format!("working {p}%"),
                    RunState::Working { progress: None } => "working".to_owned(),
                    RunState::Blocked { kind } => format!("blocked ({kind:?})").to_lowercase(),
                    RunState::Done => "done".to_owned(),
                    RunState::Error => "error".to_owned(),
                    RunState::Exited => "exited".to_owned(),
                };
                let msg = s.message.as_deref().map(|m| format!(": {m}")).unwrap_or_default();
                self.line(&format!("[state {state}{msg}]"));
            }
            Event::ItemStarted { id, kind, extensions } => {
                self.kinds.insert(id.clone(), *kind);
                if self.mid_line {
                    println!();
                    self.mid_line = false;
                }
                if !matches!(
                    kind,
                    ItemKind::AgentMessage | ItemKind::UserMessage | ItemKind::Reasoning
                ) {
                    let label = format!("{kind:?}");
                    self.line(&format!("→ {label} {}", summary(extensions)));
                }
            }
            Event::ItemDelta { id, text } => {
                let kind = self.kinds.get(id).copied();
                if matches!(kind, None | Some(ItemKind::AgentMessage)) {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                    self.mid_line = !text.ends_with('\n');
                }
            }
            Event::PermissionRequested {
                request_id,
                title,
                options,
            } => {
                self.line(&format!("? permission: {title}"));
                for o in options {
                    self.line(&format!("    {:<16} {}", o.id, o.label));
                }
                self.line(&format!(
                    "  answer with :answer <option-id>   (request {request_id})"
                ));
            }
            Event::PermissionResolved { request_id } => {
                if self.last_request.as_deref() == Some(request_id) {
                    self.last_request = None;
                }
            }
            Event::Usage(u) => {
                let mut parts = Vec::new();
                if let (Some(i), Some(o)) = (u.input_tokens, u.output_tokens) {
                    parts.push(format!("tokens in {i} out {o}"));
                }
                if let Some(c) = u.cost_usd {
                    parts.push(format!("${c:.4}"));
                }
                match (u.context_used, u.context_window) {
                    (Some(used), Some(w)) => parts.push(format!("context {used}/{w}")),
                    (Some(used), None) => parts.push(format!("context {used}")),
                    _ => {}
                }
                if !parts.is_empty() {
                    self.line(&format!("[usage {}]", parts.join(", ")));
                }
            }
            Event::Quota(q) => {
                for w in &q.windows {
                    let pct = w
                        .used_percent
                        .map(|p| format!(" {p:.0}% used"))
                        .unwrap_or_default();
                    let status = w.status.as_deref().map(|s| format!(" {s}")).unwrap_or_default();
                    self.line(&format!("[quota {}{pct}{status}]", w.id));
                }
            }
            Event::Notice { message } => self.line(&format!("! {message}")),
            Event::TurnCompleted => self.line("✓ turn completed"),
            Event::Exited { code } => self.line(&format!(
                "● exited ({})",
                code.map_or("signal".into(), |c| c.to_string())
            )),
            Event::CommandsChanged => {}
            Event::OptionsChanged => {
                let current: Vec<String> = driver
                    .options()
                    .iter()
                    .filter_map(|o| {
                        let id = format!("{:?}", o.id).to_lowercase();
                        Some(format!("{id}={}", o.current.as_deref()?))
                    })
                    .collect();
                if !current.is_empty() {
                    self.line(&format!("[options {}]", current.join(" ")));
                }
            }
            Event::ItemUpdated { .. }
            | Event::ItemCompleted { .. }
            | Event::Unrecognized { .. }
            | Event::Native { .. } => {}
        }
    }
}

/// A short one-line view of a tool item's native details.
fn summary(v: &serde_json::Value) -> String {
    for key in ["title", "command", "name", "path"] {
        if let Some(s) = v.get(key).and_then(|s| s.as_str()) {
            return truncate(s);
        }
    }
    if v.is_null() {
        String::new()
    } else {
        truncate(&v.to_string())
    }
}

fn truncate(s: &str) -> String {
    let s = s.replace('\n', " ");
    match s.char_indices().nth(100) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str, last_request: Option<&str>) -> Option<Command> {
        let mut out = Printer::new(Harness::Claude, false);
        out.last_request = last_request.map(str::to_owned);
        parse_line(line, &ClaudeDriver::default(), &out)
    }

    #[test]
    fn lines_become_commands() {
        assert_eq!(
            parse("  hi there ", None),
            Some(Command::Prompt {
                text: "hi there".into()
            })
        );
        assert_eq!(parse(":interrupt", None), Some(Command::Interrupt));
        assert_eq!(
            parse(r#"{"type":"compact","hint":null}"#, None),
            Some(Command::Compact { hint: None })
        );
        assert_eq!(
            parse(":set model opus", None),
            Some(Command::SetOption {
                id: OptionId::Model,
                value: "opus".into()
            })
        );
        assert_eq!(
            parse(":invoke review src/lib.rs", None),
            Some(Command::Invoke {
                name: "review".into(),
                args: "src/lib.rs".into()
            })
        );
        assert_eq!(parse("", None), None);
        assert_eq!(parse(":set colour red", None), None);
    }

    #[test]
    fn answer_defaults_to_the_latest_request() {
        let answer = |r: &str, o: &str| {
            Some(Command::AnswerPermission {
                request_id: r.into(),
                option_id: o.into(),
            })
        };
        assert_eq!(parse(":answer allow", Some("r1")), answer("r1", "allow"));
        assert_eq!(parse(":answer r2 deny", Some("r1")), answer("r2", "deny"));
        assert_eq!(parse(":answer allow", None), None);
    }
}

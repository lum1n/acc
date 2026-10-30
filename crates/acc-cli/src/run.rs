//! `acc run`: drives any harness as a local child process through its ACC
//! driver, for trying ACC by hand and for spikes on real hosts.
//!
//! Each stdin line is a prompt, a `:command` (see [`HELP`]) or a JSON
//! `Command`. Events print as readable lines, or as NDJSON with `--json`.
//! When stdin ends, ACC waits for the running turn, closes the harness's
//! stdin and exits, so `echo hi | acc run claude` is a one-shot smoke test.
//!
//! With `--ssh`, the harness runs detached on that host instead (see
//! [`crate::remote`]); ending stdin detaches and leaves it running, and
//! `--attach` rejoins it later from any machine.

use std::collections::HashMap;
use std::io::{BufRead as _, Read as _, Write as _};
use std::process::{Child, ChildStdin, Command as Proc, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use acc_core::acp::{AcpConfig, AcpDriver, Start};
use acc_core::claude::{ClaudeConfig, ClaudeDriver};
use acc_core::codex::{CodexConfig, CodexDriver};
use acc_core::command::{Command, OptionId};
use acc_core::driver::{Driver, DriverError};
use acc_core::event::{ItemKind, PlanStatus};
use acc_core::harness::LaunchSpec;
use acc_core::item::ItemDetail;
use acc_core::pi::PiDriver;
use acc_core::sandbox::{Preset, Sandbox};
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

pub enum Input {
    Agent(Vec<u8>),
    /// The harness's output ended. Remote sessions know the exit code.
    AgentClosed(Option<Option<i32>>),
    /// A rejoined session's replay reached the present.
    CaughtUp,
    User(String),
    UserClosed,
}

pub fn run(harness: Harness, args: &[&str]) -> Result<ExitCode, String> {
    let mut cwd = ".".to_owned();
    let mut resume = None;
    let mut model = None;
    let mut program = None;
    let mut json = false;
    let mut ssh = crate::remote::Options::default();
    let mut attach = None;
    let mut sandbox = SandboxFlags::default();
    let mut it = args.iter().copied().peekable();
    while let Some(arg) = it.next() {
        if sandbox.take(arg, &mut it)? {
            continue;
        }
        let mut value = || it.next().map(str::to_owned).ok_or(format!("{arg} needs a value"));
        match arg {
            "--resume" => resume = Some(value()?),
            "--model" => model = Some(value()?),
            "--program" => program = Some(value()?),
            "--json" => json = true,
            "--ssh" => ssh.dest = value()?,
            "--identity" => ssh.identity = Some(value()?),
            "--insecure" => ssh.insecure = true,
            "--attach" => attach = Some(value()?),
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag:?}")),
            dir => dir.clone_into(&mut cwd),
        }
    }
    let remote = if ssh.dest.is_empty() {
        if attach.is_some() {
            return Err("--attach needs --ssh".into());
        }
        None
    } else {
        Some(crate::remote::Remote::connect(&ssh)?)
    };
    // A rejoined session runs where it was started.
    let rejoin = match (&remote, &attach) {
        (Some(r), Some(id)) => Some(r.find(id)?),
        _ => None,
    };
    let replay = rejoin.as_ref().map_or(0, |l| l.status.size());
    let rejoined = rejoin
        .and_then(|l| l.spec_json)
        .and_then(|j| acc_core::session::SessionSpec::from_json(&j));
    let cwd_str = match (&remote, rejoined) {
        (Some(_), Some(s)) => s.cwd,
        (Some(r), None) => r.resolve_cwd(&cwd)?,
        (None, _) => std::fs::canonicalize(&cwd)
            .map_err(|e| format!("{cwd}: {e}"))?
            .to_string_lossy()
            .into_owned(),
    };

    let (mut driver, spec): (Box<dyn Driver>, _) = match harness {
        Harness::Claude => {
            let d = ClaudeDriver::new(ClaudeConfig {
                model: model.take(),
                auto_title: resume.is_none() && attach.is_none(),
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
            let start = match (&attach, resume.clone()) {
                // Adopt the running agent from its log instead of opening a session.
                (Some(_), _) => Start::Attach,
                (None, Some(session_id)) => Start::Resume { session_id },
                (None, None) => Start::New,
            };
            let mut cfg = AcpConfig::new(&cwd_str, start);
            cfg.model = model.take();
            cfg.client_tag = format!("cli{}", std::process::id());
            let d = AcpDriver::new(harness, cfg);
            let spec = d.launch(None);
            (Box::new(d), spec)
        }
    };

    let mut spec = spec;
    if let Some(p) = program {
        spec.program = p;
    }
    let sandbox = sandbox.get();
    spec.sandbox = sandbox.as_ref().map(|s| acc_core::sandbox::rules(harness, s));
    let (tx, rx) = mpsc::channel();
    let mut link = match remote {
        Some(r) => {
            let mut record = acc_core::session::SessionSpec::new("", harness, &cwd_str, spec.clone(), 0);
            record.sandbox = sandbox;
            Link::Remote(r.start(record, attach.as_deref(), replay, tx.clone())?)
        }
        None => spawn_local(harness, &spec, &cwd_str, tx.clone())?,
    };
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
        let verb = if attach.is_some() {
            "attached to"
        } else {
            "started"
        };
        eprintln!(
            "acc: {verb} {} in {cwd_str}; :help for commands",
            harness.display_name()
        );
    }
    // A session being rejoined already had its handshake.
    if attach.is_none() {
        link.write(&driver.handshake())?;
    }
    if let Some(m) = model.filter(|_| harness == Harness::Pi) {
        let cmd = Command::SetOption {
            id: OptionId::Model,
            value: m,
        };
        let bytes = driver.command(cmd).map_err(|e| e.to_string())?;
        link.write(&bytes)?;
    }

    // A prompt is in flight until its turn completes; stdin EOF waits for it.
    // While a rejoined session replays its history, input waits for it.
    let mut busy = attach.is_some();
    let mut eof = false;
    // Commands typed (or piped) before the harness has a session wait here
    // and are sent once `SessionStarted` arrives.
    let mut queued: Vec<Command> = Vec::new();
    let mut exit = None;
    loop {
        let input = if link.is_open() {
            rx.recv().map_err(|e| e.to_string())?
        } else if let Link::Local { child, .. } = &mut link {
            // Stdin is closed; give the harness a moment to exit on its own.
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(input) => input,
                Err(_) => {
                    let _ = child.kill();
                    break;
                }
            }
        } else {
            break;
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
                link.write(&driver.poll_transmit())?;
                if started {
                    for cmd in std::mem::take(&mut queued) {
                        send(driver.as_mut(), cmd, &mut link, &mut queued)?;
                    }
                }
            }
            Input::AgentClosed(code) => {
                exit = code;
                break;
            }
            Input::User(line) => {
                if let Some(cmd) = parse_line(&line, driver.as_ref(), &out) {
                    busy |= matches!(cmd, Command::Prompt { .. } | Command::Invoke { .. });
                    if queued.is_empty() {
                        send(driver.as_mut(), cmd, &mut link, &mut queued)?;
                    } else {
                        queued.push(cmd);
                    }
                }
            }
            Input::UserClosed => eof = true,
            Input::CaughtUp => busy = false,
        }
        if eof && !busy {
            link.close();
        }
    }
    let code = match (&mut link, exit) {
        (Link::Local { child, .. }, _) => Some(child.wait().map_err(|e| e.to_string())?.code()),
        (Link::Remote(_), code) => code,
    };
    match (code, &link) {
        (Some(code), _) => out.event(&Event::Exited { code }, driver.as_ref()),
        (None, Link::Remote(r)) => eprintln!(
            "acc: detached; {} keeps running on the host. Rejoin with: acc run {} --ssh {} --attach {}",
            harness.display_name(),
            crate::doctor::name(harness),
            ssh.dest,
            r.id()
        ),
        (None, Link::Local { .. }) => {}
    }
    Ok(ExitCode::SUCCESS)
}

/// Where the harness runs.
enum Link {
    Local { child: Child, stdin: Option<ChildStdin> },
    Remote(crate::remote::Link),
}

impl Link {
    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        match self {
            Link::Local { stdin, .. } => write(stdin, bytes),
            Link::Remote(r) if !bytes.is_empty() => r.write(bytes),
            Link::Remote(_) => Ok(()),
        }
    }

    fn is_open(&self) -> bool {
        match self {
            Link::Local { stdin, .. } => stdin.is_some(),
            Link::Remote(r) => r.is_open(),
        }
    }

    /// Local: closes the harness's stdin so it exits. Remote: detaches.
    fn close(&mut self) {
        match self {
            Link::Local { stdin, .. } => *stdin = None,
            Link::Remote(r) => r.detach(),
        }
    }
}

/// Starts the harness as a local child process forwarding its output to `tx`.
fn spawn_local(
    harness: Harness,
    spec: &LaunchSpec,
    cwd: &str,
    tx: mpsc::Sender<Input>,
) -> Result<Link, String> {
    // Sandboxed, `sh` builds the confined command line and execs it, so
    // the child is the sandbox and killing it takes the harness down.
    let (program, args, env) = match &spec.sandbox {
        Some(rules) => {
            let script = format!(
                "{}case $acc_sb in \
                 none*) echo \"acc: not sandboxed, running unconfined: ${{acc_sb#none }}\" >&2 ;; \
                 *) echo \"acc: sandboxed with $acc_sb\" >&2 ;; esac\n\
                 exec \"$@\"\n",
                acc_core::sandbox::argv_script(rules, spec)
            );
            ("sh".to_owned(), vec!["-c".to_owned(), script], Vec::new())
        }
        None => (spec.program.clone(), spec.args.clone(), spec.env.clone()),
    };
    let mut proc = Proc::new(&program);
    // Started from inside Claude Code (its Bash tool, `!`), a child Claude
    // would otherwise adopt the parent's session id.
    for var in ["CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_REMOTE_SESSION_ID"] {
        proc.env_remove(var);
    }
    let mut child = proc
        .args(&args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "spawn {program}: {e} (is {} installed? try `acc doctor`)",
                harness.display_name()
            )
        })?;
    let stdin = Some(child.stdin.take().ok_or("no stdin")?);
    let mut agent_out = child.stdout.take().ok_or("no stdout")?;
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match agent_out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(Input::Agent(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tx.send(Input::AgentClosed(None));
    });
    Ok(Link::Local { child, stdin })
}

/// `--sandbox [workspace|read-only]`, `--sandbox-write PATH`,
/// `--sandbox-hide PATH` and `--sandbox-show-credentials`; any of them
/// turns the sandbox on.
#[derive(Default)]
pub struct SandboxFlags {
    on: bool,
    sandbox: Sandbox,
}

impl SandboxFlags {
    /// Consumes `arg` (and its value from `rest`) if it is a sandbox flag.
    pub fn take<'a>(
        &mut self,
        arg: &str,
        rest: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
    ) -> Result<bool, String> {
        let mut value = || rest.next().ok_or(format!("{arg} needs a value"));
        match arg {
            "--sandbox" => {
                // The preset is optional, so only these words are taken.
                if let Some(&word @ ("workspace" | "read-only")) = rest.peek() {
                    self.sandbox.preset = if word == "workspace" {
                        Preset::Workspace
                    } else {
                        Preset::ReadOnly
                    };
                    rest.next();
                }
            }
            "--sandbox-write" => self.sandbox.write.push(value()?.to_owned()),
            "--sandbox-hide" => self.sandbox.hide.push(value()?.to_owned()),
            "--sandbox-show-credentials" => self.sandbox.show_credentials = true,
            _ => return Ok(false),
        }
        self.on = true;
        Ok(true)
    }

    /// The sandbox asked for, if any.
    pub fn get(self) -> Option<Sandbox> {
        self.on.then_some(self.sandbox)
    }
}

/// Sends a command, or queues it when the harness has no session yet.
fn send(
    driver: &mut dyn Driver,
    cmd: Command,
    link: &mut Link,
    queued: &mut Vec<Command>,
) -> Result<(), String> {
    match driver.command(cmd.clone()) {
        Ok(bytes) => link.write(&bytes),
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
        return Some(Command::prompt(line));
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
                let label = |m: &str| {
                    let def = driver.options().iter().find(|o| o.id == OptionId::Model);
                    def.map_or(m, |d| d.label(m)).to_owned()
                };
                let model = model
                    .as_deref()
                    .map(|m| format!(" ({})", label(m)))
                    .unwrap_or_default();
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
                    RunState::Waiting => "waiting for a background agent".to_owned(),
                    RunState::Error => "error".to_owned(),
                    RunState::Exited => "exited".to_owned(),
                };
                let msg = s.message.as_deref().map(|m| format!(": {m}")).unwrap_or_default();
                self.line(&format!("[state {state}{msg}]"));
            }
            Event::ItemStarted { id, kind, detail, .. } => {
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
                    self.line(&format!("→ {label} {}", summary(detail)));
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
                ..
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
            Event::TitleChanged { title } => self.line(&format!("[title {title}]")),
            Event::Panel(panel) => {
                self.line(&format!("▤ {}", panel.title));
                for section in &panel.sections {
                    if let Some(title) = &section.title {
                        self.line(&format!("  {title}"));
                    }
                    for row in &section.rows {
                        let value = row.value.as_deref().map(|v| format!(": {v}")).unwrap_or_default();
                        self.line(&format!("    {}{value}", row.label));
                    }
                }
            }
            Event::Plan(plan) => {
                self.line("☰ plan");
                for e in &plan.entries {
                    let mark = match e.status {
                        PlanStatus::Pending => ' ',
                        PlanStatus::InProgress => '~',
                        PlanStatus::Completed => 'x',
                    };
                    self.line(&format!("  [{mark}] {}", e.content));
                }
            }
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
                        Some(format!("{id}={}", o.current_label()?))
                    })
                    .collect();
                if !current.is_empty() {
                    self.line(&format!("[options {}]", current.join(" ")));
                }
            }
            Event::ItemUpdated { .. }
            | Event::ItemCompleted { .. }
            | Event::TurnDiff { .. }
            | Event::Unrecognized { .. }
            | Event::Native { .. } => {}
        }
    }
}

/// A short one-line view of a tool item's native details.
fn summary(detail: &ItemDetail) -> String {
    let s = match detail {
        ItemDetail::Tool { tool, title, .. } if title.is_empty() => tool.clone(),
        ItemDetail::Tool { tool, title, .. } => format!("{tool}: {title}"),
        ItemDetail::Command { command, .. } => command.clone(),
        ItemDetail::FileChange { files, .. } => files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        ItemDetail::Subagent { description, .. } => description.clone(),
        ItemDetail::Text => String::new(),
    };
    truncate(&s)
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
        assert_eq!(parse("  hi there ", None), Some(Command::prompt("hi there")));
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

//! Controlling harnesses inside terminal multiplexers.
//!
//! Each multiplexer leaves identifying variables in the environment of the
//! processes it runs (see `research/multiplexer-control.md`). ACC reads the
//! harness process's environment over SSH and maps it to a pane it can drive
//! with the multiplexer's own CLI. Nothing is installed on the host: every
//! operation is a shell command ([`Exec`]) run over an SSH exec channel with
//! the host's own `tmux`, `zellij`, `screen` or `rex` binary.
//!
//! Like the rest of the core this module is sans-IO. A [`Backend`] builds the
//! commands and parses what they print; the transport runs them. Long-running
//! observation ([`Backend::watch`]) yields a byte stream decoded by the
//! matching parser:
//!
//! | Backend | Watch stream | Raw bytes (OSC 7501)? |
//! |---|---|---|
//! | [`tmux`] | control mode (`tmux -C`), [`tmux::ControlParser`] | yes |
//! | [`zellij`] | `zellij subscribe -f json`, [`zellij::parse_update`] | no, rendered lines |
//! | [`screen`] | window log tailed with `tail -F` | yes |
//! | [`rex`] | `rex events --json`, [`rex::parse_event`] | no, but native program status |
//!
//! Raw byte streams go through a [`PaneObserver`] for OSC 7501 state. Harness
//! keystrokes for co-driving a TUI live in [`tui`].

pub mod rex;
pub mod screen;
pub mod tmux;
pub mod tui;
pub mod zellij;

use serde::{Deserialize, Serialize};

use crate::osc7501::{Osc7501, Report, Scanner};
use crate::state::{SessionState, StateSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Multiplexer {
    Tmux,
    Zellij,
    Screen,
    Rex,
}

/// Where a harness runs inside a multiplexer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRef {
    pub multiplexer: Multiplexer,
    /// tmux socket path, zellij/screen session name, or Rex server.
    pub server: String,
    /// Session name where the multiplexer has one.
    pub session: Option<String>,
    /// tmux `%N`, zellij `terminal_N`, screen window number, Rex block id.
    pub pane: String,
}

impl PaneRef {
    /// The server/session this pane belongs to.
    pub fn target(&self) -> Target {
        Target {
            multiplexer: self.multiplexer,
            server: self.server.clone(),
            session: self.session.clone(),
        }
    }
}

/// A multiplexer server, optionally narrowed to one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub multiplexer: Multiplexer,
    /// tmux socket path (empty for the default socket), zellij/screen session
    /// name, or Rex server address (empty for the local server).
    pub server: String,
    pub session: Option<String>,
}

/// One pane as listed by the multiplexer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane: PaneRef,
    /// The process the pane started (tmux only; zellij and Rex don't report it).
    pub pid: Option<u32>,
    /// Pane TTY (tmux only), for matching a harness by `ps -o tty`.
    pub tty: Option<String>,
    /// Name of the foreground process, where reported.
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub active: bool,
    /// The pane's program exited but the pane is kept (tmux dead, zellij held).
    pub exited: bool,
}

/// A command to run on the host (for example over an SSH exec channel).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exec {
    /// A POSIX `sh` script. Every dynamic value in it is quoted.
    pub script: String,
    /// Bytes to write to the script's stdin, then close it. Long-running
    /// watch commands document whether stdin must instead stay open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stdin: Vec<u8>,
}

impl Exec {
    pub fn new(script: String) -> Self {
        Self {
            script,
            stdin: Vec::new(),
        }
    }

    pub fn with_stdin(script: String, stdin: impl Into<Vec<u8>>) -> Self {
        Self {
            script,
            stdin: stdin.into(),
        }
    }
}

/// A key press sent to a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "key", content = "char", rename_all = "snake_case")]
pub enum Key {
    Enter,
    Escape,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    /// Ctrl plus an ASCII letter, e.g. `Ctrl('c')`.
    Ctrl(char),
    /// One printable character, e.g. a menu digit.
    Char(char),
}

/// Input for a pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Input {
    /// Literal text, typed. Should not contain newlines (use [`Input::Paste`]).
    Text(String),
    /// Text delivered as one bracketed paste, so newlines don't submit early.
    /// All six harness TUIs enable bracketed paste.
    Paste(String),
    Keys(Vec<Key>),
}

/// A program to start in a pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Program {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Program {
    /// `env K=V ... program args...` as argv, so no shell is needed in between.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::new();
        if !self.env.is_empty() {
            argv.push("env".to_owned());
            argv.extend(self.env.iter().map(|(k, v)| format!("{k}={v}")));
        }
        argv.push(self.program.clone());
        argv.extend(self.args.iter().cloned());
        argv
    }
}

impl From<&crate::harness::LaunchSpec> for Program {
    fn from(spec: &crate::harness::LaunchSpec) -> Self {
        Self {
            program: spec.program.clone(),
            args: spec.args.clone(),
            env: spec.env.clone(),
        }
    }
}

/// How to decode the bytes a [`Backend::watch`] command prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    /// tmux control mode; decode with [`tmux::ControlParser`]. Keep stdin
    /// open: closing it detaches the control client.
    TmuxControl,
    /// One JSON object per line; decode with [`zellij::parse_update`].
    ZellijSubscribe,
    /// The pane's raw output bytes; feed a [`PaneObserver`].
    RawBytes,
    /// One JSON object per line; decode with [`rex::parse_event`].
    RexEvents,
}

/// A long-running observation command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
    pub exec: Exec,
    pub stream: StreamKind,
    /// Run after the watch ends to undo what it set up (screen logging).
    pub cleanup: Option<Exec>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MuxError {
    #[error("{0:?} can't {1}")]
    Unsupported(Multiplexer, &'static str),
    #[error("unexpected {0} output: {1}")]
    Parse(&'static str, String),
}

/// Builds host commands for one multiplexer and parses their output.
pub trait Backend: Send + Sync {
    fn multiplexer(&self) -> Multiplexer;

    /// Lists the panes of `target` (all sessions when it names none).
    fn list_panes(&self, target: &Target) -> Exec;
    fn parse_panes(&self, target: &Target, stdout: &str) -> Result<Vec<PaneInfo>, MuxError>;

    /// Prints the pane's rendered text (with scrollback when asked).
    fn capture(&self, pane: &PaneRef, scrollback: bool) -> Exec;

    /// Delivers input to the pane.
    fn send(&self, pane: &PaneRef, input: &Input) -> Exec;

    /// Starts `program` in a new background pane (window, tab or block)
    /// without stealing focus from attached clients. `name` labels it and
    /// should be unique. Prints output for [`Backend::parse_spawned`].
    fn spawn(&self, target: &Target, program: &Program, cwd: &str, name: &str) -> Exec;
    fn parse_spawned(&self, target: &Target, name: &str, stdout: &str) -> Result<PaneRef, MuxError>;

    /// Closes the pane and ends its program.
    fn kill(&self, pane: &PaneRef) -> Result<Exec, MuxError>;

    /// Streams the pane's output until the command is stopped.
    fn watch(&self, pane: &PaneRef) -> Watch;
}

/// The backend for a multiplexer.
pub fn backend(m: Multiplexer) -> &'static dyn Backend {
    match m {
        Multiplexer::Tmux => &tmux::Tmux,
        Multiplexer::Zellij => &zellij::Zellij,
        Multiplexer::Screen => &screen::Screen,
        Multiplexer::Rex => &rex::Rex,
    }
}

/// Identifies the multiplexer from a process environment.
pub fn locate<'a>(env: impl IntoIterator<Item = (&'a str, &'a str)>) -> Option<PaneRef> {
    let mut vars = std::collections::HashMap::new();
    for (k, v) in env {
        vars.insert(k, v);
    }
    let get = |k: &str| vars.get(k).map(|v| (*v).to_owned());

    if let (Some(tmux), Some(pane)) = (get("TMUX"), get("TMUX_PANE")) {
        // TMUX=<socket>,<server pid>,<session index>
        let socket = tmux.split(',').next().unwrap_or_default().to_owned();
        return Some(PaneRef {
            multiplexer: Multiplexer::Tmux,
            server: socket,
            session: None,
            pane,
        });
    }
    if let (Some(session), Some(pane)) = (get("ZELLIJ_SESSION_NAME"), get("ZELLIJ_PANE_ID")) {
        return Some(PaneRef {
            multiplexer: Multiplexer::Zellij,
            server: session.clone(),
            session: Some(session),
            pane: format!("terminal_{pane}"),
        });
    }
    if let (Some(sty), Some(window)) = (get("STY"), get("WINDOW")) {
        return Some(PaneRef {
            multiplexer: Multiplexer::Screen,
            server: sty.clone(),
            session: Some(sty),
            pane: window,
        });
    }
    if let (Some(server), Some(block)) = (get("REX_SERVER"), get("REX_BLOCK")) {
        return Some(PaneRef {
            multiplexer: Multiplexer::Rex,
            server,
            session: get("REX_SESSION"),
            pane: block,
        });
    }
    None
}

/// Parses a NUL-separated environment block such as `/proc/<pid>/environ`.
pub fn parse_environ(block: &str) -> Vec<(&str, &str)> {
    block.split('\0').filter_map(|kv| kv.split_once('=')).collect()
}

/// Host script printing the environment of process `pid`, NUL-separated.
/// Linux reads `/proc`; macOS falls back to `ps eww`, which separates
/// variables with spaces (values containing spaces are then ambiguous, but
/// the multiplexer variables ACC looks for never contain them).
pub fn environ_script(pid: u32) -> Exec {
    Exec::new(format!(
        "if [ -r /proc/{pid}/environ ]; then cat /proc/{pid}/environ; \
         else ps eww -o command= -p {pid} | tr ' ' '\\000'; fi\n"
    ))
}

/// Finds the listed pane a harness runs in, from the [`locate`] result of its
/// environment, falling back to its TTY. Pane ids from the environment are
/// fixed at launch, so they are cross-checked against the live listing.
pub fn match_pane<'a>(
    panes: &'a [PaneInfo],
    located: Option<&PaneRef>,
    tty: Option<&str>,
) -> Option<&'a PaneInfo> {
    if let Some(loc) = located
        && let Some(p) = panes.iter().find(|p| p.pane.pane == loc.pane)
    {
        return Some(p);
    }
    let tty = tty?.trim_start_matches("/dev/");
    panes
        .iter()
        .find(|p| p.tty.as_deref().map(|t| t.trim_start_matches("/dev/")) == Some(tty))
}

/// Turns a raw pane byte stream into OSC 7501 state observations.
///
/// Only reports a program sends without the probe arrive through tmux and
/// screen (pi with `PI_PROGRAM_STATUS=1`): multiplexers don't answer the
/// probe, and ACC can't answer it in time (tmux's DA1 reply always wins).
#[derive(Debug, Default)]
pub struct PaneObserver {
    scanner: Scanner,
}

/// A state observation for one OSC 7501 record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusUpdate {
    /// Child record id (`agents/3`); `None` for the program's own record.
    pub id: Option<String>,
    pub app: Option<String>,
    /// `None` when the record was cleared.
    pub state: Option<SessionState>,
}

impl PaneObserver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds raw pane bytes. `now_ms` stamps the observations.
    pub fn feed(&mut self, bytes: &[u8], now_ms: u64) -> Vec<StatusUpdate> {
        self.scanner
            .feed(bytes)
            .into_iter()
            .filter_map(|m| match m {
                Osc7501::Report(r) => Some(status_update(r, now_ms)),
                Osc7501::Probe => None,
            })
            .collect()
    }
}

fn status_update(r: Report, now_ms: u64) -> StatusUpdate {
    StatusUpdate {
        id: r.id,
        app: r.app,
        state: r.state.map(|state| SessionState {
            state,
            message: r.message,
            source: StateSource::Osc7501,
            observed_at_ms: now_ms,
        }),
    }
}

/// `prefix` plus `value` reduced to `[A-Za-z0-9_-]`, for file and buffer names.
fn safe_name(prefix: &str, value: &str) -> String {
    let mut s = String::from(prefix);
    s.extend(value.chars().map(|c| {
        if c.is_ascii_alphanumeric() || c == '-' {
            c
        } else {
            '_'
        }
    }));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_from_environ() {
        let env = "HOME=/h\0TMUX=/tmp/tmux-501/default,1234,0\0TMUX_PANE=%3\0";
        let p = locate(parse_environ(env)).expect("tmux pane");
        assert_eq!(p.multiplexer, Multiplexer::Tmux);
        assert_eq!(p.server, "/tmp/tmux-501/default");
        assert_eq!(p.pane, "%3");
    }

    #[test]
    fn zellij_screen_rex() {
        let z = locate([
            ("ZELLIJ", "0"),
            ("ZELLIJ_SESSION_NAME", "dev"),
            ("ZELLIJ_PANE_ID", "2"),
        ]);
        assert_eq!(z.map(|p| p.pane), Some("terminal_2".into()));
        let s = locate([("STY", "123.work"), ("WINDOW", "1")]);
        assert_eq!(s.map(|p| p.multiplexer), Some(Multiplexer::Screen));
        let r = locate([("REX_SERVER", "local"), ("REX_SESSION", "a"), ("REX_BLOCK", "b7")]);
        assert_eq!(r.map(|p| p.pane), Some("b7".into()));
    }

    #[test]
    fn plain_terminal_is_none() {
        assert_eq!(locate([("TERM", "xterm-ghostty")]), None);
    }

    fn info(pane: &str, tty: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane: PaneRef {
                multiplexer: Multiplexer::Tmux,
                server: String::new(),
                session: None,
                pane: pane.into(),
            },
            pid: None,
            tty: tty.map(str::to_owned),
            command: None,
            cwd: None,
            title: None,
            active: false,
            exited: false,
        }
    }

    #[test]
    fn match_pane_by_env_then_tty() {
        let panes = [info("%1", Some("/dev/pts/3")), info("%2", Some("/dev/pts/4"))];
        let loc = locate([("TMUX", "/s,1,0"), ("TMUX_PANE", "%2")]);
        assert_eq!(
            match_pane(&panes, loc.as_ref(), None).map(|p| p.pane.pane.as_str()),
            Some("%2")
        );
        // A stale pane id falls back to the TTY.
        let stale = locate([("TMUX", "/s,1,0"), ("TMUX_PANE", "%9")]);
        assert_eq!(
            match_pane(&panes, stale.as_ref(), Some("pts/3")).map(|p| p.pane.pane.as_str()),
            Some("%1")
        );
        assert!(match_pane(&panes, None, None).is_none());
    }

    #[test]
    fn observer_reports_state_and_ignores_probe() {
        let mut o = PaneObserver::new();
        let ups = o.feed(b"x\x1b]7501;?\x07\x1b]7501;state=working:app=pi\x1b\\", 42);
        assert_eq!(ups.len(), 1);
        let s = ups[0].state.as_ref().expect("state");
        assert_eq!(s.source, StateSource::Osc7501);
        assert_eq!(s.observed_at_ms, 42);
        assert_eq!(ups[0].app.as_deref(), Some("pi"));
    }

    #[test]
    fn program_argv_prefixes_env() {
        let spec = crate::harness::Harness::Pi.launch(None);
        let argv = Program::from(&spec).argv();
        assert_eq!(argv[..3], ["env", "PI_PROGRAM_STATUS=1", "pi"]);
    }

    #[test]
    fn every_backend_is_reachable() {
        for m in [
            Multiplexer::Tmux,
            Multiplexer::Zellij,
            Multiplexer::Screen,
            Multiplexer::Rex,
        ] {
            assert_eq!(backend(m).multiplexer(), m);
        }
    }
}

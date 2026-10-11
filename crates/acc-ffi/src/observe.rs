//! Foreign agents (TUIs ACC didn't start): discovery, observation through
//! their transcripts, and takeover into a detached session.

use std::sync::{Arc, Mutex, MutexGuard};

use acc_core::discover::{self, ForeignAgent};
use acc_core::observe::{self, Observed};
use acc_core::takeover::{self, Takeover, TakeoverAction, TakeoverInput, TakeoverStep};

use crate::session::AccError;
use crate::types::*;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A harness process ACC didn't start.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccForeignAgent {
    /// Stable while the process lives (`tui-<program>-<pid>`).
    pub key: String,
    pub harness: AccHarness,
    pub pid: u32,
    pub args: String,
    pub cwd: Option<String>,
    pub elapsed_secs: Option<u64>,
    /// The pane it runs in (`tmux:%3`), when it runs in a multiplexer.
    pub pane: Option<String>,
    /// `session:window` of its tmux pane.
    pub pane_label: Option<String>,
    pub transcript_path: Option<String>,
    pub transcript_size: u64,
    /// The process has the transcript open (otherwise it is a guess).
    pub transcript_exact: bool,
    pub native_id: Option<String>,
    /// How it was launched; a non-empty prefix means it runs in a wrapper
    /// such as a `bwrap` sandbox, which takeover and hand-back keep.
    pub launcher: Option<AccLauncher>,
    /// The full record, to pass back to `AccObserved` and `AccTakeover`.
    pub json: String,
}

impl From<&ForeignAgent> for AccForeignAgent {
    fn from(a: &ForeignAgent) -> Self {
        Self {
            key: a.key(),
            harness: a.harness.into(),
            pid: a.pid,
            args: a.args.clone(),
            cwd: a.cwd.clone(),
            elapsed_secs: a.elapsed_secs,
            pane: a.pane.as_ref().map(takeover::pane_label),
            pane_label: a.pane_label.clone(),
            transcript_path: a.transcript.as_ref().map(|t| t.path.clone()),
            transcript_size: a.transcript.as_ref().map_or(0, |t| t.size),
            transcript_exact: a.transcript.as_ref().is_some_and(|t| t.exact),
            native_id: a.native_id.clone(),
            launcher: a.launcher.clone().map(Into::into),
            json: serde_json::to_string(a).unwrap_or_default(),
        }
    }
}

fn agent_from(json: &str) -> Result<ForeignAgent, AccError> {
    serde_json::from_str(json).map_err(|e| AccError::InvalidSession {
        message: format!("unreadable agent: {e}"),
    })
}

/// Lists harness TUIs running on the host.
#[uniffi::export]
pub fn discover_exec() -> AccExec {
    AccExec::script(discover::discover_script())
}

#[uniffi::export]
pub fn parse_discover(stdout: String) -> Vec<AccForeignAgent> {
    discover::parse_discover(&stdout).iter().map(Into::into).collect()
}

/// Why an agent can't be taken over, or `None` when it can.
#[uniffi::export]
pub fn takeover_blocker(agent_json: String, native_id: Option<String>) -> Option<String> {
    let agent = agent_from(&agent_json).ok()?;
    takeover::takeover_blocker(&agent, native_id.as_deref())
}

/// Types the resume command into the pane a session was taken from.
/// Stop the headless session first.
#[uniffi::export]
pub fn hand_back_exec(spec_json: String, native_id: String, pane: String) -> Option<AccExec> {
    let spec = acc_core::session::SessionSpec::from_json(&spec_json)?;
    takeover::hand_back_exec(spec.harness, &native_id, &pane, &spec.cwd, spec.launcher.as_ref())
        .map(Into::into)
}

/// A foreign agent followed through its transcript.
#[derive(Debug, uniffi::Object)]
pub struct AccObserved {
    inner: Mutex<Observed>,
}

#[uniffi::export]
impl AccObserved {
    #[uniffi::constructor]
    pub fn new(agent_json: String) -> Result<Arc<Self>, AccError> {
        Ok(Arc::new(Self {
            inner: Mutex::new(Observed::new(agent_from(&agent_json)?)),
        }))
    }

    pub fn agent(&self) -> AccForeignAgent {
        lock(&self.inner).agent().into()
    }

    /// The transcript to follow (with a fan-in), from what was read so far.
    pub fn feed_entry(&self) -> Option<AccFeedEntry> {
        lock(&self.inner).feed_entry().map(|e| AccFeedEntry {
            key: e.key,
            path: match e.path {
                acc_core::detached::FeedPath::File(path) => AccFeedPath::File { path },
                acc_core::detached::FeedPath::SessionLog(id) => AccFeedPath::SessionLog { id },
                acc_core::detached::FeedPath::SessionExit(id) => AccFeedPath::SessionExit { id },
            },
            offset: e.offset,
        })
    }

    pub fn feed(&self, bytes: Vec<u8>, now_ms: u64) -> Vec<AccUpdate> {
        lock(&self.inner)
            .feed(&bytes, now_ms)
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// Call every few seconds with the result of `alive_exec` and, when
    /// `screen_exec` returned one, that capture's output.
    pub fn tick(&self, now_ms: u64, process_alive: bool, screen: Option<String>) -> Vec<AccUpdate> {
        lock(&self.inner)
            .tick(now_ms, process_alive, screen.as_deref())
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// A capture of the TUI's pane to pass to the next `tick`, when only the
    /// screen can tell a slow tool from an approval dialog.
    pub fn screen_exec(&self, now_ms: u64) -> Option<AccExec> {
        lock(&self.inner).screen_exec(now_ms).map(Into::into)
    }

    /// Prints `alive` or `gone`.
    pub fn alive_exec(&self) -> AccExec {
        let pid = lock(&self.inner).agent().pid;
        AccExec::script(observe::alive_script(pid))
    }

    pub fn is_readable(&self) -> bool {
        lock(&self.inner).is_readable()
    }

    pub fn is_replaying(&self) -> bool {
        lock(&self.inner).is_replaying()
    }

    pub fn is_alive(&self) -> bool {
        lock(&self.inner).is_alive()
    }

    pub fn at_safe_point(&self) -> bool {
        lock(&self.inner).at_safe_point()
    }

    pub fn state(&self) -> Option<AccSessionState> {
        lock(&self.inner).state().cloned().map(Into::into)
    }

    pub fn items(&self) -> Vec<AccItem> {
        let o = lock(&self.inner);
        let h = o.agent().harness;
        o.transcript().items().iter().map(|i| item(i, h)).collect()
    }

    pub fn item(&self, id: String) -> Option<AccItem> {
        let o = lock(&self.inner);
        let h = o.agent().harness;
        o.transcript().get(&id).map(|i| item(i, h))
    }

    pub fn usage(&self) -> AccUsage {
        lock(&self.inner).usage().clone().into()
    }

    pub fn quota(&self) -> Option<AccQuota> {
        lock(&self.inner).quota().cloned().map(Into::into)
    }

    pub fn title(&self) -> Option<String> {
        lock(&self.inner).title().map(str::to_owned)
    }

    pub fn native_id(&self) -> Option<String> {
        lock(&self.inner).native_id().map(str::to_owned)
    }

    /// Keystrokes for `command` into the TUI's pane (prompts, interrupt,
    /// slash commands). Approvals need a takeover.
    pub fn pane_command(&self, command: AccCommand) -> Result<Vec<AccExec>, AccError> {
        lock(&self.inner)
            .pane_command(&command.into())
            .map(|execs| execs.into_iter().map(Into::into).collect())
            .map_err(|e| AccError::Unsupported {
                message: e.to_string(),
            })
    }
}

/// Where a takeover is, for progress displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum AccTakeoverStep {
    WaitingForSafePoint,
    Interrupting,
    ClosingTui,
    WaitingForExit,
    Terminating,
}

impl From<TakeoverStep> for AccTakeoverStep {
    fn from(s: TakeoverStep) -> Self {
        match s {
            TakeoverStep::WaitingForSafePoint => Self::WaitingForSafePoint,
            TakeoverStep::Interrupting => Self::Interrupting,
            TakeoverStep::ClosingTui => Self::ClosingTui,
            TakeoverStep::WaitingForExit => Self::WaitingForExit,
            TakeoverStep::Terminating => Self::Terminating,
        }
    }
}

/// What a takeover's client observed since the last step.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccTakeoverInput {
    Tick { now_ms: u64, safe: bool },
    Ran { now_ms: u64, stdout: String },
}

/// What the client should do next.
// Built once per action; boxing a launch request isn't worth the churn.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccTakeoverAction {
    Run {
        exec: AccExec,
        step: AccTakeoverStep,
    },
    Wait {
        ms: u64,
        step: AccTakeoverStep,
    },
    /// The TUI is gone: launch this as a new session.
    Launch {
        request: AccLaunchRequest,
    },
    Failed {
        reason: String,
    },
}

/// Closes a foreign TUI safely and hands its conversation to a detached
/// session. Step it with what you observe; run what it returns.
#[derive(Debug, uniffi::Object)]
pub struct AccTakeover {
    inner: Mutex<Takeover>,
}

#[uniffi::export]
impl AccTakeover {
    #[uniffi::constructor]
    pub fn new(agent_json: String, native_id: Option<String>, force: bool) -> Result<Arc<Self>, AccError> {
        let t = Takeover::new(agent_from(&agent_json)?, native_id, force)
            .map_err(|message| AccError::Unsupported { message })?;
        Ok(Arc::new(Self { inner: Mutex::new(t) }))
    }

    pub fn next(&self, input: AccTakeoverInput) -> AccTakeoverAction {
        let input = match input {
            AccTakeoverInput::Tick { now_ms, safe } => TakeoverInput::Tick { now_ms, safe },
            AccTakeoverInput::Ran { now_ms, stdout } => TakeoverInput::Ran { now_ms, stdout },
        };
        match lock(&self.inner).next(input) {
            TakeoverAction::Run { exec, step } => AccTakeoverAction::Run {
                exec: exec.into(),
                step: step.into(),
            },
            TakeoverAction::Wait { ms, step } => AccTakeoverAction::Wait {
                ms,
                step: step.into(),
            },
            TakeoverAction::Launch { request } => AccTakeoverAction::Launch {
                request: request.into(),
            },
            TakeoverAction::Failed { reason } => AccTakeoverAction::Failed { reason },
        }
    }
}

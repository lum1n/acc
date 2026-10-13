//! UniFFI surface of acc-core for Swift (iOS/macOS) and Kotlin (Android/JVM).
//!
//! Records mirror core types ([`types`]); objects wrap stateful sessions and
//! parsers behind a `Mutex` so they are safe to share across threads
//! ([`session`]). The surface is a byte pump with no callbacks: the app runs
//! the scripts it is handed over its own SSH channels and feeds back their
//! output. Panics are caught by UniFFI and surfaced as errors instead of
//! crashing the app.

pub mod observe;
pub mod session;
pub mod types;

pub use observe::*;
pub use session::*;
pub use types::*;

use std::sync::Mutex;

use acc_core::Harness;
use acc_core::osc7501::{self, Osc7501};
use acc_core::state::{BlockKind, RunState};

uniffi::setup_scaffolding!();

/// Version of the core library.
#[uniffi::export]
pub fn acc_version() -> String {
    acc_core::VERSION.to_owned()
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct HarnessInfo {
    pub id: String,
    pub program: String,
    pub display_name: String,
    pub native_acp: bool,
}

/// The harnesses ACC supports.
#[uniffi::export]
pub fn harnesses() -> Vec<HarnessInfo> {
    Harness::ALL
        .iter()
        .map(|h| HarnessInfo {
            id: h.id().to_owned(),
            program: h.program().to_owned(),
            display_name: h.display_name().to_owned(),
            native_acp: h.native_acp(),
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ProgramState {
    Idle,
    Working,
    BlockedPermission,
    BlockedQuestion,
    BlockedAuth,
    Done,
    Waiting,
    Error,
    Exited,
    Clear,
}

/// One OSC 7501 message seen in terminal output.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ProgramStatus {
    /// True for a probe (`OSC 7501 ; ?`); the terminal should reply with
    /// [`osc7501_probe_reply`] before its DA1 response.
    pub is_probe: bool,
    pub state: Option<ProgramState>,
    pub progress: Option<u8>,
    pub id: Option<String>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub message: Option<String>,
}

/// Bytes a terminal writes back to answer an OSC 7501 probe.
#[uniffi::export]
pub fn osc7501_probe_reply() -> Vec<u8> {
    osc7501::PROBE_REPLY.to_vec()
}

/// Incremental OSC 7501 extractor for a terminal byte stream.
#[derive(Debug, uniffi::Object)]
pub struct Osc7501Scanner {
    inner: Mutex<osc7501::Scanner>,
}

#[uniffi::export]
impl Osc7501Scanner {
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(osc7501::Scanner::new()),
        }
    }

    /// Feed raw terminal output; returns any status messages it completed.
    pub fn feed(&self, bytes: Vec<u8>) -> Vec<ProgramStatus> {
        let mut scanner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        scanner.feed(&bytes).into_iter().map(to_status).collect()
    }
}

impl Default for Osc7501Scanner {
    fn default() -> Self {
        Self::new()
    }
}

fn to_status(msg: Osc7501) -> ProgramStatus {
    match msg {
        Osc7501::Probe => ProgramStatus {
            is_probe: true,
            state: None,
            progress: None,
            id: None,
            app: None,
            title: None,
            message: None,
        },
        Osc7501::Report(r) => {
            let (state, progress) = match r.state {
                None => (ProgramState::Clear, None),
                Some(RunState::Idle) => (ProgramState::Idle, None),
                Some(RunState::Working { progress }) => (ProgramState::Working, progress),
                Some(RunState::Blocked {
                    kind: BlockKind::Permission,
                }) => (ProgramState::BlockedPermission, None),
                Some(RunState::Blocked {
                    kind: BlockKind::Question,
                }) => (ProgramState::BlockedQuestion, None),
                Some(RunState::Blocked {
                    kind: BlockKind::Auth,
                }) => (ProgramState::BlockedAuth, None),
                Some(RunState::Done) => (ProgramState::Done, None),
                Some(RunState::Waiting) => (ProgramState::Waiting, None),
                Some(RunState::Error) => (ProgramState::Error, None),
                Some(RunState::Exited) => (ProgramState::Exited, None),
            };
            ProgramStatus {
                is_probe: false,
                state: Some(state),
                progress,
                id: r.id,
                app: r.app,
                title: r.title,
                message: r.message,
            }
        }
    }
}

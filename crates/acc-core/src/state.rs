//! Canonical session state.
//!
//! The vocabulary is OSC 7501's (idle, working, done, blocked, error), which is
//! what Claude Code and pi already report. State can arrive from several
//! sources of different fidelity; a higher-fidelity source wins until it goes
//! stale. See `research/state-and-usage-monitoring.md`.

use serde::{Deserialize, Serialize};

/// What a session is doing right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunState {
    Idle,
    Working {
        progress: Option<u8>,
    },
    Blocked {
        kind: BlockKind,
    },
    Done,
    Error,
    /// The harness process is gone (ACC-only; OSC 7501 has no equivalent).
    Exited,
}

/// Why a session is blocked on the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Permission,
    Question,
    Auth,
}

/// Where a state observation came from, lowest fidelity first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateSource {
    /// Process table and timing guesses.
    Heuristic,
    /// Pattern match on a rendered terminal screen.
    ScreenMatch,
    /// Tailing the harness's transcript file.
    Transcript,
    /// OSC 7501 report from the harness's terminal output.
    Osc7501,
    /// The harness's own protocol (stream-json, app-server, ACP, RPC, HTTP).
    Native,
}

/// The latest known state of one session (or one child record within it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub state: RunState,
    pub message: Option<String>,
    pub source: StateSource,
    /// Milliseconds since the Unix epoch, supplied by the caller (the core has no clock).
    pub observed_at_ms: u64,
}

/// Merges observations from multiple sources into one current state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StateTracker {
    current: Option<SessionState>,
    /// After this long without an update, a lower-fidelity source may override.
    stale_after_ms: u64,
}

impl StateTracker {
    pub fn new(stale_after_ms: u64) -> Self {
        Self {
            current: None,
            stale_after_ms,
        }
    }

    pub fn current(&self) -> Option<&SessionState> {
        self.current.as_ref()
    }

    /// Applies an observation. Returns `true` if the visible state changed.
    pub fn observe(&mut self, obs: SessionState) -> bool {
        let accept = match &self.current {
            None => true,
            Some(cur) => {
                obs.source >= cur.source
                    || obs.observed_at_ms.saturating_sub(cur.observed_at_ms) >= self.stale_after_ms
            }
        };
        if !accept {
            return false;
        }
        let changed = self
            .current
            .as_ref()
            .is_none_or(|cur| cur.state != obs.state || cur.message != obs.message);
        self.current = Some(obs);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(state: RunState, source: StateSource, at: u64) -> SessionState {
        SessionState {
            state,
            message: None,
            source,
            observed_at_ms: at,
        }
    }

    #[test]
    fn higher_fidelity_wins() {
        let mut t = StateTracker::new(10_000);
        assert!(t.observe(obs(RunState::Working { progress: None }, StateSource::Native, 0)));
        assert!(!t.observe(obs(RunState::Idle, StateSource::Transcript, 1_000)));
        assert_eq!(
            t.current().map(|s| &s.state),
            Some(&RunState::Working { progress: None })
        );
    }

    #[test]
    fn lower_fidelity_overrides_when_stale() {
        let mut t = StateTracker::new(10_000);
        t.observe(obs(RunState::Working { progress: None }, StateSource::Native, 0));
        assert!(t.observe(obs(RunState::Exited, StateSource::Heuristic, 20_000)));
    }

    #[test]
    fn same_state_is_not_a_change() {
        let mut t = StateTracker::new(10_000);
        t.observe(obs(RunState::Idle, StateSource::Transcript, 0));
        assert!(!t.observe(obs(RunState::Idle, StateSource::Native, 5)));
        assert_eq!(t.current().map(|s| s.source), Some(StateSource::Native));
    }
}

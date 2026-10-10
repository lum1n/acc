//! The supported harnesses and their headless entry points.
//!
//! Scope: CLI/TUI harnesses only (no IDE integrations). See
//! `research/core-language-and-harness-survey.md` and `commands-and-options.md`.

use serde::{Deserialize, Serialize};

/// A supported agent harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Codex,
    Cursor,
    Copilot,
    Pi,
    Opencode,
}

/// The wire protocol a driver speaks to a harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// Claude Code `-p --input-format stream-json --output-format stream-json`.
    ClaudeStreamJson,
    /// Codex `app-server` JSON-RPC 2.0 over stdio.
    CodexAppServer,
    /// Agent Client Protocol (JSON-RPC 2.0 over stdio).
    Acp,
    /// pi `--mode rpc` NDJSON commands and events.
    PiRpc,
    /// opencode `serve` HTTP + SSE.
    OpencodeHttp,
}

/// How to start a harness headless. Paths and flags are resolved on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Extra environment, e.g. `PI_PROGRAM_STATUS=1`.
    pub env: Vec<(String, String)>,
    pub protocol: Protocol,
}

impl Harness {
    pub const ALL: [Harness; 6] = [
        Harness::Claude,
        Harness::Codex,
        Harness::Cursor,
        Harness::Copilot,
        Harness::Pi,
        Harness::Opencode,
    ];

    /// Executable name looked up on the host's `PATH` (via the login shell).
    pub fn program(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Cursor => "agent",
            Harness::Copilot => "copilot",
            Harness::Pi => "pi",
            Harness::Opencode => "opencode",
        }
    }

    /// Stable id, matching the `app=` value used in OSC 7501 where one exists.
    pub fn id(self) -> &'static str {
        match self {
            Harness::Claude => "claude-code",
            Harness::Codex => "codex",
            Harness::Cursor => "cursor-agent",
            Harness::Copilot => "copilot",
            Harness::Pi => "pi",
            Harness::Opencode => "opencode",
        }
    }

    /// Human-readable name.
    pub fn display_name(self) -> &'static str {
        match self {
            Harness::Claude => "Claude Code",
            Harness::Codex => "Codex",
            Harness::Cursor => "Cursor Agent",
            Harness::Copilot => "Copilot CLI",
            Harness::Pi => "pi",
            Harness::Opencode => "opencode",
        }
    }

    /// Whether the harness speaks ACP natively (used as the fallback driver).
    pub fn native_acp(self) -> bool {
        matches!(self, Harness::Cursor | Harness::Copilot | Harness::Opencode)
    }

    /// The richest protocol ACC drives this harness with.
    pub fn preferred_protocol(self) -> Protocol {
        match self {
            Harness::Claude => Protocol::ClaudeStreamJson,
            Harness::Codex => Protocol::CodexAppServer,
            Harness::Cursor | Harness::Copilot => Protocol::Acp,
            Harness::Pi => Protocol::PiRpc,
            Harness::Opencode => Protocol::OpencodeHttp,
        }
    }

    /// Headless launch for a new session, or resuming `resume` when given.
    ///
    /// For ACP and app-server harnesses resume happens in-protocol
    /// (`session/load`, `thread/resume`), so the command line is the same.
    pub fn launch(self, resume: Option<&str>) -> LaunchSpec {
        let s = |v: &[&str]| v.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        let (args, env) = match self {
            Harness::Claude => {
                let mut a = s(&[
                    "-p",
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    // Stream text as it is generated, echo prompts so every
                    // attached client sees them, and route permission prompts
                    // to ACC as `can_use_tool` control requests.
                    "--include-partial-messages",
                    "--replay-user-messages",
                    "--permission-prompt-tool",
                    "stdio",
                ]);
                if let Some(id) = resume {
                    a.extend(s(&["--resume", id]));
                }
                // Authoritative idle/running/requires_action frames.
                (
                    a,
                    vec![("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS".into(), "1".into())],
                )
            }
            Harness::Codex => (s(&["app-server"]), vec![]),
            Harness::Cursor => (s(&["acp"]), vec![]),
            Harness::Copilot => (s(&["--acp"]), vec![]),
            Harness::Pi => {
                let mut a = s(&["--mode", "rpc"]);
                if let Some(path) = resume {
                    a.extend(s(&["--session", path]));
                }
                (a, vec![("PI_PROGRAM_STATUS".into(), "1".into())])
            }
            Harness::Opencode => (s(&["serve", "--hostname", "127.0.0.1"]), vec![]),
        };
        LaunchSpec {
            program: self.program().to_owned(),
            args,
            env,
            protocol: self.preferred_protocol(),
        }
    }

    /// Arguments that print the harness version without side effects.
    pub fn version_args(self) -> &'static [&'static str] {
        &["--version"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<_> = Harness::ALL.iter().map(|h| h.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), Harness::ALL.len());
    }

    #[test]
    fn claude_resume_appends_flag() {
        let spec = Harness::Claude.launch(Some("abc"));
        assert_eq!(spec.args[spec.args.len() - 2..], ["--resume", "abc"]);
        assert_eq!(spec.protocol, Protocol::ClaudeStreamJson);
    }

    #[test]
    fn pi_forces_program_status() {
        let spec = Harness::Pi.launch(None);
        assert!(spec.env.contains(&("PI_PROGRAM_STATUS".into(), "1".into())));
    }
}

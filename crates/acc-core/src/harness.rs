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

/// How a TUI the user started was launched, so ACC can start the harness
/// the same way: inside the same wrapper (a `bwrap` sandbox, `env`, `nice`,
/// ...), from the same executable, with the user's own flags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launcher {
    /// The wrapper's argv up to the harness, e.g. `bwrap ... --`.
    #[serde(default)]
    pub prefix: Vec<String>,
    /// The harness executable as the wrapper ran it (inside a sandbox the
    /// path can differ from the host's).
    #[serde(default)]
    pub program: Option<String>,
    /// The user's flags to keep (resume flags removed).
    #[serde(default)]
    pub args: Vec<String>,
}

impl Launcher {
    /// Runs `spec` through this launcher: prefix, then the program, the
    /// harness arguments and the user's flags.
    pub fn wrap(&self, mut spec: LaunchSpec) -> LaunchSpec {
        if let Some(program) = &self.program {
            spec.program = program.clone();
        }
        spec.args.extend(self.args.iter().cloned());
        if let Some((first, rest)) = self.prefix.split_first() {
            let inner = std::mem::replace(&mut spec.program, first.clone());
            let mut args = rest.to_vec();
            args.push(inner);
            args.append(&mut spec.args);
            spec.args = args;
        }
        spec
    }

    /// The wrapper's program name, e.g. `bwrap`, when there is one.
    pub fn wrapper(&self) -> Option<&str> {
        self.prefix.first().map(|p| p.rsplit('/').next().unwrap_or(p))
    }
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
            Harness::Cursor | Harness::Copilot | Harness::Opencode => Protocol::Acp,
            Harness::Pi => Protocol::PiRpc,
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
                    // Subagents' text and thinking too, not only their tool
                    // calls, as frames with `parent_tool_use_id` (2.1.280+).
                    "--forward-subagent-text",
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
            Harness::Pi => {
                let mut a = s(&["--mode", "rpc"]);
                if let Some(path) = resume {
                    a.extend(s(&["--session", path]));
                }
                (a, vec![("PI_PROGRAM_STATUS".into(), "1".into())])
            }
            h @ (Harness::Cursor | Harness::Copilot | Harness::Opencode) => {
                return crate::acp::launch_spec(h, None);
            }
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

/// A harness found on a host by [`probe_script`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Availability {
    pub harness: Harness,
    /// Absolute path of the executable.
    pub path: String,
    /// First line of `--version`, when it printed one in time.
    pub version: Option<String>,
}

/// Script listing which harnesses are installed on the host, using the
/// user's login `PATH`. Parse it with [`parse_probe`].
pub fn probe_script() -> String {
    let programs = Harness::ALL.map(Harness::program).join(" ");
    format!(
        "{prelude}\n\
         if command -v timeout >/dev/null 2>&1; then t='timeout 8'; else t=''; fi\n\
         for p in {programs}; do\n\
         path=$(command -v \"$p\" 2>/dev/null) || continue\n\
         case $path in /*) ;; *) continue ;; esac\n\
         v=$($t \"$path\" --version </dev/null 2>/dev/null | head -n 1)\n\
         printf 'H\\t%s\\t%s\\t%s\\n' \"$p\" \"$path\" \"$v\"\n\
         done; true\n",
        prelude = crate::detached::login_path_prelude(),
    )
}

/// Parses the output of [`probe_script`].
pub fn parse_probe(out: &str) -> Vec<Availability> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.strip_prefix("H\t")?.splitn(3, '\t');
            let program = f.next()?;
            let harness = Harness::ALL.into_iter().find(|h| h.program() == program)?;
            let path = f.next()?.to_owned();
            let version = f
                .next()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned);
            Some(Availability {
                harness,
                path,
                version,
            })
        })
        .collect()
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
    fn launcher_wraps_program_and_keeps_flags() {
        let spec = LaunchSpec {
            program: "claude".into(),
            args: vec!["-p".into()],
            env: vec![],
            protocol: Protocol::ClaudeStreamJson,
        };
        let l = Launcher {
            prefix: vec![
                "bwrap".into(),
                "--bind".into(),
                "/a".into(),
                "/a".into(),
                "--".into(),
            ],
            program: Some("/usr/bin/claude".into()),
            args: vec!["--dangerously-skip-permissions".into()],
        };
        let w = l.wrap(spec.clone());
        assert_eq!(w.program, "bwrap");
        assert_eq!(
            w.args,
            [
                "--bind",
                "/a",
                "/a",
                "--",
                "/usr/bin/claude",
                "-p",
                "--dangerously-skip-permissions"
            ]
        );
        assert_eq!(l.wrapper(), Some("bwrap"));
        assert_eq!(Launcher::default().wrap(spec.clone()), spec);
    }

    #[test]
    fn claude_resume_appends_flag() {
        let spec = Harness::Claude.launch(Some("abc"));
        assert_eq!(spec.args[spec.args.len() - 2..], ["--resume", "abc"]);
        assert_eq!(spec.protocol, Protocol::ClaudeStreamJson);
    }

    #[test]
    fn probe_parses_found_harnesses() {
        let out = "H\tclaude\t/u/bin/claude\t2.1.296 (Claude Code)\nH\tagent\t/u/bin/agent\t\nnoise\n";
        let found = parse_probe(out);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].version.as_deref(), Some("2.1.296 (Claude Code)"));
        assert_eq!(
            (found[1].harness, found[1].version.clone()),
            (Harness::Cursor, None)
        );
        assert!(probe_script().contains("command -v"));
    }

    #[cfg(unix)]
    #[test]
    fn probe_script_runs_under_sh() {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(probe_script())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        // Whatever is installed here, every line must parse.
        assert_eq!(parse_probe(&out).len(), out.lines().count());
    }

    #[test]
    fn acp_harnesses_launch_over_acp() {
        for h in Harness::ALL.into_iter().filter(|h| h.native_acp()) {
            let spec = h.launch(None);
            assert_eq!(spec, crate::acp::launch_spec(h, None));
            assert_eq!(spec.protocol, h.preferred_protocol());
        }
        assert_eq!(Harness::Opencode.launch(None).args, ["acp"]);
    }

    #[test]
    fn pi_forces_program_status() {
        let spec = Harness::Pi.launch(None);
        assert!(spec.env.contains(&("PI_PROGRAM_STATUS".into(), "1".into())));
    }
}

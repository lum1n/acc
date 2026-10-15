//! Skills and MCP servers for every harness (`acc_core::loadout`): scripts
//! to run on the host and pure functions over their output.

use acc_core::loadout::{self, LoadoutError, Scope, mcp, skills};

use crate::AccError;
use crate::acc_types::{AccExec, AccHarness};

/// User-wide configuration or one project's checkout.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccScope {
    User,
    Project { dir: String },
}

impl From<AccScope> for Scope {
    fn from(s: AccScope) -> Self {
        match s {
            AccScope::User => Self::User,
            AccScope::Project { dir } => Self::Project { dir },
        }
    }
}

/// How a harness reaches an MCP server. `env` and `headers` are
/// `KEY=VALUE` pairs.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccMcpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<AccPair>,
    },
    Http {
        url: String,
        headers: Vec<AccPair>,
    },
    Sse {
        url: String,
        headers: Vec<AccPair>,
    },
    /// Listed but not translatable.
    Unknown {
        what: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccPair {
    pub key: String,
    pub value: String,
}

fn pairs(v: Vec<(String, String)>) -> Vec<AccPair> {
    v.into_iter().map(|(key, value)| AccPair { key, value }).collect()
}

fn tuples(v: Vec<AccPair>) -> Vec<(String, String)> {
    v.into_iter().map(|p| (p.key, p.value)).collect()
}

impl From<mcp::McpTransport> for AccMcpTransport {
    fn from(t: mcp::McpTransport) -> Self {
        match t {
            mcp::McpTransport::Stdio { command, args, env } => Self::Stdio {
                command,
                args,
                env: pairs(env),
            },
            mcp::McpTransport::Http { url, headers } => Self::Http {
                url,
                headers: pairs(headers),
            },
            mcp::McpTransport::Sse { url, headers } => Self::Sse {
                url,
                headers: pairs(headers),
            },
            mcp::McpTransport::Unknown { what } => Self::Unknown { what },
        }
    }
}

impl From<AccMcpTransport> for mcp::McpTransport {
    fn from(t: AccMcpTransport) -> Self {
        match t {
            AccMcpTransport::Stdio { command, args, env } => Self::Stdio {
                command,
                args,
                env: tuples(env),
            },
            AccMcpTransport::Http { url, headers } => Self::Http {
                url,
                headers: tuples(headers),
            },
            AccMcpTransport::Sse { url, headers } => Self::Sse {
                url,
                headers: tuples(headers),
            },
            AccMcpTransport::Unknown { what } => Self::Unknown { what },
        }
    }
}

/// One MCP server as one harness has it configured. `extras_json` is a
/// JSON object of the harness-specific keys ACC doesn't model; pass `{}`
/// when copying a server to another harness.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccMcpServer {
    pub name: String,
    pub transport: AccMcpTransport,
    pub enabled: bool,
    pub extras_json: String,
}

impl From<mcp::McpServer> for AccMcpServer {
    fn from(s: mcp::McpServer) -> Self {
        Self {
            name: s.name,
            transport: s.transport.into(),
            enabled: s.enabled,
            extras_json: s.extras.to_text(),
        }
    }
}

impl TryFrom<AccMcpServer> for mcp::McpServer {
    type Error = AccError;

    fn try_from(s: AccMcpServer) -> Result<Self, AccError> {
        let extras = if s.extras_json.trim().is_empty() {
            loadout::json::Json::Object(vec![])
        } else {
            loadout::json::parse(&s.extras_json).map_err(|e| AccError::InvalidArgument {
                reason: format!("extras: {e}"),
            })?
        };
        if extras.as_object().is_none() {
            return Err(AccError::InvalidArgument {
                reason: "extras must be a JSON object".into(),
            });
        }
        Ok(Self {
            name: s.name,
            transport: s.transport.into(),
            enabled: s.enabled,
            extras,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AccLinkState {
    /// A symlink into the store.
    Linked,
    /// The harness reads the store itself.
    Native,
    /// Not there for this harness.
    Missing,
    /// Something else is in the way: a real directory or a symlink elsewhere.
    Foreign { what: String },
}

impl From<skills::LinkState> for AccLinkState {
    fn from(s: skills::LinkState) -> Self {
        match s {
            skills::LinkState::Linked => Self::Linked,
            skills::LinkState::Native => Self::Native,
            skills::LinkState::Missing => Self::Missing,
            skills::LinkState::Foreign { what } => Self::Foreign { what },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccSkillPlacement {
    pub harness: AccHarness,
    pub state: AccLinkState,
}

/// Where a skill came from, as the `skills` CLI lock file records it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccSkillSource {
    pub source: String,
    pub url: Option<String>,
    pub path: Option<String>,
    pub installed_at: Option<String>,
}

/// One skill in the store: its `SKILL.md` front matter (in order), its
/// supporting files, and how each harness sees it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccSkill {
    pub name: String,
    pub description: String,
    pub frontmatter: Vec<AccPair>,
    pub files: Vec<String>,
    pub source: Option<AccSkillSource>,
    pub placements: Vec<AccSkillPlacement>,
}

impl From<skills::Skill> for AccSkill {
    fn from(s: skills::Skill) -> Self {
        Self {
            name: s.name.clone(),
            description: s.description().to_owned(),
            frontmatter: pairs(s.frontmatter.fields),
            files: s.files,
            source: s.source.map(|src| AccSkillSource {
                source: src.source,
                url: src.url,
                path: src.path,
                installed_at: src.installed_at,
            }),
            placements: s
                .placements
                .into_iter()
                .map(|p| AccSkillPlacement {
                    harness: p.harness.into(),
                    state: p.state.into(),
                })
                .collect(),
        }
    }
}

/// A harness's skills directory at a scope. `dir` is empty when it doesn't
/// exist yet; `unmanaged` names skills living only there (adoptable).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccSkillPlace {
    pub harness: AccHarness,
    pub dir: String,
    pub native: bool,
    pub unmanaged: Vec<String>,
}

/// What `skills_list_exec` found: the store (resolved on the host, empty
/// when it doesn't exist), its skills and every harness's directory.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccSkillListing {
    pub store: String,
    pub skills: Vec<AccSkill>,
    pub places: Vec<AccSkillPlace>,
}

/// A validation finding for a skill.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccSkillIssue {
    pub error: bool,
    pub message: String,
}

fn err(e: LoadoutError) -> AccError {
    match e {
        LoadoutError::Unsupported(reason) => AccError::Unsupported { reason },
        e => AccError::InvalidArgument {
            reason: e.to_string(),
        },
    }
}

fn exec(r: Result<String, LoadoutError>) -> Result<AccExec, AccError> {
    r.map(AccExec::script).map_err(err)
}

// ---- skills ----------------------------------------------------------------

/// Lists the skill store at `scope` and every harness's skills directory,
/// for `parse_skills`.
#[uniffi::export]
pub fn skills_list_exec(scope: AccScope) -> AccExec {
    AccExec::script(skills::list_script(&scope.into()))
}

#[uniffi::export]
pub fn parse_skills(stdout: String, scope: AccScope) -> AccSkillListing {
    let l = skills::parse_listing(&stdout, &scope.into());
    AccSkillListing {
        store: l.store,
        skills: l.skills.into_iter().map(Into::into).collect(),
        places: l
            .places
            .into_iter()
            .map(|p| AccSkillPlace {
                harness: p.harness.into(),
                dir: p.dir,
                native: p.native,
                unmanaged: p.unmanaged,
            })
            .collect(),
    }
}

/// Prints the store skill's `SKILL.md`; exits 9 when there is no such skill.
#[uniffi::export]
pub fn skill_read_exec(scope: AccScope, name: String) -> Result<AccExec, AccError> {
    exec(skills::read_script(&scope.into(), &name))
}

/// Writes `markdown` as the store skill's `SKILL.md`, making the skill
/// when it is new. Check `skill_issues` first.
#[uniffi::export]
pub fn skill_write_exec(scope: AccScope, name: String, markdown: String) -> Result<AccExec, AccError> {
    let script = skills::write_script(&scope.into(), &name).map_err(err)?;
    Ok(AccExec {
        script,
        stdin: markdown.into_bytes(),
    })
}

/// Deletes the store skill and every harness's link to it; exits 9 when
/// there is no such skill.
#[uniffi::export]
pub fn skill_remove_exec(scope: AccScope, name: String) -> Result<AccExec, AccError> {
    exec(skills::remove_script(&scope.into(), &name))
}

/// Links the store skill into each harness's skills directory (relative
/// symlinks; harnesses that read the store are skipped). Exits 8 when
/// something else is in the way, unless `force` replaces it, and 9 when
/// the skill isn't in the store.
#[uniffi::export]
pub fn skill_link_exec(
    scope: AccScope,
    name: String,
    harnesses: Vec<AccHarness>,
    force: bool,
) -> Result<AccExec, AccError> {
    let hs: Vec<_> = harnesses.into_iter().map(Into::into).collect();
    exec(skills::link_script(&scope.into(), &name, &hs, force))
}

/// Removes each harness's link to the store skill (only links into the
/// store are touched).
#[uniffi::export]
pub fn skill_unlink_exec(
    scope: AccScope,
    name: String,
    harnesses: Vec<AccHarness>,
) -> Result<AccExec, AccError> {
    let hs: Vec<_> = harnesses.into_iter().map(Into::into).collect();
    exec(skills::unlink_script(&scope.into(), &name, &hs))
}

/// Moves a skill that lives only in `harness`'s skills directory into the
/// store and leaves a link behind. Exits 9 when it isn't a real directory
/// there, 8 when the store already has the name.
#[uniffi::export]
pub fn skill_adopt_exec(scope: AccScope, harness: AccHarness, name: String) -> Result<AccExec, AccError> {
    exec(skills::adopt_script(&scope.into(), harness.into(), &name))
}

/// What is wrong with the skill `name` whose `SKILL.md` is `markdown`
/// (empty when it is valid).
#[uniffi::export]
pub fn skill_issues(name: String, markdown: String) -> Vec<AccSkillIssue> {
    skills::issues(&name, &markdown)
        .into_iter()
        .map(|i| AccSkillIssue {
            error: i.error,
            message: i.message,
        })
        .collect()
}

/// A `SKILL.md` for a new skill.
#[uniffi::export]
pub fn skill_template(name: String, description: String) -> String {
    skills::template(&name, &description)
}

// ---- MCP -------------------------------------------------------------------

/// Prints `harness`'s MCP config at `scope` (nothing when the file doesn't
/// exist yet), for `mcp_servers`. `None` when the harness has no such file
/// there (Copilot has no project file).
#[uniffi::export]
pub fn mcp_read_exec(harness: AccHarness, scope: AccScope) -> Option<AccExec> {
    mcp::read_script(harness.into(), &scope.into()).map(AccExec::script)
}

/// The servers in `harness`'s config `text` (from `mcp_read_exec`).
#[uniffi::export]
pub fn mcp_servers(harness: AccHarness, text: String) -> Result<Vec<AccMcpServer>, AccError> {
    mcp::servers(harness.into(), &text)
        .map(|s| s.into_iter().map(Into::into).collect())
        .map_err(err)
}

/// `text` with `server` added or replaced; only that entry changes.
#[uniffi::export]
pub fn mcp_upsert(harness: AccHarness, text: String, server: AccMcpServer) -> Result<String, AccError> {
    let server: mcp::McpServer = server.try_into()?;
    mcp::upsert(harness.into(), &text, &server).map_err(err)
}

/// `text` without the server `name`.
#[uniffi::export]
pub fn mcp_remove(harness: AccHarness, text: String, name: String) -> Result<String, AccError> {
    mcp::remove(harness.into(), &text, &name).map_err(err)
}

/// `text` with the server `name` kept but not started (or started again).
/// `Unsupported` for Claude and Copilot, whose files can't say that: remove
/// the server instead.
#[uniffi::export]
pub fn mcp_set_enabled(
    harness: AccHarness,
    text: String,
    name: String,
    enabled: bool,
) -> Result<String, AccError> {
    mcp::set_enabled(harness.into(), &text, &name, enabled).map_err(err)
}

/// Whether `harness`'s config can keep a disabled server.
#[uniffi::export]
pub fn mcp_supports_disable(harness: AccHarness) -> bool {
    mcp::supports_disable(harness.into())
}

/// Replaces `harness`'s MCP config at `scope` with `text` (from
/// `mcp_upsert` or `mcp_remove`), after a backup to `~/.acc/backups/`.
#[uniffi::export]
pub fn mcp_write_exec(harness: AccHarness, scope: AccScope, text: String) -> Option<AccExec> {
    mcp::write_script(harness.into(), &scope.into()).map(|script| AccExec {
        script,
        stdin: text.into_bytes(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn mcp_through_ffi() {
        let server = AccMcpServer {
            name: "x".into(),
            transport: AccMcpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into()],
                env: vec![AccPair {
                    key: "K".into(),
                    value: "v".into(),
                }],
            },
            enabled: true,
            extras_json: "{\"timeout\": 5}".into(),
        };
        let text = mcp_upsert(AccHarness::Codex, String::new(), server.clone()).unwrap();
        let back = mcp_servers(AccHarness::Codex, text.clone()).unwrap();
        assert_eq!(back[0].transport, server.transport);
        assert_eq!(back[0].extras_json, "{\"timeout\":5}");
        assert!(mcp_set_enabled(AccHarness::Claude, "{}".into(), "x".into(), false).is_err());
        assert!(mcp_read_exec(AccHarness::Copilot, AccScope::Project { dir: "/w".into() }).is_none());
        assert!(mcp_write_exec(AccHarness::Codex, AccScope::User, text).is_some());
        let bad = AccMcpServer {
            extras_json: "[]".into(),
            ..server
        };
        assert!(matches!(
            mcp_upsert(AccHarness::Codex, String::new(), bad),
            Err(AccError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn skills_through_ffi() {
        assert!(matches!(
            skill_read_exec(AccScope::User, "Bad Name".into()),
            Err(AccError::InvalidArgument { .. })
        ));
        let w = skill_write_exec(
            AccScope::User,
            "tdd".into(),
            skill_template("tdd".into(), "d".into()),
        )
        .unwrap();
        assert!(w.script.contains(".agents/skills/tdd") && !w.stdin.is_empty());
        assert!(
            skill_issues("tdd".into(), "---\nname: tdd\n---\n".into())
                .iter()
                .any(|i| i.error)
        );
        let l = parse_skills(
            "store\t/h/.agents/skills\nskill\ta\nf\tname: a\nf\tdescription: D\nplace\tcodex\t/h/.codex/skills\town\n".into(),
            AccScope::User,
        );
        assert_eq!(l.skills[0].description, "D");
        assert_eq!(l.skills[0].placements[0].state, AccLinkState::Missing);
        assert!(skills_list_exec(AccScope::User).script.contains("printf 'place"));
    }
}

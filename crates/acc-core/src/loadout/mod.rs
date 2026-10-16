//! Skills and MCP servers for every harness, on any host.
//!
//! Harnesses read skills from directories of their own and MCP servers from
//! config files of their own, in their own spellings. This module knows
//! those places for every harness ACC drives, at user scope and per
//! project, and keeps to the sans-IO rule: a client runs the POSIX shell
//! scripts it hands out (locally or over SSH, like [`crate::worktree`]) and
//! passes the output back to pure functions.
//!
//! **Skills** live once in a store, `~/.agents/skills/<name>/SKILL.md` (or
//! `<project>/.agents/skills`), the layout the `skills` CLI and Loadout use,
//! so the same tree can be managed by any of them. A harness has a skill
//! when its own skills directory holds a relative symlink to the store
//! entry, or when it reads the store itself ("native"). [`skills`] lists
//! the store and every harness's placements, links and unlinks, adopts a
//! skill that lives only inside a harness's directory, and validates
//! `SKILL.md` front matter.
//!
//! **MCP servers** stay in each harness's config file: [`mcp::servers`]
//! reads them into one shape, [`mcp::upsert`] and [`mcp::remove`] edit the
//! text in place (only the one entry changes; comments and unrelated keys
//! survive), and [`mcp::write_script`] writes it back with a backup under
//! `~/.acc/backups/`.

pub mod json;
pub mod mcp;
pub mod skills;
pub mod toml;

use serde::{Deserialize, Serialize};

use crate::harness::Harness;
use crate::shell::quote;

/// User-wide configuration or one project's checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum Scope {
    User,
    Project {
        /// The project's directory on the host (`~/` is expanded there).
        dir: String,
    },
}

impl Scope {
    /// The project directory as a shell expression, or `$HOME`.
    fn base(&self) -> String {
        match self {
            Self::User => "\"$HOME\"".to_owned(),
            Self::Project { dir } => crate::detached::quote_path(dir),
        }
    }

    /// The skill store at this scope, as a shell expression.
    pub fn store(&self) -> String {
        format!("{}/.agents/skills", self.base())
    }

    /// The `skills` CLI lock file next to the store (provenance).
    pub fn lock_file(&self) -> String {
        format!("{}/.agents/.skill-lock.json", self.base())
    }
}

/// Why skills or MCP servers couldn't be read or changed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadoutError {
    /// The harness has no such place at this scope, or the format can't
    /// say it.
    #[error("{0}")]
    Unsupported(String),
    /// The config file isn't readable as its format: left alone.
    #[error("the config file can't be read: {0}")]
    Unreadable(String),
    #[error("invalid name: {0}")]
    InvalidName(String),
    #[error("no such entry: {0}")]
    NotFound(String),
}

/// Where a harness reads skills at a scope: the directory as a shell
/// expression, and whether the harness reads the store itself (then the
/// store's skills are visible without linking).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPlace {
    pub dir: String,
    pub native: bool,
}

fn config_home() -> &'static str {
    "\"${XDG_CONFIG_HOME:-$HOME/.config}\""
}

fn claude_home() -> &'static str {
    "\"${CLAUDE_CONFIG_DIR:-$HOME/.claude}\""
}

fn codex_home() -> &'static str {
    "\"${CODEX_HOME:-$HOME/.codex}\""
}

/// Where `harness` reads skills at `scope`, or `None` when it has no such
/// directory there. Codex, Cursor, Copilot and opencode read a project's
/// `.agents/skills` directly, and pi reads `~/.agents/skills` too.
pub fn skill_place(harness: Harness, scope: &Scope) -> Option<SkillPlace> {
    let own = |dir: String| SkillPlace { dir, native: false };
    let place = match scope {
        Scope::User => match harness {
            Harness::Claude => own(format!("{}/skills", claude_home())),
            Harness::Codex => own(format!("{}/skills", codex_home())),
            Harness::Cursor => own("\"$HOME\"/.cursor/skills".into()),
            Harness::Copilot => own("\"$HOME\"/.copilot/skills".into()),
            Harness::Opencode => own(format!("{}/opencode/skills", config_home())),
            Harness::Pi => SkillPlace {
                dir: "\"$HOME\"/.pi/agent/skills".into(),
                native: true,
            },
        },
        Scope::Project { .. } => {
            let store = scope.store();
            match harness {
                Harness::Claude => own(format!("{}/.claude/skills", scope.base())),
                Harness::Codex | Harness::Cursor | Harness::Copilot | Harness::Opencode => SkillPlace {
                    dir: store,
                    native: true,
                },
                Harness::Pi => SkillPlace {
                    dir: format!("{}/.pi/skills", scope.base()),
                    native: true,
                },
            }
        }
    };
    Some(place)
}

/// Shell lines that set `f` to `harness`'s MCP config file at `scope`, or
/// `None` when it has none there (Copilot has no project file).
pub(crate) fn mcp_config_script(harness: Harness, scope: &Scope) -> Option<String> {
    let base = scope.base();
    let file = match (scope, harness) {
        (Scope::User, Harness::Claude) => "f=\"${CLAUDE_CONFIG_DIR:-$HOME}\"/.claude.json".to_owned(),
        (Scope::User, Harness::Codex) => format!("f={}/config.toml", codex_home()),
        (Scope::User, Harness::Cursor) => "f=\"$HOME\"/.cursor/mcp.json".to_owned(),
        (Scope::User, Harness::Copilot) => "f=\"$HOME\"/.copilot/mcp-config.json".to_owned(),
        (Scope::User, Harness::Pi) => "f=\"$HOME\"/.pi/agent/mcp.json".to_owned(),
        (Scope::User, Harness::Opencode) => opencode_file(&format!("{}/opencode", config_home())),
        (Scope::Project { .. }, Harness::Claude) => format!("f={base}/.mcp.json"),
        (Scope::Project { .. }, Harness::Codex) => format!("f={base}/.codex/config.toml"),
        (Scope::Project { .. }, Harness::Cursor) => format!("f={base}/.cursor/mcp.json"),
        (Scope::Project { .. }, Harness::Copilot) => return None,
        (Scope::Project { .. }, Harness::Pi) => format!("f={base}/.pi/mcp.json"),
        (Scope::Project { .. }, Harness::Opencode) => opencode_file(&base),
    };
    Some(format!("{file}\n"))
}

/// opencode takes `opencode.jsonc` or `opencode.json`: whichever exists.
fn opencode_file(dir: &str) -> String {
    format!("f={dir}/opencode.jsonc\n[ -f \"$f\" ] || f={dir}/opencode.json")
}

/// Script printing the file `path_script` names (nothing when it doesn't
/// exist yet).
pub(crate) fn read_file_script(path_script: &str) -> String {
    format!("{path_script}[ -f \"$f\" ] && cat \"$f\"\ntrue\n")
}

/// Script that replaces the file `path_script` names with its stdin,
/// atomically, after copying the old one to `~/.acc/backups/`.
pub(crate) fn write_file_script(path_script: &str) -> String {
    format!(
        "{path_script}\
         mkdir -p \"$(dirname \"$f\")\" || exit 1\n\
         if [ -f \"$f\" ]; then\n\
         \x20 b=\"$HOME\"/.acc/backups\n\
         \x20 mkdir -p \"$b\" && cp -p \"$f\" \"$b/$(basename \"$f\").$(date +%s)\"\n\
         fi\n\
         cat >\"$f.acc.$$\" && mv \"$f.acc.$$\" \"$f\"\n"
    )
}

/// `[A-Za-z0-9._-]+`, no leading dot: a name that is safe as one path
/// component and inside a shell argument. Skill names are stricter
/// ([`skills::name_problem`]).
pub(crate) fn valid_entry_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

pub(crate) fn quoted(name: &str) -> String {
    quote(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places() {
        assert_eq!(Scope::User.store(), "\"$HOME\"/.agents/skills");
        assert_eq!(
            Scope::Project { dir: "~/p".into() }.store(),
            "\"$HOME\"/p/.agents/skills"
        );
        let codex = skill_place(Harness::Codex, &Scope::Project { dir: "/w".into() })
            .unwrap_or_else(|| unreachable!());
        assert!(codex.native && codex.dir == "/w/.agents/skills");
        let claude = skill_place(Harness::Claude, &Scope::User).unwrap_or_else(|| unreachable!());
        assert!(!claude.native && claude.dir.contains("CLAUDE_CONFIG_DIR"));
        assert!(mcp_config_script(Harness::Copilot, &Scope::Project { dir: "/w".into() }).is_none());
        assert!(
            mcp_config_script(Harness::Opencode, &Scope::User)
                .unwrap_or_default()
                .contains("opencode.jsonc")
        );
    }
}

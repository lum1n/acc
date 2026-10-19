//! One permission policy for every harness.
//!
//! Each harness has its own permission rules, in its own format, or none.
//! A [`Policy`] is a list of allow and deny [`Rule`]s (by tool, path glob or
//! command prefix) that the core checks against every approval a harness
//! asks for, whichever harness it is: [`crate::session::Session::set_policy`]
//! answers matching requests itself with the harness's own allow-once or
//! reject option. Deny wins over allow; a request no rule matches is left
//! for the user.
//!
//! The same rules can be written into a harness's own settings, so it
//! doesn't ask in the first place: [`settings_read_script`],
//! [`merge_settings`] and [`settings_write_script`] cover Claude Code
//! (`.claude/settings.local.json`), Codex (`~/.codex/rules/default.rules`),
//! Cursor (`.cursor/cli.json`) and opencode (`opencode.json`). Copilot and pi
//! have no rules file.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::event::{PermissionOption, PermissionOptionKind};
use crate::harness::Harness;
use crate::item::ItemDetail;
use crate::transcript::Item;

/// Whether a rule allows or denies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

/// One rule. Every field given must match; a rule with none matches
/// every approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub decision: Decision,
    /// The harness's tool name (`Bash`, `Edit`, `WebFetch`, ...) or one of
    /// ACC's kinds: `shell`, `edit`, `read`, `fetch`, `mcp`. Case-insensitive,
    /// `*` and `?` match any characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Glob over the files the tool acts on, relative to the session's
    /// directory (or absolute): `*` stays within one directory, `**` crosses
    /// them. An allow rule needs every file to match, a deny rule any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A shell command's start, whole words: `git status` matches
    /// `git status -s` but not `git statusx`. Allow rules never match a
    /// command that chains others (`;`, `&&`, `|`, `$(...)`, redirects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Rules checked against every approval a session's harness asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub rules: Vec<Rule>,
}

pub use crate::item::ToolKind;

/// What an approval is about, as far as the harness says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The harness's tool name.
    pub tool: Option<String>,
    pub kind: Option<ToolKind>,
    pub command: Option<String>,
    pub paths: Vec<String>,
}

impl Request {
    /// Reads a request from the tool call it is about (when the transcript
    /// has it) and the request's own typed detail
    /// ([`crate::event::Event::PermissionRequested`]).
    pub fn new(item: Option<&Item>, detail: Option<&ItemDetail>) -> Self {
        let mut r = Request::default();
        for detail in item.map(|i| &i.detail).into_iter().chain(detail) {
            r.read(detail);
        }
        if r.kind.is_none() {
            r.kind = r.tool.as_deref().map(ToolKind::of);
        }
        if matches!(r.kind, None | Some(ToolKind::Other)) && r.command.is_some() {
            r.kind = Some(ToolKind::Shell);
        }
        r
    }

    /// Adds what `detail` says; what an earlier detail said stays.
    fn read(&mut self, detail: &ItemDetail) {
        let (tool, kind) = match detail {
            ItemDetail::Command { tool, command, .. } => {
                if self.command.is_none() {
                    self.command = Some(command.clone()).filter(|c| !c.trim().is_empty());
                }
                (tool, Some(ToolKind::Shell))
            }
            ItemDetail::FileChange { tool, files } => {
                for f in files {
                    push_path(&mut self.paths, &f.path);
                }
                (tool, Some(ToolKind::Edit))
            }
            ItemDetail::Tool {
                tool,
                kind,
                input,
                paths,
                ..
            } => {
                for p in paths {
                    push_path(&mut self.paths, p);
                }
                if self.command.is_none() {
                    self.command = command_arg(input);
                }
                (tool, Some(*kind))
            }
            ItemDetail::Subagent { tool, .. } => (tool, None),
            ItemDetail::Text => return,
        };
        if self.tool.is_none() {
            self.tool = Some(tool.clone());
        }
        if self.kind.is_none() {
            self.kind = kind;
        }
    }
}

fn push_path(paths: &mut Vec<String>, p: &str) {
    if !p.is_empty() && !paths.iter().any(|q| q == p) {
        paths.push(p.to_owned());
    }
}

/// The shell command among a tool's arguments, for tools ACC has no
/// command shape for.
fn command_arg(input: &Value) -> Option<String> {
    match input.get("command") {
        Some(Value::String(c)) if !c.trim().is_empty() => Some(c.clone()),
        Some(Value::Array(parts)) => {
            let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            // `["bash", "-lc", "<script>"]` is the script.
            match words.as_slice() {
                [sh, flag, script] if sh.ends_with("sh") && flag.starts_with('-') => {
                    Some((*script).to_owned())
                }
                _ if !words.is_empty() => Some(words.join(" ")),
                _ => None,
            }
        }
        _ => None,
    }
}

impl Policy {
    /// What the policy says about `request` in a session working in `cwd`:
    /// the decision and the index of the rule that made it, or `None` to ask.
    pub fn decide(&self, request: &Request, cwd: &str) -> Option<(Decision, usize)> {
        let matching = |decision: Decision| {
            self.rules
                .iter()
                .position(|r| r.decision == decision && r.matches(request, cwd))
                .map(|i| (decision, i))
        };
        matching(Decision::Deny).or_else(|| matching(Decision::Allow))
    }
}

impl Rule {
    fn matches(&self, req: &Request, cwd: &str) -> bool {
        if let Some(tool) = &self.tool {
            let tool = tool.to_ascii_lowercase();
            let native = req
                .tool
                .as_deref()
                .is_some_and(|t| glob(&tool, &t.to_ascii_lowercase()));
            let kind = req.kind.is_some_and(|k| glob(&tool, k.name()));
            if !native && !kind {
                return false;
            }
        }
        if let Some(prefix) = &self.command {
            let Some(cmd) = &req.command else { return false };
            let ok = match self.decision {
                Decision::Allow => !chains(cmd) && starts_with_words(cmd.trim(), prefix.trim()),
                Decision::Deny => segments(cmd).any(|s| starts_with_words(s, prefix.trim())),
            };
            if !ok {
                return false;
            }
        }
        if let Some(pattern) = &self.path {
            if req.paths.is_empty() {
                return false;
            }
            let hit = |p: &String| path_matches(pattern, p, cwd);
            let ok = match self.decision {
                Decision::Allow => req.paths.iter().all(hit),
                Decision::Deny => req.paths.iter().any(hit),
            };
            if !ok {
                return false;
            }
        }
        true
    }
}

/// The option to answer with for `decision`, if the harness offers one.
pub fn option_for(decision: Decision, options: &[PermissionOption]) -> Option<&PermissionOption> {
    let kinds: &[PermissionOptionKind] = match decision {
        Decision::Allow => &[PermissionOptionKind::AllowOnce],
        Decision::Deny => &[
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ],
    };
    kinds
        .iter()
        .find_map(|k| options.iter().find(|o| o.kind == Some(*k)))
}

fn starts_with_words(cmd: &str, prefix: &str) -> bool {
    !prefix.is_empty()
        && cmd
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

const CHAIN: &[&str] = &[";", "&", "|", "`", "$(", ">", "<", "\n"];

/// Whether a command runs more than one thing.
fn chains(cmd: &str) -> bool {
    CHAIN.iter().any(|c| cmd.contains(c))
}

/// The commands a shell line runs, split at its operators.
fn segments(cmd: &str) -> impl Iterator<Item = &str> {
    cmd.split([';', '&', '|', '\n', '`', '(', ')'])
        .map(|s| s.trim().trim_start_matches('$').trim())
        .filter(|s| !s.is_empty())
}

fn path_matches(pattern: &str, path: &str, cwd: &str) -> bool {
    let cwd = cwd.trim_end_matches('/');
    let rel = path
        .strip_prefix(cwd)
        .and_then(|r| r.strip_prefix('/'))
        .unwrap_or(path);
    let rel = rel.strip_prefix("./").unwrap_or(rel);
    glob(pattern, rel) || glob(pattern, path)
}

/// `*` and `?` within a path segment, `**` across them (`**/` also
/// matches no directory at all).
pub fn glob(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p {
            [] => t.is_empty(),
            [b'*', b'*', b'/', rest @ ..] => {
                go(rest, t) || (0..t.len()).any(|i| t[i] == b'/' && go(rest, &t[i + 1..]))
            }
            [b'*', b'*', rest @ ..] => (0..=t.len()).any(|i| go(rest, &t[i..])),
            [b'*', rest @ ..] => {
                let stop = t.iter().position(|&c| c == b'/').unwrap_or(t.len());
                (0..=stop).any(|i| go(rest, &t[i..]))
            }
            [b'?', rest @ ..] => t.first().is_some_and(|&c| c != b'/') && go(rest, &t[1..]),
            [c, rest @ ..] => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

// ---- harness settings ----------------------------------------------------

/// Why a harness's settings couldn't be updated.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SettingsError {
    #[error("{0:?} has no permission settings ACC can write")]
    Unsupported(Harness),
    /// The file isn't plain JSON (opencode allows comments): left alone.
    #[error("the settings file can't be read: {0}")]
    Unreadable(String),
}

/// The settings file a harness reads its rules from, as a shell expression.
fn settings_path(harness: Harness, cwd: &str) -> Option<String> {
    let dir = crate::detached::quote_path(cwd);
    match harness {
        Harness::Claude => Some(format!("{dir}/.claude/settings.local.json")),
        Harness::Codex => Some("\"$HOME\"/.codex/rules/default.rules".to_owned()),
        Harness::Cursor => Some(format!("{dir}/.cursor/cli.json")),
        Harness::Opencode => Some(format!("{dir}/opencode.json")),
        Harness::Copilot | Harness::Pi => None,
    }
}

/// Script printing `harness`'s permission settings for a session in `cwd`
/// (nothing when the file doesn't exist yet). `None` for harnesses without
/// a rules file.
pub fn settings_read_script(harness: Harness, cwd: &str) -> Option<String> {
    let path = settings_path(harness, cwd)?;
    Some(format!("f={path}\n[ -f \"$f\" ] && cat \"$f\"\ntrue\n"))
}

/// Script that replaces the settings file with its stdin (the output of
/// [`merge_settings`]), atomically.
pub fn settings_write_script(harness: Harness, cwd: &str) -> Option<String> {
    let path = settings_path(harness, cwd)?;
    Some(format!(
        "f={path}\n\
         mkdir -p \"$(dirname \"$f\")\" || exit 1\n\
         cat >\"$f.acc.$$\" && mv \"$f.acc.$$\" \"$f\"\n"
    ))
}

/// `existing` settings (from [`settings_read_script`]) with `rules` added,
/// and the indexes of rules the harness has no way to say. Rules already
/// there aren't added twice.
pub fn merge_settings(
    harness: Harness,
    existing: &str,
    rules: &[Rule],
) -> Result<(String, Vec<usize>), SettingsError> {
    let mut skipped = Vec::new();
    match harness {
        Harness::Claude | Harness::Cursor => {
            let mut root = parse_object(existing)?;
            let perms = object_at(&mut root, "permissions");
            for (i, rule) in rules.iter().enumerate() {
                let entry = if harness == Harness::Claude {
                    claude_rule(rule)
                } else {
                    cursor_rule(rule)
                };
                let Some(entry) = entry else {
                    skipped.push(i);
                    continue;
                };
                let key = match rule.decision {
                    Decision::Allow => "allow",
                    Decision::Deny => "deny",
                };
                let list = perms.entry(key).or_insert_with(|| json!([]));
                if !list.is_array() {
                    *list = json!([]);
                }
                if let Some(list) = list.as_array_mut()
                    && !list.iter().any(|v| v.as_str() == Some(&entry))
                {
                    list.push(Value::String(entry));
                }
            }
            Ok((pretty(&root), skipped))
        }
        Harness::Opencode => {
            let mut root = parse_object(existing)?;
            let perms = object_at(&mut root, "permission");
            for (i, rule) in rules.iter().enumerate() {
                let word = match rule.decision {
                    Decision::Allow => "allow",
                    Decision::Deny => "deny",
                };
                let kind = rule.tool.as_deref().map(ToolKind::of);
                match (kind, &rule.command, &rule.path) {
                    (None | Some(ToolKind::Shell), Some(prefix), None) => {
                        let bash = perms.entry("bash").or_insert_with(|| json!({}));
                        if let Value::String(old) = bash {
                            *bash = json!({ "*": old.clone() });
                        }
                        if let Some(map) = bash.as_object_mut() {
                            let prefix = prefix.trim();
                            map.insert(prefix.to_owned(), json!(word));
                            map.insert(format!("{prefix} *"), json!(word));
                        }
                    }
                    (Some(ToolKind::Edit), None, None) => {
                        perms.insert("edit".into(), json!(word));
                    }
                    (Some(ToolKind::Fetch), None, None) => {
                        perms.insert("webfetch".into(), json!(word));
                    }
                    _ => skipped.push(i),
                }
            }
            Ok((pretty(&root), skipped))
        }
        Harness::Codex => {
            let mut out = existing.to_owned();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            for (i, rule) in rules.iter().enumerate() {
                let kind = rule.tool.as_deref().map(ToolKind::of);
                let (Some(prefix), None, None | Some(ToolKind::Shell)) = (&rule.command, &rule.path, kind)
                else {
                    skipped.push(i);
                    continue;
                };
                let words: Vec<String> = prefix.split_whitespace().map(|w| json!(w).to_string()).collect();
                if words.is_empty() {
                    skipped.push(i);
                    continue;
                }
                let decision = match rule.decision {
                    Decision::Allow => "allow",
                    Decision::Deny => "forbidden",
                };
                let line = format!(
                    "prefix_rule(pattern = [{}], decision = \"{decision}\")",
                    words.join(", ")
                );
                if !out.lines().any(|l| l.trim() == line) {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
            Ok((out, skipped))
        }
        h @ (Harness::Copilot | Harness::Pi) => Err(SettingsError::Unsupported(h)),
    }
}

fn parse_object(text: &str) -> Result<Map<String, Value>, SettingsError> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(SettingsError::Unreadable("not a JSON object".into())),
        Err(e) => Err(SettingsError::Unreadable(e.to_string())),
    }
}

fn object_at<'a>(root: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let v = root.entry(key).or_insert_with(|| json!({}));
    if !v.is_object() {
        *v = json!({});
    }
    match v {
        Value::Object(m) => m,
        _ => unreachable!("just made an object"),
    }
}

fn pretty(v: &Map<String, Value>) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_default();
    s.push('\n');
    s
}

/// A rule in Claude Code's `permissions` syntax.
fn claude_rule(rule: &Rule) -> Option<String> {
    let kind = rule.tool.as_deref().map(ToolKind::of);
    match (rule.tool.as_deref(), kind, &rule.command, &rule.path) {
        (_, None | Some(ToolKind::Shell), Some(c), None) => Some(format!("Bash({}:*)", c.trim())),
        (_, Some(ToolKind::Shell), None, None) => Some("Bash".into()),
        (_, Some(ToolKind::Edit), None, Some(p)) => Some(format!("Edit({p})")),
        (_, Some(ToolKind::Read), None, Some(p)) => Some(format!("Read({p})")),
        (_, Some(ToolKind::Edit), None, None) => Some("Edit".into()),
        (_, Some(ToolKind::Read), None, None) => Some("Read".into()),
        (_, Some(ToolKind::Fetch), None, None) => Some("WebFetch".into()),
        // A tool by its own name, without wildcards.
        (Some(t), Some(ToolKind::Other | ToolKind::Mcp), None, None) if !t.contains(['*', '?']) => {
            Some(t.to_owned())
        }
        _ => None,
    }
}

/// A rule in Cursor CLI's `permissions` syntax.
fn cursor_rule(rule: &Rule) -> Option<String> {
    let kind = rule.tool.as_deref().map(ToolKind::of);
    match (kind, &rule.command, &rule.path) {
        (None | Some(ToolKind::Shell), Some(c), None) => Some(format!("Shell({})", c.trim())),
        (Some(ToolKind::Edit), None, Some(p)) => Some(format!("Write({p})")),
        (Some(ToolKind::Read), None, Some(p)) => Some(format!("Read({p})")),
        (Some(ToolKind::Edit), None, None) => Some("Write(**)".into()),
        (Some(ToolKind::Read), None, None) => Some("Read(**)".into()),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn rule(decision: Decision, tool: Option<&str>, path: Option<&str>, command: Option<&str>) -> Rule {
        Rule {
            decision,
            tool: tool.map(Into::into),
            path: path.map(Into::into),
            command: command.map(Into::into),
        }
    }

    fn shell(cmd: &str) -> Request {
        Request {
            tool: Some("Bash".into()),
            kind: Some(ToolKind::Shell),
            command: Some(cmd.into()),
            paths: vec![],
        }
    }

    fn edit(paths: &[&str]) -> Request {
        Request {
            tool: Some("Edit".into()),
            kind: Some(ToolKind::Edit),
            command: None,
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn globs() {
        assert!(glob("src/*.rs", "src/a.rs"));
        assert!(!glob("src/*.rs", "src/x/a.rs"));
        assert!(glob("src/**/*.rs", "src/a.rs") && glob("src/**/*.rs", "src/x/y/a.rs"));
        assert!(glob("**", "any/thing") && glob("a?c", "abc") && !glob("a?c", "a/c"));
    }

    #[test]
    fn deny_wins_and_chained_commands_are_never_allowed() {
        let p = Policy {
            rules: vec![
                rule(Decision::Allow, Some("shell"), None, Some("git")),
                rule(Decision::Deny, None, None, Some("git push")),
                rule(Decision::Allow, Some("edit"), Some("src/**"), None),
                rule(Decision::Deny, Some("edit"), Some("**/*.lock"), None),
            ],
        };
        assert_eq!(
            p.decide(&shell("git status -s"), "/w"),
            Some((Decision::Allow, 0))
        );
        assert_eq!(p.decide(&shell("gitk"), "/w"), None);
        assert_eq!(
            p.decide(&shell("git push origin"), "/w"),
            Some((Decision::Deny, 1))
        );
        assert_eq!(p.decide(&shell("git status && rm -rf /"), "/w"), None);
        assert_eq!(p.decide(&shell("ls; git push"), "/w"), Some((Decision::Deny, 1)));
        assert_eq!(
            p.decide(&edit(&["/w/src/a.rs", "src/b/c.rs"]), "/w"),
            Some((Decision::Allow, 2))
        );
        assert_eq!(p.decide(&edit(&["/w/src/a.rs", "/w/README.md"]), "/w"), None);
        assert_eq!(
            p.decide(&edit(&["/w/src/Cargo.lock"]), "/w"),
            Some((Decision::Deny, 3))
        );
        // Tool names match the harness's own name too, case-insensitively.
        let by_name = Policy {
            rules: vec![rule(Decision::Allow, Some("webfetch"), None, None)],
        };
        let fetch = Request {
            tool: Some("WebFetch".into()),
            kind: Some(ToolKind::Fetch),
            ..Request::default()
        };
        assert!(by_name.decide(&fetch, "/w").is_some());
        assert!(by_name.decide(&shell("ls"), "/w").is_none());
    }

    #[test]
    fn requests_read_the_request_detail() {
        use crate::event::ItemKind;
        use crate::item::ItemFeed;
        let codex = ItemFeed::new(Harness::Codex).preview(
            None,
            ItemKind::CommandExecution,
            &json!({"type": "commandExecution", "command": "cargo test", "cwd": "/w"}),
        );
        let r = Request::new(None, Some(&codex));
        assert_eq!(
            (r.kind, r.command.as_deref()),
            (Some(ToolKind::Shell), Some("cargo test"))
        );
        let tool = ItemDetail::Tool {
            tool: "exec".into(),
            kind: ToolKind::Other,
            title: String::new(),
            input: json!({"command": ["bash", "-lc", "ls -la"]}),
            output: None,
            paths: vec![],
        };
        let r = Request::new(None, Some(&tool));
        assert_eq!(
            (r.kind, r.command.as_deref()),
            (Some(ToolKind::Shell), Some("ls -la"))
        );
        let claude = ItemFeed::new(Harness::Claude).preview(
            None,
            ItemKind::ToolCall,
            &json!({"name": "Read", "input": {"file_path": "/w/a.rs"}}),
        );
        let r = Request::new(None, Some(&claude));
        assert_eq!(
            (r.kind, r.paths.as_slice()),
            (Some(ToolKind::Read), &["/w/a.rs".to_owned()][..])
        );
    }

    #[test]
    fn requests_read_the_typed_item() {
        let mut feed = crate::item::ItemFeed::new(Harness::Cursor);
        let mut t = crate::transcript::Transcript::new();
        let mut out = Vec::new();
        feed.start(
            "t1",
            crate::event::ItemKind::ToolCall,
            None,
            json!({"toolCallId": "t1", "kind": "read", "title": "Read a.rs",
                "locations": [{"path": "/w/a.rs"}]}),
            &mut out,
        );
        for e in &out {
            t.apply(e);
        }
        let r = Request::new(t.get("t1"), None);
        assert_eq!(r.tool.as_deref(), Some("read"));
        assert_eq!(r.kind, Some(ToolKind::Read));
        assert_eq!(r.paths, ["/w/a.rs"]);
    }

    #[test]
    fn options_by_kind() {
        let opts = [
            PermissionOption::new("a", "Allow").with_kind(PermissionOptionKind::AllowOnce),
            PermissionOption::new("x", "Always").with_kind(PermissionOptionKind::AllowAlways),
            PermissionOption::new("r", "Reject").with_kind(PermissionOptionKind::RejectAlways),
        ];
        assert_eq!(
            option_for(Decision::Allow, &opts).map(|o| o.id.as_str()),
            Some("a")
        );
        assert_eq!(
            option_for(Decision::Deny, &opts).map(|o| o.id.as_str()),
            Some("r")
        );
        assert!(option_for(Decision::Allow, &opts[1..]).is_none());
    }

    #[test]
    fn settings_merge_per_harness() {
        let rules = [
            rule(Decision::Allow, None, None, Some("npm test")),
            rule(Decision::Allow, Some("edit"), Some("src/**"), None),
            rule(Decision::Deny, Some("WebFetch"), None, None),
        ];
        let (claude, skipped) = merge_settings(
            Harness::Claude,
            r#"{"model":"opus","permissions":{"allow":["Bash(npm test:*)"]}}"#,
            &rules,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&claude).unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(
            v["permissions"]["allow"],
            json!(["Bash(npm test:*)", "Edit(src/**)"])
        );
        assert_eq!(v["permissions"]["deny"], json!(["WebFetch"]));
        assert!(skipped.is_empty());

        let (codex, skipped) = merge_settings(Harness::Codex, "# mine\n", &rules).unwrap();
        assert_eq!(
            codex,
            "# mine\nprefix_rule(pattern = [\"npm\", \"test\"], decision = \"allow\")\n"
        );
        assert_eq!(skipped, [1, 2]);
        let (again, _) = merge_settings(Harness::Codex, &codex, &rules).unwrap();
        assert_eq!(again, codex);

        let (cursor, _) = merge_settings(Harness::Cursor, "", &rules).unwrap();
        let v: Value = serde_json::from_str(&cursor).unwrap();
        assert_eq!(
            v["permissions"]["allow"],
            json!(["Shell(npm test)", "Write(src/**)"])
        );

        let (oc, skipped) =
            merge_settings(Harness::Opencode, r#"{"permission":{"bash":"ask"}}"#, &rules).unwrap();
        let v: Value = serde_json::from_str(&oc).unwrap();
        assert_eq!(v["permission"]["bash"]["*"], "ask");
        assert_eq!(v["permission"]["bash"]["npm test *"], "allow");
        assert_eq!(v["permission"]["webfetch"], "deny");
        assert_eq!(skipped, [1]);
        assert!(matches!(
            merge_settings(Harness::Opencode, "{ // mine\n}", &rules),
            Err(SettingsError::Unreadable(_))
        ));
        assert_eq!(
            merge_settings(Harness::Pi, "", &rules),
            Err(SettingsError::Unsupported(Harness::Pi))
        );
        assert!(settings_read_script(Harness::Copilot, "/w").is_none());
        assert!(
            settings_write_script(Harness::Claude, "~/p")
                .unwrap()
                .contains("\"$HOME\"/p/.claude/settings.local.json")
        );
    }
}

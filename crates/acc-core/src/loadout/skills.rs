//! The skill store and every harness's view of it.
//!
//! [`list_script`] prints the store (each skill's front matter and files,
//! the `skills` CLI lock file) and every harness's skills directory with
//! what is in it; [`parse_listing`] reads that into a [`Listing`]. The
//! other scripts change one thing each and exit [`EXIT_NO_SKILL`] or
//! [`EXIT_CONFLICT`] when they can't.

use serde::{Deserialize, Serialize};

use super::json::{self, Json};
use super::{LoadoutError, Scope, SkillPlace, quoted};
use crate::harness::Harness;

/// Exit code when the skill (or the directory to adopt) isn't there.
pub const EXIT_NO_SKILL: u32 = 9;
/// Exit code when something else is in the way (a real directory or a
/// symlink elsewhere where the link would go, a store entry with the
/// adopted skill's name).
pub const EXIT_CONFLICT: u32 = 8;

pub const MAX_NAME: usize = 64;
pub const MAX_DESCRIPTION: usize = 1024;

/// Why `name` can't name a skill, or `None`: lowercase letters, digits and
/// hyphens, no leading, trailing or doubled hyphen, at most 64 characters.
pub fn name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("name is empty".into());
    }
    if name.len() > MAX_NAME {
        return Some(format!("name is longer than {MAX_NAME} characters"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Some("name may only contain lowercase letters, digits and hyphens".into());
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return Some("name may not start or end with a hyphen or contain '--'".into());
    }
    None
}

fn checked(name: &str) -> Result<String, LoadoutError> {
    match name_problem(name) {
        Some(problem) => Err(LoadoutError::InvalidName(problem)),
        None => Ok(quoted(name)),
    }
}

/// The YAML front matter of a `SKILL.md`: `name`, `description` and
/// whatever else the author added, in order. Deliberately simple
/// (`key: value`, `|`/`>` blocks and indented continuations), which is all
/// the skill spec uses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frontmatter {
    pub fields: Vec<(String, String)>,
}

impl Frontmatter {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        match self.fields.iter_mut().find(|(k, _)| k == key) {
            Some(field) => field.1 = value,
            None => self.fields.push((key.to_owned(), value)),
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.get("name")
    }

    pub fn description(&self) -> Option<&str> {
        self.get("description")
    }

    /// Splits a `SKILL.md` into front matter and body. Without a leading
    /// `---` block the front matter is empty and the body the whole text.
    pub fn parse(markdown: &str) -> (Self, String) {
        let lines: Vec<&str> = markdown.split('\n').collect();
        if lines.first().map(|l| l.trim()) != Some("---") {
            return (Self::default(), markdown.to_owned());
        }
        let Some(close) = lines
            .iter()
            .skip(1)
            .position(|l| l.trim() == "---")
            .map(|i| i + 1)
        else {
            return (Self::default(), markdown.to_owned());
        };
        let fm = Self::parse_block(&lines[1..close]);
        (fm, lines[close + 1..].join("\n"))
    }

    /// The fields of a front matter block (the lines between the `---`s).
    pub(crate) fn parse_block(lines: &[&str]) -> Self {
        let mut fields = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            i += 1;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if line.starts_with(char::is_whitespace) {
                continue;
            }
            let Some(colon) = line.find(':') else { continue };
            let key = line[..colon].trim().to_owned();
            let mut value = line[colon + 1..].trim().to_owned();
            let block = matches!(value.as_str(), "|" | ">" | "|-" | ">-" | "");
            let mut continuation = Vec::new();
            while i < lines.len()
                && (lines[i].starts_with(char::is_whitespace) || (block && lines[i].is_empty()))
            {
                continuation.push(lines[i].trim());
                i += 1;
            }
            if block {
                let folded = value.starts_with('>');
                value = continuation
                    .join(if folded { " " } else { "\n" })
                    .trim()
                    .to_owned();
            } else if !continuation.is_empty() {
                value = std::iter::once(value.as_str())
                    .chain(continuation.iter().copied())
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            fields.push((key, unquote(&value)));
        }
        Self { fields }
    }

    /// The block as text, quoting values YAML would misread.
    pub fn render(&self) -> String {
        let mut out = String::from("---\n");
        for (key, value) in &self.fields {
            let needs_quote = value.contains(": ")
                || value.contains(" #")
                || value.starts_with(['"', '\'', '[', '{'])
                || value.contains('\n');
            if needs_quote {
                let escaped = value
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', " ");
                out.push_str(&format!("{key}: \"{escaped}\"\n"));
            } else {
                out.push_str(&format!("{key}: {value}\n"));
            }
        }
        out.push_str("---\n");
        out
    }
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if value.len() >= 2 && bytes[0] == bytes[value.len() - 1] && (bytes[0] == b'"' || bytes[0] == b'\'') {
        let inner = &value[1..value.len() - 1];
        return if bytes[0] == b'"' {
            inner.replace("\\\"", "\"")
        } else {
            inner.replace("''", "'")
        };
    }
    value.to_owned()
}

/// A validation finding for a skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub error: bool,
    pub message: String,
}

fn issue(error: bool, message: impl Into<String>) -> Issue {
    Issue {
        error,
        message: message.into(),
    }
}

/// What is wrong with the skill `name` whose `SKILL.md` is `markdown`.
pub fn issues(name: &str, markdown: &str) -> Vec<Issue> {
    let (fm, body) = Frontmatter::parse(markdown);
    let mut out = Vec::new();
    match fm.name() {
        Some(n) if n != name => out.push(issue(
            true,
            format!("front matter name {n:?} does not match directory {name:?}"),
        )),
        Some(_) => {}
        None => out.push(issue(true, "missing 'name' in front matter")),
    }
    if let Some(problem) = name_problem(name) {
        out.push(issue(true, problem));
    }
    match fm.description() {
        Some(d) if !d.is_empty() => {
            let n = d.chars().count();
            if n > MAX_DESCRIPTION {
                out.push(issue(
                    true,
                    format!("description is {n} characters (max {MAX_DESCRIPTION})"),
                ));
            }
        }
        _ => out.push(issue(true, "missing 'description' in front matter")),
    }
    if body.trim().is_empty() {
        out.push(issue(false, "SKILL.md has no body"));
    }
    let n = body.chars().count();
    if n > 20_000 {
        out.push(issue(
            false,
            format!("SKILL.md body is long ({n} characters); consider moving detail into supporting files"),
        ));
    }
    out
}

/// A `SKILL.md` for a new skill.
pub fn template(name: &str, description: &str) -> String {
    let mut fm = Frontmatter::default();
    fm.set("name", name);
    fm.set("description", description);
    let title: Vec<String> = name
        .split('-')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect();
    format!(
        "{}\n# {}\n\n## When to use\n\n## Steps\n\n",
        fm.render(),
        title.join(" ")
    )
}

/// Where a skill came from, as the `skills` CLI lock file records it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
}

/// How a harness sees one store skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LinkState {
    /// A symlink into the store.
    Linked,
    /// The harness reads the store itself.
    Native,
    /// Not there for this harness.
    Missing,
    /// Something else is in the way: a real directory or a symlink elsewhere.
    Foreign { what: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub harness: Harness,
    pub state: LinkState,
}

/// One skill in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub frontmatter: Frontmatter,
    /// Supporting files, relative to the skill directory (at most 200).
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// One entry per harness that has a skills directory at this scope.
    pub placements: Vec<Placement>,
}

impl Skill {
    pub fn description(&self) -> &str {
        self.frontmatter.description().unwrap_or("")
    }
}

/// A harness's skills directory at this scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    pub harness: Harness,
    /// Resolved on the host; empty when the directory doesn't exist yet.
    pub dir: String,
    pub native: bool,
    /// Skills living only in this directory (real directories with a
    /// `SKILL.md`): candidates for [`adopt_script`].
    pub unmanaged: Vec<String>,
}

/// What [`list_script`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    /// The store directory resolved on the host; empty when it doesn't exist.
    pub store: String,
    pub skills: Vec<Skill>,
    pub places: Vec<Place>,
}

fn places(scope: &Scope) -> Vec<(Harness, SkillPlace)> {
    Harness::ALL
        .into_iter()
        .filter_map(|h| super::skill_place(h, scope).map(|p| (h, p)))
        .collect()
}

/// Script listing the store and every harness's skills directory at
/// `scope`, for [`parse_listing`].
pub fn list_script(scope: &Scope) -> String {
    let mut s = format!(
        "store={}\n\
         lock={}\n\
         printf 'store\\t%s\\n' \"$(cd \"$store\" 2>/dev/null && pwd -P)\"\n\
         if [ -d \"$store\" ]; then for d in \"$store\"/*/; do\n\
         \x20 d=${{d%/}}; n=${{d##*/}}\n\
         \x20 [ -f \"$d/SKILL.md\" ] || continue\n\
         \x20 printf 'skill\\t%s\\n' \"$n\"\n\
         \x20 awk 'NR==1{{if($0!=\"---\")exit; next}} $0==\"---\"{{exit}} {{print \"f\\t\" $0}}' \"$d/SKILL.md\"\n\
         \x20 (cd \"$d\" && find . -type f ! -name SKILL.md ! -path '*/.*' 2>/dev/null | head -n 200 | awk '{{sub(/^\\.\\//, \"\"); print \"file\\t\" $0}}')\n\
         done; fi\n\
         [ -f \"$lock\" ] && awk '{{print \"lock\\t\" $0}}' \"$lock\"\n",
        scope.store(),
        scope.lock_file()
    );
    for (h, place) in places(scope) {
        let kind = if place.native { "native" } else { "own" };
        s.push_str(&format!(
            "d={}\n\
             printf 'place\\t{id}\\t%s\\t{kind}\\n' \"$(cd \"$d\" 2>/dev/null && pwd -P)\"\n\
             if [ -d \"$d\" ]; then for e in \"$d\"/*; do\n\
             \x20 n=${{e##*/}}\n\
             \x20 if [ -L \"$e\" ]; then printf 'link\\t{id}\\t%s\\t%s\\n' \"$n\" \"$(cd \"$d\" && cd -P -- \"$(readlink \"$e\")\" 2>/dev/null && pwd -P)\"\n\
             \x20 elif [ -f \"$e/SKILL.md\" ]; then printf 'own\\t{id}\\t%s\\n' \"$n\"\n\
             \x20 elif [ -d \"$e\" ]; then printf 'dir\\t{id}\\t%s\\n' \"$n\"; fi\n\
             done; fi\n",
            place.dir,
            id = h.id(),
        ));
    }
    s.push_str("true\n");
    s
}

fn harness_by_id(id: &str) -> Option<Harness> {
    Harness::ALL.into_iter().find(|h| h.id() == id)
}

/// Reads [`list_script`]'s output.
pub fn parse_listing(stdout: &str, scope: &Scope) -> Listing {
    let mut listing = Listing::default();
    let mut lock = String::new();
    let mut links: Vec<(Harness, String, String)> = Vec::new();
    let mut dirs: Vec<(Harness, String)> = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.splitn(4, '\t');
        let tag = parts.next().unwrap_or("");
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        let c = parts.next().unwrap_or("");
        match tag {
            "store" => listing.store = a.to_owned(),
            "skill" => listing.skills.push(Skill {
                name: a.to_owned(),
                frontmatter: Frontmatter::default(),
                files: vec![],
                source: None,
                placements: vec![],
            }),
            "f" | "file" => {
                if let Some(skill) = listing.skills.last_mut() {
                    // The rest of the line, tabs included.
                    let rest = &line[tag.len() + 1..];
                    if tag == "f" {
                        skill.frontmatter.fields.push((String::new(), rest.to_owned()));
                    } else {
                        skill.files.push(rest.to_owned());
                    }
                }
            }
            "lock" => {
                lock.push_str(&line[5..]);
                lock.push('\n');
            }
            "place" => {
                if let Some(h) = harness_by_id(a) {
                    listing.places.push(Place {
                        harness: h,
                        dir: b.to_owned(),
                        native: c == "native",
                        unmanaged: vec![],
                    });
                }
            }
            "link" => {
                if let Some(h) = harness_by_id(a) {
                    links.push((h, b.to_owned(), c.to_owned()));
                }
            }
            "own" => {
                if let Some(place) = listing.places.iter_mut().find(|p| p.harness.id() == a) {
                    place.unmanaged.push(b.to_owned());
                }
                if let Some(h) = harness_by_id(a) {
                    dirs.push((h, b.to_owned()));
                }
            }
            "dir" => {
                if let Some(h) = harness_by_id(a) {
                    dirs.push((h, b.to_owned()));
                }
            }
            _ => {}
        }
    }
    // Front matter lines were collected raw; parse them now.
    for skill in &mut listing.skills {
        let raw: Vec<String> = skill.frontmatter.fields.drain(..).map(|(_, v)| v).collect();
        let lines: Vec<&str> = raw.iter().map(String::as_str).collect();
        skill.frontmatter = Frontmatter::parse_block(&lines);
    }
    let sources = parse_lock(&lock);
    let store = listing.store.clone();
    for skill in &mut listing.skills {
        skill.source = sources
            .iter()
            .find(|(n, _)| *n == skill.name)
            .map(|(_, s)| s.clone());
        for place in &listing.places {
            let entry = format!("{store}/{}", skill.name);
            let state = if place.native && (place.dir.is_empty() || place.dir == store) {
                LinkState::Native
            } else if let Some((_, _, target)) = links
                .iter()
                .find(|(h, n, _)| *h == place.harness && *n == skill.name)
            {
                if !store.is_empty() && *target == entry {
                    LinkState::Linked
                } else if target.is_empty() {
                    LinkState::Foreign {
                        what: "broken symlink".into(),
                    }
                } else {
                    LinkState::Foreign {
                        what: format!("symlink to {target}"),
                    }
                }
            } else if dirs.iter().any(|(h, n)| *h == place.harness && *n == skill.name) {
                LinkState::Foreign {
                    what: "real directory".into(),
                }
            } else if place.native {
                LinkState::Native
            } else {
                LinkState::Missing
            };
            skill.placements.push(Placement {
                harness: place.harness,
                state,
            });
        }
    }
    let _ = scope;
    listing
}

fn parse_lock(text: &str) -> Vec<(String, Source)> {
    let Ok(v) = json::parse(text) else {
        return vec![];
    };
    let Some(skills) = v.get("skills").and_then(Json::as_object) else {
        return vec![];
    };
    skills
        .iter()
        .map(|(name, entry)| {
            let s = |k: &str| entry.get(k).and_then(Json::as_str).map(str::to_owned);
            (
                name.clone(),
                Source {
                    source: s("source").unwrap_or_default(),
                    url: s("sourceUrl"),
                    path: s("skillPath"),
                    installed_at: s("installedAt"),
                },
            )
        })
        .collect()
}

/// Script printing the store skill's `SKILL.md`; exits [`EXIT_NO_SKILL`].
pub fn read_script(scope: &Scope, name: &str) -> Result<String, LoadoutError> {
    let name = checked(name)?;
    Ok(format!(
        "f={}/{name}/SKILL.md\n[ -f \"$f\" ] || exit {EXIT_NO_SKILL}\ncat \"$f\"\n",
        scope.store()
    ))
}

/// Script writing its stdin as the store skill's `SKILL.md` (making the
/// skill when it is new).
pub fn write_script(scope: &Scope, name: &str) -> Result<String, LoadoutError> {
    let name = checked(name)?;
    Ok(format!(
        "d={}/{name}\nmkdir -p \"$d\" || exit 1\ncat >\"$d/SKILL.md.acc.$$\" && mv \"$d/SKILL.md.acc.$$\" \"$d/SKILL.md\"\n",
        scope.store()
    ))
}

/// Script deleting the store skill and every harness's link to it.
pub fn remove_script(scope: &Scope, name: &str) -> Result<String, LoadoutError> {
    let q = checked(name)?;
    let mut s = format!("store={}\n", scope.store());
    for (_, place) in places(scope) {
        if !place.native {
            s.push_str(&format!("e={}/{q}\n[ -L \"$e\" ] && rm -f \"$e\"\n", place.dir));
        }
    }
    s.push_str(&format!(
        "[ -d \"$store\"/{q} ] || exit {EXIT_NO_SKILL}\nrm -rf \"$store\"/{q}\n"
    ));
    Ok(s)
}

/// `rel FROM TO` prints `TO` relative to the directory `FROM` (both
/// absolute), or `TO` itself when they share nothing but `/`.
const REL: &str = "rel() {\n\
  \x20 f=$1; t=$2; up=''\n\
  \x20 while [ \"${t#\"$f\"/}\" = \"$t\" ] && [ \"$f\" != / ]; do f=${f%/*}; [ -z \"$f\" ] && f=/; up=\"../$up\"; done\n\
  \x20 if [ \"$f\" = / ]; then printf '%s\\n' \"$t\"; else printf '%s%s\\n' \"$up\" \"${t#\"$f\"/}\"; fi\n\
}\n";

/// Script linking the store skill into each of `harnesses`' skills
/// directories (relative symlinks). Harnesses that read the store and
/// links already there are skipped. Exits [`EXIT_CONFLICT`] when something
/// else is in the way, unless `force` replaces it; [`EXIT_NO_SKILL`] when
/// the skill isn't in the store.
pub fn link_script(
    scope: &Scope,
    name: &str,
    harnesses: &[Harness],
    force: bool,
) -> Result<String, LoadoutError> {
    let q = checked(name)?;
    let mut s = format!(
        "{REL}store=$(cd {} 2>/dev/null && pwd -P) || exit {EXIT_NO_SKILL}\n\
         [ -f \"$store\"/{q}/SKILL.md ] || exit {EXIT_NO_SKILL}\n",
        scope.store()
    );
    for h in harnesses {
        let Some(place) = super::skill_place(*h, scope) else {
            continue;
        };
        if place.native {
            continue;
        }
        let replace = if force {
            "rm -rf \"$e\""
        } else {
            "printf '%s: %s is in the way\\n' DISPLAY \"$e\" >&2; exit CONFLICT"
        }
        .replace("DISPLAY", h.id())
        .replace("CONFLICT", &EXIT_CONFLICT.to_string());
        s.push_str(&format!(
            "mkdir -p {dir} || exit 1\n\
             d=$(cd {dir} && pwd -P); e=\"$d\"/{q}\n\
             if [ -L \"$e\" ] && [ \"$(cd \"$d\" && cd -P -- \"$(readlink \"$e\")\" 2>/dev/null && pwd -P)\" = \"$store\"/{q} ]; then :\n\
             else\n\
             \x20 if [ -e \"$e\" ] || [ -L \"$e\" ]; then {replace}; fi\n\
             \x20 ln -s \"$(rel \"$d\" \"$store\"/{q})\" \"$e\" || exit 1\n\
             fi\n",
            dir = place.dir,
        ));
    }
    Ok(s)
}

/// Script removing each of `harnesses`' links to the store skill. Only
/// symlinks into the store are touched.
pub fn unlink_script(scope: &Scope, name: &str, harnesses: &[Harness]) -> Result<String, LoadoutError> {
    let q = checked(name)?;
    let mut s = format!("store=$(cd {} 2>/dev/null && pwd -P)\n", scope.store());
    for h in harnesses {
        let Some(place) = super::skill_place(*h, scope) else {
            continue;
        };
        if place.native {
            continue;
        }
        s.push_str(&format!(
            "d=$(cd {dir} 2>/dev/null && pwd -P); e=\"$d\"/{q}\n\
             if [ -n \"$d\" ] && [ -L \"$e\" ] && [ \"$(cd \"$d\" && cd -P -- \"$(readlink \"$e\")\" 2>/dev/null && pwd -P)\" = \"$store\"/{q} ]; then rm -f \"$e\"; fi\n",
            dir = place.dir,
        ));
    }
    s.push_str("true\n");
    Ok(s)
}

/// Script moving a skill that lives only in `harness`'s skills directory
/// into the store and leaving a link behind. Exits [`EXIT_NO_SKILL`] when
/// it isn't a real directory there, [`EXIT_CONFLICT`] when the store
/// already has the name, or without doing anything when the harness reads
/// the store itself.
pub fn adopt_script(scope: &Scope, harness: Harness, name: &str) -> Result<String, LoadoutError> {
    let q = checked(name)?;
    let place = super::skill_place(harness, scope)
        .ok_or_else(|| LoadoutError::Unsupported(format!("{} has no skills directory here", harness.id())))?;
    let store = scope.store();
    let link = if place.native {
        String::new()
    } else {
        link_script(scope, name, &[harness], false)?
    };
    Ok(format!(
        "src={dir}/{q}\n\
         [ -d \"$src\" ] && [ ! -L \"$src\" ] && [ -f \"$src/SKILL.md\" ] || exit {EXIT_NO_SKILL}\n\
         mkdir -p {store} || exit 1\n\
         [ -e {store}/{q} ] && exit {EXIT_CONFLICT}\n\
         mv \"$src\" {store}/{q} || exit 1\n\
         {link}",
        dir = place.dir,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn front_matter() {
        let md = "---\nname: tdd\ndescription: >\n  Red, green,\n  refactor.\nextra: \"a: b\"\n---\n# TDD\n\nBody\n";
        let (fm, body) = Frontmatter::parse(md);
        assert_eq!(fm.name(), Some("tdd"));
        assert_eq!(fm.description(), Some("Red, green, refactor."));
        assert_eq!(fm.get("extra"), Some("a: b"));
        assert_eq!(body, "# TDD\n\nBody\n");
        assert_eq!(
            fm.render(),
            "---\nname: tdd\ndescription: Red, green, refactor.\nextra: \"a: b\"\n---\n"
        );
        let (none, whole) = Frontmatter::parse("no front matter");
        assert!(none.fields.is_empty() && whole == "no front matter");
    }

    #[test]
    fn validation() {
        assert!(name_problem("tdd-2").is_none());
        assert!(
            name_problem("TDD").is_some() && name_problem("-a").is_some() && name_problem("a--b").is_some()
        );
        let bad = issues("tdd", "---\nname: other\n---\n");
        assert!(bad.iter().any(|i| i.message.contains("does not match")));
        assert!(bad.iter().any(|i| i.message.contains("description")));
        assert!(bad.iter().any(|i| !i.error && i.message.contains("no body")));
        assert!(issues("tdd", &template("tdd", "Test first")).is_empty());
        assert!(matches!(
            read_script(&Scope::User, "Bad"),
            Err(LoadoutError::InvalidName(_))
        ));
    }

    #[test]
    fn listing_is_read_back() {
        let out = "store\t/h/.agents/skills\nskill\ttdd\nf\tname: tdd\nf\tdescription: Test first\nfile\tref/a.md\n\
                   lock\t{\"skills\": {\"tdd\": {\"source\": \"gh/x\", \"installedAt\": \"2026\"}}}\n\
                   place\tclaude-code\t/h/.claude/skills\town\nlink\tclaude-code\ttdd\t/h/.agents/skills/tdd\n\
                   place\tcodex\t/h/.codex/skills\town\nlink\tcodex\ttdd\t/elsewhere/tdd\nown\tcodex\tmine\n\
                   place\tcursor-agent\t\town\nplace\tpi\t/h/.pi/agent/skills\tnative\ndir\tpi\ttdd\n";
        let l = parse_listing(out, &Scope::User);
        assert_eq!(l.store, "/h/.agents/skills");
        let tdd = &l.skills[0];
        assert_eq!(tdd.description(), "Test first");
        assert_eq!(tdd.files, ["ref/a.md"]);
        assert_eq!(tdd.source.as_ref().unwrap().source, "gh/x");
        let state = |id: &str| {
            &tdd.placements
                .iter()
                .find(|p| p.harness.id() == id)
                .unwrap()
                .state
        };
        assert_eq!(state("claude-code"), &LinkState::Linked);
        assert!(matches!(state("codex"), LinkState::Foreign { what } if what == "symlink to /elsewhere/tdd"));
        assert_eq!(state("cursor-agent"), &LinkState::Missing);
        assert!(matches!(state("pi"), LinkState::Foreign { what } if what == "real directory"));
        assert_eq!(
            l.places
                .iter()
                .find(|p| p.harness == Harness::Codex)
                .unwrap()
                .unmanaged,
            ["mine"]
        );
        let native = parse_listing(
            "store\t/h/.agents/skills\nskill\ta\nplace\tpi\t/h/.pi/agent/skills\tnative\n",
            &Scope::User,
        );
        assert_eq!(native.skills[0].placements[0].state, LinkState::Native);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod sh_tests {
    use super::*;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    fn home(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("acc-loadout-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn sh(home: &Path, script: &str, stdin: &[u8]) -> (String, Option<i32>) {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("HOME", home)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.code(),
        )
    }

    #[test]
    fn store_lifecycle_on_a_host() {
        let home = home("store");
        let scope = Scope::User;
        let md = template("tdd", "Test first");
        assert_eq!(
            sh(&home, &write_script(&scope, "tdd").unwrap(), md.as_bytes()).1,
            Some(0)
        );
        std::fs::write(home.join(".agents/skills/tdd/notes.md"), "n").unwrap();
        let (read, code) = sh(&home, &read_script(&scope, "tdd").unwrap(), b"");
        assert_eq!((read.as_str(), code), (md.as_str(), Some(0)));
        assert_eq!(
            sh(&home, &read_script(&scope, "nope").unwrap(), b"").1,
            Some(EXIT_NO_SKILL as i32)
        );

        // Link into Claude and Codex; a real directory blocks Cursor until forced.
        std::fs::create_dir_all(home.join(".cursor/skills/tdd")).unwrap();
        let all = [Harness::Claude, Harness::Codex, Harness::Cursor, Harness::Pi];
        let (_, code) = sh(&home, &link_script(&scope, "tdd", &all, false).unwrap(), b"");
        assert_eq!(code, Some(EXIT_CONFLICT as i32));
        let claude = home.join(".claude/skills/tdd");
        assert_eq!(
            std::fs::read_link(&claude).unwrap(),
            Path::new("../../.agents/skills/tdd")
        );
        assert!(claude.join("SKILL.md").is_file());
        let (_, code) = sh(&home, &link_script(&scope, "tdd", &all, true).unwrap(), b"");
        assert_eq!(code, Some(0));
        assert!(std::fs::read_link(home.join(".cursor/skills/tdd")).is_ok());
        assert!(
            !home.join(".pi/agent/skills/tdd").exists(),
            "pi reads the store itself"
        );

        let (out, _) = sh(&home, &list_script(&scope), b"");
        let l = parse_listing(&out, &scope);
        assert_eq!(l.skills.len(), 1);
        let tdd = &l.skills[0];
        assert_eq!(tdd.frontmatter.name(), Some("tdd"));
        assert_eq!(tdd.files, ["notes.md"]);
        let state = |h: Harness| &tdd.placements.iter().find(|p| p.harness == h).unwrap().state;
        assert_eq!(state(Harness::Claude), &LinkState::Linked);
        assert_eq!(state(Harness::Codex), &LinkState::Linked);
        assert_eq!(state(Harness::Cursor), &LinkState::Linked);
        assert_eq!(state(Harness::Copilot), &LinkState::Missing);
        assert_eq!(state(Harness::Pi), &LinkState::Native);

        // Unlink only touches links into the store.
        sh(
            &home,
            &unlink_script(&scope, "tdd", &[Harness::Codex]).unwrap(),
            b"",
        );
        assert!(!home.join(".codex/skills/tdd").exists());
        assert!(home.join(".claude/skills/tdd").exists());

        // Adopt a skill that lives only in Copilot's directory.
        std::fs::create_dir_all(home.join(".copilot/skills/mine")).unwrap();
        std::fs::write(home.join(".copilot/skills/mine/SKILL.md"), template("mine", "d")).unwrap();
        let (out, _) = sh(&home, &list_script(&scope), b"");
        let l = parse_listing(&out, &scope);
        assert_eq!(
            l.places
                .iter()
                .find(|p| p.harness == Harness::Copilot)
                .unwrap()
                .unmanaged,
            ["mine"]
        );
        let (_, code) = sh(
            &home,
            &adopt_script(&scope, Harness::Copilot, "mine").unwrap(),
            b"",
        );
        assert_eq!(code, Some(0));
        assert!(home.join(".agents/skills/mine/SKILL.md").is_file());
        assert!(std::fs::read_link(home.join(".copilot/skills/mine")).is_ok());
        let (_, code) = sh(
            &home,
            &adopt_script(&scope, Harness::Copilot, "mine").unwrap(),
            b"",
        );
        assert_eq!(code, Some(EXIT_NO_SKILL as i32));

        // Remove takes the links with it.
        let (_, code) = sh(&home, &remove_script(&scope, "tdd").unwrap(), b"");
        assert_eq!(code, Some(0));
        assert!(!home.join(".agents/skills/tdd").exists());
        assert!(std::fs::symlink_metadata(home.join(".claude/skills/tdd")).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn project_scope_links_relative_to_the_project() {
        let home = home("project");
        let project = home.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let scope = Scope::Project {
            dir: project.to_string_lossy().into_owned(),
        };
        sh(
            &home,
            &write_script(&scope, "rev").unwrap(),
            template("rev", "Review").as_bytes(),
        );
        let (_, code) = sh(
            &home,
            &link_script(&scope, "rev", &[Harness::Claude, Harness::Codex], false).unwrap(),
            b"",
        );
        assert_eq!(code, Some(0));
        assert_eq!(
            std::fs::read_link(project.join(".claude/skills/rev")).unwrap(),
            Path::new("../../.agents/skills/rev")
        );
        let (out, _) = sh(&home, &list_script(&scope), b"");
        let l = parse_listing(&out, &scope);
        let rev = &l.skills[0];
        let state = |h: Harness| &rev.placements.iter().find(|p| p.harness == h).unwrap().state;
        assert_eq!(state(Harness::Claude), &LinkState::Linked);
        assert_eq!(state(Harness::Codex), &LinkState::Native);
        assert_eq!(state(Harness::Opencode), &LinkState::Native);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn mcp_files_on_a_host() {
        use super::super::mcp;
        let home = home("mcp");
        let scope = Scope::User;
        std::fs::write(home.join(".claude.json"), "{\n  \"theme\": \"dark\"\n}\n").unwrap();
        let (text, _) = sh(&home, &mcp::read_script(Harness::Claude, &scope).unwrap(), b"");
        let server = mcp::McpServer {
            name: "x".into(),
            transport: mcp::McpTransport::Http {
                url: "https://x/mcp".into(),
                headers: vec![],
            },
            enabled: true,
            extras: Json::Object(vec![]),
        };
        let edited = mcp::upsert(Harness::Claude, &text, &server).unwrap();
        let (_, code) = sh(
            &home,
            &mcp::write_script(Harness::Claude, &scope).unwrap(),
            edited.as_bytes(),
        );
        assert_eq!(code, Some(0));
        assert_eq!(
            std::fs::read_to_string(home.join(".claude.json")).unwrap(),
            edited
        );
        let backups: Vec<_> = std::fs::read_dir(home.join(".acc/backups")).unwrap().collect();
        assert_eq!(backups.len(), 1);
        // A file that doesn't exist yet reads as empty and is created.
        let (text, _) = sh(&home, &mcp::read_script(Harness::Codex, &scope).unwrap(), b"");
        assert_eq!(text, "");
        let toml = mcp::upsert(Harness::Codex, &text, &server).unwrap();
        sh(
            &home,
            &mcp::write_script(Harness::Codex, &scope).unwrap(),
            toml.as_bytes(),
        );
        assert_eq!(
            std::fs::read_to_string(home.join(".codex/config.toml")).unwrap(),
            toml
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}

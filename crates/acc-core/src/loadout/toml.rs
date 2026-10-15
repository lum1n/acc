//! A line-oriented TOML editor for Codex's `config.toml`.
//!
//! It knows table headers and the value subset `[mcp_servers.*]` uses, and
//! rewrites only the tables it is asked to change; every other line stays
//! byte for byte (Codex keeps a lot of unrelated state in the same file).

use super::json::Json;

/// The TOML values harness configs use.
#[derive(Debug, Clone, PartialEq)]
pub enum Toml {
    String(String),
    Bool(bool),
    Integer(i64),
    Float(f64),
    Array(Vec<Toml>),
    Table(Vec<(String, Toml)>),
}

impl Toml {
    pub fn to_json(&self) -> Json {
        match self {
            Self::String(s) => Json::String(s.clone()),
            Self::Bool(b) => Json::Bool(*b),
            Self::Integer(i) => Json::Number(i.to_string()),
            Self::Float(f) => Json::Number(f.to_string()),
            Self::Array(a) => Json::Array(a.iter().map(Self::to_json).collect()),
            Self::Table(t) => Json::Object(t.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()),
        }
    }

    /// `None` for `null`, which TOML can't say.
    pub fn from_json(json: &Json) -> Option<Self> {
        Some(match json {
            Json::Null => return None,
            Json::Bool(b) => Self::Bool(*b),
            Json::Number(n) => match (n.parse::<i64>(), n.parse::<f64>()) {
                (Ok(i), _) => Self::Integer(i),
                (_, Ok(f)) => Self::Float(f),
                _ => return None,
            },
            Json::String(s) => Self::String(s.clone()),
            Json::Array(a) => Self::Array(a.iter().filter_map(Self::from_json).collect()),
            Json::Object(o) => Self::Table(
                o.iter()
                    .filter_map(|(k, v)| Self::from_json(v).map(|v| (k.clone(), v)))
                    .collect(),
            ),
        })
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }
}

/// A `[table]`: its dotted name, entries and line range (header to last
/// non-blank line).
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: String,
    pub entries: Vec<(String, Toml)>,
    pub lines: std::ops::Range<usize>,
}

impl Table {
    pub fn get(&self, key: &str) -> Option<&Toml> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn to_json(&self) -> Json {
        Json::Object(
            self.entries
                .iter()
                .map(|(k, v)| (k.clone(), v.to_json()))
                .collect(),
        )
    }
}

#[derive(Debug, Clone)]
pub struct Document {
    lines: Vec<String>,
}

impl Document {
    pub fn parse(text: &str) -> Self {
        Self {
            lines: text.split('\n').map(str::to_owned).collect(),
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Every table, sub-tables included, in file order.
    pub fn tables(&self) -> Vec<Table> {
        let mut out = Vec::new();
        let mut open: Option<(String, usize)> = None;
        let close = |out: &mut Vec<Table>, open: &Option<(String, usize)>, boundary: usize| {
            let Some((name, start)) = open else { return };
            let mut end = boundary;
            while end > start + 1 && self.lines[end - 1].trim().is_empty() {
                end -= 1;
            }
            out.push(Table {
                name: name.clone(),
                entries: parse_entries(&self.lines[start + 1..end]),
                lines: *start..end,
            });
        };
        for (number, line) in self.lines.iter().enumerate() {
            if let Some(name) = table_header(line) {
                close(&mut out, &open, number);
                open = Some((name, number));
            }
        }
        close(&mut out, &open, self.lines.len());
        out
    }

    /// `[name]` with its direct sub-tables folded in (`[a.b.env]` → `env`).
    pub fn table(&self, name: &str) -> Option<Table> {
        let all = self.tables();
        let base = all.iter().find(|t| t.name == name)?;
        let mut entries = base.entries.clone();
        for sub in &all {
            if let Some(key) = sub.name.strip_prefix(&format!("{name}."))
                && !key.contains('.')
            {
                entries.push((key.to_owned(), Toml::Table(sub.entries.clone())));
            }
        }
        Some(Table {
            name: name.to_owned(),
            entries,
            lines: base.lines.clone(),
        })
    }

    /// The direct children of `prefix` (`mcp_servers` → every
    /// `[mcp_servers.x]`), sub-tables folded in.
    pub fn tables_under(&self, prefix: &str) -> Vec<Table> {
        let mut names: Vec<String> = Vec::new();
        for t in self.tables() {
            if let Some(child) = t.name.strip_prefix(&format!("{prefix}.")) {
                let direct = child.split('.').next().unwrap_or(child);
                let name = format!("{prefix}.{direct}");
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
            .iter()
            .map(|name| {
                self.table(name).unwrap_or(Table {
                    name: name.clone(),
                    entries: vec![],
                    lines: 0..0,
                })
            })
            .collect()
    }

    /// Replaces `[name]` in place (or appends it after its last sibling)
    /// with `entries`; nested tables become `[name.key]` sub-tables.
    pub fn set_table(&mut self, name: &str, entries: &[(String, Toml)]) {
        let mut block = vec![format!("[{}]", header_key(name))];
        let mut subtables = Vec::new();
        for (key, value) in entries {
            match value {
                Toml::Table(pairs) => subtables.push((key, pairs)),
                v => block.push(format!("{} = {}", bare_key(key), render(v))),
            }
        }
        for (key, pairs) in subtables {
            block.push(String::new());
            block.push(format!("[{}.{}]", header_key(name), bare_key(key)));
            for (k, v) in pairs {
                block.push(format!("{} = {}", bare_key(k), render(v)));
            }
        }
        let existing = self
            .tables()
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.lines.start);
        self.remove_table_keeping(name, existing.is_some());
        let mut insertion = block;
        let at = match existing {
            Some(at) => at,
            None => {
                let parent = name
                    .rsplit_once('.')
                    .map(|(p, _)| p.to_owned())
                    .unwrap_or_default();
                let siblings: Vec<Table> = self
                    .tables()
                    .into_iter()
                    .filter(|t| {
                        if parent.is_empty() {
                            !t.name.contains('.')
                        } else {
                            t.name.starts_with(&format!("{parent}."))
                        }
                    })
                    .collect();
                let at = match siblings.last() {
                    Some(last) => last.lines.end,
                    None => {
                        let mut at = self.lines.len();
                        while at > 0 && self.lines[at - 1].trim().is_empty() {
                            at -= 1;
                        }
                        at
                    }
                };
                if at > 0 {
                    insertion.insert(0, String::new());
                }
                at
            }
        };
        let tail = self.lines.split_off(at);
        self.lines.extend(insertion);
        self.lines.extend(tail);
        if self.lines.last().is_none_or(|l| !l.is_empty()) {
            self.lines.push(String::new());
        }
    }

    /// Removes `[name]` and every `[name.*]`, with one blank line before
    /// each so the spacing between the rest stays as it was.
    pub fn remove_table(&mut self, name: &str) {
        self.remove_table_keeping(name, false);
    }

    fn remove_table_keeping(&mut self, name: &str, keep_first_blank: bool) {
        let prefix = format!("{name}.");
        let doomed: Vec<Table> = self
            .tables()
            .into_iter()
            .filter(|t| t.name == name || t.name.starts_with(&prefix))
            .collect();
        let Some(first) = doomed.first() else { return };
        let mut deleted = std::collections::BTreeSet::new();
        for t in &doomed {
            deleted.extend(t.lines.clone());
        }
        for t in &doomed {
            if keep_first_blank && t.lines.start == first.lines.start {
                continue;
            }
            if t.lines.start > 0 {
                let before = t.lines.start - 1;
                if !deleted.contains(&before) && self.lines[before].trim().is_empty() {
                    deleted.insert(before);
                }
            }
        }
        let mut n = 0;
        self.lines.retain(|_| {
            let keep = !deleted.contains(&n);
            n += 1;
            keep
        });
    }
}

/// The dotted name of a `[table]` or `[[array]]` header line.
fn table_header(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let (inner, close) = match (trimmed.strip_prefix("[["), trimmed.strip_prefix('[')) {
        (Some(rest), _) => (rest, "]]"),
        (None, Some(rest)) => (rest, "]"),
        (None, None) => return None,
    };
    let end = inner.find(close)?;
    Some(dotted_key(&inner[..end]))
}

/// `a."b c".d` → `a.b c.d`
fn dotted_key(raw: &str) -> String {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in raw.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '.' => {
                parts.push(current.trim().to_owned());
                current.clear();
            }
            None => current.push(c),
        }
    }
    parts.push(current.trim().to_owned());
    parts.join(".")
}

fn parse_entries(body: &[String]) -> Vec<(String, Toml)> {
    let mut entries = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let line = body[i].trim();
        i += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = split_assignment(line) else {
            continue;
        };
        let mut text = value.to_owned();
        while !balanced(&text) && i < body.len() {
            text.push('\n');
            text.push_str(&body[i]);
            i += 1;
        }
        if let Some(v) = parse_value(&text) {
            entries.push((dotted_key(key), v));
        }
    }
    entries
}

fn split_assignment(line: &str) -> Option<(&str, &str)> {
    let mut quote: Option<char> = None;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '=' => return Some((line[..i].trim(), line[i + 1..].trim())),
            None => {}
        }
    }
    None
}

fn balanced(text: &str) -> bool {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut previous = ' ';
    for c in text.chars() {
        match quote {
            Some(q) if c == q && previous != '\\' => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                '#' => break,
                _ => {}
            },
        }
        previous = c;
    }
    depth <= 0 && quote.is_none()
}

pub(crate) fn parse_value(raw: &str) -> Option<Toml> {
    Scanner {
        chars: raw.chars().collect(),
        pos: 0,
    }
    .value()
}

struct Scanner {
    chars: Vec<char>,
    pos: usize,
}

impl Scanner {
    fn skip(&mut self) {
        while self.pos < self.chars.len() {
            match self.chars[self.pos] {
                c if c.is_whitespace() => self.pos += 1,
                '#' => {
                    while self.pos < self.chars.len() && self.chars[self.pos] != '\n' {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn value(&mut self) -> Option<Toml> {
        self.skip();
        let c = *self.chars.get(self.pos)?;
        match c {
            '"' | '\'' => self.string(c).map(Toml::String),
            '[' => Some(self.array()),
            '{' => Some(self.inline_table()),
            _ => {
                let mut token = String::new();
                while let Some(&c) = self.chars.get(self.pos) {
                    if c.is_whitespace() || matches!(c, ',' | ']' | '}' | '#') {
                        break;
                    }
                    token.push(c);
                    self.pos += 1;
                }
                match token.as_str() {
                    "" => None,
                    "true" => Some(Toml::Bool(true)),
                    "false" => Some(Toml::Bool(false)),
                    _ => {
                        let numeric = token.replace('_', "");
                        if let Ok(i) = numeric.parse::<i64>() {
                            Some(Toml::Integer(i))
                        } else if let Ok(f) = numeric.parse::<f64>() {
                            Some(Toml::Float(f))
                        } else {
                            Some(Toml::String(token))
                        }
                    }
                }
            }
        }
    }

    fn string(&mut self, quote: char) -> Option<String> {
        let triple =
            self.chars.get(self.pos + 1) == Some(&quote) && self.chars.get(self.pos + 2) == Some(&quote);
        self.pos += if triple { 3 } else { 1 };
        if triple && self.chars.get(self.pos) == Some(&'\n') {
            self.pos += 1;
        }
        let mut out = String::new();
        while let Some(&c) = self.chars.get(self.pos) {
            if c == quote {
                if !triple {
                    self.pos += 1;
                    return Some(out);
                }
                if self.chars.get(self.pos + 1) == Some(&quote)
                    && self.chars.get(self.pos + 2) == Some(&quote)
                {
                    self.pos += 3;
                    return Some(out);
                }
            }
            if c == '\\' && quote == '"' {
                self.pos += 1;
                match self.chars.get(self.pos) {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('\\') => out.push('\\'),
                    Some('"') => out.push('"'),
                    Some(&u @ ('u' | 'U')) => {
                        let width = if u == 'u' { 4 } else { 8 };
                        let hex: String = self.chars
                            [self.pos + 1..(self.pos + 1 + width).min(self.chars.len())]
                            .iter()
                            .collect();
                        if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                            out.push(ch);
                            self.pos += width;
                        }
                    }
                    Some(&other) => out.push(other),
                    None => break,
                }
                self.pos += 1;
                continue;
            }
            out.push(c);
            self.pos += 1;
        }
        Some(out)
    }

    fn array(&mut self) -> Toml {
        self.pos += 1;
        let mut values = Vec::new();
        loop {
            self.skip();
            match self.chars.get(self.pos) {
                None => return Toml::Array(values),
                Some(']') => {
                    self.pos += 1;
                    return Toml::Array(values);
                }
                Some(',') => self.pos += 1,
                Some(_) => match self.value() {
                    Some(v) => values.push(v),
                    None => return Toml::Array(values),
                },
            }
        }
    }

    fn inline_table(&mut self) -> Toml {
        self.pos += 1;
        let mut pairs = Vec::new();
        loop {
            self.skip();
            match self.chars.get(self.pos) {
                None => return Toml::Table(pairs),
                Some('}') => {
                    self.pos += 1;
                    return Toml::Table(pairs);
                }
                Some(',') => self.pos += 1,
                Some(&c) => {
                    let key = if c == '"' || c == '\'' {
                        self.string(c).unwrap_or_default()
                    } else {
                        let mut key = String::new();
                        while let Some(&c) = self.chars.get(self.pos) {
                            if c == '=' || c.is_whitespace() {
                                break;
                            }
                            key.push(c);
                            self.pos += 1;
                        }
                        key
                    };
                    self.skip();
                    if self.chars.get(self.pos) != Some(&'=') {
                        return Toml::Table(pairs);
                    }
                    self.pos += 1;
                    match self.value() {
                        Some(v) => pairs.push((key, v)),
                        None => return Toml::Table(pairs),
                    }
                }
            }
        }
    }
}

fn bare_key(key: &str) -> String {
    if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
        key.to_owned()
    } else {
        quote(key)
    }
}

fn header_key(dotted: &str) -> String {
    dotted.split('.').map(bare_key).collect::<Vec<_>>().join(".")
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn render(value: &Toml) -> String {
    match value {
        Toml::String(s) => quote(s),
        Toml::Bool(b) => b.to_string(),
        Toml::Integer(i) => i.to_string(),
        Toml::Float(f) => f.to_string(),
        Toml::Array(a) => format!("[{}]", a.iter().map(render).collect::<Vec<_>>().join(", ")),
        Toml::Table(t) => format!(
            "{{ {} }}",
            t.iter()
                .map(|(k, v)| format!("{} = {}", bare_key(k), render(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const CONFIG: &str = "model = \"o3\"\n\n[mcp_servers.a]\ncommand = \"npx\"\nargs = [\"-y\", \"x\"]\n\n[mcp_servers.a.env]\nK = \"v\"\n\n[other]\nx = 1\n";

    #[test]
    fn reads_tables_with_subtables_folded() {
        let d = Document::parse(CONFIG);
        let servers = d.tables_under("mcp_servers");
        assert_eq!(servers.len(), 1);
        let a = &servers[0];
        assert_eq!(a.get("command").unwrap().as_str(), Some("npx"));
        assert_eq!(
            a.get("args").unwrap().to_json().string_array().unwrap(),
            ["-y", "x"]
        );
        assert_eq!(
            a.get("env").unwrap().to_json().get("K").unwrap().as_str(),
            Some("v")
        );
    }

    #[test]
    fn set_replaces_in_place_and_appends_after_siblings() {
        let mut d = Document::parse(CONFIG);
        d.set_table(
            "mcp_servers.a",
            &[
                ("url".into(), Toml::String("https://x".into())),
                ("enabled".into(), Toml::Bool(false)),
            ],
        );
        assert_eq!(
            d.text(),
            "model = \"o3\"\n\n[mcp_servers.a]\nurl = \"https://x\"\nenabled = false\n\n[other]\nx = 1\n"
        );
        d.set_table("mcp_servers.b", &[("command".into(), Toml::String("b".into()))]);
        assert_eq!(
            d.text(),
            "model = \"o3\"\n\n[mcp_servers.a]\nurl = \"https://x\"\nenabled = false\n\n[mcp_servers.b]\ncommand = \"b\"\n\n[other]\nx = 1\n"
        );
        d.remove_table("mcp_servers.a");
        assert_eq!(
            d.text(),
            "model = \"o3\"\n\n[mcp_servers.b]\ncommand = \"b\"\n\n[other]\nx = 1\n"
        );
        let mut e = Document::parse("");
        e.set_table("mcp_servers.x", &[("command".into(), Toml::String("c".into()))]);
        assert_eq!(e.text(), "[mcp_servers.x]\ncommand = \"c\"\n");
    }

    #[test]
    fn values() {
        assert_eq!(
            parse_value("{ a = 1, b = \"s\" }"),
            Some(Toml::Table(vec![
                ("a".into(), Toml::Integer(1)),
                ("b".into(), Toml::String("s".into()))
            ]))
        );
        assert_eq!(
            parse_value("[1, 2.5] # c"),
            Some(Toml::Array(vec![Toml::Integer(1), Toml::Float(2.5)]))
        );
        assert_eq!(parse_value("'lit\\n'"), Some(Toml::String("lit\\n".into())));
        assert_eq!(parse_value("\"a\\\"b\""), Some(Toml::String("a\"b".into())));
        assert_eq!(dotted_key("mcp_servers.\"my server\""), "mcp_servers.my server");
        assert_eq!(header_key("mcp_servers.my server"), "mcp_servers.\"my server\"");
    }
}

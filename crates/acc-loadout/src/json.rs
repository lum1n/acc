//! A JSON(C) file edited in place.
//!
//! Harness config files carry a lot of unrelated state (`~/.claude.json`
//! is rewritten by Claude itself, `opencode.jsonc` has comments), so an
//! edit splices only the affected value's text: comments, key order and
//! formatting elsewhere survive byte for byte. The parser takes `//` and
//! `/* */` comments and trailing commas (JSONC) and records byte ranges.

use std::fmt;

use serde::ser::{SerializeMap, SerializeSeq};

/// A JSON value with its object keys in document order and numbers as
/// written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Json {
    Null,
    Bool(bool),
    /// The literal as written (`1`, `2.5`, `1e3`).
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn str(s: impl Into<String>) -> Self {
        Self::String(s.into())
    }

    pub fn strings<'a>(items: impl IntoIterator<Item = &'a str>) -> Self {
        Self::Array(items.into_iter().map(Self::str).collect())
    }

    /// `{ k: v }` with string values, keys sorted.
    pub fn string_map(pairs: &[(String, String)]) -> Self {
        let mut pairs: Vec<_> = pairs.to_vec();
        pairs.sort();
        Self::Object(pairs.into_iter().map(|(k, v)| (k, Self::String(v))).collect())
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Self::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Array(a) => Some(a),
            _ => None,
        }
    }

    /// The member `key` of an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        self.as_object()?.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Every element as a string, or `None` when one isn't.
    pub fn string_array(&self) -> Option<Vec<String>> {
        self.as_array()?
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect()
    }

    /// Every string member of an object, in document order.
    pub fn string_pairs(&self) -> Vec<(String, String)> {
        self.as_object()
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// An object without the members named in `keys`.
    pub fn without(&self, keys: &[&str]) -> Vec<(String, Json)> {
        self.as_object()
            .map(|o| {
                o.iter()
                    .filter(|(k, _)| !keys.contains(&k.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Compact JSON text.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        write(self, "", 0, &mut out);
        out
    }
}

impl From<serde_json::Value> for Json {
    fn from(v: serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(b) => Self::Bool(b),
            serde_json::Value::Number(n) => Self::Number(n.to_string()),
            serde_json::Value::String(s) => Self::String(s),
            serde_json::Value::Array(a) => Self::Array(a.into_iter().map(Into::into).collect()),
            serde_json::Value::Object(o) => Self::Object(o.into_iter().map(|(k, v)| (k, v.into())).collect()),
        }
    }
}

impl serde::Serialize for Json {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => s.serialize_unit(),
            Self::Bool(b) => s.serialize_bool(*b),
            Self::Number(n) => match (n.parse::<i64>(), n.parse::<f64>()) {
                (Ok(i), _) => s.serialize_i64(i),
                (_, Ok(f)) => s.serialize_f64(f),
                _ => s.serialize_str(n),
            },
            Self::String(t) => s.serialize_str(t),
            Self::Array(a) => {
                let mut seq = s.serialize_seq(Some(a.len()))?;
                for v in a {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            Self::Object(o) => {
                let mut map = s.serialize_map(Some(o.len()))?;
                for (k, v) in o {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for Json {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        serde_json::Value::deserialize(d).map(Into::into)
    }
}

/// Where the text couldn't be read as JSON.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}:{column}: {message}")]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

/// A parsed value with the byte range of its text.
#[derive(Debug, Clone)]
struct Node {
    value: Json,
    range: std::ops::Range<usize>,
    /// Object members in document order: key, key range, value node.
    members: Vec<(String, std::ops::Range<usize>, Node)>,
}

impl Node {
    fn member(&self, key: &str) -> Option<&Node> {
        self.members.iter().find(|(k, _, _)| k == key).map(|(_, _, n)| n)
    }
}

struct Parser<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn error(&self, message: &str) -> ParseError {
        let before = &self.text[..self.pos];
        let line = before.matches('\n').count() + 1;
        let column = before.rfind('\n').map_or(before.len(), |i| before.len() - i - 1) + 1;
        ParseError {
            message: message.to_owned(),
            line,
            column,
        }
    }

    fn peek(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') => {
                    let rest = &self.text[self.pos..];
                    if rest.starts_with("//") {
                        let end = rest.find('\n').unwrap_or(rest.len());
                        self.pos += end;
                    } else if let Some(body) = rest.strip_prefix("/*") {
                        let end = body.find("*/").map_or(rest.len(), |i| i + 4);
                        self.pos += end;
                    } else {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    fn value(&mut self) -> Result<Node, ParseError> {
        match self.peek() {
            None => Err(self.error("unexpected end of input")),
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('"') => {
                let start = self.pos;
                let s = self.string()?;
                Ok(Node {
                    value: Json::String(s),
                    range: start..self.pos,
                    members: vec![],
                })
            }
            Some('t') => self.literal("true", Json::Bool(true)),
            Some('f') => self.literal("false", Json::Bool(false)),
            Some('n') => self.literal("null", Json::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            Some(c) => Err(self.error(&format!("unexpected character {c:?}"))),
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Node, ParseError> {
        let start = self.pos;
        if !self.text[self.pos..].starts_with(word) {
            return Err(self.error("invalid literal"));
        }
        self.pos += word.len();
        Ok(Node {
            value,
            range: start..self.pos,
            members: vec![],
        })
    }

    fn number(&mut self) -> Result<Node, ParseError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == '-' || c == '+' || c == '.' || c == 'e' || c == 'E' || c.is_ascii_digit() {
                self.bump();
            } else {
                break;
            }
        }
        let literal = &self.text[start..self.pos];
        if literal.parse::<f64>().is_err() {
            return Err(self.error(&format!("invalid number {literal:?}")));
        }
        Ok(Node {
            value: Json::Number(literal.to_owned()),
            range: start..self.pos,
            members: vec![],
        })
    }

    fn string(&mut self) -> Result<String, ParseError> {
        if self.bump() != Some('"') {
            return Err(self.error("expected a string"));
        }
        let mut out = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err(self.error("unterminated string"));
            };
            match c {
                '"' => return Ok(out),
                '\\' => {
                    let Some(e) = self.bump() else {
                        return Err(self.error("unterminated escape"));
                    };
                    match e {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let mut code = self.hex4()?;
                            if (0xD800..0xDC00).contains(&code) && self.text[self.pos..].starts_with("\\u") {
                                self.pos += 2;
                                let low = self.hex4()?;
                                code = 0x10000 + ((code - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
                            }
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        other => return Err(self.error(&format!("invalid escape \\{other}"))),
                    }
                }
                c => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let hex = self.text.get(self.pos..self.pos + 4).unwrap_or("");
        let code = u32::from_str_radix(hex, 16).map_err(|_| self.error("invalid \\u escape"))?;
        self.pos += 4;
        Ok(code)
    }

    fn array(&mut self) -> Result<Node, ParseError> {
        let start = self.pos;
        self.bump();
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            if self.peek() == Some(']') {
                self.bump();
                break;
            }
            items.push(self.value()?.value);
            self.skip_trivia();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    break;
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
        Ok(Node {
            value: Json::Array(items),
            range: start..self.pos,
            members: vec![],
        })
    }

    fn object(&mut self) -> Result<Node, ParseError> {
        let start = self.pos;
        self.bump();
        let mut members: Vec<(String, std::ops::Range<usize>, Node)> = Vec::new();
        loop {
            self.skip_trivia();
            if self.peek() == Some('}') {
                self.bump();
                break;
            }
            let key_start = self.pos;
            let key = self.string()?;
            let key_range = key_start..self.pos;
            self.skip_trivia();
            if self.bump() != Some(':') {
                return Err(self.error("expected ':'"));
            }
            self.skip_trivia();
            let node = self.value()?;
            members.push((key, key_range, node));
            self.skip_trivia();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {
                    self.bump();
                    break;
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
        let value = Json::Object(
            members
                .iter()
                .map(|(k, _, n)| (k.clone(), n.value.clone()))
                .collect(),
        );
        Ok(Node {
            value,
            range: start..self.pos,
            members,
        })
    }
}

fn parse_node(text: &str) -> Result<Node, ParseError> {
    let mut p = Parser { text, pos: 0 };
    p.skip_trivia();
    let node = p.value()?;
    p.skip_trivia();
    if p.pos != text.len() {
        return Err(p.error("unexpected trailing content"));
    }
    Ok(node)
}

/// Parses JSON(C) text.
pub fn parse(text: &str) -> Result<Json, ParseError> {
    parse_node(text).map(|n| n.value)
}

/// Quotes a string as a JSON literal.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = fmt::Write::write_fmt(&mut out, format_args!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Writes `value` pretty-printed with `indent` per level (compact when it
/// is empty), starting at `level`.
fn write(value: &Json, indent: &str, level: usize, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Number(n) => out.push_str(n),
        Json::String(s) => out.push_str(&quote(s)),
        Json::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            let inline = indent.is_empty()
                || a.iter()
                    .all(|v| matches!(v, Json::String(_) | Json::Number(_) | Json::Bool(_) | Json::Null));
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if inline && !indent.is_empty() {
                        out.push(' ');
                    }
                }
                if !inline {
                    out.push('\n');
                    out.push_str(&indent.repeat(level + 1));
                }
                write(v, indent, level + 1, out);
            }
            if !inline {
                out.push('\n');
                out.push_str(&indent.repeat(level));
            }
            out.push(']');
        }
        Json::Object(o) => {
            if o.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, v)) in o.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if !indent.is_empty() {
                    out.push('\n');
                    out.push_str(&indent.repeat(level + 1));
                }
                out.push_str(&quote(k));
                out.push(':');
                if !indent.is_empty() {
                    out.push(' ');
                }
                write(v, indent, level + 1, out);
            }
            if !indent.is_empty() {
                out.push('\n');
                out.push_str(&indent.repeat(level));
            }
            out.push('}');
        }
    }
}

/// A JSON(C) document whose edits touch only the value they change.
#[derive(Debug, Clone)]
pub struct Document {
    text: String,
    root: Node,
    indent: String,
}

impl Document {
    /// Parses `text`; blank text is an empty object.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let text = if text.trim().is_empty() {
            "{}\n".to_owned()
        } else {
            text.to_owned()
        };
        let root = parse_node(&text)?;
        let indent = detect_indent(&text);
        Ok(Self { text, root, indent })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn value(&self) -> &Json {
        &self.root.value
    }

    /// The value at `path`, when every component exists.
    pub fn get(&self, path: &[&str]) -> Option<&Json> {
        self.node(path).map(|n| &n.value)
    }

    fn node(&self, path: &[&str]) -> Option<&Node> {
        let mut node = &self.root;
        for key in path {
            node = node.member(key)?;
        }
        Some(node)
    }

    /// Sets the value at `path`, making intermediate objects. An existing
    /// value is replaced within its own text; a new member is appended to
    /// its parent.
    pub fn set(&mut self, path: &[&str], value: Json) -> Result<(), ParseError> {
        let mut depth = 0;
        let mut node = &self.root;
        for key in path {
            match node.member(key) {
                Some(next) => {
                    node = next;
                    depth += 1;
                }
                None => break,
            }
        }
        if depth == path.len() {
            let mut rendered = String::new();
            write(&value, &self.indent, depth, &mut rendered);
            let range = node.range.clone();
            self.text.replace_range(range, &rendered);
        } else {
            let (container_range, last_member_end) = (
                node.range.clone(),
                node.members.last().map(|(_, _, n)| n.range.end),
            );
            if !matches!(node.value, Json::Object(_)) {
                return Err(ParseError {
                    message: format!("{:?} is not an object", path[..depth].join(".")),
                    line: 0,
                    column: 0,
                });
            }
            let mut tail = value;
            for key in path[depth + 1..].iter().rev() {
                tail = Json::Object(vec![((*key).to_owned(), tail)]);
            }
            let mut rendered = quote(path[depth]);
            rendered.push_str(": ");
            write(&tail, &self.indent, depth + 1, &mut rendered);
            let member_indent = self.indent.repeat(depth + 1);
            match last_member_end {
                Some(end) => {
                    let insertion = format!(",\n{member_indent}{rendered}");
                    self.text.insert_str(end, &insertion);
                }
                None => {
                    let open = container_range.start + 1;
                    let close = container_range.end - 1;
                    let closing_indent = self.indent.repeat(depth);
                    self.text.replace_range(
                        open..close,
                        &format!("\n{member_indent}{rendered}\n{closing_indent}"),
                    );
                }
            }
        }
        self.root = parse_node(&self.text)?;
        Ok(())
    }

    /// Removes the member at `path`; nothing happens when it isn't there.
    pub fn remove(&mut self, path: &[&str]) -> Result<(), ParseError> {
        let Some((last, parent_path)) = path.split_last() else {
            return Ok(());
        };
        let Some(parent) = self.node(parent_path) else {
            return Ok(());
        };
        let Some(position) = parent.members.iter().position(|(k, _, _)| k == last) else {
            return Ok(());
        };
        let (_, key_range, node) = &parent.members[position];
        let mut start = key_range.start;
        let mut end = node.range.end;
        if position + 1 < parent.members.len() {
            end = parent.members[position + 1].1.start;
        } else if position > 0 {
            start = parent.members[position - 1].2.range.end;
        } else {
            start = parent.range.start + 1;
            end = parent.range.end - 1;
        }
        self.text.replace_range(start..end, "");
        self.root = parse_node(&self.text)?;
        Ok(())
    }
}

fn detect_indent(text: &str) -> String {
    for line in text.lines() {
        let leading: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        if !leading.is_empty() && leading.len() < line.len() {
            return if leading.starts_with('\t') {
                "\t".to_owned()
            } else {
                leading
            };
        }
    }
    "  ".to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonc() {
        let v = parse("// top\n{\n  \"a\": [1, 2,], /* c */ \"b\": {\"c\": \"d\\n\"},\n}").unwrap();
        assert_eq!(v.get("b").unwrap().get("c").unwrap().as_str(), Some("d\n"));
        assert_eq!(v.get("a").unwrap().as_array().unwrap().len(), 2);
        assert!(parse("{\"a\": }").is_err());
    }

    #[test]
    fn set_replaces_in_place_and_keeps_the_rest() {
        let text = "{\n  // keep me\n  \"mcpServers\": {\n    \"a\": {\"command\": \"x\"}\n  },\n  \"other\": 1\n}\n";
        let mut d = Document::parse(text).unwrap();
        d.set(
            &["mcpServers", "a"],
            Json::Object(vec![("command".into(), Json::str("y"))]),
        )
        .unwrap();
        assert_eq!(
            d.text(),
            "{\n  // keep me\n  \"mcpServers\": {\n    \"a\": {\n      \"command\": \"y\"\n    }\n  },\n  \"other\": 1\n}\n"
        );
        d.set(
            &["mcpServers", "b"],
            Json::Object(vec![("url".into(), Json::str("u"))]),
        )
        .unwrap();
        assert!(
            d.text()
                .contains("    },\n    \"b\": {\n      \"url\": \"u\"\n    }\n  },")
        );
        d.remove(&["mcpServers", "a"]).unwrap();
        assert!(!d.text().contains("\"a\""));
        assert!(d.text().contains("\"mcpServers\": {\n    \"b\": {"));
        d.remove(&["mcpServers", "b"]).unwrap();
        assert!(d.text().contains("\"mcpServers\": {},\n  \"other\": 1"));
    }

    #[test]
    fn set_makes_missing_objects_and_detects_indent() {
        let mut d = Document::parse("{\n\t\"x\": true\n}\n").unwrap();
        d.set(&["mcp", "s"], Json::Bool(false)).unwrap();
        assert_eq!(
            d.text(),
            "{\n\t\"x\": true,\n\t\"mcp\": {\n\t\t\"s\": false\n\t}\n}\n"
        );
        let mut e = Document::parse("").unwrap();
        e.set(&["a"], Json::strings(["1", "2"])).unwrap();
        assert_eq!(e.text(), "{\n  \"a\": [\"1\", \"2\"]\n}\n");
        assert!(e.set(&["a", "b"], Json::Null).is_err());
    }

    #[test]
    fn remove_of_a_middle_member_keeps_commas_right() {
        let mut d = Document::parse("{\"a\": 1, \"b\": 2, \"c\": 3}").unwrap();
        d.remove(&["b"]).unwrap();
        assert_eq!(d.text(), "{\"a\": 1, \"c\": 3}");
        d.remove(&["c"]).unwrap();
        assert_eq!(d.text(), "{\"a\": 1}");
        d.remove(&["nope", "x"]).unwrap();
        assert_eq!(d.text(), "{\"a\": 1}");
    }

    #[test]
    fn serde_round_trip_keeps_order() {
        let v = parse("{\"z\": 1, \"a\": [true, null, \"s\"]}").unwrap();
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            "{\"z\":1,\"a\":[true,null,\"s\"]}"
        );
        assert_eq!(v.to_text(), "{\"z\":1,\"a\":[true,null,\"s\"]}");
        let back: Json = serde_json::from_str("{\"k\": \"v\"}").unwrap();
        assert_eq!(back.get("k").unwrap().as_str(), Some("v"));
    }
}

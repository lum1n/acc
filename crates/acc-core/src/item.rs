//! Typed views of transcript items.
//!
//! Drivers pass each harness's native item JSON through in `extensions`. This
//! module turns it into one shape a UI can render without knowing the
//! harness: tool calls with their input and output, shell commands with
//! output and exit code, and file changes as structured diffs.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use similar::{ChangeTag, TextDiff};

use crate::event::ItemKind;
use crate::harness::Harness;
use crate::wire::{owned, str_at};

/// What an item is, in a harness-independent shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ItemDetail {
    /// User or agent message, or reasoning: the item's text is the content.
    Text,
    Tool {
        name: String,
        /// One-line summary of what the tool acts on (path, pattern, URL...).
        title: String,
        input: Value,
        output: Option<String>,
    },
    Command {
        command: String,
        cwd: Option<String>,
        output: String,
        exit_code: Option<i32>,
    },
    FileChange {
        files: Vec<FileDiff>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Add,
    Delete,
    Update,
    Rename,
}

/// The change to one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    /// The previous path, for renames.
    pub old_path: Option<String>,
    pub kind: FileChangeKind,
    pub hunks: Vec<Hunk>,
    pub added: u32,
    pub removed: u32,
}

/// A contiguous region of a diff. Line numbers are 1-based; 0 means unknown
/// (an edit reported as old/new snippets without its position in the file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineTag {
    Context,
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub tag: LineTag,
    pub text: String,
}

impl FileDiff {
    fn new(path: String, kind: FileChangeKind, hunks: Vec<Hunk>) -> Self {
        let count = |tag| {
            hunks
                .iter()
                .flat_map(|h| &h.lines)
                .filter(|l| l.tag == tag)
                .count() as u32
        };
        Self {
            added: count(LineTag::Add),
            removed: count(LineTag::Remove),
            path,
            old_path: None,
            kind,
            hunks,
        }
    }

    /// The change from `old` to `new` text, with three lines of context.
    pub fn from_texts(path: &str, old: &str, new: &str, kind: FileChangeKind) -> Self {
        Self::new(
            path.to_owned(),
            kind,
            text_hunks(old, new, kind == FileChangeKind::Update),
        )
    }
}

/// Hunks between two texts. `positioned` is false for snippets whose place
/// in the file is unknown, which get line numbers of 0.
fn text_hunks(old: &str, new: &str, positioned: bool) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(old, new);
    diff.grouped_ops(3)
        .iter()
        .map(|group| {
            let mut lines = Vec::new();
            for op in group {
                for change in diff.iter_changes(op) {
                    let tag = match change.tag() {
                        ChangeTag::Equal => LineTag::Context,
                        ChangeTag::Insert => LineTag::Add,
                        ChangeTag::Delete => LineTag::Remove,
                    };
                    let text = change.value().trim_end_matches(['\n', '\r']).to_owned();
                    lines.push(DiffLine { tag, text });
                }
            }
            let (first, last) = (group.first(), group.last());
            let old_range = first
                .zip(last)
                .map(|(f, l)| (f.old_range().start, l.old_range().end));
            let new_range = first
                .zip(last)
                .map(|(f, l)| (f.new_range().start, l.new_range().end));
            let (os, oe) = old_range.unwrap_or_default();
            let (ns, ne) = new_range.unwrap_or_default();
            let start = |s: usize, len: usize| {
                if positioned && len > 0 {
                    s as u32 + 1
                } else if positioned {
                    s as u32
                } else {
                    0
                }
            };
            Hunk {
                old_start: start(os, oe - os),
                old_lines: (oe - os) as u32,
                new_start: start(ns, ne - ns),
                new_lines: (ne - ns) as u32,
                lines,
            }
        })
        .collect()
}

/// Every line of `text` as one hunk of additions (or removals).
fn whole(text: &str, tag: LineTag) -> Vec<Hunk> {
    let lines: Vec<DiffLine> = text
        .lines()
        .map(|l| DiffLine {
            tag,
            text: l.to_owned(),
        })
        .collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let n = lines.len() as u32;
    let (old, new) = if tag == LineTag::Add {
        ((0, 0), (1, n))
    } else {
        ((1, n), (0, 0))
    };
    vec![Hunk {
        old_start: old.0,
        old_lines: old.1,
        new_start: new.0,
        new_lines: new.1,
        lines,
    }]
}

/// Parses a unified diff (`git diff` output, or one file's hunks) into files.
/// Hunks before any file header are reported under `default_path`.
pub fn parse_unified(text: &str, default_path: &str) -> Vec<FileDiff> {
    struct Open {
        path: String,
        old_path: Option<String>,
        kind: FileChangeKind,
        hunks: Vec<Hunk>,
    }
    fn close(open: Option<Open>, out: &mut Vec<FileDiff>) {
        if let Some(o) = open {
            let mut f = FileDiff::new(o.path, o.kind, o.hunks);
            f.old_path = o.old_path;
            out.push(f);
        }
    }
    fn strip(p: &str) -> String {
        let p = p.split('\t').next().unwrap_or(p);
        p.strip_prefix("a/")
            .or_else(|| p.strip_prefix("b/"))
            .unwrap_or(p)
            .to_owned()
    }
    let mut out = Vec::new();
    let mut open: Option<Open> = None;
    let mut minus: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            close(open.take(), &mut out);
            let path = rest.split(" b/").last().unwrap_or(rest).to_owned();
            open = Some(Open {
                path,
                old_path: None,
                kind: FileChangeKind::Update,
                hunks: Vec::new(),
            });
        } else if let Some(p) = line.strip_prefix("--- ") {
            minus = Some(p.to_owned());
        } else if let Some(p) = line.strip_prefix("+++ ") {
            let old = minus.take().unwrap_or_default();
            let o = open.get_or_insert_with(|| Open {
                path: String::new(),
                old_path: None,
                kind: FileChangeKind::Update,
                hunks: Vec::new(),
            });
            if p.starts_with("/dev/null") {
                o.kind = FileChangeKind::Delete;
                o.path = strip(&old);
            } else {
                o.path = strip(p);
                if old.starts_with("/dev/null") {
                    o.kind = FileChangeKind::Add;
                }
            }
        } else if let Some(p) = line.strip_prefix("rename from ") {
            if let Some(o) = open.as_mut() {
                o.old_path = Some(p.to_owned());
                o.kind = FileChangeKind::Rename;
            }
        } else if let Some(p) = line.strip_prefix("rename to ") {
            if let Some(o) = open.as_mut() {
                o.path = p.to_owned();
            }
        } else if line.starts_with("new file mode") {
            if let Some(o) = open.as_mut() {
                o.kind = FileChangeKind::Add;
            }
        } else if line.starts_with("deleted file mode") {
            if let Some(o) = open.as_mut() {
                o.kind = FileChangeKind::Delete;
            }
        } else if let Some(header) = line.strip_prefix("@@ ") {
            let o = open.get_or_insert_with(|| Open {
                path: default_path.to_owned(),
                old_path: None,
                kind: FileChangeKind::Update,
                hunks: Vec::new(),
            });
            let (old, new) = parse_range_header(header);
            o.hunks.push(Hunk {
                old_start: old.0,
                old_lines: old.1,
                new_start: new.0,
                new_lines: new.1,
                lines: Vec::new(),
            });
        } else if let Some(hunk) = open.as_mut().and_then(|o| o.hunks.last_mut()) {
            let (tag, text) = match line.as_bytes().first() {
                Some(b'+') => (LineTag::Add, &line[1..]),
                Some(b'-') => (LineTag::Remove, &line[1..]),
                Some(b' ') => (LineTag::Context, &line[1..]),
                None => (LineTag::Context, ""),
                // `\ No newline at end of file` and unknown lines.
                _ => continue,
            };
            hunk.lines.push(DiffLine {
                tag,
                text: text.to_owned(),
            });
        }
    }
    close(open, &mut out);
    out
}

/// Converts a Codex `apply_patch` body (`*** Begin Patch` ... `*** End
/// Patch`) into the `changes` array Codex's app-server reports for file
/// changes, so rollouts and live sessions render the same way.
pub fn apply_patch_changes(patch: &str) -> Value {
    let mut changes: Vec<Value> = Vec::new();
    let mut cur: Option<(String, &'static str, Option<String>, Vec<String>)> = None;
    let flush = |cur: &mut Option<(String, &'static str, Option<String>, Vec<String>)>,
                 changes: &mut Vec<Value>| {
        if let Some((path, kind, mv, lines)) = cur.take() {
            let diff = if kind == "add" {
                lines
                    .iter()
                    .map(|l| l.strip_prefix('+').unwrap_or(l))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                lines.join("\n")
            };
            changes.push(serde_json::json!({
                "path": path,
                "kind": {"type": kind, "move_path": mv},
                "diff": diff,
            }));
        }
    };
    for line in patch.lines() {
        if let Some(p) = line.strip_prefix("*** Update File: ") {
            flush(&mut cur, &mut changes);
            cur = Some((p.trim().to_owned(), "update", None, Vec::new()));
        } else if let Some(p) = line.strip_prefix("*** Add File: ") {
            flush(&mut cur, &mut changes);
            cur = Some((p.trim().to_owned(), "add", None, Vec::new()));
        } else if let Some(p) = line.strip_prefix("*** Delete File: ") {
            flush(&mut cur, &mut changes);
            cur = Some((p.trim().to_owned(), "delete", None, Vec::new()));
        } else if let Some(p) = line.strip_prefix("*** Move to: ") {
            if let Some(c) = cur.as_mut() {
                c.2 = Some(p.trim().to_owned());
            }
        } else if line.starts_with("*** ") {
            // Begin/End Patch, End of File.
        } else if let Some(c) = cur.as_mut() {
            if line == "@@" || (line.starts_with("@@") && !line.starts_with("@@ -")) {
                c.3.push("@@ -0,0 +0,0 @@".to_owned());
            } else {
                c.3.push(line.to_owned());
            }
        }
    }
    flush(&mut cur, &mut changes);
    Value::Array(changes)
}

/// `-a,b +c,d @@ ...` → ((a, b), (c, d)). A missing count means 1.
fn parse_range_header(h: &str) -> ((u32, u32), (u32, u32)) {
    let range = |s: &str| {
        let mut it = s.splitn(2, ',');
        let start = it.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        let len = it.next().and_then(|n| n.parse().ok()).unwrap_or(1);
        (start, len)
    };
    let mut parts = h.split_whitespace();
    let old = parts
        .next()
        .and_then(|p| p.strip_prefix('-'))
        .map(range)
        .unwrap_or_default();
    let new = parts
        .next()
        .and_then(|p| p.strip_prefix('+'))
        .map(range)
        .unwrap_or_default();
    (old, new)
}

/// Text of a tool result: a string, or content blocks of which text counts.
fn content_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter_map(|b| {
                    str_at(b, "text").or_else(|| b.pointer("/content/text").and_then(Value::as_str))
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Value::Object(_) => v
            .get("content")
            .and_then(content_text)
            .or_else(|| owned(v, "text"))
            .or_else(|| owned(v, "output")),
        other => Some(other.to_string()),
    }
}

/// A short subject for a tool call from its input.
fn subject(input: &Value) -> String {
    for key in [
        "command",
        "file_path",
        "path",
        "filePath",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
    ] {
        if let Some(s) = str_at(input, key) {
            return crate::wire::one_line(s, 160);
        }
    }
    String::new()
}

fn i32_at(v: &Value, key: &str) -> Option<i32> {
    v.get(key)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
}

/// Builds the typed detail of an item from its harness's extensions.
///
/// `ext` is the item's extensions with every update and the completion
/// merged over the start (later keys win); `text` is its streamed text.
pub fn detail(harness: Harness, kind: ItemKind, ext: &Value, text: &str) -> ItemDetail {
    match kind {
        ItemKind::UserMessage | ItemKind::AgentMessage | ItemKind::Reasoning => ItemDetail::Text,
        _ => match harness {
            Harness::Claude => claude(kind, ext, text),
            Harness::Codex => codex(kind, ext, text),
            Harness::Pi => pi(kind, ext, text),
            Harness::Cursor | Harness::Copilot | Harness::Opencode => acp(kind, ext, text),
        },
    }
}

fn tool(name: &str, input: &Value, output: Option<String>) -> ItemDetail {
    ItemDetail::Tool {
        name: name.to_owned(),
        title: subject(input),
        input: input.clone(),
        output,
    }
}

fn claude(kind: ItemKind, ext: &Value, text: &str) -> ItemDetail {
    let name = str_at(ext, "name").unwrap_or("tool");
    let input = ext.get("input").cloned().unwrap_or(Value::Null);
    let output = ext.get("content").and_then(content_text);
    match (kind, name) {
        (ItemKind::CommandExecution, _) => ItemDetail::Command {
            command: owned(&input, "command").unwrap_or_default(),
            cwd: None,
            output: output.unwrap_or_else(|| text.to_owned()),
            exit_code: None,
        },
        (ItemKind::FileChange, "Edit" | "MultiEdit" | "Write") => {
            let path = owned(&input, "file_path").unwrap_or_default();
            let file = match name {
                "Write" => FileDiff::new(
                    path,
                    FileChangeKind::Add,
                    whole(str_at(&input, "content").unwrap_or_default(), LineTag::Add),
                ),
                "MultiEdit" => {
                    let hunks = input
                        .get("edits")
                        .and_then(Value::as_array)
                        .map(|edits| {
                            edits
                                .iter()
                                .flat_map(|e| {
                                    text_hunks(
                                        str_at(e, "old_string").unwrap_or_default(),
                                        str_at(e, "new_string").unwrap_or_default(),
                                        false,
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    FileDiff::new(path, FileChangeKind::Update, hunks)
                }
                _ => FileDiff::new(
                    path,
                    FileChangeKind::Update,
                    text_hunks(
                        str_at(&input, "old_string").unwrap_or_default(),
                        str_at(&input, "new_string").unwrap_or_default(),
                        false,
                    ),
                ),
            };
            ItemDetail::FileChange { files: vec![file] }
        }
        _ => tool(name, &input, output),
    }
}

fn codex(kind: ItemKind, ext: &Value, text: &str) -> ItemDetail {
    match str_at(ext, "type") {
        Some("commandExecution") => {
            let command = match ext.get("command") {
                Some(Value::Array(argv)) => crate::shell::join(argv.iter().filter_map(Value::as_str)),
                Some(v) => v.as_str().unwrap_or_default().to_owned(),
                None => String::new(),
            };
            ItemDetail::Command {
                command,
                cwd: owned(ext, "cwd"),
                output: owned(ext, "aggregatedOutput").unwrap_or_else(|| text.to_owned()),
                exit_code: i32_at(ext, "exitCode"),
            }
        }
        Some("fileChange") => {
            let files = ext
                .get("changes")
                .and_then(Value::as_array)
                .map(|changes| changes.iter().flat_map(codex_change).collect())
                .unwrap_or_default();
            ItemDetail::FileChange { files }
        }
        Some("mcpToolCall") => {
            let name = format!(
                "{}/{}",
                str_at(ext, "server").unwrap_or("mcp"),
                str_at(ext, "tool").unwrap_or("tool")
            );
            let input = ext.get("arguments").cloned().unwrap_or(Value::Null);
            let output = ext.get("result").and_then(content_text).or_else(|| {
                ext.pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            tool(&name, &input, output)
        }
        Some("dynamicToolCall") => {
            let input = ext.get("arguments").cloned().unwrap_or(Value::Null);
            let output = ext.get("contentItems").and_then(content_text);
            tool(str_at(ext, "tool").unwrap_or("tool"), &input, output)
        }
        Some("webSearch") => tool("web_search", ext, None),
        Some(other) => tool(other, ext, (!text.is_empty()).then(|| text.to_owned())),
        None => match kind {
            ItemKind::CommandExecution => ItemDetail::Command {
                command: String::new(),
                cwd: None,
                output: text.to_owned(),
                exit_code: None,
            },
            _ => tool("tool", ext, None),
        },
    }
}

fn codex_change(change: &Value) -> Option<FileDiff> {
    let path = owned(change, "path")?;
    let diff = str_at(change, "diff").unwrap_or_default();
    let kind = change.get("kind").cloned().unwrap_or(Value::Null);
    let kind_name = str_at(&kind, "type").or_else(|| kind.as_str());
    let mut file = match kind_name {
        Some("add") => FileDiff::new(path, FileChangeKind::Add, whole(diff, LineTag::Add)),
        Some("delete") => FileDiff::new(path, FileChangeKind::Delete, whole(diff, LineTag::Remove)),
        _ => {
            let hunks = parse_unified(diff, &path)
                .into_iter()
                .flat_map(|f| f.hunks)
                .collect();
            FileDiff::new(path, FileChangeKind::Update, hunks)
        }
    };
    if let Some(to) = owned(&kind, "move_path").or_else(|| owned(&kind, "movePath")) {
        file.old_path = Some(std::mem::replace(&mut file.path, to));
        file.kind = FileChangeKind::Rename;
    }
    Some(file)
}

fn pi(kind: ItemKind, ext: &Value, text: &str) -> ItemDetail {
    let name = str_at(ext, "tool").unwrap_or("tool");
    let args = ext.get("args").cloned().unwrap_or(Value::Null);
    let output = ext.get("result").and_then(content_text);
    match (kind, name) {
        (ItemKind::CommandExecution, _) => ItemDetail::Command {
            command: owned(&args, "command")
                .or_else(|| owned(ext, "command"))
                .unwrap_or_default(),
            cwd: None,
            output: output
                .or_else(|| owned(ext, "output"))
                .unwrap_or_else(|| text.to_owned()),
            exit_code: i32_at(ext, "exitCode"),
        },
        (ItemKind::FileChange, "write") => {
            let path = owned(&args, "path").unwrap_or_default();
            let body = str_at(&args, "content").unwrap_or_default();
            ItemDetail::FileChange {
                files: vec![FileDiff::new(
                    path,
                    FileChangeKind::Add,
                    whole(body, LineTag::Add),
                )],
            }
        }
        (ItemKind::FileChange, _) => {
            let path = owned(&args, "path").unwrap_or_default();
            let edits: Vec<(&str, &str)> = match args.get("edits").and_then(Value::as_array) {
                Some(edits) => edits
                    .iter()
                    .map(|e| {
                        (
                            str_at(e, "oldText").unwrap_or_default(),
                            str_at(e, "newText").unwrap_or_default(),
                        )
                    })
                    .collect(),
                None => vec![(
                    str_at(&args, "oldText").unwrap_or_default(),
                    str_at(&args, "newText").unwrap_or_default(),
                )],
            };
            let hunks = edits
                .into_iter()
                .flat_map(|(old, new)| text_hunks(old, new, false))
                .collect();
            ItemDetail::FileChange {
                files: vec![FileDiff::new(path, FileChangeKind::Update, hunks)],
            }
        }
        _ => tool(name, &args, output),
    }
}

fn acp(kind: ItemKind, ext: &Value, text: &str) -> ItemDetail {
    let content: &[Value] = ext
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let diffs: Vec<FileDiff> = content
        .iter()
        .filter(|c| str_at(c, "type") == Some("diff"))
        .map(|c| {
            let path = str_at(c, "path").unwrap_or_default();
            let new = str_at(c, "newText").unwrap_or_default();
            match c.get("oldText").and_then(Value::as_str) {
                Some(old) => FileDiff::from_texts(path, old, new, FileChangeKind::Update),
                None => FileDiff::new(path.to_owned(), FileChangeKind::Add, whole(new, LineTag::Add)),
            }
        })
        .collect();
    if !diffs.is_empty() {
        return ItemDetail::FileChange { files: diffs };
    }
    let output = content
        .iter()
        .filter(|c| str_at(c, "type") == Some("content"))
        .filter_map(|c| c.pointer("/content/text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let raw_out = ext.get("rawOutput").and_then(content_text);
    let output = if output.is_empty() { raw_out } else { Some(output) };
    let input = ext.get("rawInput").cloned().unwrap_or(Value::Null);
    let title = owned(ext, "title").unwrap_or_default();
    if kind == ItemKind::CommandExecution || str_at(ext, "kind") == Some("execute") {
        return ItemDetail::Command {
            command: owned(&input, "command").unwrap_or(title),
            cwd: owned(&input, "cwd"),
            output: output.unwrap_or_else(|| text.to_owned()),
            exit_code: ext
                .pointer("/rawOutput/exitCode")
                .or_else(|| ext.pointer("/rawOutput/exit_code"))
                .and_then(Value::as_i64)
                .and_then(|n| i32::try_from(n).ok()),
        };
    }
    let name = str_at(ext, "kind").unwrap_or("tool");
    ItemDetail::Tool {
        name: name.to_owned(),
        title: if title.is_empty() { subject(&input) } else { title },
        input,
        output,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_edit_becomes_unpositioned_hunk() {
        let ext = json!({"name": "Edit", "input": {"file_path": "/p/a.rs", "old_string": "a\nb\n", "new_string": "a\nc\n"}});
        let ItemDetail::FileChange { files } = detail(Harness::Claude, ItemKind::FileChange, &ext, "") else {
            panic!("file change")
        };
        assert_eq!(files[0].path, "/p/a.rs");
        assert_eq!((files[0].added, files[0].removed), (1, 1));
        assert_eq!(files[0].hunks[0].old_start, 0);
        let tags: Vec<_> = files[0].hunks[0].lines.iter().map(|l| l.tag).collect();
        assert_eq!(tags, [LineTag::Context, LineTag::Remove, LineTag::Add]);
    }

    #[test]
    fn claude_bash_is_a_command_with_result_output() {
        let ext = json!({"name": "Bash", "input": {"command": "ls"}, "is_error": false,
            "content": [{"type": "text", "text": "a\nb"}]});
        assert_eq!(
            detail(Harness::Claude, ItemKind::CommandExecution, &ext, ""),
            ItemDetail::Command {
                command: "ls".into(),
                cwd: None,
                output: "a\nb".into(),
                exit_code: None
            }
        );
    }

    #[test]
    fn codex_file_change_parses_unified_hunks_and_moves() {
        let ext = json!({"type": "fileChange", "changes": [
            {"path": "src/x.rs", "kind": {"type": "update", "move_path": "src/y.rs"},
             "diff": "@@ -1,2 +1,2 @@\n a\n-b\n+c\n"},
            {"path": "new.txt", "kind": {"type": "add"}, "diff": "hello\nworld\n"}]});
        let ItemDetail::FileChange { files } = detail(Harness::Codex, ItemKind::FileChange, &ext, "") else {
            panic!("file change")
        };
        assert_eq!(files[0].kind, FileChangeKind::Rename);
        assert_eq!(files[0].path, "src/y.rs");
        assert_eq!(files[0].old_path.as_deref(), Some("src/x.rs"));
        assert_eq!(files[0].hunks[0].new_start, 1);
        assert_eq!((files[0].added, files[0].removed), (1, 1));
        assert_eq!((files[1].kind, files[1].added), (FileChangeKind::Add, 2));
    }

    #[test]
    fn codex_command_uses_aggregated_output_and_exit_code() {
        let ext = json!({"type": "commandExecution", "command": "cargo test", "cwd": "/w",
            "aggregatedOutput": "ok", "exitCode": 101});
        assert_eq!(
            detail(Harness::Codex, ItemKind::CommandExecution, &ext, "streamed"),
            ItemDetail::Command {
                command: "cargo test".into(),
                cwd: Some("/w".into()),
                output: "ok".into(),
                exit_code: Some(101)
            }
        );
    }

    #[test]
    fn acp_diff_content_is_a_positioned_file_change() {
        let ext = json!({"toolCallId": "t", "kind": "edit", "title": "Edit a.txt",
            "content": [{"type": "diff", "path": "/w/a.txt", "oldText": "1\n2\n3\n", "newText": "1\n2\n4\n"}]});
        let ItemDetail::FileChange { files } = detail(Harness::Opencode, ItemKind::FileChange, &ext, "")
        else {
            panic!("file change")
        };
        assert_eq!(files[0].hunks[0].old_start, 1);
        assert_eq!((files[0].added, files[0].removed), (1, 1));
    }

    #[test]
    fn pi_edit_and_tool_output() {
        let ext = json!({"tool": "edit", "args": {"path": "a", "oldText": "x", "newText": "y"}});
        assert!(matches!(
            detail(Harness::Pi, ItemKind::FileChange, &ext, ""),
            ItemDetail::FileChange { .. }
        ));
        let ext = json!({"tool": "read", "args": {"path": "a"}, "result": {"content": [{"type": "text", "text": "body"}]}});
        assert_eq!(
            detail(Harness::Pi, ItemKind::ToolCall, &ext, ""),
            ItemDetail::Tool {
                name: "read".into(),
                title: "a".into(),
                input: json!({"path": "a"}),
                output: Some("body".into())
            }
        );
    }

    #[test]
    fn codex_apply_patch_becomes_file_changes() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@ fn main\n-old\n+new\n ctx\n*** Add File: b.txt\n+one\n+two\n*** Delete File: c.txt\n*** End Patch";
        let ext = json!({"type": "fileChange", "changes": apply_patch_changes(patch)});
        let ItemDetail::FileChange { files } = detail(Harness::Codex, ItemKind::FileChange, &ext, "") else {
            panic!("file change")
        };
        let summary: Vec<_> = files
            .iter()
            .map(|f| (f.path.as_str(), f.kind, f.added, f.removed))
            .collect();
        assert_eq!(
            summary,
            [
                ("src/a.rs", FileChangeKind::Update, 1, 1),
                ("b.txt", FileChangeKind::Add, 2, 0),
                ("c.txt", FileChangeKind::Delete, 0, 0),
            ]
        );
    }

    #[test]
    fn git_diff_with_add_delete_and_rename() {
        let text = "diff --git a/a.txt b/a.txt\nindex 1..2 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n x\n-y\n+z\n w\n\
diff --git a/n.txt b/n.txt\nnew file mode 100644\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+new\n\
diff --git a/d.txt b/d.txt\ndeleted file mode 100644\n--- a/d.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n\\ No newline at end of file\n\
diff --git a/o.txt b/r.txt\nsimilarity index 100%\nrename from o.txt\nrename to r.txt\n";
        let files = parse_unified(text, "");
        let summary: Vec<_> = files
            .iter()
            .map(|f| (f.path.as_str(), f.kind, f.added, f.removed))
            .collect();
        assert_eq!(
            summary,
            [
                ("a.txt", FileChangeKind::Update, 1, 1),
                ("n.txt", FileChangeKind::Add, 1, 0),
                ("d.txt", FileChangeKind::Delete, 0, 1),
                ("r.txt", FileChangeKind::Rename, 0, 0),
            ]
        );
        assert_eq!(files[3].old_path.as_deref(), Some("o.txt"));
        assert_eq!(files[0].hunks[0].lines.len(), 4);
    }
}

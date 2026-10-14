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
    /// A run of another agent: what it was asked and what it reported. The
    /// child conversation's items carry this item's id as their `parent`.
    Subagent {
        /// The agent type or name (Claude's `subagent_type`, opencode's
        /// agent, a Codex nickname or role), when known.
        agent: Option<String>,
        /// One-line summary of the task.
        description: String,
        prompt: Option<String>,
        /// The harness's id for the child conversation: Claude agent id,
        /// Codex thread id, opencode session id.
        child: Option<String>,
        /// The harness's own status word (`running`, `completed`, ...).
        status: Option<String>,
        /// The agent's final report, or its latest summary while it runs.
        output: Option<String>,
        model: Option<String>,
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

/// Text the hand-back frame of a Claude subagent wraps its report in, after
/// which every line of the report is indented by two spaces.
const HAND_BACK_MARK: &str = "The report follows:\n";

/// The report inside a Claude subagent's tool result. Claude 2.1 wraps it
/// in a "[Subagent hand-back]" preamble and indents it; older versions
/// return it bare. A background launch's acknowledgement is no report.
fn claude_report(text: &str) -> Option<String> {
    if text.starts_with("Async agent launched successfully") {
        return None;
    }
    let report = match text.split_once(HAND_BACK_MARK) {
        Some((_, rest)) => rest
            .lines()
            .map(|l| l.strip_prefix("  ").unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n"),
        None => text.to_owned(),
    };
    Some(report.trim().to_owned()).filter(|r| !r.is_empty())
}

/// A Claude `Agent`/`Task` call. The input names the agent and task; the
/// driver merges `task_*` frames under `task` and the structured result
/// (`toolUseResult`) under `result`.
fn claude_subagent(ext: &Value, text: &str) -> ItemDetail {
    let null = Value::Null;
    let input = ext.get("input").unwrap_or(&null);
    let task = ext.get("task").unwrap_or(&null);
    let result = ext.get("result").unwrap_or(&null);
    let report = ext
        .get("content")
        .and_then(content_text)
        .or_else(|| (!text.is_empty()).then(|| text.to_owned()))
        .and_then(|t| claude_report(&t));
    let status = owned(task, "status")
        .or_else(|| owned(result, "status"))
        .or_else(|| {
            (ext.get("is_error").and_then(Value::as_bool) == Some(true)).then(|| "failed".to_owned())
        });
    ItemDetail::Subagent {
        agent: owned(input, "subagent_type")
            .or_else(|| owned(task, "subagent_type"))
            .or_else(|| owned(result, "agentType")),
        description: owned(input, "description")
            .or_else(|| owned(task, "description"))
            .or_else(|| owned(result, "description"))
            .unwrap_or_default(),
        prompt: owned(input, "prompt")
            .or_else(|| owned(task, "prompt"))
            .or_else(|| owned(result, "prompt")),
        child: owned(result, "agentId").or_else(|| owned(task, "task_id")),
        status,
        output: report.or_else(|| owned(task, "summary")),
        model: owned(result, "resolvedModel"),
    }
}

fn claude(kind: ItemKind, ext: &Value, text: &str) -> ItemDetail {
    let name = str_at(ext, "name").unwrap_or("tool");
    if kind == ItemKind::Subagent {
        return claude_subagent(ext, text);
    }
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

/// A Codex collab tool call (`spawn_agent`, `wait_agent`, `send_input`,
/// `close_agent`, `resume_agent`), as the app-server's `collabAgentToolCall`
/// item. The driver merges the child thread's nickname or role as `agent`.
fn codex_subagent(ext: &Value, text: &str) -> ItemDetail {
    // 0.162: the spawn is a `subAgentActivity` naming the child thread and
    // its path (`/root/pong`); the driver merges `childStatus` and `report`
    // when it stops.
    if str_at(ext, "type") == Some("subAgentActivity") {
        let agent = str_at(ext, "agentPath")
            .and_then(|p| p.rsplit('/').next())
            .filter(|n| !n.is_empty())
            .map(str::to_owned);
        return ItemDetail::Subagent {
            description: match &agent {
                Some(name) => format!("Spawn agent {name}"),
                None => "Spawn agent".to_owned(),
            },
            agent,
            prompt: None,
            child: owned(ext, "agentThreadId"),
            status: owned(ext, "childStatus").or_else(|| Some("running".to_owned())),
            output: owned(ext, "report"),
            model: owned(ext, "model"),
        };
    }
    let tool = str_at(ext, "tool").unwrap_or("spawnAgent");
    let child = ext
        .get("receiverThreadIds")
        .and_then(Value::as_array)
        .and_then(|r| r.first())
        .and_then(Value::as_str)
        .map(str::to_owned);
    let state = child
        .as_deref()
        .and_then(|c| ext.pointer(&format!("/agentsStates/{}", escape_pointer(c))));
    let description = match tool {
        "spawnAgent" => "Spawn agent",
        "sendInput" => "Send input to agent",
        "wait" => "Wait for agent",
        "closeAgent" => "Close agent",
        "resumeAgent" => "Resume agent",
        other => other,
    };
    let description = match owned(ext, "agent") {
        Some(name) => format!("{description} {name}"),
        None => description.to_owned(),
    };
    ItemDetail::Subagent {
        agent: owned(ext, "agent"),
        description,
        prompt: owned(ext, "prompt"),
        child,
        status: state
            .and_then(|s| owned(s, "status"))
            .or_else(|| owned(ext, "childStatus"))
            .or_else(|| owned(ext, "status")),
        output: state
            .and_then(|s| owned(s, "message"))
            .filter(|m| !m.is_empty())
            .or_else(|| owned(ext, "report"))
            .or_else(|| (!text.is_empty()).then(|| text.to_owned())),
        model: owned(ext, "model"),
    }
}

/// Escapes a JSON Pointer reference token (RFC 6901).
fn escape_pointer(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

fn codex(kind: ItemKind, ext: &Value, text: &str) -> ItemDetail {
    if kind == ItemKind::Subagent {
        return codex_subagent(ext, text);
    }
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
    if kind == ItemKind::Subagent {
        // pi-subagents: `{agent, task}`; batches and chains carry `tasks`.
        let first = args
            .get("tasks")
            .and_then(Value::as_array)
            .and_then(|t| t.first());
        let task = owned(&args, "task")
            .or_else(|| owned(&args, "prompt"))
            .or_else(|| first.and_then(|t| owned(t, "task")));
        let failed = ext.get("is_error").and_then(Value::as_bool) == Some(true);
        // An async launch's result only acknowledges it; `notify` is its report.
        let running = ext.pointer("/result/details/asyncId").is_some() && ext.get("notify").is_none();
        return ItemDetail::Subagent {
            agent: owned(&args, "agent").or_else(|| first.and_then(|t| owned(t, "agent"))),
            description: owned(&args, "description")
                .or_else(|| task.as_deref().map(|t| crate::wire::one_line(t, 160)))
                .unwrap_or_default(),
            prompt: task,
            child: None,
            status: if running {
                Some("running".to_owned())
            } else {
                output
                    .as_ref()
                    .map(|_| if failed { "failed" } else { "completed" }.to_owned())
            },
            output: output.or_else(|| (!text.is_empty()).then(|| text.to_owned())),
            model: None,
        };
    }
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

/// Whether an ACP tool call runs another agent: opencode's `task` (input
/// `subagent_type`/`agent`, `description`, `prompt`), and agents that name
/// the tool `subagent`, `task` or `Task` with an agent and a prompt.
pub(crate) fn acp_is_subagent(update: &Value) -> bool {
    let input = update.get("rawInput").unwrap_or(&Value::Null);
    let title = str_at(update, "title").unwrap_or_default();
    let named = |k: &str| input.get(k).is_some_and(|v| v.is_string());
    (named("subagent_type") && (named("prompt") || named("description")))
        || (named("agent") && (named("task") || named("prompt")))
        || matches!(title, "task" | "Task" | "subagent" | "Subagent" | "Agent")
        || title.ends_with(" Subagent")
        || title.contains(" Subagent \u{2014} ")
}

/// A subagent tool's output: opencode 2.0 wraps the report in
/// `<subagent sessionID="…" state="…">`, 1.x starts with `task_id: ses_…`
/// and wraps it in `<task_result>`. Other agents return the report bare.
fn acp_task_output(text: &str) -> (Option<String>, Option<String>) {
    let attr = |tag: &str, name: &str| {
        let open = text.find(tag)?;
        let head = &text[open..open + text[open..].find('>')?];
        let start = head.find(&format!("{name}=\""))? + name.len() + 2;
        head[start..].split('"').next().map(str::to_owned)
    };
    let child = attr("<subagent", "sessionID").or_else(|| {
        text.lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .and_then(|rest| rest.split_whitespace().next())
            .map(str::to_owned)
    });
    let inner = |open: &str, close: &str| {
        let start = text.find(open)?;
        let body = &text[start..];
        let body = &body[body.find('>')? + 1..];
        Some(body.split(close).next().unwrap_or(body))
    };
    let report = inner("<subagent", "</subagent>")
        .or_else(|| inner("<task_result", "</task_result>"))
        .unwrap_or(text)
        .trim();
    (child, (!report.is_empty()).then(|| report.to_owned()))
}

fn acp_subagent(ext: &Value, output: Option<String>, text: &str) -> ItemDetail {
    let null = Value::Null;
    let input = ext.get("rawInput").unwrap_or(&null);
    let raw_out = ext.get("rawOutput").unwrap_or(&null);
    let (child_in_text, report) = output.as_deref().map(acp_task_output).unwrap_or_default();
    let child = raw_out
        .pointer("/metadata/sessionID")
        .or_else(|| raw_out.pointer("/metadata/sessionId"))
        .or_else(|| raw_out.get("sessionId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or(child_in_text);
    let title = owned(ext, "title").filter(|t| !t.is_empty());
    ItemDetail::Subagent {
        agent: owned(input, "subagent_type").or_else(|| owned(input, "agent")),
        description: owned(input, "description").or(title).unwrap_or_default(),
        prompt: owned(input, "prompt").or_else(|| owned(input, "task")),
        child,
        status: raw_out
            .pointer("/metadata/status")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| owned(ext, "status")),
        output: report.or_else(|| (!text.is_empty()).then(|| text.to_owned())),
        model: raw_out
            .pointer("/metadata/model/modelID")
            .and_then(Value::as_str)
            .map(str::to_owned),
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
    if kind == ItemKind::Subagent {
        return acp_subagent(ext, output, text);
    }
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
    fn claude_subagent_detail_unwraps_the_hand_back() {
        let ext = json!({"name": "Agent",
            "input": {"subagent_type": "Explore", "description": "Find it", "prompt": "Look for x"},
            "content": [{"type": "text", "text": "[Subagent hand-back] preamble. The report follows:\n  Found x\n  in a.rs"}],
            "result": {"status": "completed", "agentId": "a1", "resolvedModel": "claude-haiku-4-5"}});
        assert_eq!(
            detail(Harness::Claude, ItemKind::Subagent, &ext, ""),
            ItemDetail::Subagent {
                agent: Some("Explore".into()),
                description: "Find it".into(),
                prompt: Some("Look for x".into()),
                child: Some("a1".into()),
                status: Some("completed".into()),
                output: Some("Found x\nin a.rs".into()),
                model: Some("claude-haiku-4-5".into()),
            }
        );
        // A background launch: no report yet, the task frames' summary instead.
        let ext = json!({"name": "Agent", "input": {"description": "Bg"},
            "content": [{"type": "text", "text": "Async agent launched successfully. agentId: a2"}],
            "task": {"task_id": "a2", "status": "running", "summary": "Reading files"}});
        let ItemDetail::Subagent {
            output,
            status,
            child,
            ..
        } = detail(Harness::Claude, ItemKind::Subagent, &ext, "")
        else {
            panic!()
        };
        assert_eq!(output.as_deref(), Some("Reading files"));
        assert_eq!(status.as_deref(), Some("running"));
        assert_eq!(child.as_deref(), Some("a2"));
    }

    #[test]
    fn acp_subagent_outputs_of_both_opencode_versions() {
        let v1 = "task_id: ses_abc (for resuming to continue this task if needed)\n\n<task_result>\nDone.\n</task_result>";
        assert_eq!(
            acp_task_output(v1),
            (Some("ses_abc".into()), Some("Done.".into()))
        );
        let v2 = "<subagent sessionID=\"ses_def\" state=\"completed\">\nFiles: a\n</subagent>";
        assert_eq!(
            acp_task_output(v2),
            (Some("ses_def".into()), Some("Files: a".into()))
        );
        assert_eq!(acp_task_output("plain"), (None, Some("plain".into())));
        assert!(acp_is_subagent(&json!({"title": "subagent", "rawInput": {}})));
        assert!(acp_is_subagent(
            &json!({"title": "x", "rawInput": {"subagent_type": "general", "prompt": "p"}})
        ));
        assert!(!acp_is_subagent(
            &json!({"title": "read", "rawInput": {"path": "a"}})
        ));
    }

    #[test]
    fn pi_subagent_tool() {
        let ext = json!({"tool": "subagent", "args": {"agent": "scout", "task": "Map the repo"},
            "result": {"content": [{"type": "text", "text": "Mapped."}]}, "is_error": false});
        let ItemDetail::Subagent {
            agent,
            prompt,
            output,
            status,
            ..
        } = detail(Harness::Pi, ItemKind::Subagent, &ext, "")
        else {
            panic!()
        };
        assert_eq!(agent.as_deref(), Some("scout"));
        assert_eq!(prompt.as_deref(), Some("Map the repo"));
        assert_eq!(output.as_deref(), Some("Mapped."));
        assert_eq!(status.as_deref(), Some("completed"));
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

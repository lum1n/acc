//! Carrying a conversation over to another harness.
//!
//! When one agent can't go on (out of quota, the wrong model for the job),
//! [`crate::command::Command::Handoff`] starts another harness on the same
//! directory, primed with what happened so far. [`primer`] writes that
//! history as the new session's first prompt: messages, the commands that
//! ran and the files that changed, newest kept whole when it has to be cut.
//! Every harness takes a prompt, so this works from any harness to any
//! other; files the first agent changed are already on disk.

use crate::event::ItemKind;
use crate::harness::Harness;
use crate::item::ItemDetail;
use crate::transcript::{Item, Transcript};

/// Default size of a primer, in characters: room for a long conversation
/// while leaving the new agent most of its context window.
pub const PRIMER_CHARS: usize = 60_000;

/// Longest any one entry gets; longer text keeps its start and its end.
const ENTRY_CHARS: usize = 4_000;

/// The first prompt for a harness taking over `transcript` from `from`,
/// at most about `budget` characters. `next` is what the user asks the
/// new agent to do; without it the agent is asked to sum up and wait.
pub fn primer(
    transcript: &Transcript,
    from: Harness,
    cwd: &str,
    next: Option<&str>,
    budget: usize,
) -> String {
    let entries: Vec<String> = transcript.top_level().filter_map(|i| entry(i, from)).collect();
    let mut kept = Vec::new();
    let mut used = 0;
    for e in entries.iter().rev() {
        if used + e.len() > budget && !kept.is_empty() {
            break;
        }
        used += e.len();
        kept.push(e.as_str());
    }
    kept.reverse();
    let left_out = entries.len() - kept.len();
    let mut out = format!(
        "This conversation started in {} and continues here with you. Its tool calls ran in this \
         working directory ({cwd}), so any files it changed are already on disk.\n\n",
        from.display_name()
    );
    if left_out > 0 {
        out.push_str(&format!(
            "The conversation so far, oldest first (the first {left_out} entries were left out):\n\n"
        ));
    } else {
        out.push_str("The conversation so far, oldest first:\n\n");
    }
    for e in kept {
        out.push_str(e);
        out.push_str("\n\n");
    }
    match next.map(str::trim).filter(|n| !n.is_empty()) {
        Some(next) => {
            out.push_str("The user's next message:\n\n");
            out.push_str(next);
        }
        None => {
            out.push_str("Sum up in a few lines where things stand, then wait for the user's next message.")
        }
    }
    out
}

/// One item as a primer entry, or `None` for items with nothing to carry
/// over (reasoning, empty messages).
fn entry(item: &Item, harness: Harness) -> Option<String> {
    let text = item.text.trim();
    let e = match item.kind {
        ItemKind::Reasoning => return None,
        ItemKind::UserMessage if !text.is_empty() => format!("[user]\n{}", clip(text)),
        ItemKind::AgentMessage if !text.is_empty() => format!("[assistant]\n{}", clip(text)),
        ItemKind::UserMessage | ItemKind::AgentMessage => return None,
        _ => match item.detail(harness) {
            ItemDetail::Command {
                command,
                output,
                exit_code,
                ..
            } => {
                let status = exit_code.map_or_else(String::new, |c| format!(" (exit {c})"));
                let output = output.trim();
                if command.trim().is_empty() && output.is_empty() {
                    return None;
                }
                if output.is_empty() {
                    format!("[ran] {command}{status}")
                } else {
                    format!("[ran] {command}{status}\n{}", clip(output))
                }
            }
            ItemDetail::FileChange { files } if !files.is_empty() => {
                let paths: Vec<String> = files
                    .iter()
                    .map(|f| format!("{} (+{} -{})", f.path, f.added, f.removed))
                    .collect();
                format!("[changed files] {}", paths.join(", "))
            }
            ItemDetail::Tool { name, title, .. } => {
                if title.is_empty() {
                    format!("[tool] {name}")
                } else {
                    format!("[tool] {name}: {title}")
                }
            }
            ItemDetail::Subagent {
                agent,
                description,
                output,
                ..
            } => {
                let who = agent.map_or_else(|| "subagent".to_owned(), |a| format!("subagent {a}"));
                match output.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
                    Some(report) => format!("[{who}] {description}\nReported:\n{}", clip(report)),
                    None => format!("[{who}] {description}"),
                }
            }
            ItemDetail::FileChange { .. } | ItemDetail::Text => return None,
        },
    };
    Some(e)
}

/// `text` cut to [`ENTRY_CHARS`], keeping its start and end.
fn clip(text: &str) -> String {
    let n = text.chars().count();
    if n <= ENTRY_CHARS {
        return text.to_owned();
    }
    let half = ENTRY_CHARS / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text.chars().skip(n - half).collect();
    format!("{head}\n[... {} characters left out ...]\n{tail}", n - 2 * half)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::event::Event;
    use serde_json::{Value, json};

    fn add(t: &mut Transcript, id: &str, kind: ItemKind, text: &str, ext: Value) {
        t.apply(&Event::ItemStarted {
            id: id.into(),
            kind,
            parent: None,
            extensions: Value::Null,
        });
        if !text.is_empty() {
            t.apply(&Event::ItemDelta {
                id: id.into(),
                text: text.into(),
            });
        }
        t.apply(&Event::ItemCompleted {
            id: id.into(),
            extensions: ext,
        });
    }

    #[test]
    fn primer_carries_messages_and_commands_and_cuts_oldest_first() {
        let mut t = Transcript::new();
        add(&mut t, "u1", ItemKind::UserMessage, "fix the tests", Value::Null);
        add(&mut t, "r1", ItemKind::Reasoning, "secret thoughts", Value::Null);
        add(
            &mut t,
            "c1",
            ItemKind::CommandExecution,
            "",
            json!({"type": "commandExecution", "command": "cargo test", "aggregatedOutput": "1 failed", "exitCode": 101}),
        );
        add(
            &mut t,
            "a1",
            ItemKind::AgentMessage,
            "One test fails.",
            Value::Null,
        );
        let p = primer(&t, Harness::Codex, "/w", Some("go on"), PRIMER_CHARS);
        assert!(p.starts_with("This conversation started in Codex"), "{p}");
        assert!(p.contains("[user]\nfix the tests"));
        assert!(!p.contains("secret"));
        assert!(p.contains("[ran] cargo test (exit 101)\n1 failed"), "{p}");
        assert!(p.find("[user]").unwrap() < p.find("[assistant]").unwrap());
        assert!(p.ends_with("The user's next message:\n\ngo on"));

        let short = primer(&t, Harness::Codex, "/w", None, 30);
        assert!(short.contains("the first 2 entries were left out"), "{short}");
        assert!(short.contains("One test fails.") && !short.contains("fix the tests"));
        assert!(short.ends_with("wait for the user's next message."));
    }

    #[test]
    fn long_entries_keep_their_ends() {
        let long = format!("start{}end", "x".repeat(10_000));
        let c = clip(&long);
        assert!(c.starts_with("start") && c.ends_with("end") && c.len() < 4_100);
    }
}

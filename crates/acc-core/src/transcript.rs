//! A session's conversation as an ordered list of items.
//!
//! [`Transcript`] folds the item events of any driver or observer into items
//! with their accumulated text and merged extensions, and reports what
//! changed so a UI can update one row instead of re-reading everything.
//! Deltas are passed on as-is, never as the whole text, so a long streamed
//! answer costs O(n) to follow rather than O(n²).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::{Event, ItemId, ItemKind};
use crate::harness::Harness;
use crate::item::{ItemDetail, detail};
use crate::state::RunState;
use crate::wire::{owned, str_at};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Running,
    Completed,
    Failed,
}

/// One message, reasoning block, tool call, command or file change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub kind: ItemKind,
    pub status: ItemStatus,
    /// Streamed text: message content, reasoning, or command output.
    pub text: String,
    /// The start extensions with every update and the completion merged
    /// over them (later keys win). Harness-native JSON.
    pub ext: Value,
    /// The tool call this item ran inside (subagents), when reported.
    pub parent: Option<ItemId>,
    /// Position in the session, for stable ordering across reloads.
    pub seq: u64,
}

impl Item {
    /// The harness-independent view of this item.
    pub fn detail(&self, harness: Harness) -> ItemDetail {
        detail(harness, self.kind, &self.ext, &self.text)
    }
}

/// What an event did to the transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ItemChange {
    Started {
        id: ItemId,
    },
    /// Text appended to the item.
    Delta {
        id: ItemId,
        text: String,
    },
    Updated {
        id: ItemId,
    },
    Completed {
        id: ItemId,
    },
}

impl ItemChange {
    pub fn id(&self) -> &str {
        match self {
            ItemChange::Started { id }
            | ItemChange::Delta { id, .. }
            | ItemChange::Updated { id }
            | ItemChange::Completed { id } => id,
        }
    }
}

/// The items of one session, in order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "Stored")]
pub struct Transcript {
    items: Vec<Item>,
    #[serde(skip)]
    index: HashMap<ItemId, usize>,
    next_seq: u64,
}

/// A deserialized [`Transcript`] before its id index is rebuilt.
#[derive(Deserialize)]
struct Stored {
    items: Vec<Item>,
    next_seq: u64,
}

impl From<Stored> for Transcript {
    fn from(s: Stored) -> Self {
        let mut t = Self {
            items: s.items,
            index: HashMap::new(),
            next_seq: s.next_seq,
        };
        t.reindex();
        t
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn get(&self, id: &str) -> Option<&Item> {
        self.index.get(id).map(|&i| &self.items[i])
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The items of the main conversation: everything not inside a subagent run.
    pub fn top_level(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| i.parent.is_none())
    }

    /// The items a subagent run produced, in order: the child conversation
    /// of the [`ItemKind::Subagent`] item `parent`.
    pub fn children<'a>(&'a self, parent: &'a str) -> impl Iterator<Item = &'a Item> + 'a {
        self.items
            .iter()
            .filter(move |i| i.parent.as_deref() == Some(parent))
    }

    /// Every subagent run in the session, in order.
    pub fn subagents(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| i.kind == ItemKind::Subagent)
    }

    /// Rebuilds the id index (deserializing does it already).
    pub fn reindex(&mut self) {
        self.index = self
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| (item.id.clone(), i))
            .collect();
    }

    /// Items still running when the harness says a turn ended or the
    /// process exited can never finish; mark them done.
    pub fn close_running(&mut self) -> Vec<ItemChange> {
        self.items
            .iter_mut()
            .filter(|i| i.status == ItemStatus::Running)
            .map(|i| {
                i.status = ItemStatus::Completed;
                ItemChange::Completed { id: i.id.clone() }
            })
            .collect()
    }

    /// [`Transcript::close_running`] when `event` leaves nothing running:
    /// the turn ended, the session went idle (interrupted), finished,
    /// failed, or the process exited. Harnesses don't always complete
    /// every tool call they started, which would otherwise spin forever.
    ///
    /// A subagent launched in the background outlives the turn that started
    /// it, so it stays running until it reports back or the process exits.
    pub fn settle(&mut self, event: &Event) -> Vec<ItemChange> {
        let exits = matches!(event, Event::Exited { .. })
            || matches!(event, Event::StateChanged(s) if s.state == RunState::Exited);
        let ends = exits
            || match event {
                Event::TurnCompleted => true,
                Event::StateChanged(s) => {
                    matches!(
                        s.state,
                        RunState::Idle | RunState::Done | RunState::Waiting | RunState::Error
                    )
                }
                _ => false,
            };
        if !ends {
            return Vec::new();
        }
        self.items
            .iter_mut()
            .filter(|i| i.status == ItemStatus::Running && (exits || !in_background(i)))
            .map(|i| {
                i.status = ItemStatus::Completed;
                ItemChange::Completed { id: i.id.clone() }
            })
            .collect()
    }

    fn slot(&mut self, id: &str, kind: ItemKind) -> (&mut Item, bool) {
        if let Some(&i) = self.index.get(id) {
            return (&mut self.items[i], false);
        }
        self.next_seq += 1;
        self.index.insert(id.to_owned(), self.items.len());
        self.items.push(Item {
            id: id.to_owned(),
            kind,
            status: ItemStatus::Running,
            text: String::new(),
            ext: Value::Null,
            parent: None,
            seq: self.next_seq,
        });
        let last = self.items.len() - 1;
        (&mut self.items[last], true)
    }

    /// Applies one event. Returns the change, or `None` for events that
    /// aren't about items (or refer to an unknown item).
    pub fn apply(&mut self, event: &Event) -> Option<ItemChange> {
        match event {
            Event::ItemStarted {
                id,
                kind,
                parent: reported,
                extensions,
            } => {
                let (item, new) = self.slot(id, *kind);
                item.kind = *kind;
                merge(&mut item.ext, extensions);
                item.parent = reported
                    .clone()
                    .or_else(|| parent(extensions))
                    .or(item.parent.take());
                Some(if new {
                    ItemChange::Started { id: id.clone() }
                } else {
                    ItemChange::Updated { id: id.clone() }
                })
            }
            Event::ItemDelta { id, text } => {
                let &i = self.index.get(id)?;
                self.items[i].text.push_str(text);
                Some(ItemChange::Delta {
                    id: id.clone(),
                    text: text.clone(),
                })
            }
            Event::ItemUpdated { id, extensions } => {
                let &i = self.index.get(id)?;
                merge(&mut self.items[i].ext, extensions);
                Some(ItemChange::Updated { id: id.clone() })
            }
            Event::ItemCompleted { id, extensions } => {
                let &i = self.index.get(id)?;
                let item = &mut self.items[i];
                merge(&mut item.ext, extensions);
                item.status = if failed(extensions) {
                    ItemStatus::Failed
                } else {
                    ItemStatus::Completed
                };
                // Some harnesses only put the final text in the completion.
                if item.text.is_empty()
                    && matches!(
                        item.kind,
                        ItemKind::AgentMessage | ItemKind::UserMessage | ItemKind::Reasoning
                    )
                    && let Some(text) = owned(extensions, "text")
                {
                    item.text = text;
                }
                Some(ItemChange::Completed { id: id.clone() })
            }
            _ => None,
        }
    }
}

/// Shallow merge: object keys from `from` overwrite `into`; a non-object
/// replaces it; null leaves it alone.
fn merge(into: &mut Value, from: &Value) {
    match (into, from) {
        (_, Value::Null) => {}
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                if !v.is_null() {
                    a.insert(k.clone(), v.clone());
                }
            }
        }
        (slot, v) => *slot = v.clone(),
    }
}

fn parent(ext: &Value) -> Option<ItemId> {
    owned(ext, "parent_tool_use_id").or_else(|| owned(ext, "parentToolCallId"))
}

/// A subagent the harness runs in the background (Claude's `isAsync`
/// launch, a `task` record still running, a pi-subagents `async: true` run).
fn in_background(item: &Item) -> bool {
    item.kind == ItemKind::Subagent
        && (item.ext.pointer("/result/isAsync").and_then(Value::as_bool) == Some(true)
            || item.ext.pointer("/task/is_backgrounded").and_then(Value::as_bool) == Some(true)
            // pi-subagents' `async: true` launch.
            || item.ext.pointer("/result/details/asyncId").is_some()
            // A Codex 0.162 spawn until its `subAgentActivity` says it stopped.
            || (str_at(&item.ext, "type") == Some("subAgentActivity") && item.ext.get("childStatus").is_none()))
}

fn failed(ext: &Value) -> bool {
    ext.get("is_error").and_then(Value::as_bool) == Some(true)
        || matches!(
            str_at(ext, "status"),
            Some("failed" | "declined" | "error" | "rejected")
        )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn folds_start_deltas_and_completion() {
        let mut t = Transcript::new();
        let id = "m1".to_owned();
        assert_eq!(
            t.apply(&Event::ItemStarted {
                id: id.clone(),
                kind: ItemKind::AgentMessage,
                parent: None,
                extensions: Value::Null
            }),
            Some(ItemChange::Started { id: id.clone() })
        );
        t.apply(&Event::ItemDelta {
            id: id.clone(),
            text: "he".into(),
        });
        t.apply(&Event::ItemDelta {
            id: id.clone(),
            text: "llo".into(),
        });
        t.apply(&Event::ItemCompleted {
            id: id.clone(),
            extensions: Value::Null,
        });
        let item = t.get("m1").unwrap();
        assert_eq!(
            (item.text.as_str(), item.status),
            ("hello", ItemStatus::Completed)
        );
    }

    #[test]
    fn merges_tool_extensions_and_marks_failure() {
        let mut t = Transcript::new();
        t.apply(&Event::ItemStarted {
            id: "t".into(),
            kind: ItemKind::CommandExecution,
            parent: None,
            extensions: json!({"name": "Bash", "input": {"command": "false"}, "parent_tool_use_id": "p"}),
        });
        t.apply(&Event::ItemCompleted {
            id: "t".into(),
            extensions: json!({"is_error": true, "content": "exit 1"}),
        });
        let item = t.get("t").unwrap();
        assert_eq!(item.status, ItemStatus::Failed);
        assert_eq!(item.ext["input"]["command"], "false");
        assert_eq!(item.ext["content"], "exit 1");
        assert_eq!(item.parent.as_deref(), Some("p"));
        assert!(matches!(item.detail(Harness::Claude), ItemDetail::Command { .. }));
    }

    #[test]
    fn subagent_children_and_top_level() {
        let mut t = Transcript::new();
        let start = |id: &str, kind, parent: Option<&str>, ext: Value| Event::ItemStarted {
            id: id.into(),
            kind,
            parent: parent.map(Into::into),
            extensions: ext,
        };
        t.apply(&start("p", ItemKind::UserMessage, None, Value::Null));
        t.apply(&start("s", ItemKind::Subagent, None, Value::Null));
        t.apply(&start("c1", ItemKind::ToolCall, Some("s"), Value::Null));
        // The extension key still works for logs written before `parent`.
        t.apply(&start(
            "c2",
            ItemKind::AgentMessage,
            None,
            json!({"parent_tool_use_id": "s"}),
        ));
        t.apply(&start("a", ItemKind::AgentMessage, None, Value::Null));
        let ids = |it: Vec<&Item>| it.iter().map(|i| i.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(t.top_level().collect()), ["p", "s", "a"]);
        assert_eq!(ids(t.children("s").collect()), ["c1", "c2"]);
        assert_eq!(ids(t.subagents().collect()), ["s"]);
        // Old serialized events without `parent` still parse.
        let old: Event =
            serde_json::from_str(r#"{"type":"item_started","id":"x","kind":"tool_call"}"#).unwrap();
        assert!(matches!(old, Event::ItemStarted { parent: None, .. }));
    }

    #[test]
    fn background_subagents_outlive_the_turn() {
        let mut t = Transcript::new();
        t.apply(&Event::ItemStarted {
            id: "bg".into(),
            kind: ItemKind::Subagent,
            parent: None,
            extensions: json!({"name": "Agent"}),
        });
        t.apply(&Event::ItemUpdated {
            id: "bg".into(),
            extensions: json!({"result": {"isAsync": true, "agentId": "a1"}}),
        });
        assert!(t.settle(&Event::TurnCompleted).is_empty());
        assert_eq!(t.get("bg").unwrap().status, ItemStatus::Running);
        assert_eq!(t.settle(&Event::Exited { code: Some(0) }).len(), 1);
    }

    #[test]
    fn codex_spawns_outlive_the_turn_until_their_activity_completes() {
        let mut t = Transcript::new();
        let spawn = json!({"type": "subAgentActivity", "kind": "started", "agentThreadId": "c1"});
        t.apply(&Event::ItemStarted {
            id: "s".into(),
            kind: ItemKind::Subagent,
            parent: None,
            extensions: spawn.clone(),
        });
        t.apply(&Event::ItemUpdated {
            id: "s".into(),
            extensions: spawn,
        });
        assert!(t.settle(&Event::TurnCompleted).is_empty());
        t.apply(&Event::ItemCompleted {
            id: "s".into(),
            extensions: json!({"childStatus": "completed", "report": "pong"}),
        });
        assert_eq!(t.get("s").unwrap().status, ItemStatus::Completed);
    }

    #[test]
    fn turn_end_closes_tools_left_running() {
        let mut t = Transcript::new();
        t.apply(&Event::ItemStarted {
            id: "t".into(),
            kind: ItemKind::ToolCall,
            parent: None,
            extensions: Value::Null,
        });
        let working = Event::StateChanged(crate::state::SessionState {
            state: RunState::Working { progress: None },
            message: None,
            source: crate::state::StateSource::Native,
            observed_at_ms: 0,
        });
        assert!(t.settle(&working).is_empty());
        assert_eq!(
            t.settle(&Event::TurnCompleted),
            [ItemChange::Completed { id: "t".into() }]
        );
        assert_eq!(t.get("t").unwrap().status, ItemStatus::Completed);
    }

    #[test]
    fn unknown_items_and_other_events_are_ignored() {
        let mut t = Transcript::new();
        assert!(
            t.apply(&Event::ItemDelta {
                id: "x".into(),
                text: "y".into()
            })
            .is_none()
        );
        assert!(t.apply(&Event::TurnCompleted).is_none());
        assert!(t.is_empty());
    }
}

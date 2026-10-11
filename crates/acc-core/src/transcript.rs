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
pub struct Transcript {
    items: Vec<Item>,
    #[serde(skip)]
    index: HashMap<ItemId, usize>,
    next_seq: u64,
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

    /// Rebuilds the id index after deserializing.
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
            Event::ItemStarted { id, kind, extensions } => {
                let (item, new) = self.slot(id, *kind);
                item.kind = *kind;
                merge(&mut item.ext, extensions);
                item.parent = parent(extensions).or(item.parent.take());
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

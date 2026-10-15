//! A session's conversation as an ordered list of items.
//!
//! [`Transcript`] folds the item events of any driver into items with their
//! accumulated text and latest detail, and reports what changed so a UI can
//! update one row instead of re-reading everything.
//! Deltas are passed on as-is, never as the whole text, so a long streamed
//! answer costs O(n) to follow rather than O(n²).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::event::ItemStatus;
use crate::event::{Event, ItemId, ItemKind};
use crate::item::{ItemDetail, merge};
use crate::state::RunState;

/// One message, reasoning block, tool call, command or file change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub kind: ItemKind,
    pub status: ItemStatus,
    /// Streamed text: message content, reasoning, or command output.
    pub text: String,
    /// What the item is, the same shape on every harness.
    pub detail: ItemDetail,
    /// The harness's own JSON, every frame merged over the start (later
    /// keys win). For debugging and harness-specific UI only.
    pub native: Value,
    /// The tool call this item ran inside (subagents), when reported.
    pub parent: Option<ItemId>,
    /// Position in the session, for stable ordering across reloads.
    pub seq: u64,
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
            .filter(|i| {
                i.status == ItemStatus::Running
                    && (exits || !matches!(i.detail, ItemDetail::Subagent { background: true, .. }))
            })
            .map(|i| {
                i.status = ItemStatus::Completed;
                ItemChange::Completed { id: i.id.clone() }
            })
            .collect()
    }

    /// The `ItemCompleted` event for an item the transcript closed itself
    /// ([`Transcript::settle`], [`Transcript::close_running`]), as if the
    /// harness had completed it.
    pub fn completion(&self, change: &ItemChange) -> Event {
        let item = self.get(change.id());
        Event::ItemCompleted {
            id: change.id().to_owned(),
            status: item.map_or(ItemStatus::Completed, |i| i.status),
            detail: item.map_or(ItemDetail::Text, |i| i.detail.clone()),
            native: Value::Null,
        }
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
            detail: ItemDetail::Text,
            native: Value::Null,
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
                parent,
                detail,
                native,
            } => {
                let (item, new) = self.slot(id, *kind);
                item.kind = *kind;
                item.detail = detail.clone();
                merge(&mut item.native, native);
                if parent.is_some() {
                    item.parent.clone_from(parent);
                }
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
            Event::ItemUpdated { id, detail, native } => {
                let &i = self.index.get(id)?;
                let item = &mut self.items[i];
                item.detail = detail.clone();
                merge(&mut item.native, native);
                Some(ItemChange::Updated { id: id.clone() })
            }
            Event::ItemCompleted {
                id,
                status,
                detail,
                native,
            } => {
                let &i = self.index.get(id)?;
                let item = &mut self.items[i];
                item.status = *status;
                item.detail = detail.clone();
                merge(&mut item.native, native);
                Some(ItemChange::Completed { id: id.clone() })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::harness::Harness;
    use crate::item::ItemFeed;
    use serde_json::json;

    /// A transcript fed the way a driver feeds it: native frames through an
    /// [`ItemFeed`].
    struct Fed {
        feed: ItemFeed,
        t: Transcript,
    }

    impl Fed {
        fn new(h: Harness) -> Self {
            Self {
                feed: ItemFeed::new(h),
                t: Transcript::new(),
            }
        }

        fn run(&mut self, f: impl FnOnce(&mut ItemFeed, &mut Vec<Event>)) -> Vec<ItemChange> {
            let mut out = Vec::new();
            f(&mut self.feed, &mut out);
            out.iter().filter_map(|e| self.t.apply(e)).collect()
        }
    }

    #[test]
    fn folds_start_deltas_and_completion() {
        let mut f = Fed::new(Harness::Claude);
        assert_eq!(
            f.run(|i, o| i.start("m1", ItemKind::AgentMessage, None, Value::Null, o)),
            [ItemChange::Started { id: "m1".into() }]
        );
        f.run(|i, o| i.delta("m1", "he", o));
        f.run(|i, o| i.delta("m1", "llo", o));
        f.run(|i, o| i.complete("m1", Value::Null, o));
        let item = f.t.get("m1").unwrap();
        assert_eq!(
            (item.text.as_str(), item.status),
            ("hello", ItemStatus::Completed)
        );
    }

    #[test]
    fn text_sent_only_on_completion_arrives_as_a_delta() {
        let mut f = Fed::new(Harness::Codex);
        f.run(|i, o| i.start("a", ItemKind::AgentMessage, None, Value::Null, o));
        let changes = f.run(|i, o| i.complete("a", json!({"text": "hi"}), o));
        assert_eq!(
            changes,
            [
                ItemChange::Delta {
                    id: "a".into(),
                    text: "hi".into()
                },
                ItemChange::Completed { id: "a".into() }
            ]
        );
        assert_eq!(f.t.get("a").unwrap().text, "hi");
    }

    #[test]
    fn merges_tool_frames_and_marks_failure() {
        let mut f = Fed::new(Harness::Claude);
        f.run(|i, o| {
            i.start(
                "t",
                ItemKind::CommandExecution,
                None,
                json!({"name": "Bash", "input": {"command": "false"}, "parent_tool_use_id": "p"}),
                o,
            )
        });
        f.run(|i, o| i.complete("t", json!({"is_error": true, "content": "exit 1"}), o));
        let item = f.t.get("t").unwrap();
        assert_eq!(item.status, ItemStatus::Failed);
        assert_eq!(item.native["input"]["command"], "false");
        assert_eq!(item.native["content"], "exit 1");
        assert_eq!(item.parent.as_deref(), Some("p"));
        assert_eq!(
            item.detail,
            ItemDetail::Command {
                tool: "Bash".into(),
                command: "false".into(),
                cwd: None,
                output: "exit 1".into(),
                exit_code: None,
            }
        );
    }

    #[test]
    fn subagent_children_and_top_level() {
        let mut f = Fed::new(Harness::Claude);
        let mut start = |id: &str, kind, parent: Option<&str>, ext: Value| {
            f.run(|i, o| i.start(id, kind, parent.map(Into::into), ext, o));
        };
        start("p", ItemKind::UserMessage, None, Value::Null);
        start("s", ItemKind::Subagent, None, Value::Null);
        start("c1", ItemKind::ToolCall, Some("s"), Value::Null);
        // The harness's own parent key counts when the driver passes none.
        start(
            "c2",
            ItemKind::AgentMessage,
            None,
            json!({"parent_tool_use_id": "s"}),
        );
        start("a", ItemKind::AgentMessage, None, Value::Null);
        let ids = |it: Vec<&Item>| it.iter().map(|i| i.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(f.t.top_level().collect()), ["p", "s", "a"]);
        assert_eq!(ids(f.t.children("s").collect()), ["c1", "c2"]);
        assert_eq!(ids(f.t.subagents().collect()), ["s"]);
    }

    #[test]
    fn background_subagents_outlive_the_turn() {
        let mut f = Fed::new(Harness::Claude);
        f.run(|i, o| i.start("bg", ItemKind::Subagent, None, json!({"name": "Agent"}), o));
        f.run(|i, o| i.update("bg", json!({"result": {"isAsync": true, "agentId": "a1"}}), o));
        assert!(f.t.settle(&Event::TurnCompleted).is_empty());
        assert_eq!(f.t.get("bg").unwrap().status, ItemStatus::Running);
        assert_eq!(f.t.settle(&Event::Exited { code: Some(0) }).len(), 1);
    }

    #[test]
    fn codex_spawns_outlive_the_turn_until_their_activity_completes() {
        let mut f = Fed::new(Harness::Codex);
        let spawn = json!({"type": "subAgentActivity", "kind": "started", "agentThreadId": "c1"});
        f.run(|i, o| i.start("s", ItemKind::Subagent, None, spawn.clone(), o));
        f.run(|i, o| i.update("s", spawn, o));
        assert!(f.t.settle(&Event::TurnCompleted).is_empty());
        f.run(|i, o| i.complete("s", json!({"childStatus": "completed", "report": "pong"}), o));
        let item = f.t.get("s").unwrap();
        assert_eq!(item.status, ItemStatus::Completed);
        assert!(matches!(
            &item.detail,
            ItemDetail::Subagent { output: Some(o), background: false, .. } if o == "pong"
        ));
    }

    #[test]
    fn turn_end_closes_tools_left_running() {
        let mut f = Fed::new(Harness::Claude);
        f.run(|i, o| i.start("t", ItemKind::ToolCall, None, Value::Null, o));
        let working = Event::StateChanged(crate::state::SessionState {
            state: RunState::Working { progress: None },
            message: None,
            source: crate::state::StateSource::Native,
            observed_at_ms: 0,
        });
        assert!(f.t.settle(&working).is_empty());
        assert_eq!(
            f.t.settle(&Event::TurnCompleted),
            [ItemChange::Completed { id: "t".into() }]
        );
        assert_eq!(f.t.get("t").unwrap().status, ItemStatus::Completed);
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

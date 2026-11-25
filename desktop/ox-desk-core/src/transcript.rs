//! A transcript is owned by its thread even when a different thread is selected.
pub mod item;
pub mod review;
use crate::codex::history::TurnPage;
use item::Item;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;

#[derive(Clone, Copy, Debug)]
pub enum DeltaTarget {
    Agent,
    Plan,
    Command,
    File,
    Summary(usize),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ItemKey {
    pub turn: String,
    pub item: String,
}

pub enum Row {
    Message(ItemKey),
    Tools(VecDeque<ItemKey>),
}
pub struct Entry {
    pub item: Item,
    pub revision: u64,
    pub output: String,
    position: i64,
}

#[derive(Default)]
pub struct Transcript {
    pub reviews: review::Reviews,
    pub usage: Option<crate::usage::TokenUsage>,
    order: VecDeque<Row>,
    entries: HashMap<ItemKey, Entry>,
    front: i64,
    remeasure: HashSet<i64>,
    cursors: HashSet<String>,
    pub(crate) cursor: Option<String>,
    pub(crate) loading: bool,
    pub(crate) ready: bool,
    pub(crate) unread: bool,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) send_options: bool,
    pub(crate) tier: Option<String>,
    pub(crate) command_notice: Option<String>,
    pub(crate) access: Option<crate::codex::access::Access>,
    pub(crate) error: Option<String>,
    pub(crate) active_turn: Option<String>,
    // Number of history rows inserted at the front, used to preserve the viewport.
    prepended: usize,
    pub(crate) last_finished: Option<String>,
    /// The turn the user asked to stop, so its completion reads as stopped, not done.
    pub(crate) stop_requested: Option<String>,
    /// The last finished turn was interrupted by the user.
    pub(crate) interrupted: bool,
    pub(crate) verified: bool,
    /// Another client holds this thread's write lock (Codex refuses a second resume).
    pub(crate) native_live: bool,
    pub(crate) native_next: Option<std::time::Instant>,
    /// `updatedAt` from the last metadata read, and from the read the shown history is of.
    pub(crate) read_updated: i64,
    pub(crate) loaded_updated: i64,
    pub(crate) before_resume: Vec<crate::codex::activity::Activity>,
}

impl Transcript {
    pub fn len(&self) -> usize {
        self.order.len()
    }
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
    pub fn row(&self, ix: usize) -> Option<&Row> {
        self.order.get(ix)
    }
    pub fn entry(&self, key: &ItemKey) -> Option<&Entry> {
        self.entries.get(key)
    }
    pub fn prepended(&self) -> usize {
        self.prepended
    }
    pub fn is_ready(&self) -> bool {
        self.ready && self.verified
    }
    pub fn is_loading(&self) -> bool {
        self.loading
    }
    pub fn has_older(&self) -> bool {
        self.cursor.is_some()
    }
    /// The last turn ended because the user stopped it.
    pub fn interrupted(&self) -> bool {
        self.interrupted
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub(crate) fn add_history(&mut self, page: TurnPage) -> io::Result<()> {
        let already_open = self.ready;
        self.loading = false;
        self.ready = true;
        self.error = None;
        // Codex orders the turns newest first, but items inside a turn oldest first.
        // Reverse just the items when prepending. Live items received during the request win.
        let mut added = 0;
        let buffered = std::mem::take(&mut self.before_resume);
        if !already_open {
            // The first page is authoritative for the running turn. A stale id from an earlier
            // session must not survive a reopen; live events replayed below still apply.
            self.active_turn = page
                .data
                .iter()
                .find(|t| t.status == "inProgress")
                .map(|t| t.id.clone());
        }
        // A resume and its first page can both arrive after stream notifications.
        // The page is authoritative for the turns and items it includes.
        let mut snapshot_turns = HashSet::new();
        let mut snapshot_items = HashSet::new();
        for turn in &page.data {
            self.reviews.turn(&turn.id, true);
        }
        if !already_open {
            self.reviews
                .initial_history_order(page.data.iter().map(|t| t.id.as_str()));
        }
        for turn in page.data {
            let review = self.reviews.turn(&turn.id, true);
            for row in &turn.items {
                review.observe_item(&row.id, &row.item, true);
            }
            if !buffered.is_empty() {
                snapshot_turns.insert(turn.id.clone());
            }
            for row in turn.items.into_iter().rev() {
                let key = ItemKey {
                    turn: turn.id.clone(),
                    item: row.id,
                };
                if !buffered.is_empty() {
                    snapshot_items.insert(key.clone());
                }
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    self.entries.entry(key.clone())
                {
                    let tool = is_tool(&row.item);
                    let join = tool
                        && matches!(self.order.front(), Some(Row::Tools(group))
                        if group.front().is_some_and(|other| other.turn == key.turn));
                    if join {
                        if let Some(Row::Tools(group)) = self.order.front_mut() {
                            group.push_front(key);
                        }
                        self.remeasure.insert(self.front);
                    } else {
                        self.front -= 1;
                        added += 1;
                        self.order.push_front(if tool {
                            Row::Tools(VecDeque::from([key]))
                        } else {
                            Row::Message(key)
                        });
                    }
                    entry.insert(Entry {
                        item: row.item,
                        revision: 0,
                        output: String::new(),
                        position: self.front,
                    });
                }
            }
        }
        for event in buffered {
            use crate::codex::activity::Change;
            let in_snapshot = match &event.change {
                Change::TurnStarted => snapshot_turns.contains(&event.turn),
                Change::TurnCompleted { .. } => false,
                Change::Item { row, completed } => {
                    !completed
                        && snapshot_items.contains(&ItemKey {
                            turn: event.turn.clone(),
                            item: row.id.clone(),
                        })
                }
                Change::Delta { item, target, .. } => {
                    !matches!(target, crate::codex::activity::Target::File)
                        && snapshot_items.contains(&ItemKey {
                            turn: event.turn.clone(),
                            item: item.clone(),
                        })
                }
            };
            if !in_snapshot {
                self.apply_activity(event);
            }
        }
        if already_open {
            self.prepended += added;
        }
        self.cursor = page.next_cursor;
        if let Some(cursor) = &self.cursor
            && !self.cursors.insert(cursor.clone())
        {
            self.cursor = None;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Codex repeated a history cursor",
            ));
        }
        Ok(())
    }
}

fn is_tool(item: &Item) -> bool {
    matches!(
        item,
        Item::CommandExecution { .. }
            | Item::FileChange { .. }
            | Item::AgentOperation { .. }
            | Item::McpToolCall { .. }
            | Item::ToolCall { .. }
            | Item::WebSearch { .. }
            | Item::Unknown { .. }
    )
}

impl Transcript {
    pub fn access(&self) -> Option<crate::codex::access::Access> {
        self.access
    }
    pub fn unread(&self) -> bool {
        self.unread
    }
    pub fn active_turn(&self) -> Option<&str> {
        self.active_turn.as_deref()
    }

    pub(crate) fn take_remeasure(&mut self) -> Vec<usize> {
        self.remeasure
            .drain()
            .map(|position| (position - self.front) as usize)
            .collect()
    }

    fn live_entry(&mut self, key: ItemKey, make: impl FnOnce() -> Item) -> &mut Entry {
        let entry = self.entries.entry(key.clone()).or_insert_with(|| {
            let item = make();
            if is_tool(&item) {
                if let Some(Row::Tools(group)) = self.order.back_mut()
                    && group.back().is_some_and(|k| k.turn == key.turn)
                {
                    group.push_back(key);
                } else {
                    self.order.push_back(Row::Tools(VecDeque::from([key])));
                }
            } else {
                self.order.push_back(Row::Message(key));
            }
            Entry {
                item,
                revision: 0,
                output: String::new(),
                position: self.front + self.order.len() as i64 - 1,
            }
        });
        self.remeasure.insert(entry.position);
        entry
    }

    pub(crate) fn apply_activity(&mut self, event: crate::codex::activity::Activity) {
        use crate::codex::activity::Change;
        match event.change {
            Change::TurnStarted => {
                self.interrupted = false;
                self.reviews.turn(&event.turn, false);
                if self.last_finished.as_deref() != Some(event.turn.as_str()) {
                    self.active_turn = Some(event.turn);
                }
            }
            Change::TurnCompleted { error, interrupted } => {
                if self.active_turn.as_ref() == Some(&event.turn) {
                    self.active_turn = None;
                }
                self.interrupted = interrupted || self.stop_requested.as_ref() == Some(&event.turn);
                self.stop_requested = None;
                self.last_finished = Some(event.turn);
                if let Some(error) = error {
                    self.error = Some(error);
                }
            }
            Change::Item { row, completed } => {
                self.observe_item(event.turn, row.id, row.item, completed, None)
            }
            Change::Delta { item, target, text } => {
                self.observe_delta(event.turn, item, target, text)
            }
        }
    }
    pub(crate) fn observe_item(
        &mut self,
        turn: String,
        id: String,
        item: Item,
        completed: bool,
        output: Option<String>,
    ) {
        self.reviews
            .turn(&turn, false)
            .observe_item(&id, &item, false);
        let key = ItemKey { turn, item: id };
        let output_key = output.as_ref().map(|_| key.clone());
        if completed && let Some(entry) = self.entries.get_mut(&key) {
            self.remeasure.insert(entry.position);
            entry.item = item;
            entry.revision += 1;
        } else {
            self.live_entry(key, || item);
        }
        if let Some((key, text)) = output_key.zip(output)
            && let Some(entry) = self.entries.get_mut(&key)
        {
            entry.output = text;
            entry.revision += 1;
        }
    }
    pub(crate) fn observe_delta(
        &mut self,
        turn: String,
        item: String,
        target: DeltaTarget,
        text: String,
    ) {
        if matches!(target, DeltaTarget::Plan) {
            self.reviews.turn(&turn, false).plan_text.push_str(&text);
        }
        let key = ItemKey { turn, item };
        let entry = self.live_entry(key, || match target {
            DeltaTarget::Agent => Item::AgentMessage {
                text: String::new(),
                is_update: false,
            },
            DeltaTarget::Plan => Item::Plan {
                text: String::new(),
            },
            DeltaTarget::Summary(_) => Item::Reasoning {
                summary: Vec::new(),
            },
            DeltaTarget::Command => Item::CommandExecution {
                cwd: None,
                command: String::new(),
                status: "inProgress".into(),
                aggregated_output: None,
                exit_code: None,
            },
            DeltaTarget::File => Item::FileChange {
                status: "inProgress".into(),
                changes: Vec::new(),
            },
        });
        match (&mut entry.item, target) {
            (Item::AgentMessage { text: body, .. }, DeltaTarget::Agent)
            | (Item::Plan { text: body }, DeltaTarget::Plan) => body.push_str(&text),
            (
                Item::CommandExecution {
                    aggregated_output, ..
                },
                DeltaTarget::Command,
            ) => aggregated_output
                .get_or_insert_with(String::new)
                .push_str(&text),
            (Item::FileChange { .. }, DeltaTarget::File) => entry.output.push_str(&text),
            (Item::Reasoning { summary }, DeltaTarget::Summary(index)) => {
                let index = index.min(summary.len());
                if index == summary.len() {
                    summary.push(text);
                } else {
                    summary[index].push_str(&text);
                }
            }
            _ => return,
        }
        entry.revision += 1;
    }
}

impl Transcript {
    pub(crate) fn host_revision(&mut self, key: &ItemKey, revision: u64) {
        if let Some(e) = self.entries.get_mut(key) {
            e.revision = revision;
        }
    }
    pub(crate) fn host_history_replaced(&mut self, previous: usize) {
        if previous > 0 {
            self.prepended = self.len().saturating_sub(previous);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::{activity::Activity, names::Notice};
    use serde_json::json;

    #[test]
    fn resume_with_live_activity_retains_one_copy_and_remeasures_the_correct_row() {
        let mut t = Transcript::default();
        let delta = |id, text| {
            Activity::decode(
                Notice::Text,
                json!({"threadId":"a","turnId":"new","itemId":id,"delta":text}),
            )
            .unwrap()
        };
        t.before_resume
            .extend([delta("one", "hello"), delta("two", "world")]);
        t.add_history(serde_json::from_value(json!({"data":[{"id":"new","status":"inProgress","items":[{"id":"one","type":"agentMessage","text":"hello"}]}],"nextCursor":"older"})).unwrap()).unwrap();
        assert_eq!(t.active_turn(), Some("new"));
        assert_eq!(t.len(), 2);
        assert_eq!(t.take_remeasure(), vec![1]);
        t.apply_activity(delta("one", "!"));
        t.add_history(serde_json::from_value(json!({"data":[{"id":"older","status":"completed","items":[{"id":"one","type":"agentMessage","text":"prior"}]}]})).unwrap()).unwrap();
        assert_eq!(t.take_remeasure(), vec![1]);
        let Row::Message(key) = t.row(1).unwrap() else {
            panic!("missing first live message")
        };
        assert!(
            matches!(&t.entry(key).unwrap().item,Item::AgentMessage{text,..} if text=="hello!")
        );
        let Row::Message(key) = t.row(2).unwrap() else {
            panic!("missing second live message")
        };
        assert!(matches!(&t.entry(key).unwrap().item,Item::AgentMessage{text,..} if text=="world"));
    }

    #[test]
    fn reopen_reconciles_active_turn_with_the_first_snapshot() {
        let page = |turns: serde_json::Value| -> TurnPage {
            serde_json::from_value(json!({"data": turns})).unwrap()
        };
        let mut t = Transcript {
            active_turn: Some("stale".into()),
            ..Default::default()
        };
        t.add_history(page(json!([
            {"id":"done","status":"completed","items":[]}
        ])))
        .unwrap();
        assert_eq!(t.active_turn(), None);

        t.active_turn = Some("stale".into());
        t.ready = false;
        t.add_history(page(json!([
            {"id":"live","status":"inProgress","items":[]},
            {"id":"done","status":"completed","items":[]}
        ])))
        .unwrap();
        assert_eq!(t.active_turn(), Some("live"));

        // A later page of older history must not touch the running turn.
        t.add_history(page(json!([
            {"id":"older","status":"completed","items":[]}
        ])))
        .unwrap();
        assert_eq!(t.active_turn(), Some("live"));
    }
}

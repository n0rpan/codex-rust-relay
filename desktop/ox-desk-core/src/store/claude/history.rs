//! Build transcript and diff indexes on the journal reader. The UI installs at most eight
//! fully projected threads per delivery, then returns superseded data to the reader to drop.
use super::*;
use crate::transcript::Transcript;
use std::{
    collections::hash_map::Entry,
    sync::{Mutex, Weak, mpsc},
    time::Duration,
};
const THREADS_PER_BATCH: usize = 8;

pub(super) struct Snapshot {
    id: String,
    row: Option<ThreadRow>,
    transcript: Transcript,
    turn: String,
    pending: HashMap<String, String>,
}
impl Snapshot {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            row: None,
            transcript: Default::default(),
            turn: String::new(),
            pending: Default::default(),
        }
    }
    fn item(&mut self, id: String, item: Item, completed: bool, output: Option<String>) {
        if self.turn.is_empty() || matches!(item, Item::UserMessage { .. }) {
            self.turn.clone_from(&id);
        }
        let turn = if completed {
            self.pending
                .remove(&id)
                .unwrap_or_else(|| self.turn.clone())
        } else {
            self.pending
                .entry(id.clone())
                .or_insert_with(|| self.turn.clone())
                .clone()
        };
        self.transcript
            .observe_item(turn, id, item, completed, output);
    }
}
struct Projection<'a> {
    owner: &'a str,
    profile: &'a Profile,
    updated: i64,
    threads: HashMap<String, Snapshot>,
}
impl<'a> Projection<'a> {
    fn new(owner: &'a str, profile: &'a Profile, updated: i64) -> Self {
        let mut root = Snapshot::new(owner);
        root.transcript.loading = true;
        Self {
            owner,
            profile,
            updated,
            threads: HashMap::from([(owner.into(), root)]),
        }
    }
    fn child(&mut self, id: &str, parent: &str, label: &str) {
        let snapshot = self
            .threads
            .entry(id.into())
            .or_insert_with(|| Snapshot::new(id));
        let r = snapshot
            .row
            .get_or_insert_with(|| row(self.profile, id.into(), label.into(), self.updated));
        r.parent_thread_id = Some(parent.into());
        r.agent_nickname = Some(label.into());
        r.worker = true;
        // Cold child histories are read-only. The project, session and parent journal have
        // already been verified, and Store::can_send never permits sending into a child.
        snapshot.transcript.ready = true;
        snapshot.transcript.verified = true;
    }
    fn observe(&mut self, event: wire::Event) {
        // Catalog::history yields canonical per-block assistant and user records only.
        let wire::Event::Item {
            scope,
            id,
            mut item,
            completed,
            output,
        } = event
        else {
            return;
        };
        let owner = self.owner;
        let display = scope
            .parent_tool_use_id
            .as_deref()
            .map(|tool| child_key(owner, tool))
            .unwrap_or_else(|| owner.into());
        if display != owner && !self.threads.contains_key(&display) {
            self.child(&display, owner, "Claude agent");
        }
        super::events::rebase_agents(owner, &display, &mut item, |child, label, _| {
            self.child(child, &display, label)
        });
        self.threads
            .get_mut(&display)
            .unwrap()
            .item(id, item, completed, output);
    }
}

type Retired = (Transcript, Option<HashMap<String, String>>);
pub(super) struct Batch {
    snapshots: Arc<Mutex<Vec<Snapshot>>>,
    applied: mpsc::Sender<Vec<Retired>>,
}
impl Batch {
    pub(super) fn apply(self, store: &mut Store, owner: &str) {
        let snapshots = std::mem::take(&mut *self.snapshots.lock().unwrap());
        let mut retired = Vec::with_capacity(snapshots.len());
        for snapshot in snapshots {
            let Snapshot {
                id,
                row,
                transcript,
                pending,
                ..
            } = snapshot;
            if let Some(row) = row {
                store.remember_claude(row);
            }
            let old_pending = store
                .claude_mut()
                .sessions
                .get_mut(owner)
                .unwrap()
                .events
                .pending
                .insert(id.clone(), pending);
            store.changed.transcript(&id);
            match store.transcripts.entry(id) {
                Entry::Occupied(mut e) => retired.push((e.insert(transcript), old_pending)),
                Entry::Vacant(e) => {
                    e.insert(transcript);
                }
            }
        }
        let _ = self.applied.send(retired);
    }
}

pub(super) fn load(
    profile: &Profile,
    owner: &str,
    saved: &wire::catalog::Session,
    alive: &Weak<()>,
    emit: impl FnMut(Batch),
) -> io::Result<()> {
    let mut projection = Projection::new(owner, profile, saved.updated_at);
    let mut decoder = wire::Decoder::default();
    saved.history(|record| {
        if alive.strong_count() == 0 {
            return Err(io::Error::other("Claude history was closed"));
        }
        for e in decoder.decode_value(record)? {
            projection.observe(e);
        }
        Ok(())
    })?;
    publish(projection.threads.into_values(), alive, emit)
}
fn publish(
    mut snapshots: impl Iterator<Item = Snapshot>,
    alive: &Weak<()>,
    mut emit: impl FnMut(Batch),
) -> io::Result<()> {
    loop {
        let contents: Vec<_> = snapshots.by_ref().take(THREADS_PER_BATCH).collect();
        if contents.is_empty() {
            return Ok(());
        }
        let contents = Arc::new(Mutex::new(contents));
        let (tx, rx) = mpsc::channel();
        emit(Batch {
            snapshots: contents.clone(),
            applied: tx,
        });
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(_retired) => break,
                Err(mpsc::RecvTimeoutError::Timeout) if alive.strong_count() > 0 => continue,
                _ => {
                    // The mailbox may outlive its window. Reclaim the unapplied page here.
                    contents.lock().unwrap().clear();
                    return Err(io::Error::other("Claude history was closed"));
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) fn fixture(profile: &Profile, owner: &str, events: Vec<wire::Event>) -> Batch {
    let mut projection = Projection::new(owner, profile, 0);
    for event in events {
        projection.observe(event);
    }
    let (applied, _) = mpsc::channel();
    Batch {
        snapshots: Arc::new(Mutex::new(projection.threads.into_values().collect())),
        applied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn next_page_waits_for_the_ui_and_dropped_windows_reclaim_unswapped_history() {
        let token = Arc::new(());
        let alive = Arc::downgrade(&token);
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let snapshots = (0..(THREADS_PER_BATCH * 3)).map(|i| Snapshot::new(&i.to_string()));
            publish(snapshots, &alive, |batch| tx.send(batch).unwrap())
        });
        let first = rx.recv().unwrap();
        assert_eq!(first.snapshots.lock().unwrap().len(), THREADS_PER_BATCH);
        assert!(rx.try_recv().is_err());
        first.applied.send(Vec::new()).unwrap();
        let orphan = rx.recv().unwrap();
        assert_eq!(orphan.snapshots.lock().unwrap().len(), THREADS_PER_BATCH);
        drop(token);
        assert!(reader.join().unwrap().is_err());
        assert!(orphan.snapshots.lock().unwrap().is_empty());
    }
}

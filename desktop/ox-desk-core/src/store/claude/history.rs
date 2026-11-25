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
    /// A follower keeps no transcript: it records what to apply to the one the UI holds.
    log: Option<Vec<Op>>,
}
/// One projected item for the UI thread to apply to an existing transcript.
pub(super) struct Op {
    turn: String,
    id: String,
    item: Item,
    completed: bool,
    output: Option<String>,
}
impl Snapshot {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            row: None,
            transcript: Default::default(),
            turn: String::new(),
            pending: Default::default(),
            log: None,
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
        if let Some(log) = &mut self.log {
            log.push(Op {
                turn,
                id,
                item,
                completed,
                output,
            });
        } else {
            self.transcript
                .observe_item(turn, id, item, completed, output);
        }
    }
}
struct Projection {
    owner: String,
    profile: Profile,
    updated: i64,
    threads: HashMap<String, Snapshot>,
    /// Children first seen while following: their rows go to the UI with the next tail.
    born: Vec<ThreadRow>,
}
impl Projection {
    fn new(owner: &str, profile: &Profile, updated: i64) -> Self {
        let mut root = Snapshot::new(owner);
        root.transcript.loading = true;
        Self {
            owner: owner.into(),
            profile: profile.clone(),
            updated,
            threads: HashMap::from([(owner.into(), root)]),
            born: Vec::new(),
        }
    }
    /// After the first full load: keep only the turn and pending state, and record from now on.
    fn follow(self) -> Self {
        let threads = self
            .threads
            .into_iter()
            .map(|(k, s)| {
                let kept = Snapshot {
                    id: s.id,
                    row: None,
                    transcript: Default::default(),
                    turn: s.turn,
                    pending: s.pending,
                    log: Some(Vec::new()),
                };
                (k, kept)
            })
            .collect();
        Self { threads, ..self }
    }
    fn child(&mut self, id: &str, parent: &str, label: &str) {
        let recording = self.threads.values().any(|s| s.log.is_some());
        let snapshot = self.threads.entry(id.into()).or_insert_with(|| {
            let mut s = Snapshot::new(id);
            s.log = recording.then(Vec::new);
            s
        });
        let r = snapshot
            .row
            .get_or_insert_with(|| row(&self.profile, id.into(), label.into(), self.updated));
        r.parent_thread_id = Some(parent.into());
        r.agent_nickname = Some(label.into());
        r.worker = true;
        // Cold child histories are read-only. The project, session and parent journal have
        // already been verified, and Store::can_send never permits sending into a child.
        snapshot.transcript.ready = true;
        snapshot.transcript.verified = true;
        if snapshot.log.is_some()
            && let Some(row) = snapshot.row.take()
        {
            self.born.push(row);
        }
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
        let owner = self.owner.clone();
        let owner = owner.as_str();
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
        let viewing = store.is_viewer(owner);
        let mut retired = Vec::with_capacity(snapshots.len());
        for snapshot in snapshots {
            let Snapshot {
                id,
                row,
                mut transcript,
                pending,
                ..
            } = snapshot;
            if viewing {
                // A foreign client owns the process: the journal is all there is to read.
                transcript.loading = false;
                transcript.ready = true;
                transcript.verified = true;
            }
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

/// What a read-only viewer keeps between ticks so only appended journal bytes are parsed:
/// the decoder and projection state, and where the last complete line ended.
pub(super) struct Follower {
    saved: wire::catalog::Session,
    decoder: wire::Decoder,
    projection: Projection,
    visited: std::collections::HashSet<String>,
    offset: u64,
    identity: Option<u64>,
}
/// The items one tick appended, for the UI thread to apply in place.
pub(super) struct Tail {
    ops: Vec<(String, Vec<Op>)>,
    born: Vec<ThreadRow>,
}
impl Tail {
    pub(super) fn apply(self, store: &mut Store) {
        for row in self.born {
            let key = row.id.clone();
            store.remember_claude(row);
            let t = store.transcripts.entry(key).or_default();
            t.loading = false;
            t.ready = true;
            t.verified = true;
        }
        for (key, ops) in self.ops {
            let t = store.transcripts.entry(key.clone()).or_default();
            for op in ops {
                t.observe_item(op.turn, op.id, op.item, op.completed, op.output);
            }
            store.changed.transcript(&key);
        }
    }
}
impl Follower {
    /// Read the whole journal, publish it, and keep what following needs.
    pub(super) fn load(
        profile: &Profile,
        owner: &str,
        saved: wire::catalog::Session,
        alive: &Weak<()>,
        emit: impl FnMut(Batch),
    ) -> io::Result<Self> {
        let mut projection = Projection::new(owner, profile, saved.updated_at);
        let mut decoder = wire::Decoder::default();
        let mut visited = std::collections::HashSet::new();
        let (offset, identity) = saved
            .history_from(0, None, &mut visited, |record| {
                if alive.strong_count() == 0 {
                    return Err(io::Error::other("Claude history was closed"));
                }
                for e in decoder.decode_value(record)? {
                    projection.observe(e);
                }
                Ok(())
            })?
            .expect("a read from the start never goes stale");
        // The snapshots are consumed by the UI; a follower keeps copies of their state.
        let kept: Vec<(String, String, HashMap<String, String>)> = projection
            .threads
            .values()
            .map(|s| (s.id.clone(), s.turn.clone(), s.pending.clone()))
            .collect();
        publish(projection.threads.into_values(), alive, emit)?;
        let mut follow = Projection::new(owner, profile, saved.updated_at);
        follow.threads = kept
            .into_iter()
            .map(|(id, turn, pending)| {
                let mut s = Snapshot::new(&id);
                s.turn = turn;
                s.pending = pending;
                (id, s)
            })
            .collect();
        Ok(Self {
            saved,
            decoder,
            projection: follow.follow(),
            visited,
            offset,
            identity: Some(identity),
        })
    }

    /// Parse what was appended since the last read. `false` when the journal was truncated
    /// or replaced: the caller reads it again from the start.
    pub(super) fn advance(
        &mut self,
        alive: &Weak<()>,
        mut emit: impl FnMut(Tail),
    ) -> io::Result<bool> {
        let Self {
            saved,
            decoder,
            projection,
            visited,
            offset,
            identity,
        } = self;
        let Some((next, ino)) = saved.history_from(*offset, *identity, visited, |record| {
            if alive.strong_count() == 0 {
                return Err(io::Error::other("Claude history was closed"));
            }
            for e in decoder.decode_value(record)? {
                projection.observe(e);
            }
            Ok(())
        })?
        else {
            return Ok(false);
        };
        *offset = next;
        *identity = Some(ino);
        let ops: Vec<_> = projection
            .threads
            .values_mut()
            .filter_map(|s| {
                let log = s.log.as_mut().map(std::mem::take)?;
                (!log.is_empty()).then(|| (s.id.clone(), log))
            })
            .collect();
        let born = std::mem::take(&mut projection.born);
        if !ops.is_empty() || !born.is_empty() {
            emit(Tail { ops, born });
        }
        Ok(true)
    }
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

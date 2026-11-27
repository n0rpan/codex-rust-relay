//! A Claude Code session that a stock client is driving outside OX. OX reads its journal
//! read-only, refreshes it while it is open, and never resumes it, so the two clients do
//! not write the same transcript. The user can fork it into a new OX session instead.
use super::*;
use crate::claude::live;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant, SystemTime},
};

pub(super) const HINT: &str = "This conversation is running in Claude Code right now. OX shows it read-only so the two don't collide. When it's closed there, you can continue it here.";
const EVERY: Duration = Duration::from_secs(4);

#[derive(Default)]
pub(super) struct Native {
    live: HashSet<String>,
    /// Sessions OX has driven in this run: their journal age says nothing about a
    /// foreign client, so only a foreign process names them.
    /// The journal's modification time when OX took the session over.
    pub(super) ox_seen: HashMap<String, Option<SystemTime>>,
    next: Option<Instant>,
}
#[derive(Default)]
pub(super) struct Viewer {
    /// The session was closed when selected; release the viewer after the first read.
    snapshot_only: bool,
    confirm_alias: bool,
    loaded: Option<SystemTime>,
    /// Drain child-only appends once after the native owner disappears.
    checked_after_exit: bool,
    last_child_write: Option<SystemTime>,
    pub(super) loading: bool,
    /// Where the last read stopped; `None` until the first full read finishes (or after
    /// a failure), so the next tick reads from the start.
    follower: Option<Box<history::Follower>>,
}

impl Store {
    /// A stock client is running this session right now. A remote client reads the
    /// host's `native_live` capability.
    pub fn is_native_live(&self, id: &str) -> bool {
        if let Some(remote) = self.remote() {
            return remote.can(id, "native_live");
        }
        if self.is_session_scoped() {
            return self.claude().native.live.contains(id);
        }
        self.transcripts.get(id).is_some_and(|t| t.native_live)
    }
    /// The stock client that has the session open outside OX.
    pub fn native_app(&self) -> &'static str {
        if self.profile.route.runtime == crate::runtime::Runtime::Codex {
            "Codex"
        } else {
            "Claude Code"
        }
    }
    pub fn native_live_hint(&self) -> String {
        HINT.replace("Claude Code", self.native_app())
    }
    /// The next refresh while a native session is open, so an idle host wakes for it.
    pub(in crate::store) fn native_deadline(&self) -> Option<Instant> {
        if self.remote().is_some() {
            return None;
        }
        if !self.is_session_scoped() {
            // While a read is outstanding the probe cannot run, so its deadline is not
            // re-armed: an overdue timer would spin the host loop until the reply lands.
            return self
                .selected
                .as_ref()
                .filter(|id| !self.native_request_outstanding(id))
                .and_then(|id| self.transcripts.get(id))
                .filter(|t| t.native_live)
                .and_then(|t| t.native_next);
        }
        let p = self.claude();
        p.sessions
            .values()
            .any(|s| s.viewer.is_some())
            .then(|| p.native.next.unwrap_or_else(Instant::now))
    }
    pub(in crate::store) fn native_live_now(&mut self, id: &str) -> bool {
        self.refresh_native(Instant::now(), true);
        self.claude().native.live.contains(id)
    }

    /// One stat per saved session and at most one /proc scan.
    fn scan_native(&self) -> HashSet<String> {
        let p = self.claude();
        let origin = if self.profile.route.is_api() {
            Origin::Api
        } else {
            Origin::Subscription
        };
        let mut processes = live::ids_in_processes();
        // A terminal client registers itself; that holds while it is quiet for minutes.
        let listed = live::ids_in_registry(match origin {
            Origin::Api => &p.launcher.profiles.api,
            _ => &p.launcher.profiles.subscription,
        });
        processes.extend(listed.ids);
        // Current Claude Code keeps a registry of its live clients. When it exists it is
        // authoritative: a journal written moments ago (a fixture, a terminal client that
        // just closed) says nothing without a live registered pid. Only an older Claude
        // Code, with no registry, falls back to the journal's age.
        // When a pid's liveness cannot be determined (no procfs, odd errno) the registry is
        // not authoritative and the journal's age still counts.
        let registry = listed.authoritative;
        let now = SystemTime::now();
        let mut found = HashSet::new();
        for key in p.catalog.keys().filter(|k| !k.contains('/')) {
            // OX's own process for it is not a foreign one.
            if p.sessions.get(key).is_some_and(|s| s.viewer.is_none()) {
                continue;
            }
            let Ok(raw) = stock_id(key) else { continue };
            // A recent write means "someone else" when the journal's last row came from a
            // client other than OX's headless one. OX's own earlier runs (before an
            // upgrade) left `sdk-cli` on the last row. A session OX drove in this run is
            // only masked until the journal moves past what OX found: a stock
            // `claude --continue` has no session id on its command line, so a fresh
            // `cli` row after OX let go is what names it.
            let profiles = &p.launcher.profiles;
            let modified = profiles.modified(&self.profile.project, raw, origin).ok();
            let moved_since_ox = match p.native.ox_seen.get(key) {
                Some(Some(baseline)) => modified.is_some_and(|m| m > *baseline),
                Some(None) | None => true,
            };
            // Older clients have no process registry. While viewing their journal, use
            // the same freshness window for independent child writes as for the parent.
            let child_write = p
                .sessions
                .get(key)
                .and_then(|s| s.viewer.as_ref())
                .and_then(|v| v.last_child_write);
            let fresh = !registry
                && moved_since_ox
                && (modified.is_some_and(|m| live::is_fresh(m, now))
                    || child_write.is_some_and(|m| live::is_fresh(m, now)))
                && profiles
                    .last_entrypoint(&self.profile.project, raw, origin)
                    .as_deref()
                    != Some(live::HEADLESS_ENTRYPOINT);
            if fresh || processes.contains(raw) {
                found.insert(key.clone());
            }
        }
        found
    }

    pub(in crate::store) fn refresh_native(&mut self, now: Instant, force: bool) {
        if self.remote().is_some() {
            return;
        }
        if !self.is_session_scoped() {
            self.refresh_native_codex(now);
            return;
        }
        if !force && self.claude().native.next.is_some_and(|at| now < at) {
            return;
        }
        let found = self.scan_native();
        let p = self.claude_mut();
        p.native.next = Some(now + EVERY);
        let changed = p.native.live != found;
        p.native.live = found;
        let viewers: Vec<String> = p
            .sessions
            .iter()
            .filter(|(_, s)| s.viewer.is_some())
            .map(|(k, _)| k.clone())
            .collect();
        for id in viewers {
            self.follow_native(&id);
        }
        // A viewer that was let go while the other client was quiet comes back when its
        // journal is live again, so the open transcript keeps following.
        if let Some(id) = self.selected.clone()
            && !id.contains('/')
            && self.claude().native.live.contains(&id)
            && !self.claude().sessions.contains_key(&id)
        {
            let _ = self.view_native(&id);
        }
        if changed {
            self.changed.mark(Changed::THREADS);
        }
    }

    /// Re-read a viewed journal that moved; leave it once the other client is gone.
    fn follow_native(&mut self, id: &str) {
        let origin = if self.profile.route.is_api() {
            Origin::Api
        } else {
            Origin::Subscription
        };
        let Ok(raw) = stock_id(id) else { return };
        let modified = self
            .claude()
            .launcher
            .profiles
            .modified(&self.profile.project, raw, origin)
            .ok();
        let Some(viewer) = self
            .claude()
            .sessions
            .get(id)
            .and_then(|s| s.viewer.as_ref())
        else {
            return;
        };
        if viewer.loading {
            return;
        }
        let moved = modified != viewer.loaded;
        let live = self.claude().native.live.contains(id);
        let check_children = viewer
            .follower
            .as_ref()
            .is_some_and(|f| f.watches_children())
            && (live || !viewer.checked_after_exit);
        if moved {
            // The row's age follows the journal.
            if let Some(secs) = modified
                .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
            {
                if let Some(row) = self.claude_mut().catalog.get_mut(id) {
                    row.updated_at = row.updated_at.max(secs);
                }
                if let Some(row) = self.catalog.get_mut(id) {
                    row.updated_at = row.updated_at.max(secs);
                }
                self.catalog_revision += 1;
                self.changed.mark(Changed::THREADS);
            }
        }
        if moved || check_children {
            self.start_view_load(id);
        } else if !live {
            self.leave_native(id);
        }
    }

    /// Read history from disk. Keep following only while stock owns the journal.
    pub(super) fn view_native(&mut self, id: &str) -> io::Result<()> {
        self.view_journal(id, false)
    }

    pub(super) fn view_journal(&mut self, id: &str, confirm_alias: bool) -> io::Result<()> {
        if self.claude().sessions.contains_key(id) {
            return Ok(());
        }
        let snapshot_only = !self.is_native_live(id);
        if snapshot_only {
            if let Billing::Api(alias) = &self.profile.route.billing
                && !self.resume_models.contains_key(id)
            {
                let launcher = &self.claude().launcher;
                if let Some(model) = Bindings::new(launcher.index.clone()).selected_model(
                    &self.profile.project,
                    &launcher.profiles.api,
                    stock_id(id)?,
                    alias,
                )? {
                    self.resume_models.insert(id.into(), model);
                }
            }
            self.ended_threads.insert(id.into());
        }
        let p = self.claude_mut();
        p.serial += 1;
        let generation = p.serial;
        p.sessions.insert(
            id.to_owned(),
            Process {
                generation,
                viewer: Some(Viewer {
                    snapshot_only,
                    confirm_alias,
                    ..Viewer::default()
                }),
                ..Default::default()
            },
        );
        let t = self.transcripts.entry(id.into()).or_default();
        t.loading = true;
        t.error = None;
        self.start_view_load(id);
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }

    fn start_view_load(&mut self, id: &str) {
        let origin = if self.profile.route.is_api() {
            Origin::Api
        } else {
            Origin::Subscription
        };
        let profile = self.profile.clone();
        let Ok(raw) = stock_id(id).map(str::to_owned) else {
            return;
        };
        let p = self.claude_mut();
        let launcher = p.launcher.clone();
        let receive = p.deliver.clone();
        let Some(process) = p.sessions.get_mut(id) else {
            return;
        };
        let generation = process.generation;
        let alive = process.events.lifetime();
        let Some(viewer) = process.viewer.as_mut() else {
            return;
        };
        viewer.checked_after_exit = !p.native.live.contains(id);
        let mut follower = viewer.follower.take();
        let confirm_alias = viewer.confirm_alias;
        viewer.loading = true;
        viewer.loaded = launcher
            .profiles
            .modified(&profile.project, &raw, origin)
            .ok();
        let key = id.to_owned();
        thread::spawn(move || {
            let emit = |data| {
                receive(LinkEvent::Claude(Update {
                    owner: key.clone(),
                    generation,
                    data,
                }))
            };
            // Only the appended bytes when this viewer already holds the journal's state.
            let resumed = match follower.as_mut() {
                Some(f) => f.advance(&alive, |tail| emit(Data::Tail(tail))),
                None => Ok(false),
            };
            let loaded = match resumed {
                Ok(true) => Ok(follower),
                Ok(false) => launcher
                    .saved_session(&profile, &raw, confirm_alias)
                    .and_then(|saved| {
                        history::Follower::load(
                            &profile,
                            &key,
                            saved,
                            follower.as_deref(),
                            &alive,
                            |batch| emit(Data::History(batch)),
                        )
                    })
                    .map(|f| Some(Box::new(f))),
                Err(e) => Err(e),
            };
            emit(match loaded {
                Ok(f) => Data::Viewed(f),
                Err(e) => Data::Failed(e.to_string()),
            });
        });
    }

    /// The other client closed the session: OX becomes its owner again, so a send resumes it.
    fn leave_native(&mut self, id: &str) {
        self.claude_mut().sessions.remove(id);
        if let Some(t) = self.transcripts.get_mut(id) {
            t.loading = false;
            t.error = None;
        }
        self.ended_threads.insert(id.into());
        self.changed.transcript(id);
        self.changed
            .mark(Changed::THREADS | Changed::CONNECTION | Changed::ASK);
    }

    pub(super) fn viewed(&mut self, owner: &str, follower: Option<Box<history::Follower>>) {
        if !self.is_native_live(owner)
            && self.claude().sessions[owner]
                .viewer
                .as_ref()
                .is_some_and(|v| v.snapshot_only)
        {
            self.leave_native(owner);
            return;
        }
        if let Some(v) = self
            .claude_mut()
            .sessions
            .get_mut(owner)
            .and_then(|s| s.viewer.as_mut())
        {
            v.loading = false;
            v.last_child_write = follower.as_ref().and_then(|f| f.last_child_write());
            v.follower = follower;
        }
    }
    pub(super) fn is_viewer(&self, owner: &str) -> bool {
        self.claude()
            .sessions
            .get(owner)
            .is_some_and(|s| s.viewer.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{Route, Runtime};
    use std::sync::{Mutex, mpsc};

    const ID: &str = "aaaaaaaa-1111-4111-8111-111111111111";

    fn row_json(kind: &str, project: &std::path::Path, text: &str) -> String {
        let uuid = format!(
            "00000000-0000-4000-8000-{:012}",
            text.bytes().fold(kind.len() as u64, |h, b| h
                .wrapping_mul(131)
                .wrapping_add(b as u64))
                % 1_000_000_000_000
        );
        let content = if kind == "user" {
            json!(text)
        } else {
            json!([{"type":"text","text":text}])
        };
        json!({"type":kind,"uuid":uuid,"cwd":project,"sessionId":ID,"entrypoint":"cli","message":{"role":kind,"id":uuid,"content":content}})
            .to_string()
            + "\n"
    }
    fn pump(store: &mut Store, rx: &mpsc::Receiver<LinkEvent>) {
        // Apply every update up to and including the end of one load.
        loop {
            let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let LinkEvent::Claude(update) = event else {
                panic!("claude link only")
            };
            let done = matches!(update.data, Data::Viewed(_) | Data::Failed(_));
            store.claude_event(update).unwrap();
            if done {
                break;
            }
        }
    }

    #[test]
    fn a_session_running_natively_is_shown_read_only_and_follows_its_journal() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().canonicalize().unwrap().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let profiles = Profiles {
            subscription: root.path().join("native"),
            api: root.path().join("relay"),
        };
        let dir = profiles.subscription.join("projects").join(
            project
                .to_string_lossy()
                .replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
        );
        std::fs::create_dir_all(&dir).unwrap();
        let journal = dir.join(format!("{ID}.jsonl"));
        std::fs::write(
            &journal,
            row_json("user", &project, "hello") + &row_json("assistant", &project, "hi"),
        )
        .unwrap();
        let profile = Profile {
            project: project.clone(),
            route: Route {
                runtime: Runtime::Claude,
                billing: Billing::Subscription,
            },
            provider: SUBSCRIPTION_PROVIDER.into(),
        };
        let key = session_key(&profile.route.billing, ID);
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let launcher = Launcher {
            ox: PathBuf::new(),
            path: Default::default(),
            profiles,
            index: PathBuf::new(),
            aliases: vec![],
        };
        let project_state = Project {
            launcher: Arc::new(launcher),
            deliver: Arc::new(move |e| {
                let _ = tx.lock().unwrap().send(e);
            }),
            sessions: Default::default(),
            catalog: Default::default(),
            asks: Default::default(),
            dispatching_worker: None,
            worker: None,
            worker_blank: None,
            worker_model: None,
            serial: 0,
            native: Default::default(),
        };
        let mut store = Store::with_link(profile, links::Link::Claude(Box::new(project_state)));
        store.state = Connection::Ready;
        store.remember_claude(row(&store.profile, key.clone(), "hello".into(), 1));
        store.claude_list();

        // Written a moment ago and no OX process: running elsewhere. Opening it must
        // not start a process; the history is read from the journal.
        store.select(&key).unwrap();
        assert!(store.is_native_live(&key));
        pump(&mut store, &rx);
        let t = store.transcript(&key).unwrap();
        assert!(
            t.is_ready() && t.error().is_none(),
            "{:?} {:?}",
            t.error(),
            store.notice()
        );
        assert!(!store.claude_has_live(&key));
        assert!(!store.can_send(Some(&key)) && !store.can_compose(Some(&key)));
        assert!(store.can_manage_thread(&key), "forkable into a new session");
        let before = store.transcript(&key).unwrap().len();

        // The other client keeps writing: the open view follows.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap();
        std::io::Write::write_all(&mut f, row_json("user", &project, "more").as_bytes()).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(2))
            .unwrap();
        store.refresh_native(Instant::now(), true);
        pump(&mut store, &rx);
        assert!(store.transcript(&key).unwrap().len() > before);

        // A journal OX's own headless client wrote last is never a foreign session, however
        // fresh (an earlier host drove it before an upgrade).
        let mut row = serde_json::from_str::<Value>(&row_json("user", &project, "ours")).unwrap();
        row["entrypoint"] = "sdk-cli".into();
        std::io::Write::write_all(&mut f, format!("{row}\n").as_bytes()).unwrap();
        f.set_modified(SystemTime::now()).unwrap();
        store.refresh_native(Instant::now(), true);
        assert!(!store.is_native_live(&key));
        pump(&mut store, &rx);
        row["entrypoint"] = "cli".into();
        row["uuid"] = "00000000-0000-4000-8000-000000000999".into();
        std::io::Write::write_all(&mut f, format!("{row}\n").as_bytes()).unwrap();
        f.set_modified(SystemTime::now()).unwrap();
        store.refresh_native(Instant::now(), true);
        assert!(store.is_native_live(&key));
        pump(&mut store, &rx);

        // It goes quiet and the process is gone: OX owns it again and a send resumes it.
        f.set_modified(SystemTime::now() - Duration::from_secs(600))
            .unwrap();
        store.refresh_native(Instant::now(), true);
        pump(&mut store, &rx);
        store.refresh_native(Instant::now(), true);
        assert!(!store.is_native_live(&key));
        assert!(store.is_ended(&key) && store.can_compose(Some(&key)));
    }

    struct Rig {
        store: Store,
        rx: mpsc::Receiver<LinkEvent>,
        journal: PathBuf,
        project: PathBuf,
        key: String,
        _root: tempfile::TempDir,
    }
    fn rig(rows: usize) -> Rig {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().canonicalize().unwrap().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let profiles = Profiles {
            subscription: root.path().join("native"),
            api: root.path().join("relay"),
        };
        let dir = profiles.subscription.join("projects").join(
            project
                .to_string_lossy()
                .replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
        );
        std::fs::create_dir_all(&dir).unwrap();
        let journal = dir.join(format!("{ID}.jsonl"));
        let mut body = String::new();
        for n in 0..rows {
            body += &row_json(
                if n % 2 == 0 { "user" } else { "assistant" },
                &project,
                &format!("message number {n}"),
            );
        }
        std::fs::write(&journal, body).unwrap();
        let profile = Profile {
            project: project.clone(),
            route: Route {
                runtime: Runtime::Claude,
                billing: Billing::Subscription,
            },
            provider: SUBSCRIPTION_PROVIDER.into(),
        };
        let key = session_key(&profile.route.billing, ID);
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let launcher = Launcher {
            ox: PathBuf::new(),
            path: Default::default(),
            profiles,
            index: PathBuf::new(),
            aliases: vec![],
        };
        let project_state = Project {
            launcher: Arc::new(launcher),
            deliver: Arc::new(move |e| {
                let _ = tx.lock().unwrap().send(e);
            }),
            sessions: Default::default(),
            catalog: Default::default(),
            asks: Default::default(),
            dispatching_worker: None,
            worker: None,
            worker_blank: None,
            worker_model: None,
            serial: 0,
            native: Default::default(),
        };
        let mut store = Store::with_link(profile, links::Link::Claude(Box::new(project_state)));
        store.state = Connection::Ready;
        store.remember_claude(row(&store.profile, key.clone(), "hello".into(), 1));
        store.claude_list();
        store.select(&key).unwrap();
        Rig {
            store,
            rx,
            journal,
            project,
            key,
            _root: root,
        }
    }
    /// Apply one read; report whether any full-history batch was delivered.
    fn pump_kinds(store: &mut Store, rx: &mpsc::Receiver<LinkEvent>) -> (bool, bool) {
        let (mut full, mut tail) = (false, false);
        loop {
            let LinkEvent::Claude(update) = rx.recv_timeout(Duration::from_secs(20)).unwrap()
            else {
                panic!("claude link only")
            };
            full |= matches!(update.data, Data::History(_));
            tail |= matches!(update.data, Data::Tail(_));
            let done = matches!(update.data, Data::Viewed(_) | Data::Failed(_));
            store.claude_event(update).unwrap();
            if done {
                return (full, tail);
            }
        }
    }
    fn append(rig: &Rig, kind: &str, text: &str, age: u64) {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&rig.journal)
            .unwrap();
        std::io::Write::write_all(&mut f, row_json(kind, &rig.project, text).as_bytes()).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(age))
            .unwrap();
    }

    #[test]
    fn appended_rows_are_parsed_alone_and_appear_without_a_full_read() {
        let mut r = rig(4);
        assert_eq!(pump_kinds(&mut r.store, &r.rx), (true, false));
        let before = r.store.transcript(&r.key).unwrap().len();
        append(&r, "assistant", "a brand new reply", 0);
        r.store.refresh_native(Instant::now(), true);
        assert_eq!(pump_kinds(&mut r.store, &r.rx), (false, true));
        assert_eq!(r.store.transcript(&r.key).unwrap().len(), before + 1);

        // A line the writer has not finished is left for the next tick, then completes once.
        let half = row_json("user", &r.project, "split in two");
        let (a, b) = half.split_at(half.len() / 2);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&r.journal)
            .unwrap();
        std::io::Write::write_all(&mut f, a.as_bytes()).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(3))
            .unwrap();
        r.store.refresh_native(Instant::now(), true);
        assert_eq!(pump_kinds(&mut r.store, &r.rx), (false, false));
        std::io::Write::write_all(&mut f, b.as_bytes()).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(4))
            .unwrap();
        r.store.refresh_native(Instant::now(), true);
        assert_eq!(pump_kinds(&mut r.store, &r.rx), (false, true));
        assert_eq!(r.store.transcript(&r.key).unwrap().len(), before + 2);

        // A truncated journal is read again from the start.
        std::fs::write(&r.journal, row_json("user", &r.project, "fresh start")).unwrap();
        std::fs::File::options()
            .append(true)
            .open(&r.journal)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(8))
            .unwrap();
        r.store.refresh_native(Instant::now(), true);
        assert!(pump_kinds(&mut r.store, &r.rx).0);
        assert_eq!(r.store.transcript(&r.key).unwrap().len(), 1);
    }

    fn append_parent_row(r: &Rig, value: Value) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&r.journal)
            .unwrap();
        writeln!(f, "{value}").unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(1))
            .unwrap();
    }

    fn append_agent_call(r: &Rig, background: bool) {
        let mut call: Value =
            serde_json::from_str(&row_json("assistant", &r.project, "launch")).unwrap();
        call["message"]["content"] = json!([{"type":"tool_use","name":"Agent","id":"call-child",
            "input":{"description":"Review","run_in_background":background}}]);
        append_parent_row(r, call);
    }

    fn append_agent_result(r: &Rig, status: &str) {
        let mut result: Value =
            serde_json::from_str(&row_json("user", &r.project, "result")).unwrap();
        result["message"]["content"] =
            json!([{"type":"tool_result","tool_use_id":"call-child","content":"Agent result."}]);
        result["toolUseResult"] = json!({"agentId":"a0","description":"Review","status":status});
        append_parent_row(r, result);
    }

    fn append_child_link(r: &Rig) {
        append_agent_call(r, true);
        append_agent_result(r, "async_launched");
    }

    fn child_row(r: &Rig, text: &str) -> String {
        let mut row: Value =
            serde_json::from_str(&row_json("assistant", &r.project, text)).unwrap();
        row["agentId"] = "a0".into();
        row["isSidechain"] = true.into();
        format!("{row}\n")
    }

    fn poll_view(r: &mut Rig) -> (bool, bool) {
        r.store.refresh_native(Instant::now(), true);
        if r.store.claude().sessions[&r.key]
            .viewer
            .as_ref()
            .unwrap()
            .loading
        {
            pump_kinds(&mut r.store, &r.rx)
        } else {
            (false, false)
        }
    }

    fn child_messages(r: &Rig) -> Vec<&str> {
        let child = child_key(&r.key, "call-child");
        let t = r.store.transcript(&child).unwrap();
        (0..t.len())
            .map(|i| {
                let Some(crate::transcript::Row::Message(key)) = t.row(i) else {
                    panic!("expected a child message")
                };
                let item = t.entry(key).unwrap();
                assert_eq!(item.revision, 0, "replayed child message");
                let Item::AgentMessage { text, .. } = &item.item else {
                    panic!("expected assistant output")
                };
                text.as_str()
            })
            .collect()
    }

    #[test]
    fn quiet_parent_follows_child_appends_once_across_partial_writes() {
        use std::io::Write;
        let mut r = rig(2);
        pump(&mut r.store, &r.rx);
        let dir = r.journal.with_extension("").join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        let mut file = std::fs::File::create(dir.join("agent-a0.jsonl")).unwrap();
        file.write_all(child_row(&r, "started").as_bytes()).unwrap();
        append_child_link(&r);
        assert_eq!(poll_view(&mut r), (false, true));
        assert_eq!(child_messages(&r), ["started"]);
        let parent_rows = r.store.transcript(&r.key).unwrap().len();

        let pending = child_row(&r, "still working");
        let (a, b) = pending.split_at(pending.len() / 2);
        file.write_all(a.as_bytes()).unwrap();
        assert_eq!(poll_view(&mut r), (false, false));
        file.write_all(b.as_bytes()).unwrap();
        assert_eq!(poll_view(&mut r), (false, true));
        assert_eq!(poll_view(&mut r), (false, false));
        assert_eq!(child_messages(&r), ["started", "still working"]);
        assert_eq!(r.store.transcript(&r.key).unwrap().len(), parent_rows);

        append(&r, "assistant", "parent resumed", 0);
        assert_eq!(poll_view(&mut r), (false, true));
        assert_eq!(child_messages(&r), ["started", "still working"]);
        assert_eq!(r.store.transcript(&r.key).unwrap().len(), parent_rows + 1);
    }

    #[test]
    fn linked_child_journal_can_appear_after_its_parent_goes_quiet() {
        let mut r = rig(2);
        pump(&mut r.store, &r.rx);
        append_child_link(&r);
        assert_eq!(poll_view(&mut r), (false, true));

        let dir = r.journal.with_extension("").join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent-a0.jsonl"), child_row(&r, "created later")).unwrap();
        assert_eq!(poll_view(&mut r), (false, true));
        assert_eq!(child_messages(&r), ["created later"]);
    }

    #[test]
    fn replacing_a_live_child_journal_resets_only_what_is_shown_from_that_file() {
        let mut r = rig(2);
        pump(&mut r.store, &r.rx);
        let dir = r.journal.with_extension("").join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent-a0.jsonl");
        std::fs::write(&path, child_row(&r, "before")).unwrap();
        append_child_link(&r);
        assert_eq!(poll_view(&mut r), (false, true));
        assert_eq!(child_messages(&r), ["before"]);
        let scope = crate::host_protocol::Scope {
            project: r.project.clone(),
            route: r.store.profile.route.clone(),
        };
        let child = child_key(&r.key, "call-child");
        let mut host = r.store.host_snapshot(&scope);
        let original = host.transcripts[&child].items[0].clone();
        r.store.changed = Default::default();

        // A rotated file can already be larger than the cursor and have the same timestamp.
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let replacement = dir.join("replacement");
        let mut corrected: Value = serde_json::from_str(&child_row(&r, "before")).unwrap();
        corrected["message"]["content"][0]["text"] = "after the replacement".into();
        std::fs::write(&replacement, format!("{corrected}\n")).unwrap();
        std::fs::File::open(&replacement)
            .unwrap()
            .set_modified(before)
            .unwrap();
        std::fs::rename(replacement, &path).unwrap();
        poll_view(&mut r);
        assert_eq!(child_messages(&r), ["after the replacement"]);
        let dirty = std::mem::take(&mut r.store.changed);
        let changes = r.store.host_project(&scope, &dirty, &mut host);
        assert!(changes.iter().any(
            |c| matches!(c, crate::host_protocol::Change::Transcript { thread, timeline }
            if thread == &child && timeline.items[0].text == "after the replacement")
        ));
        let current = &host.transcripts[&child].items[0];
        assert_eq!(current.id, original.id);
        assert_eq!(current.revision, original.revision);
        assert_eq!(current.text, "after the replacement");
        assert_eq!(
            r.store
                .transcript(&r.key)
                .unwrap()
                .user_messages("message number 0", &[]),
            1
        );
        assert_eq!(poll_view(&mut r), (false, false));
    }

    #[test]
    fn foreground_child_output_arrives_before_the_parent_records_its_result() {
        let mut r = rig(2);
        pump(&mut r.store, &r.rx);
        append_agent_call(&r, false);
        assert_eq!(poll_view(&mut r), (false, true));
        assert!(child_messages(&r).is_empty());

        let dir = r.journal.with_extension("").join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent-a0.jsonl"), child_row(&r, "looking")).unwrap();
        std::fs::write(
            dir.join("agent-a0.meta.json"),
            r#"{"toolUseId":"call-child"}"#,
        )
        .unwrap();
        assert_eq!(poll_view(&mut r), (false, true));
        assert_eq!(child_messages(&r), ["looking"]);
        append_agent_result(&r, "completed");
        assert_eq!(poll_view(&mut r), (false, true));
        assert_eq!(child_messages(&r), ["looking"]);
        assert_eq!(poll_view(&mut r), (false, false));
    }

    #[test]
    fn legacy_client_stays_live_while_only_its_child_writes() {
        let mut r = rig(2);
        pump(&mut r.store, &r.rx);
        append_child_link(&r);
        let dir = r.journal.with_extension("").join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent-a0.jsonl");
        std::fs::write(&path, child_row(&r, "busy")).unwrap();
        poll_view(&mut r);

        let old = SystemTime::now() - Duration::from_secs(600);
        std::fs::File::open(&r.journal)
            .unwrap()
            .set_modified(old)
            .unwrap();
        poll_view(&mut r);
        poll_view(&mut r);
        assert!(r.store.is_native_live(&r.key));
        assert!(r.store.is_viewer(&r.key));

        std::fs::File::open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        poll_view(&mut r); // The reader sees the changed child timestamp.
        poll_view(&mut r); // No live owner: drain one last time.
        r.store.refresh_native(Instant::now(), true);
        assert!(!r.store.is_viewer(&r.key));
        assert!(!r.store.is_native_live(&r.key));
    }

    #[test]
    fn registered_client_exit_drains_child_once_then_releases_even_a_fresh_journal() {
        use std::io::Write;
        let mut r = rig(2);
        pump(&mut r.store, &r.rx);
        let registry = r
            .store
            .claude()
            .launcher
            .profiles
            .subscription
            .join("sessions");
        std::fs::create_dir_all(&registry).unwrap();
        let pid = std::process::id();
        let registration = registry.join(format!("{pid}.json"));
        std::fs::write(
            &registration,
            json!({"pid":pid,"sessionId":ID,"entrypoint":"cli"}).to_string(),
        )
        .unwrap();

        append_child_link(&r);
        let dir = r.journal.with_extension("").join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        let mut file = std::fs::File::create(dir.join("agent-a0.jsonl")).unwrap();
        file.write_all(child_row(&r, "started").as_bytes()).unwrap();
        poll_view(&mut r);
        assert!(r.store.is_native_live(&r.key));
        file.write_all(child_row(&r, "finished").as_bytes())
            .unwrap();
        std::fs::remove_file(registration).unwrap();
        assert_eq!(poll_view(&mut r), (false, true));
        assert_eq!(child_messages(&r), ["started", "finished"]);
        r.store.refresh_native(Instant::now(), true);
        assert!(!r.store.is_viewer(&r.key));
        assert!(!r.store.is_native_live(&r.key));
    }

    #[test]
    fn a_large_journal_follows_within_one_tick() {
        let mut r = rig(20_000);
        assert!(pump_kinds(&mut r.store, &r.rx).0);
        append(&r, "assistant", "the newest message", 0);
        let started = Instant::now();
        r.store.refresh_native(started, true);
        assert_eq!(pump_kinds(&mut r.store, &r.rx), (false, true));
        assert!(
            started.elapsed() < EVERY,
            "follow took {:?}",
            started.elapsed()
        );
        assert_eq!(r.store.transcript(&r.key).unwrap().len(), 20_001);
    }

    #[test]
    fn a_journal_that_keeps_growing_is_followed_past_the_quiet_heuristic() {
        let mut r = rig(2);
        pump_kinds(&mut r.store, &r.rx);
        // Written, but the stamp looks 10 minutes old: the age rule says not live.
        append(&r, "assistant", "still working", 600);
        r.store.refresh_native(Instant::now(), true);
        assert!(!r.store.is_native_live(&r.key));
        assert!(
            pump_kinds(&mut r.store, &r.rx).1,
            "the new row still arrives"
        );
        assert!(r.store.is_viewer(&r.key));

        // Quiet and unmoved: let go. When it writes again the open thread re-attaches.
        r.store.refresh_native(Instant::now(), true);
        assert!(!r.store.is_viewer(&r.key));
        append(&r, "user", "back again", 0);
        r.store.refresh_native(Instant::now(), true);
        assert!(r.store.is_native_live(&r.key));
        assert!(r.store.is_viewer(&r.key), "re-attached to the live journal");
        pump_kinds(&mut r.store, &r.rx);
        assert_eq!(r.store.transcript(&r.key).unwrap().len(), 4);
    }

    fn append_as(rig: &Rig, entrypoint: &str, text: &str) {
        let row = row_json("user", &rig.project, text).replace(
            "\"entrypoint\":\"cli\"",
            &format!("\"entrypoint\":\"{entrypoint}\""),
        );
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&rig.journal)
            .unwrap();
        std::io::Write::write_all(&mut f, row.as_bytes()).unwrap();
        f.set_modified(SystemTime::now()).unwrap();
    }

    #[test]
    fn a_stock_continue_after_ox_ended_its_session_is_the_newest_owner() {
        let mut r = rig(2);
        // OX drove the session and ended it; its journal is as OX left it.
        let baseline = SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .append(true)
            .open(&r.journal)
            .unwrap()
            .set_modified(baseline)
            .unwrap();
        r.store
            .claude_mut()
            .native
            .ox_seen
            .insert(r.key.clone(), Some(baseline));
        // OX's own headless write is fresh but is not a foreign owner.
        append_as(&r, live::HEADLESS_ENTRYPOINT, "reply written by OX");
        assert!(!r.store.native_live_now(&r.key));
        // `claude --continue` has no id on its argv: only its journal row names it.
        append_as(&r, "cli", "typed into stock claude --continue");
        assert!(r.store.native_live_now(&r.key));
        assert!(
            r.store.claude_resume_send(&r.key, None).is_err(),
            "no second writer"
        );
    }

    #[test]
    fn a_replaced_child_retires_nested_tools_that_its_new_journal_no_longer_names() {
        let mut r = rig(2);
        pump(&mut r.store, &r.rx);
        let dir = r.journal.with_extension("").join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        let child = dir.join("agent-a0.jsonl");
        let mut launch: Value = serde_json::from_str(&child_row(&r, "research")).unwrap();
        launch["message"]["content"] = json!([{"type":"tool_use","name":"Agent","id":"nested-tool","input":{"description":"Investigate"}}]);
        std::fs::write(&child, format!("{launch}\n")).unwrap();
        append_child_link(&r);
        assert_eq!(poll_view(&mut r), (false, true));
        let nested = child_key(&r.key, "nested-tool");
        assert!(r.store.transcript(&nested).is_some());
        r.store.select(&nested).unwrap();

        let fresh = dir.join("replacement");
        std::fs::write(&fresh, child_row(&r, "rewritten without a nested call")).unwrap();
        std::fs::rename(fresh, child).unwrap();
        assert!(poll_view(&mut r).0);
        assert!(r.store.transcript(&nested).is_none());
        assert!(r.store.catalog_row(&nested).is_none());
        assert_eq!(r.store.selected(), Some(r.key.as_str()));
        assert_eq!(child_messages(&r), ["rewritten without a nested call"]);
    }

    #[test]
    fn a_registry_makes_a_fresh_unregistered_journal_not_live() {
        let mut r = rig(2);
        append_as(&r, "cli", "written seconds ago by a closed terminal");
        let sessions = r
            .store
            .claude()
            .launcher
            .profiles
            .subscription
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        // The registry exists and names no live owner of this session.
        assert!(!r.store.native_live_now(&r.key));
        // A stale entry whose pid is gone does not count either.
        std::fs::write(
            sessions.join("4294967295.json"),
            json!({"pid":4294967295u64,"sessionId":ID,"entrypoint":"cli"}).to_string(),
        )
        .unwrap();
        assert!(!r.store.native_live_now(&r.key));
        // Without a registry directory the journal's age still decides.
        let mut old = rig(2);
        append_as(&old, "cli", "fresh");
        assert!(old.store.native_live_now(&old.key));
    }

    #[test]
    fn a_terminal_handoff_rechecks_the_owner_of_a_browsed_journal() {
        let mut r = rig(2);
        pump(&mut r.store, &r.rx);
        let registry = r
            .store
            .claude()
            .launcher
            .profiles
            .subscription
            .join("sessions");
        std::fs::create_dir_all(&registry).unwrap();
        assert!(!r.store.native_live_now(&r.key));
        assert!(r.store.can_configure_saved_claude(&r.key));

        let pid = std::process::id();
        std::fs::write(
            registry.join(format!("{pid}.json")),
            json!({"pid":pid,"sessionId":ID,"entrypoint":"cli"}).to_string(),
        )
        .unwrap();
        assert!(!r.store.is_native_live(&r.key));
        let result = r.store.dispatch(crate::actions::Action::TerminalText(
            Some(r.key.clone()),
            "/terminal".into(),
        ));
        assert_eq!(
            result
                .err()
                .expect("terminal accepted another writer")
                .to_string(),
            HINT
        );
        assert!(r.store.is_native_live(&r.key));
        assert!(!r.store.in_terminal(&r.key));
        assert!(r.store.take_handoff().is_none());
    }

    #[test]
    fn a_quiet_registered_terminal_client_owns_the_session_despite_ox_history() {
        let mut r = rig(2);
        let baseline = SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .append(true)
            .open(&r.journal)
            .unwrap()
            .set_modified(baseline)
            .unwrap();
        r.store
            .claude_mut()
            .native
            .ox_seen
            .insert(r.key.clone(), Some(baseline));
        assert!(!r.store.native_live_now(&r.key), "nobody else has it");
        // A live stock process registered the session; its journal is quiet.
        let sessions = r
            .store
            .claude()
            .launcher
            .profiles
            .subscription
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join(format!("{}.json", std::process::id())),
            json!({"pid":std::process::id(),"sessionId":ID,"entrypoint":"cli"}).to_string(),
        )
        .unwrap();
        assert!(r.store.native_live_now(&r.key));
    }
}

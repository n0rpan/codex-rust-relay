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
    loaded: Option<SystemTime>,
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
    pub(super) fn native_live_now(&mut self, id: &str) -> bool {
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
        processes.extend(live::ids_in_registry(match origin {
            Origin::Api => &p.launcher.profiles.api,
            _ => &p.launcher.profiles.subscription,
        }));
        // Current Claude Code keeps a registry of its live clients. When it exists it is
        // authoritative: a journal written moments ago (a fixture, a terminal client that
        // just closed) says nothing without a live registered pid. Only an older Claude
        // Code, with no registry, falls back to the journal's age.
        let registry = match origin {
            Origin::Api => &p.launcher.profiles.api,
            _ => &p.launcher.profiles.subscription,
        }
        .join("sessions")
        .is_dir();
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
            let fresh = !registry
                && moved_since_ox
                && modified.is_some_and(|m| live::is_fresh(m, now))
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
            self.start_view_load(id);
        } else if !live {
            self.leave_native(id);
        }
    }

    pub(super) fn view_native(&mut self, id: &str) -> io::Result<()> {
        if self.claude().sessions.contains_key(id) {
            return Ok(());
        }
        let p = self.claude_mut();
        p.serial += 1;
        let generation = p.serial;
        p.sessions.insert(
            id.to_owned(),
            Process {
                generation,
                viewer: Some(Viewer::default()),
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
        let mut follower = viewer.follower.take();
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
                    .profiles
                    .session(&profile.project, &raw, origin)
                    .and_then(|saved| {
                        history::Follower::load(&profile, &key, saved, &alive, |batch| {
                            emit(Data::History(batch))
                        })
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
        if let Some(v) = self
            .claude_mut()
            .sessions
            .get_mut(owner)
            .and_then(|s| s.viewer.as_mut())
        {
            v.loading = false;
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
            blank: None,
            worker: None,
            worker_blank: None,
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
        let mut row = row;
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
            blank: None,
            worker: None,
            worker_blank: None,
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
            r.store.claude_resume_send(&r.key).is_err(),
            "no second writer"
        );
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

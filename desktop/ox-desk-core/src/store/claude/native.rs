//! A Claude Code session that a stock client is driving outside OX. OX reads its journal
//! read-only, refreshes it while it is open, and never resumes it, so the two clients do
//! not write the same transcript. The user can fork it into a new OX session instead.
use super::*;
use crate::claude::live;
use std::{
    collections::HashSet,
    time::{Duration, Instant, SystemTime},
};

pub(super) const HINT: &str = "This conversation is running in Claude Code right now. OX shows it read-only so the two don't collide. When it's closed there, you can continue it here.";
const EVERY: Duration = Duration::from_secs(4);

#[derive(Default)]
pub(super) struct Native {
    live: HashSet<String>,
    /// Sessions OX has driven in this run: their journal age says nothing about a
    /// foreign client, so only a foreign process names them.
    pub(super) ox_seen: HashSet<String>,
    next: Option<Instant>,
}
#[derive(Default)]
pub(super) struct Viewer {
    loaded: Option<SystemTime>,
    pub(super) loading: bool,
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
            return self
                .selected
                .as_ref()
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
        let processes = live::ids_in_processes();
        let now = SystemTime::now();
        let mut found = HashSet::new();
        for key in p.catalog.keys().filter(|k| !k.contains('/')) {
            // OX's own process for it is not a foreign one.
            if p.sessions.get(key).is_some_and(|s| s.viewer.is_none()) {
                continue;
            }
            let Ok(raw) = stock_id(key) else { continue };
            let fresh = !p.native.ox_seen.contains(key)
                && p.launcher
                    .profiles
                    .modified(&self.profile.project, raw, origin)
                    .is_ok_and(|m| live::is_fresh(m, now));
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
            let loaded = launcher
                .profiles
                .session(&profile.project, &raw, origin)
                .and_then(|saved| {
                    history::load(&profile, &key, &saved, &alive, |batch| {
                        emit(Data::History(batch))
                    })
                });
            emit(match loaded {
                Ok(()) => Data::Viewed,
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

    pub(super) fn viewed(&mut self, owner: &str) {
        if let Some(v) = self
            .claude_mut()
            .sessions
            .get_mut(owner)
            .and_then(|s| s.viewer.as_mut())
        {
            v.loading = false;
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
            text.len() * 7 + kind.len()
        );
        let content = if kind == "user" {
            json!(text)
        } else {
            json!([{"type":"text","text":text}])
        };
        json!({"type":kind,"uuid":uuid,"cwd":project,"sessionId":ID,"message":{"role":kind,"id":uuid,"content":content}})
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
            let done = matches!(update.data, Data::Viewed | Data::Failed(_));
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

        // It goes quiet and the process is gone: OX owns it again and a send resumes it.
        f.set_modified(SystemTime::now() - Duration::from_secs(600))
            .unwrap();
        store.refresh_native(Instant::now(), true);
        pump(&mut store, &rx);
        store.refresh_native(Instant::now(), true);
        assert!(!store.is_native_live(&key));
        assert!(store.is_ended(&key) && store.can_compose(Some(&key)));
    }
}

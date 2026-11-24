use ox_desk_core::host_protocol::{Scope, ScopeState, SessionView};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Owned {
    pub(crate) scopes: Vec<SavedScope>,
    pub(crate) reserved: u64,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SavedScope {
    pub(crate) scope: Scope,
    pub(crate) sessions: Vec<SavedSession>,
    // Tokens suffice to retire old approvals. No commands or tool arguments on disk.
    pub(crate) pending: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SavedSession {
    id: String,
    title: String,
    parent: Option<String>,
    updated_at: i64,
    active: bool,
    /// The immutable worker stamp. No task, prompt or result is stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<ox_desk_core::origin::Origin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    marks: Option<crate::workers::Marks>,
}
impl SavedScope {
    pub(crate) fn from_state(s: &ScopeState) -> Self {
        let mut sessions: Vec<_> = s
            .sessions
            .iter()
            .filter(|r| {
                r.detail["owned"] == true || r.parent.is_some() || !r.detail["origin"].is_null()
            })
            .map(|r| SavedSession {
                id: r.id.clone(),
                title: r.title.chars().take(192).collect(),
                parent: r.parent.clone(),
                updated_at: r.updated_at,
                active: matches!(
                    r.status.as_str(),
                    "running" | "needs_input" | "interrupted" | "Last seen running"
                ),
                origin: serde_json::from_value(r.detail["origin"].clone()).ok(),
                marks: serde_json::from_value(r.detail["worker_marks"].clone()).ok(),
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        let mut pending: Vec<_> = s.asks.iter().map(|a| a.token.clone()).collect();
        pending.sort();
        Self {
            scope: s.scope.clone(),
            sessions,
            pending,
        }
    }
    pub(crate) fn origins(
        &self,
    ) -> impl Iterator<Item = (&str, ox_desk_core::origin::Origin, crate::workers::Marks)> {
        self.sessions.iter().filter_map(|s| {
            Some((
                s.id.as_str(),
                s.origin.clone()?,
                s.marks.clone().unwrap_or_default(),
            ))
        })
    }
    pub(crate) fn recover(&self) -> ScopeState {
        ScopeState {
            scope: self.scope.clone(),
            connection: "disconnected".into(),
            capabilities: vec![],
            asks: vec![],
            transcripts: Default::default(),
            detail: serde_json::json!({"retired_approvals":self.pending.len()}),
            sessions: self
                .sessions
                .iter()
                .map(|s| SessionView {
                    id: s.id.clone(),
                    title: s.title.clone(),
                    parent: s.parent.clone(),
                    updated_at: s.updated_at,
                    status: if s.active { "interrupted" } else { "idle" }.into(),
                    capabilities: vec![],
                    detail: serde_json::json!({"owned":true,"recovered":true,"worker":s.origin.is_some(),
                        "origin":s.origin,"worker_marks":s.marks}),
                })
                .collect(),
        }
    }
}

pub(crate) struct Ledger {
    failed: bool,
    path: PathBuf,
    pub(crate) data: Owned,
}
impl Ledger {
    pub(crate) fn open(dir: &Path) -> io::Result<Self> {
        let path = dir.join("ownership-v1.json");
        let data = match File::open(&path) {
            Ok(f) => serde_json::from_reader(BufReader::new(f))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Owned::default(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            path,
            data,
            failed: false,
        })
    }
    pub(crate) fn failed(&self) -> bool {
        self.failed
    }
    pub(crate) fn begin(&mut self, seq: u64) -> io::Result<()> {
        for scope in &mut self.data.scopes {
            scope.pending.clear();
        }
        self.reserve(seq)
    }
    pub(crate) fn next_sequence(&mut self, current: u64) -> io::Result<u64> {
        let next = current
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Host sequence exhausted"))?;
        self.reserve(next)?;
        Ok(next)
    }
    fn reserve(&mut self, seq: u64) -> io::Result<()> {
        if self.data.reserved <= seq {
            let old = self.data.reserved;
            self.data.reserved = seq
                .checked_add(4096)
                .ok_or_else(|| io::Error::other("Sequence exhausted"))?;
            if let Err(e) = self.save() {
                self.data.reserved = old;
                self.failed = true;
                return Err(e);
            }
        }
        Ok(())
    }
    pub(crate) fn update(&mut self, state: SavedScope) -> io::Result<()> {
        let index = self.data.scopes.iter().position(|e| e.scope == state.scope);
        let previous = match index {
            Some(i) if self.data.scopes[i] == state => return Ok(()),
            Some(i) => Some(std::mem::replace(&mut self.data.scopes[i], state)),
            None => {
                self.data.scopes.push(state);
                None
            }
        };
        if let Err(e) = self.save() {
            if let (Some(i), Some(prior)) = (index, previous) {
                self.data.scopes[i] = prior;
            } else {
                self.data.scopes.pop();
            }
            self.failed = true;
            return Err(e);
        }
        Ok(())
    }
    fn save(&self) -> io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        let bytes = serde_json::to_vec(&self.data)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(tmp, &self.path)?;
        // fsync is defined for directory descriptors on supported local filesystems.
        // Rust may use Darwin F_FULLFSYNC for File::sync_all, intended for regular files.
        let dir = File::open(self.path.parent().expect("state directory"))?;
        if unsafe { libc::fsync(std::os::fd::AsRawFd::as_raw_fd(&dir)) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::{
        host_protocol::{AskView, Timeline, TimelineItem},
        runtime::{RequestId, Route},
    };
    #[test]
    fn a_publication_at_the_exact_block_boundary_survives_host_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Ledger::open(tmp.path()).unwrap();
        db.begin(0).unwrap();
        let last = db.next_sequence(db.data.reserved - 1).unwrap();
        let mut restarted = Ledger::open(tmp.path()).unwrap();
        let initial = restarted.data.reserved;
        assert!(initial > last);
        restarted.begin(initial).unwrap();
        assert!(restarted.next_sequence(initial).unwrap() > initial);
        assert!(restarted.next_sequence(u64::MAX).is_err());
    }
    #[test]
    fn a_failed_reservation_never_advances_the_published_watermark() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Ledger::open(tmp.path()).unwrap();
        db.begin(0).unwrap();
        let previous = db.data.reserved;
        fs::create_dir(db.path.with_extension("tmp")).unwrap();
        assert!(db.reserve(previous).is_err());
        assert!(db.failed());
        assert_eq!(db.data.reserved, previous);
        assert_eq!(Ledger::open(tmp.path()).unwrap().data.reserved, previous);
    }
    #[test]
    fn restart_retires_effects_and_preserves_only_metadata_and_a_monotonic_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let scope = Scope {
            project: "/test".into(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let row = SessionView {
            id: "worker".into(),
            title: "inspect tests".into(),
            parent: Some("parent".into()),
            status: "running".into(),
            updated_at: 3,
            capabilities: vec!["stop".into()],
            detail: serde_json::json!({"owned":true,"authorization":"live-token"}),
        };
        let ask = AskView {
            command: None,
            id: RequestId::Number(22),
            turn: None,
            item: None,
            choice_details: vec![],
            token: "ask-token".into(),
            thread: Some("worker".into()),
            title: "Review command".into(),
            detail: "sensitive command".into(),
            choices: vec![],
            questions: vec![],
        };
        let timeline = Timeline {
            items: vec![TimelineItem {
                id: "item".into(),
                turn: "turn".into(),
                role: "user".into(),
                text: "private transcript".into(),
                revision: 1,
                detail: serde_json::Value::Null,
            }],
            detail: serde_json::Value::Null,
        };
        let view = ScopeState {
            scope,
            connection: "ready".into(),
            sessions: vec![row],
            asks: vec![ask],
            transcripts: [("worker".into(), timeline)].into(),
            capabilities: vec![],
            detail: serde_json::Value::Null,
        };
        let mut db = Ledger::open(tmp.path()).unwrap();
        db.reserve(42).unwrap();
        db.update(SavedScope::from_state(&view)).unwrap();
        drop(db);
        let raw = fs::read_to_string(tmp.path().join("ownership-v1.json")).unwrap();
        for private in ["private transcript", "sensitive command", "live-token"] {
            assert!(!raw.contains(private));
        }
        let db = Ledger::open(tmp.path()).unwrap();
        assert!(db.data.reserved > 42);
        let restored = db.data.scopes[0].recover();
        assert_eq!(restored.sessions[0].status, "interrupted");
        assert_eq!(restored.sessions[0].parent.as_deref(), Some("parent"));
        assert!(restored.sessions[0].capabilities.is_empty());
        assert!(restored.asks.is_empty());
        assert!(restored.transcripts.is_empty());
        assert_eq!(restored.detail["retired_approvals"], 1);
    }
}

use crate::{
    Kind, MemoryEntry, MemoryError, NewMemory, RecallHit, RecallQuery, Scope, Sensitivity,
    format_tier1_brief, generate_id, normalize_tags, normalize_text, now_ms, rank_memories,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::fd::AsRawFd,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    next_seq: u64,
    #[serde(default)]
    entries: Vec<MemoryEntry>,
}

impl Default for StoreFile {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            next_seq: 0,
            entries: Vec::new(),
        }
    }
}

impl StoreFile {
    fn alloc_seq(&mut self) -> u64 {
        let max_existing = self.entries.iter().map(|e| e.seq).max().unwrap_or(0);
        let next = self.next_seq.max(max_existing).saturating_add(1);
        self.next_seq = next;
        next
    }
}

fn default_version() -> u32 {
    SCHEMA_VERSION
}

/// Result of upserting a memory entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpsertOutcome {
    Created(MemoryEntry),
    Updated(MemoryEntry),
    Deduplicated(MemoryEntry),
}

impl UpsertOutcome {
    pub fn entry(&self) -> &MemoryEntry {
        match self {
            Self::Created(e) | Self::Updated(e) | Self::Deduplicated(e) => e,
        }
    }

    pub fn action(&self) -> &'static str {
        match self {
            Self::Created(_) => "created",
            Self::Updated(_) => "updated",
            Self::Deduplicated(_) => "deduplicated",
        }
    }
}

/// Summary of a remote LWW sync merge.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MergeStats {
    pub inserted: usize,
    pub updated: usize,
    pub ignored: usize,
    pub rejected_local_only: usize,
}

/// File-backed, lock-protected memory store.
#[derive(Clone, Debug)]
pub struct MemoryStore {
    path: PathBuf,
}

impl MemoryStore {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn open_default() -> Result<Self, MemoryError> {
        Ok(Self::open(crate::default_store_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create, update (by `id`), or deduplicate a memory entry.
    pub fn upsert(&self, input: NewMemory) -> Result<UpsertOutcome, MemoryError> {
        let text = normalize_text(&input.text)?;
        let tags = normalize_tags(&input.tags);

        let _lock = self.lock_exclusive()?;
        let mut file = self.read_unlocked()?;
        let now = now_ms();

        if let Some(id) = input.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let idx = file
                .entries
                .iter()
                .position(|e| e.id == id)
                .ok_or_else(|| MemoryError::NotFound(format!("No memory found with id '{id}'.")))?;
            if file.entries[idx].deleted {
                return Err(MemoryError::InvalidInput(format!(
                    "Memory '{id}' was removed; create a new memory instead of resurrecting it."
                )));
            }
            if file.entries[idx].scope != input.scope {
                return Err(MemoryError::InvalidInput(format!(
                    "Cannot change scope of memory '{id}' in place; forget it and create a new memory."
                )));
            }
            if input.sensitivity < file.entries[idx].sensitivity {
                return Err(MemoryError::InvalidInput(format!(
                    "Cannot lower sensitivity of memory '{id}' from {} to {} in place.",
                    file.entries[idx].sensitivity, input.sensitivity
                )));
            }
            // Promoting a previously shareable entry to LocalOnly withdraws the old shared ID
            // via a payload-free exportable tombstone and stores the local-only copy under a new ID.
            if file.entries[idx].sensitivity != Sensitivity::LocalOnly
                && input.sensitivity == Sensitivity::LocalOnly
            {
                let created_at_ms = file.entries[idx].created_at_ms;
                let tomb_seq = file.alloc_seq();
                let old = &mut file.entries[idx];
                old.deleted = true;
                old.pin = false;
                old.text.clear();
                old.tags.clear();
                old.updated_at_ms = now.max(old.updated_at_ms.saturating_add(1));
                old.source_host = input.source_host.clone();
                old.seq = tomb_seq;

                let mut new_id = generate_id();
                while file.entries.iter().any(|e| e.id == new_id) {
                    new_id = generate_id();
                }
                let new_seq = file.alloc_seq();
                let promoted = MemoryEntry {
                    id: new_id,
                    scope: input.scope,
                    kind: input.kind,
                    sensitivity: Sensitivity::LocalOnly,
                    text,
                    tags,
                    pin: input.pin,
                    created_at_ms,
                    updated_at_ms: now,
                    source_host: input.source_host,
                    seq: new_seq,
                    deleted: false,
                };
                file.entries.push(promoted.clone());
                self.write_unlocked(&file)?;
                return Ok(UpsertOutcome::Updated(promoted));
            }

            let seq = file.alloc_seq();
            let entry = &mut file.entries[idx];
            entry.kind = input.kind;
            entry.sensitivity = input.sensitivity;
            entry.text = text;
            entry.tags = tags;
            entry.pin = input.pin;
            entry.updated_at_ms = now.max(entry.updated_at_ms.saturating_add(1));
            entry.source_host = input.source_host;
            entry.seq = seq;
            let saved = entry.clone();
            self.write_unlocked(&file)?;
            return Ok(UpsertOutcome::Updated(saved));
        }

        // Deduplicate only within the exact same (scope, kind, sensitivity) boundary.
        let existing_idx = file.entries.iter().position(|e| {
            !e.deleted
                && e.scope == input.scope
                && e.kind == input.kind
                && e.sensitivity == input.sensitivity
                && e.text.eq_ignore_ascii_case(&text)
        });
        if let Some(idx) = existing_idx {
            let mut changed = false;
            if !file.entries[idx].pin && input.pin {
                file.entries[idx].pin = true;
                changed = true;
            }
            for tag in tags {
                if !file.entries[idx].tags.contains(&tag)
                    && file.entries[idx].tags.len() < crate::MAX_TAGS
                {
                    file.entries[idx].tags.push(tag);
                    changed = true;
                }
            }
            if changed {
                let seq = file.alloc_seq();
                let entry = &mut file.entries[idx];
                entry.updated_at_ms = now.max(entry.updated_at_ms.saturating_add(1));
                entry.source_host = input.source_host;
                entry.seq = seq;
                let saved = entry.clone();
                self.write_unlocked(&file)?;
                return Ok(UpsertOutcome::Updated(saved));
            }
            return Ok(UpsertOutcome::Deduplicated(file.entries[idx].clone()));
        }

        let mut id = generate_id();
        while file.entries.iter().any(|e| e.id == id) {
            id = generate_id();
        }
        let seq = file.alloc_seq();
        let entry = MemoryEntry {
            id,
            scope: input.scope,
            kind: input.kind,
            sensitivity: input.sensitivity,
            text,
            tags,
            pin: input.pin,
            created_at_ms: now,
            updated_at_ms: now,
            source_host: input.source_host,
            seq,
            deleted: false,
        };
        file.entries.push(entry.clone());
        self.write_unlocked(&file)?;
        Ok(UpsertOutcome::Created(entry))
    }

    /// Tombstone-delete a memory entry by id.
    ///
    /// Clears `text` and `tags` so tombstones remain payload-free during peer sync.
    pub fn forget(&self, id: &str, source_host: &str) -> Result<MemoryEntry, MemoryError> {
        let id = id.trim();
        let _lock = self.lock_exclusive()?;
        let mut file = self.read_unlocked()?;
        let now = now_ms();
        let seq = file.alloc_seq();
        let entry = file
            .entries
            .iter_mut()
            .find(|e| e.id == id && !e.deleted)
            .ok_or_else(|| {
                MemoryError::NotFound(format!("No active memory found with id '{id}'."))
            })?;
        entry.deleted = true;
        entry.pin = false;
        entry.text.clear();
        entry.tags.clear();
        entry.updated_at_ms = now.max(entry.updated_at_ms.saturating_add(1));
        entry.source_host = source_host.to_string();
        entry.seq = seq;
        let saved = entry.clone();
        self.write_unlocked(&file)?;
        Ok(saved)
    }

    /// Edit text without changing scope, sensitivity, or a newer revision of the entry.
    pub fn edit_text(
        &self,
        id: &str,
        expected_seq: u64,
        text: &str,
        source_host: &str,
    ) -> Result<MemoryEntry, MemoryError> {
        let text = normalize_text(text)?;
        let _lock = self.lock_exclusive()?;
        let mut file = self.read_unlocked()?;
        let seq = file.alloc_seq();
        let entry = file
            .entries
            .iter_mut()
            .find(|e| e.id == id && !e.deleted)
            .ok_or_else(|| MemoryError::NotFound("This memory is no longer available.".into()))?;
        if entry.seq != expected_seq {
            return Err(MemoryError::InvalidInput(
                "This memory changed on the host. Refresh it before editing.".into(),
            ));
        }
        entry.text = text;
        entry.seq = seq;
        entry.updated_at_ms = now_ms().max(entry.updated_at_ms.saturating_add(1));
        entry.source_host = source_host.to_owned();
        let saved = entry.clone();
        self.write_unlocked(&file)?;
        Ok(saved)
    }

    /// Pin or unpin an active memory entry.
    pub fn set_pin(
        &self,
        id: &str,
        pin: bool,
        source_host: &str,
    ) -> Result<MemoryEntry, MemoryError> {
        let id = id.trim();
        let _lock = self.lock_exclusive()?;
        let mut file = self.read_unlocked()?;
        let now = now_ms();
        let seq = file.alloc_seq();
        let entry = file
            .entries
            .iter_mut()
            .find(|e| e.id == id && !e.deleted)
            .ok_or_else(|| {
                MemoryError::NotFound(format!("No active memory found with id '{id}'."))
            })?;
        entry.pin = pin;
        entry.updated_at_ms = now.max(entry.updated_at_ms.saturating_add(1));
        entry.source_host = source_host.to_string();
        entry.seq = seq;
        let saved = entry.clone();
        self.write_unlocked(&file)?;
        Ok(saved)
    }

    /// List active entries, optionally filtered by scope, kind, and sensitivity ceiling.
    pub fn list(
        &self,
        scope: Option<&Scope>,
        kind: Option<Kind>,
        max_sensitivity: Sensitivity,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        let entries = self.snapshot_entries()?;
        let mut items: Vec<MemoryEntry> = entries
            .into_iter()
            .filter(|e| {
                !e.deleted
                    && e.sensitivity <= max_sensitivity
                    && scope.is_none_or(|s| &e.scope == s)
                    && kind.is_none_or(|k| e.kind == k)
            })
            .collect();
        items.sort_by(|a, b| {
            b.pin
                .cmp(&a.pin)
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(items)
    }

    /// Active memories available to the project or this host, without other projects.
    pub fn list_for_project(
        &self,
        project: &Path,
        host: &str,
        max_sensitivity: Sensitivity,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        let project_scope = Scope::Project(crate::canonical_project_id(project));
        let host_scope = Scope::Host(host.to_owned());
        let mut entries = self.list(None, None, max_sensitivity)?;
        entries.retain(|entry| {
            entry.scope == Scope::Global
                || entry.scope == project_scope
                || entry.scope == host_scope
        });
        Ok(entries)
    }

    /// Rank and recall memories matching `query`.
    pub fn recall(&self, query: &RecallQuery) -> Result<Vec<RecallHit>, MemoryError> {
        let entries = self.snapshot_entries()?;
        Ok(rank_memories(&entries, query))
    }

    /// Build a compact, token-budgeted Tier-1 brief of pinned memories.
    pub fn tier1_brief(
        &self,
        project: Option<&str>,
        host: Option<&str>,
        max_sensitivity: Sensitivity,
        max_chars: usize,
    ) -> Result<Option<String>, MemoryError> {
        let entries = self.snapshot_entries()?;
        Ok(format_tier1_brief(
            &entries,
            project,
            host,
            max_sensitivity,
            max_chars,
        ))
    }

    /// Export non-`LocalOnly` entries with `seq > since_seq` (including payload-free tombstones)
    /// for peer synchronization. Returns `(entries, latest_seq)`.
    ///
    /// By construction, this API can never export `Sensitivity::LocalOnly` records.
    pub fn export_peer_since(
        &self,
        since_seq: u64,
    ) -> Result<(Vec<MemoryEntry>, u64), MemoryError> {
        let file = {
            let _lock = self.lock_shared()?;
            self.read_unlocked()?
        };
        let mut entries: Vec<MemoryEntry> = file
            .entries
            .into_iter()
            .filter(|e| e.seq > since_seq && e.sensitivity != Sensitivity::LocalOnly)
            .collect();
        entries.sort_by_key(|e| e.seq);
        Ok((entries, file.next_seq))
    }

    /// Conflict-free LWW merge of remote entries ordered by `(updated_at_ms, source_host)`.
    ///
    /// Rejects incoming `LocalOnly` records and never overwrites an existing local `LocalOnly` entry.
    pub fn merge_remote(&self, incoming: &[MemoryEntry]) -> Result<MergeStats, MemoryError> {
        let _lock = self.lock_exclusive()?;
        let mut file = self.read_unlocked()?;
        let mut stats = MergeStats::default();
        let mut dirty = false;

        for remote in incoming {
            if remote.sensitivity == Sensitivity::LocalOnly {
                stats.rejected_local_only += 1;
                continue;
            }
            if remote.id.trim().is_empty()
                || remote.id.len() > 64
                || (!remote.deleted && remote.text.chars().count() > crate::MAX_TEXT_CHARS)
            {
                stats.ignored += 1;
                continue;
            }
            let mut sanitized = remote.clone();
            if sanitized.deleted {
                sanitized.text.clear();
                sanitized.tags.clear();
                sanitized.pin = false;
            }
            if let Some(idx) = file.entries.iter().position(|e| e.id == sanitized.id) {
                if file.entries[idx].sensitivity == Sensitivity::LocalOnly {
                    stats.rejected_local_only += 1;
                    continue;
                }
                let merged_sensitivity = file.entries[idx].sensitivity.max(sanitized.sensitivity);
                let remote_key = (sanitized.updated_at_ms, sanitized.source_host.as_str());
                let local_key = (
                    file.entries[idx].updated_at_ms,
                    file.entries[idx].source_host.as_str(),
                );
                if remote_key > local_key {
                    sanitized.sensitivity = merged_sensitivity;
                    sanitized.seq = file.alloc_seq();
                    file.entries[idx] = sanitized;
                    stats.updated += 1;
                    dirty = true;
                } else if merged_sensitivity > file.entries[idx].sensitivity {
                    let seq = file.alloc_seq();
                    file.entries[idx].sensitivity = merged_sensitivity;
                    file.entries[idx].seq = seq;
                    stats.updated += 1;
                    dirty = true;
                } else {
                    stats.ignored += 1;
                }
            } else {
                sanitized.seq = file.alloc_seq();
                file.entries.push(sanitized);
                stats.inserted += 1;
                dirty = true;
            }
        }

        if dirty {
            self.write_unlocked(&file)?;
        }
        Ok(stats)
    }

    fn snapshot_entries(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        let _lock = self.lock_shared()?;
        Ok(self.read_unlocked()?.entries)
    }

    fn lock_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "memory.json".into());
        name.push(".lock");
        self.path.with_file_name(name)
    }

    fn ensure_parent_dir(&self) -> Result<(), MemoryError> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            if let Ok(meta) = fs::symlink_metadata(parent) {
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(MemoryError::InvalidInput(format!(
                        "Refusing non-directory or symlink memory parent '{}'.",
                        parent.display()
                    )));
                }
            } else {
                fs::create_dir_all(parent)?;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }

    fn refuse_symlink(path: &Path) -> Result<(), MemoryError> {
        if let Ok(meta) = fs::symlink_metadata(path)
            && meta.file_type().is_symlink()
        {
            return Err(MemoryError::InvalidInput(format!(
                "Refusing symlink at '{}'.",
                path.display()
            )));
        }
        Ok(())
    }

    fn lock_shared(&self) -> Result<FileLock, MemoryError> {
        self.lock_with(libc::LOCK_SH)
    }

    fn lock_exclusive(&self) -> Result<FileLock, MemoryError> {
        self.lock_with(libc::LOCK_EX)
    }

    fn lock_with(&self, op: libc::c_int) -> Result<FileLock, MemoryError> {
        self.ensure_parent_dir()?;
        let lock_path = self.lock_path();
        Self::refuse_symlink(&lock_path)?;
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lock_path)?;
        loop {
            let rc = unsafe { libc::flock(lock_file.as_raw_fd(), op) };
            if rc == 0 {
                return Ok(FileLock(lock_file));
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(MemoryError::Io(err));
            }
        }
    }

    fn read_unlocked(&self) -> Result<StoreFile, MemoryError> {
        Self::refuse_symlink(&self.path)?;
        match fs::read_to_string(&self.path) {
            Ok(raw) if raw.trim().is_empty() => Ok(StoreFile::default()),
            Ok(raw) => {
                let parsed: StoreFile = serde_json::from_str(&raw)?;
                if parsed.version != SCHEMA_VERSION {
                    return Err(MemoryError::InvalidInput(format!(
                        "Unsupported memory store schema version {} (expected {SCHEMA_VERSION}).",
                        parsed.version
                    )));
                }
                Ok(parsed)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile::default()),
            Err(err) => Err(MemoryError::Io(err)),
        }
    }

    fn write_unlocked(&self, file: &StoreFile) -> Result<(), MemoryError> {
        self.ensure_parent_dir()?;
        Self::refuse_symlink(&self.path)?;
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o600))?;
        let payload = serde_json::to_vec_pretty(file)?;
        tmp.write_all(&payload)?;
        tmp.write_all(b"\n")?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.path)
            .map_err(|e| MemoryError::Io(e.error))?;
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

struct FileLock(File);

impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_edits_preserve_scope_and_reject_stale_revisions() {
        let temp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(temp.path().join("memory.json"));
        let initial = store
            .upsert(new_mem(
                "Test in the sandbox.",
                Scope::Project("payments".into()),
                Kind::Preference,
                Sensitivity::LocalOnly,
                true,
            ))
            .unwrap();
        let initial = initial.entry();
        let updated = store
            .edit_text(
                &initial.id,
                initial.seq,
                "Keep tests in the sandbox.",
                "host-a",
            )
            .unwrap();
        assert_eq!(updated.scope, initial.scope);
        assert_eq!(updated.sensitivity, Sensitivity::LocalOnly);
        assert_eq!(updated.tags, initial.tags);
        assert!(updated.pin);
        assert!(
            store
                .edit_text(&initial.id, initial.seq, "Stale change.", "host-b")
                .is_err()
        );
        assert_eq!(
            store.list(None, None, Sensitivity::LocalOnly).unwrap(),
            vec![updated]
        );
    }

    fn new_mem(
        text: &str,
        scope: Scope,
        kind: Kind,
        sensitivity: Sensitivity,
        pin: bool,
    ) -> NewMemory {
        NewMemory {
            id: None,
            scope,
            kind,
            sensitivity,
            text: text.to_string(),
            tags: vec!["Rust".to_string(), "OX".to_string()],
            pin,
            source_host: "host-a".to_string(),
        }
    }

    #[test]
    fn upsert_deduplicates_within_sensitivity_and_scrubs_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(dir.path().join("memory.json"));

        let first = store
            .upsert(new_mem(
                "Use Claude subscription for private work.",
                Scope::Global,
                Kind::RoutingPolicy,
                Sensitivity::Private,
                false,
            ))
            .unwrap();
        assert_eq!(first.action(), "created");
        let id = first.entry().id.clone();

        // Exact duplicate with same sensitivity returns Deduplicated
        let dup = store
            .upsert(new_mem(
                "use claude subscription for private work.",
                Scope::Global,
                Kind::RoutingPolicy,
                Sensitivity::Private,
                false,
            ))
            .unwrap();
        assert_eq!(dup.action(), "deduplicated");
        assert_eq!(dup.entry().id, id);

        // Saving same text at a different sensitivity creates a distinct entry (no downgrade/leak)
        let public_copy = store
            .upsert(new_mem(
                "Use Claude subscription for private work.",
                Scope::Global,
                Kind::RoutingPolicy,
                Sensitivity::Public,
                false,
            ))
            .unwrap();
        assert_eq!(public_copy.action(), "created");
        assert_ne!(public_copy.entry().id, id);

        // Updating by id cannot lower sensitivity
        let downgrade = store.upsert(NewMemory {
            id: Some(id.clone()),
            ..new_mem(
                "Use Claude subscription for private work.",
                Scope::Global,
                Kind::RoutingPolicy,
                Sensitivity::Public,
                false,
            )
        });
        assert!(downgrade.is_err());

        // Forget leaves a payload-free tombstone excluded from list()
        store.forget(&id, "host-a").unwrap();
        let (exported, seq) = store.export_peer_since(0).unwrap();
        assert!(seq >= 3);
        let tomb = exported.iter().find(|e| e.id == id).unwrap();
        assert!(tomb.deleted);
        assert!(tomb.text.is_empty());
        assert!(tomb.tags.is_empty());
    }

    #[test]
    fn remote_sync_uses_monotonic_seq_respects_lww_and_blocks_local_only() {
        let dir = tempfile::tempdir().unwrap();
        let store_a = MemoryStore::open(dir.path().join("a.json"));
        let store_b = MemoryStore::open(dir.path().join("b.json"));

        let shared = store_a
            .upsert(new_mem(
                "Prefer fast models for web research.",
                Scope::Global,
                Kind::RoutingPolicy,
                Sensitivity::Public,
                true,
            ))
            .unwrap()
            .entry()
            .clone();
        let local_secret = store_a
            .upsert(new_mem(
                "Local GPU box has 24GB VRAM.",
                Scope::Host("yoga".to_string()),
                Kind::Fact,
                Sensitivity::LocalOnly,
                false,
            ))
            .unwrap()
            .entry()
            .clone();

        // Peer export never returns LocalOnly
        let (exported, cursor) = store_a.export_peer_since(0).unwrap();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].id, shared.id);
        assert_eq!(store_a.export_peer_since(cursor).unwrap().0.len(), 0);

        // Merge into store_b inserts shared; attempting to force local_only is rejected
        let stats = store_b
            .merge_remote(&[shared.clone(), local_secret])
            .unwrap();
        assert_eq!(stats.inserted, 1);
        assert_eq!(stats.rejected_local_only, 1);

        // Newer update wins over older
        let mut newer = shared.clone();
        newer.text = "Prefer OpenRouter fast models for public research.".to_string();
        newer.updated_at_ms = shared.updated_at_ms + 50;
        newer.source_host = "host-b".to_string();
        let stats = store_a.merge_remote(&[newer.clone()]).unwrap();
        assert_eq!(stats.updated, 1);

        // Stale update is ignored
        let stats = store_a.merge_remote(&[shared]).unwrap();
        assert_eq!(stats.ignored, 1);
        let listed = store_a.list(None, None, Sensitivity::Private).unwrap();
        assert_eq!(listed[0].text, newer.text);
    }

    #[test]
    fn concurrent_writers_do_not_lose_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.json");
        let handles: Vec<_> = (0..4)
            .map(|worker| {
                let p = path.clone();
                std::thread::spawn(move || {
                    let store = MemoryStore::open(p);
                    for i in 0..10 {
                        store
                            .upsert(NewMemory {
                                id: None,
                                scope: Scope::Global,
                                kind: Kind::Fact,
                                sensitivity: Sensitivity::Private,
                                text: format!("worker {worker} fact {i}"),
                                tags: vec![format!("w{worker}")],
                                pin: false,
                                source_host: "host-a".to_string(),
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let store = MemoryStore::open(path);
        let items = store.list(None, None, Sensitivity::LocalOnly).unwrap();
        assert_eq!(items.len(), 40);
    }

    #[test]
    fn merge_remote_preserves_monotonic_sensitivity() {
        let dir = tempfile::tempdir().unwrap();
        let store_a = MemoryStore::open(dir.path().join("a.json"));
        let store_b = MemoryStore::open(dir.path().join("b.json"));

        let base = store_a
            .upsert(new_mem(
                "Shared public note.",
                Scope::Global,
                Kind::Fact,
                Sensitivity::Public,
                false,
            ))
            .unwrap()
            .entry()
            .clone();
        store_b.merge_remote(std::slice::from_ref(&base)).unwrap();

        // Store A promotes to Private
        let promoted = store_a
            .upsert(NewMemory {
                id: Some(base.id.clone()),
                ..new_mem(
                    "Shared public note.",
                    Scope::Global,
                    Kind::Fact,
                    Sensitivity::Private,
                    false,
                )
            })
            .unwrap()
            .entry()
            .clone();

        // Store B concurrently edits still-Public copy with a higher timestamp
        let mut concurrent_public = base;
        concurrent_public.text = "Edited public copy.".to_string();
        concurrent_public.updated_at_ms = promoted.updated_at_ms + 100;
        concurrent_public.source_host = "host-b".to_string();

        // Merging B into A updates text, but NEVER downgrades sensitivity back to Public
        let stats = store_a.merge_remote(&[concurrent_public]).unwrap();
        assert_eq!(stats.updated, 1);
        let listed = store_a.list(None, None, Sensitivity::LocalOnly).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].text, "Edited public copy.");
        assert_eq!(listed[0].sensitivity, Sensitivity::Private);
    }

    #[test]
    fn promoting_shared_entry_to_local_only_withdraws_peer_copy() {
        let dir = tempfile::tempdir().unwrap();
        let store_a = MemoryStore::open(dir.path().join("a.json"));
        let store_b = MemoryStore::open(dir.path().join("b.json"));

        let shared = store_a
            .upsert(new_mem(
                "Originally shared note.",
                Scope::Global,
                Kind::Fact,
                Sensitivity::Private,
                true,
            ))
            .unwrap()
            .entry()
            .clone();
        let (batch1, cursor1) = store_a.export_peer_since(0).unwrap();
        store_b.merge_remote(&batch1).unwrap();
        assert_eq!(
            store_b
                .list(None, None, Sensitivity::LocalOnly)
                .unwrap()
                .len(),
            1
        );

        // Promote to LocalOnly on store A
        let promoted = store_a
            .upsert(NewMemory {
                id: Some(shared.id.clone()),
                ..new_mem(
                    "Now strictly local note.",
                    Scope::Global,
                    Kind::Fact,
                    Sensitivity::LocalOnly,
                    true,
                )
            })
            .unwrap()
            .entry()
            .clone();
        assert_ne!(promoted.id, shared.id);
        assert_eq!(promoted.sensitivity, Sensitivity::LocalOnly);

        // Export delta from store A: only payload-free tombstone for old shared ID is exported
        let (batch2, _cursor2) = store_a.export_peer_since(cursor1).unwrap();
        assert_eq!(batch2.len(), 1);
        assert_eq!(batch2[0].id, shared.id);
        assert!(batch2[0].deleted);
        assert!(batch2[0].text.is_empty());

        // Merging delta into store B removes the old copy completely
        store_b.merge_remote(&batch2).unwrap();
        assert!(
            store_b
                .list(None, None, Sensitivity::LocalOnly)
                .unwrap()
                .is_empty()
        );
        // Store A still keeps the promoted LocalOnly entry
        let local_items = store_a.list(None, None, Sensitivity::LocalOnly).unwrap();
        assert_eq!(local_items.len(), 1);
        assert_eq!(local_items[0].id, promoted.id);
        assert_eq!(local_items[0].text, "Now strictly local note.");
    }
}

//! Keep catalog lookups independent of the number of paged-in threads.
//! Display order belongs to the merged, cross-route list.
use super::ThreadRow;
use crate::codex::notice::Status;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy)]
pub(super) enum PromptSource {
    FirstTurn,
    Live,
    History { oldest_page: bool },
}

#[derive(Default)]
pub(super) struct Catalog {
    pub(super) titles: crate::titles::Index,
    pub(super) rows: Vec<ThreadRow>,
    positions: HashMap<String, usize>,
    // True once the excerpt is the first known prompt. Stock previews always take precedence.
    inferred_previews: HashMap<String, bool>,
}

impl Catalog {
    pub(super) fn clear(&mut self) {
        *self = Self {
            titles: self.titles.clone(),
            ..Default::default()
        };
    }
    pub(super) fn get(&self, id: &str) -> Option<&ThreadRow> {
        self.rows.get(*self.positions.get(id)?)
    }
    pub(super) fn get_mut(&mut self, id: &str) -> Option<&mut ThreadRow> {
        self.rows.get_mut(*self.positions.get(id)?)
    }
    pub(super) fn has_first_preview(&self, id: &str) -> bool {
        self.inferred_previews.get(id) != Some(&false)
    }
    pub(super) fn insert(&mut self, mut row: ThreadRow) {
        self.titles.decorate(&mut row);
        match self.positions.entry(row.id.clone()) {
            std::collections::hash_map::Entry::Vacant(pos) => {
                pos.insert(self.rows.len());
                self.rows.push(row);
            }
            std::collections::hash_map::Entry::Occupied(pos) => self.rows[*pos.get()] = row,
        }
    }
    /// A late list/read can enrich a placeholder without undoing a live rename.
    fn merge_title(&mut self, source: &ThreadRow) -> bool {
        use crate::codex::history::nonblank;
        let Some(&index) = self.positions.get(&source.id) else {
            return false;
        };
        let row = &mut self.rows[index];
        // A selection restored from older layout files may precede its first stock list or read.
        let mut changed = source.updated_at > row.updated_at;
        row.updated_at = row.updated_at.max(source.updated_at);
        for (existing, incoming) in [
            (&mut row.name, &source.name),
            (&mut row.agent_nickname, &source.agent_nickname),
        ] {
            if existing.as_deref().and_then(nonblank).is_none()
                && let Some(value) = incoming.as_deref().and_then(nonblank)
            {
                *existing = Some(value.into());
                changed = true;
            }
        }
        if nonblank(&source.preview).is_some()
            && (self.inferred_previews.remove(&row.id).is_some()
                || nonblank(&row.preview).is_none())
        {
            row.preview.clone_from(&source.preview);
            changed = true;
        }
        changed
    }

    fn infer_preview<'a>(
        &mut self,
        id: &str,
        source: PromptSource,
        line: impl FnOnce() -> Option<&'a str>,
    ) -> bool {
        let Some(&index) = self.positions.get(id) else {
            return false;
        };
        let row = &mut self.rows[index];
        let can_refine = matches!(source, PromptSource::History { .. })
            && self.inferred_previews.get(id) == Some(&false);
        if !row.preview.trim().is_empty() && !can_refine {
            return false;
        }
        let Some(line) = line() else { return false };
        let excerpt = line.chars().take(120).collect::<String>();
        let known_first = matches!(
            source,
            PromptSource::FirstTurn | PromptSource::History { oldest_page: true }
        );
        self.inferred_previews.insert(id.into(), known_first);
        if row.preview == excerpt {
            return false;
        }
        row.preview = excerpt;
        true
    }

    pub(super) fn remove(&mut self, id: &str) {
        self.inferred_previews.remove(id);
        if let Some(pos) = self.positions.remove(id) {
            self.rows.swap_remove(pos);
            if let Some(row) = self.rows.get(pos) {
                *self.positions.get_mut(&row.id).unwrap() = pos;
            }
        }
    }
    pub(super) fn keep(&mut self, ids: &HashSet<String>) {
        let mut kept = Self {
            titles: self.titles.clone(),
            ..Default::default()
        };
        for id in ids {
            if let Some(row) = self.get(id) {
                kept.insert(row.clone());
                if let Some(&known_first) = self.inferred_previews.get(id) {
                    kept.inferred_previews.insert(id.clone(), known_first);
                }
            }
        }
        *self = kept;
    }
}

impl super::Store {
    pub(super) fn sync_title(&mut self, source: &ThreadRow) {
        if self.catalog.merge_title(source) {
            self.title_changed();
        }
    }
    fn title_changed(&mut self) {
        self.catalog_revision += 1;
        self.changed.mark(crate::changed::Changed::THREADS);
    }
    pub(super) fn prompt_preview<'a>(
        &mut self,
        id: &str,
        source: PromptSource,
        line: impl FnOnce() -> Option<&'a str>,
    ) {
        if self.catalog.infer_preview(id, source, line) {
            self.title_changed();
        }
    }
    pub fn catalog_row(&self, id: &str) -> Option<&ThreadRow> {
        self.catalog.get(id)
    }

    pub(super) fn record_lineage(&mut self, row: &ThreadRow) {
        if let Some(parent) = &row.parent_thread_id {
            // A shared list contains children from other launch profiles as well.
            if row.model_provider == self.profile.provider
                && let Some(parent_alias) = self.authorized_models.get(parent)
            {
                let child_alias = row.model.as_ref().unwrap_or(parent_alias).clone();
                if self.same_model_route(&child_alias) {
                    self.authorized_models
                        .entry(row.id.clone())
                        .or_insert(child_alias);
                }
            }
            self.child_threads
                .insert(row.id.clone(), (parent.clone(), row.cwd.clone()));
        }
    }
    pub(super) fn activity_status(&mut self, id: &str, status: Status) {
        if let Some(row) = self.catalog.get_mut(id) {
            row.status = Some(status);
            self.changed.mark(crate::changed::Changed::THREADS);
        }
    }
    pub(super) fn observed_row(&mut self, row: ThreadRow) {
        let known_parent = row.parent_thread_id.as_ref().is_some_and(|parent| {
            self.transcripts.get(parent).is_some_and(|t| t.verified)
                || self.child_threads.contains_key(parent)
        });
        if (row.cwd != self.profile.project
            && !(known_parent && row.cwd.starts_with(&self.profile.project)))
            || (!self.query.is_empty()
                && self.catalog.get(&row.id).is_none()
                && !row
                    .parent_thread_id
                    .as_ref()
                    .is_some_and(|p| self.catalog.get(p).is_some()))
        {
            return;
        }
        self.record_row(row);
    }

    pub(super) fn record_row(&mut self, mut row: ThreadRow) {
        if self.listing {
            self.created_while_listing.insert(row.id.clone());
        }
        if let Some(previous) = self.catalog.get(&row.id) {
            row.unread = previous.unread;
            row.activity_order = previous.activity_order;
            row.updated_at = previous.updated_at.max(row.updated_at);
            if row.model.is_none() {
                row.model.clone_from(&previous.model);
            }
            // A started notification or opening reply can arrive after a rename.
            if previous
                .name
                .as_deref()
                .and_then(crate::codex::history::nonblank)
                .is_some()
            {
                row.name.clone_from(&previous.name);
            }
            if row
                .agent_nickname
                .as_deref()
                .and_then(crate::codex::history::nonblank)
                .is_none()
            {
                row.agent_nickname.clone_from(&previous.agent_nickname);
            }
            if row.preview.trim().is_empty() {
                row.preview.clone_from(&previous.preview);
            } else {
                self.catalog.inferred_previews.remove(&row.id);
            }
        }
        self.record_lineage(&row);
        self.catalog.insert(row);
        self.catalog_revision += 1;
        self.changed.mark(crate::changed::Changed::THREADS);
    }
}

pub(super) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

impl super::Store {
    pub fn set_front(&mut self, front: bool) {
        self.front = front;
        if front && let Some(id) = &self.selected {
            if let Some(row) = self.catalog.get_mut(id) {
                row.unread = false;
            }
            if let Some(t) = self.transcripts.get_mut(id) {
                t.unread = false;
            }
        }
    }
    pub(super) fn catalog_activity(&mut self, activity: &crate::codex::activity::Activity) {
        use crate::codex::activity::Change;
        if let Change::Item { row, .. } = &activity.change
            && matches!(row.item, crate::transcript::item::Item::UserMessage { .. })
        {
            self.prompt_preview(&activity.thread, PromptSource::Live, || {
                row.item.user_prompt_line()
            });
        }
        let Some(row) = self.catalog.get_mut(&activity.thread) else {
            return;
        };
        let unread = !self.front || self.selected.as_ref() != Some(&activity.thread);
        if unread && !row.unread {
            row.unread = true;
            self.changed.mark(crate::changed::Changed::THREADS);
        }
        let kind = match &activity.change {
            Change::TurnStarted => "active",
            Change::TurnCompleted { error: Some(_) } => "systemError",
            Change::TurnCompleted { error: None } => "idle",
            _ => return,
        };
        row.status = Some(Status {
            kind: kind.into(),
            flags: Vec::new(),
        });
        let elapsed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        row.updated_at = elapsed.as_secs() as i64;
        row.activity_order = elapsed.as_nanos();
        self.catalog_revision += 1;
        self.changed.mark(crate::changed::Changed::THREADS);
    }
}

#[cfg(test)]
#[path = "catalog_titles_tests.rs"]
mod title_tests;

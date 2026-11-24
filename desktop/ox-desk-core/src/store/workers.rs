//! A descendant retains the directory, model and permissions reported by Codex.
//! The reader verifies the parent again before subscribing to its history.
use super::{Pending, Store, ThreadRow};
use crate::codex::{history::ThreadPage, request::Call};
use std::{
    collections::{HashMap, HashSet},
    io,
};

#[derive(Debug)]
pub(super) struct Discovery {
    root: String,
    cursors: HashSet<String>,
    rows: Vec<ThreadRow>,
}

impl Store {
    // A freshly spawned thread can ask for approval before it is indexed by thread/list.
    pub(super) fn read_missing_child(&mut self, id: &str) -> io::Result<()> {
        if self.catalog.get(id).is_some()
            || self
                .pending
                .values()
                .any(|(_, p)| matches!(p, Pending::ChildIdentity(child) if child == id))
        {
            return Ok(());
        }
        self.request(Call::Read(id.into()), Pending::ChildIdentity(id.into()))
    }
    pub(super) fn observe_child_identity(&mut self, id: &str, row: ThreadRow) {
        if row.id == id
            && row.model_provider == self.profile.provider
            && row.cwd.starts_with(&self.profile.project)
            && row.parent_thread_id.as_deref().is_some_and(|parent| {
                self.transcripts.get(parent).is_some_and(|t| t.verified)
                    || self.child_threads.contains_key(parent)
            })
        {
            self.observed_row(row);
        }
    }

    pub(super) fn discover_children(&mut self, root: &str) -> io::Result<()> {
        if !self.transcripts.get(root).is_some_and(|t| t.verified)
            || self
                .pending
                .values()
                .any(|(_, p)| matches!(p,Pending::Descendants(d) if d.root==root))
        {
            return Ok(());
        }
        self.request(
            Call::Descendants {
                root: root.into(),
                cursor: None,
            },
            Pending::Descendants(Discovery {
                root: root.into(),
                cursors: Default::default(),
                rows: vec![],
            }),
        )
    }
    pub(super) fn discovered(
        &mut self,
        mut discovery: Discovery,
        page: ThreadPage,
    ) -> io::Result<()> {
        discovery.rows.extend(page.data);
        if let Some(cursor) = page.next_cursor {
            if !discovery.cursors.insert(cursor.clone()) {
                return Err(io::Error::other("Codex repeated a subagent catalog cursor"));
            }
            return self.request(
                Call::Descendants {
                    root: discovery.root.clone(),
                    cursor: Some(cursor),
                },
                Pending::Descendants(discovery),
            );
        }
        let mut children: HashMap<String, Vec<ThreadRow>> = HashMap::new();
        for row in discovery.rows {
            if let Some(parent) = &row.parent_thread_id {
                children.entry(parent.clone()).or_default().push(row);
            }
        }
        let mut pending = vec![discovery.root];
        while let Some(parent) = pending.pop() {
            for row in children.remove(&parent).unwrap_or_default() {
                if row.cwd.starts_with(&self.profile.project)
                    && row.model_provider == self.profile.provider
                {
                    pending.push(row.id.clone());
                    self.record_lineage(&row);
                    self.observed_row(row);
                }
            }
        }
        Ok(())
    }
}

use super::{Connection, Pending, Store, ThreadRow};
use crate::{
    codex::{
        history::{ReadThread, ResumedThread, TurnPage},
        request::Call,
    },
    transcript::Transcript,
};

use std::io;

impl Store {
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }
    pub fn transcript(&self, id: &str) -> Option<&Transcript> {
        self.transcripts.get(id)
    }

    /// Loaded threads with a turn in progress, whether or not the catalog
    /// page lists them (workers, search results, later pages).
    pub fn active_turns(&self) -> usize {
        self.transcripts
            .values()
            .filter(|t| t.active_turn().is_some())
            .count()
    }

    pub fn take_remeasure(&mut self, id: &str) -> Vec<usize> {
        self.transcripts
            .get_mut(id)
            .map(Transcript::take_remeasure)
            .unwrap_or_default()
    }

    pub fn select(&mut self, id: &str) -> io::Result<()> {
        if self.remote().is_some() {
            return self
                .dispatch_remote(crate::actions::Action::SelectThread(id.into()))
                .map(|_| ());
        }
        let row = self.catalog.get(id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Thread is outside this catalog")
        })?;
        self.check_route(row)?;
        self.open_catalog_id(id)
    }

    pub(super) fn check_route(&self, row: &ThreadRow) -> io::Result<()> {
        let child = row.parent_thread_id.as_ref().is_some_and(|parent| {
            self.child_threads
                .get(&row.id)
                .is_some_and(|(p, dir)| p == parent && dir == &row.cwd)
        });
        if (row.cwd != self.profile.project
            && !(child && row.cwd.starts_with(&self.profile.project)))
            || row.model_provider != self.profile.provider
            || self
                .expected_alias(&row.id)
                .is_some_and(|alias| row.model.as_ref().is_some_and(|m| m != alias))
        {
            return Err(io::Error::other(
                "Select the saved thread’s project and billing route",
            ));
        }
        Ok(())
    }

    /// Open metadata from another connection to the same project. Read and resume
    /// still verify the identity and applied route with the owning server.
    pub fn select_from_catalog(&mut self, row: ThreadRow) -> io::Result<()> {
        self.select_catalog_origin(row, false)
    }
    pub fn select_catalog_origin(&mut self, row: ThreadRow, confirm_alias: bool) -> io::Result<()> {
        if self.remote().is_some() {
            let a = if confirm_alias {
                crate::actions::Action::ConfirmAliasAndResume(Box::new(row))
            } else {
                crate::actions::Action::OpenCatalogRow(Box::new(row))
            };
            return self.dispatch_remote(a).map(|_| ());
        }
        if confirm_alias
            && (!self.is_session_scoped()
                || !self.profile.route.is_api()
                || !row.needs_claude_alias())
        {
            return Err(io::Error::other(
                "The saved billing origin changed; select this session again",
            ));
        }
        if row.worker {
            if self.is_session_scoped()
                && self
                    .child_threads
                    .get(&row.id)
                    .is_some_and(|(parent, cwd)| {
                        Some(parent) == row.parent_thread_id.as_ref()
                            && cwd == &self.profile.project
                    })
                && self.transcript(&row.id).is_some()
            {
                return self.open_catalog_id(&row.id);
            }
            return Err(io::Error::other(
                "Open the worker on its originating parent connection",
            ));
        }
        self.check_route(&row)?;
        self.record_lineage(&row);
        let id = row.id.clone();
        if self.catalog.get(&id).is_none() {
            if self.listing {
                self.created_while_listing.insert(id.clone());
            }
            self.catalog.insert(row);
            self.catalog_revision += 1;
        } else {
            self.sync_title(&row);
        }
        self.open_catalog_alias(&id, confirm_alias)
    }
    pub(super) fn open_catalog_id(&mut self, id: &str) -> io::Result<()> {
        self.open_catalog_alias(id, false)
    }
    fn open_catalog_alias(&mut self, id: &str, confirm_alias: bool) -> io::Result<()> {
        self.selection += 1;
        self.selected = Some(id.into());
        let entry = self.transcripts.entry(id.into()).or_default();
        entry.unread = false;
        if let Some(row) = self.catalog.get_mut(id) {
            row.unread = false;
        }
        if entry.ready || entry.loading || self.terminal_threads.contains(id) {
            return Ok(());
        }
        if self.state != Connection::Ready {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Reconnect to open this thread",
            ));
        }
        if self.is_session_scoped() {
            return self.claude_open_alias(id, confirm_alias);
        }
        self.request(Call::Read(id.into()), Pending::Read(id.into()))?;
        self.mark_loading(id);
        Ok(())
    }

    pub(super) fn mark_loading(&mut self, id: &str) {
        let transcript = self.transcripts.entry(id.into()).or_default();
        transcript.loading = true;
        transcript.error = None;
    }

    pub fn load_older(&mut self, id: &str) -> io::Result<()> {
        let Some(t) = self.transcripts.get(id) else {
            return Ok(());
        };
        if t.loading {
            return Ok(());
        }
        let Some(cursor) = t.cursor.clone() else {
            return Ok(());
        };
        self.request_history(id, Some(cursor))
    }

    fn request_history(&mut self, id: &str, cursor: Option<String>) -> io::Result<()> {
        self.request(
            Call::History {
                thread: id.into(),
                cursor,
            },
            Pending::History(id.into()),
        )?;
        self.mark_loading(id);
        Ok(())
    }

    fn verify_identity(&self, id: &str, identity: &ThreadRow) -> io::Result<()> {
        let profile = &self.profile;
        let directory_ok = match (&identity.parent_thread_id, self.child_threads.get(id)) {
            (None, None) => identity.cwd == profile.project,
            (Some(parent), Some((expected, cwd))) => {
                parent == expected
                    && cwd == &identity.cwd
                    && identity.cwd.starts_with(&profile.project)
            }
            _ => false,
        };
        if identity.id != id
            || !directory_ok
            || identity.model_provider != profile.provider
            || self
                .expected_alias(id)
                .is_some_and(|alias| identity.model.as_deref().is_some_and(|m| m != alias))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Thread belongs to a different project, provider, or model",
            ));
        }
        Ok(())
    }

    pub(super) fn checked_read(&mut self, id: &str, read: ReadThread) {
        let result = (|| -> io::Result<()> {
            let identity = read.thread;
            self.verify_identity(id, &identity)?;
            if let Some(expected) = self.expected_alias(id)
                && identity.model.is_none()
                && !self.authorized_models.contains_key(id)
                && self.catalog.get(id).and_then(|row| row.model.as_deref()) != Some(expected)
            {
                return Err(io::Error::other(
                    "The saved API model is unknown. Open a new session on the intended billing route.",
                ));
            }
            self.sync_title(&identity);
            self.request(
                Call::Resume {
                    thread: id.into(),
                    project: identity
                        .parent_thread_id
                        .is_none()
                        .then(|| self.profile.project.clone()),
                },
                Pending::Resume(id.into()),
            )
        })();
        if let Err(err) = result {
            self.failed(Pending::Read(id.into()), err.to_string());
        }
    }

    pub(super) fn verify_opened(&self, id: &str, reply: &ResumedThread) -> io::Result<()> {
        self.verify_identity(id, &reply.thread)?;
        if reply.model_provider != self.profile.provider
            || self.expected_alias(id).is_some_and(|m| m != reply.model)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Codex opened the thread with a different billing route",
            ));
        }
        Ok(())
    }
    pub(super) fn checked_resume(&mut self, id: &str, reply: ResumedThread) {
        let result = (|| -> io::Result<()> {
            self.verify_opened(id, &reply)?;
            self.sync_title(&reply.thread);
            if self.profile.route.is_api() {
                self.authorized_models
                    .insert(id.into(), reply.model.clone());
            }
            let mut initial = reply.initial_turns_page;
            self.transcripts.entry(id.into()).or_default().verified = true;
            if let Some(page) = &mut initial {
                self.history_preview(id, page);
            }
            let t = self.transcripts.entry(id.into()).or_default();
            t.model = Some(reply.model);
            t.tier = reply.service_tier;
            t.effort = reply.reasoning_effort;
            t.access = Some(reply.access);
            t.verified = true;
            if let Some(page) = initial {
                t.add_history(page)?;
            } else {
                self.request_history(id, None)?;
            }
            Ok(())
        })();
        if let Err(err) = result {
            self.failed(Pending::Resume(id.into()), err.to_string());
        }
    }

    fn history_preview(&mut self, id: &str, page: &TurnPage) {
        self.prompt_preview(
            id,
            super::catalog::PromptSource::History {
                oldest_page: page.next_cursor.is_none(),
            },
            || {
                page.data
                    .iter()
                    .rev()
                    .flat_map(|turn| &turn.items)
                    .find_map(|row| row.item.user_prompt_line())
            },
        );
    }

    pub(super) fn history_page(&mut self, id: &str, page: TurnPage) {
        self.history_preview(id, &page);
        let result = self
            .transcripts
            .entry(id.into())
            .or_default()
            .add_history(page);
        if let Err(err) = result {
            self.failed(Pending::History(id.into()), err.to_string());
        }
    }
}

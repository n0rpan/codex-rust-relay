//! A stored worker result gives history. A live status on the originating
//! parent's OX MCP connection enables controls after checking its identity.
use super::{Connection, Pending, Store, ThreadRow};
use crate::{
    changed::Changed,
    codex::{
        history::TurnPage,
        jobs::{Observation, Response},
        request::Call,
    },
};
use crate::{
    ports::ox_workers::Snapshot,
    worker::{WorkerAction, WorkerView},
};
use std::{
    io,
    time::{Duration, Instant},
};

pub(super) type HostControl = Box<dyn Fn(&str, WorkerAction) -> bool + Send>;
pub struct Job {
    pub parent: String,
    pub title: String,
    pub snapshot: Snapshot,
    pub live: bool,
    pub(super) launched_here: bool,
    pub pending: bool,
    pub error: Option<String>,
    pub(super) poll: Option<Instant>,
}
impl Job {
    pub fn view(&self) -> WorkerView<'_> {
        WorkerView {
            title: &self.title,
            parent: &self.parent,
            runtime: &self.snapshot.runtime,
            model: &self.snapshot.model,
            status: self.status(),
            messages: &self.snapshot.messages,
            progress: &self.snapshot.progress,
            result: self.snapshot.result.as_deref(),
            error: self.error.as_deref().or(self.snapshot.error.as_deref()),
            can_refresh: !self.pending,
            can_steer: self.can_steer(),
            can_stop: self.can_stop(),
        }
    }
    pub fn can_stop(&self) -> bool {
        self.live && !self.pending && self.snapshot.status == "running"
    }
    pub fn can_steer(&self) -> bool {
        self.can_stop() && self.snapshot.can_steer
    }
    pub fn status(&self) -> &str {
        if !self.live && self.snapshot.status == "running" {
            "Last seen running"
        } else {
            &self.snapshot.status
        }
    }
}

impl Store {
    pub fn host_control(&mut self, control: impl Fn(&str, WorkerAction) -> bool + Send + 'static) {
        self.host_jobs = Some(Box::new(control));
    }
    pub fn host_started_job(&mut self, parent: &str, snapshot: Snapshot) {
        let title = snapshot
            .messages
            .iter()
            .find(|m| m.role == "user")
            .and_then(|m| m.text.lines().next())
            .unwrap_or("OX worker")
            .chars()
            .take(100)
            .collect();
        self.observe_job(
            parent,
            Observation {
                snapshot,
                title: Some(title),
            },
            true,
        );
    }
    pub fn host_job_reply(
        &mut self,
        id: &str,
        operation: WorkerAction,
        reply: Result<Response, String>,
    ) {
        self.job_reply(id, operation, reply);
    }

    pub fn job(&self, id: &str) -> Option<&Job> {
        self.jobs.get(id)
    }
    pub fn jobs(&self) -> impl Iterator<Item = (&str, &Job)> {
        self.jobs.iter().map(|(id, j)| (id.as_str(), j))
    }
    pub fn set_jobs_visible(&mut self, visible: bool) {
        self.jobs_visible = visible;
    }
    pub(super) fn job_deadline(&self) -> Option<Instant> {
        if self.jobs_visible && self.state == Connection::Ready {
            self.jobs.values().filter_map(|j| j.poll).min()
        } else {
            None
        }
    }
    pub(super) fn tick_jobs(&mut self, now: Instant) {
        if !self.jobs_visible || self.state != Connection::Ready {
            return;
        }
        let due: Vec<_> = self
            .jobs
            .iter()
            .filter(|(_, j)| j.poll.is_some_and(|t| t <= now))
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            if let Some(job) = self.jobs.get_mut(&id) {
                job.poll = None;
            }
            let _ = self.job_request(&id, WorkerAction::Refresh);
        }
    }
    pub(super) fn observe_jobs_page(&mut self, id: &str, page: &mut TurnPage) {
        // Newest updates may arrive on a page before the launch that proves ownership.
        for turn in &mut page.data {
            for item in turn.items.iter_mut().rev() {
                if let Some(mut observation) = item.job.take() {
                    let key = format!("ox:{id}:{}", observation.snapshot.job_id);
                    if self.jobs.contains_key(&key) {
                        continue;
                    }
                    if observation.title.is_none() {
                        self.archived_updates
                            .entry(key)
                            .or_insert(observation.snapshot);
                        continue;
                    }
                    if let Some(update) = self.archived_updates.remove(&key)
                        && update.runtime == observation.snapshot.runtime
                        && update.model == observation.snapshot.model
                        && observation
                            .snapshot
                            .native_session
                            .as_ref()
                            .is_none_or(|s| update.native_session.as_ref() == Some(s))
                    {
                        observation.snapshot = update;
                    }
                    self.observe_job(id, *observation, false);
                }
            }
        }
    }
    pub(super) fn observe_job(&mut self, parent: &str, observation: Observation, live_event: bool) {
        let snapshot = observation.snapshot;
        let id = format!("ox:{parent}:{}", snapshot.job_id);
        let launched = live_event && observation.title.is_some();
        if let Some(job) = self.jobs.get_mut(&id) {
            // Live status supersedes archived tool items. Keep a newer snapshot
            // when an older history page is prepended.
            if !live_event || !Self::same_job(job, &snapshot) {
                return;
            }
            job.launched_here |= launched;
            job.snapshot = snapshot;
        } else if let Some(title) = observation.title {
            self.jobs.insert(
                id.clone(),
                Job {
                    parent: parent.into(),
                    title,
                    snapshot,
                    live: false,
                    launched_here: launched,
                    pending: false,
                    error: None,
                    poll: None,
                },
            );
        } else {
            return;
        }
        self.insert_job_row(&id);
        let ready = self.transcripts.get(parent).is_some_and(|t| t.verified);
        let job = self.jobs.get_mut(&id).unwrap();
        if ready && !job.pending && job.snapshot.status == "running" {
            job.poll = Some(Instant::now());
        }
        self.changed.mark(Changed::THREADS);
    }
    fn same_job(job: &Job, s: &Snapshot) -> bool {
        job.snapshot.job_id == s.job_id
            && job.snapshot.runtime == s.runtime
            && job.snapshot.model == s.model
            && job
                .snapshot
                .native_session
                .as_ref()
                .is_none_or(|old| s.native_session.as_ref() == Some(old))
    }
    pub(super) fn insert_job_row(&mut self, id: &str) {
        if self.catalog.get(id).is_some() {
            return;
        }
        let job = &self.jobs[id];
        let Some(parent) = self.catalog.get(&job.parent) else {
            return;
        };
        self.catalog.insert(ThreadRow {
            worker: true,
            id: id.into(),
            parent_thread_id: Some(job.parent.clone()),
            agent_nickname: None,
            cwd: parent.cwd.clone(),
            model_provider: parent.model_provider.clone(),
            model: parent.model.clone(),
            name: Some(job.title.clone()),
            generated_title: None,
            preview: String::new(),
            updated_at: parent.updated_at,
            status: None,
            unread: false,
            activity_order: 0,
        });
        self.catalog_revision += 1;
    }
    pub(super) fn restore_job_rows(&mut self) {
        let ids = self.jobs.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            self.insert_job_row(&id);
        }
    }
    pub fn job_request(&mut self, id: &str, operation: WorkerAction) -> io::Result<()> {
        if self.remote().is_some() {
            return self.forward(crate::actions::Action::WorkerControl(id.into(), operation));
        }
        let job = self
            .jobs
            .get(id)
            .ok_or_else(|| io::Error::other("Worker is outside this connection"))?;
        if self.state != Connection::Ready
            || self.in_terminal(&job.parent)
            || !self
                .transcripts
                .get(&job.parent)
                .is_some_and(|t| t.verified)
                && !self.jobs.get(&job.parent).is_some_and(|parent| parent.live)
        {
            return Err(io::Error::other(
                "Open the parent thread on its original billing route",
            ));
        }
        let valid = match &operation {
            WorkerAction::Refresh => !job.pending,
            WorkerAction::Steer(text) => job.can_steer() && !text.trim().is_empty(),
            WorkerAction::Stop => job.can_stop(),
        };
        if !valid {
            return Err(io::Error::other(
                "This worker cannot accept that action right now",
            ));
        }
        let parent = job.parent.clone();
        let number = job.snapshot.job_id;
        let job = self.jobs.get_mut(id).unwrap();
        job.pending = true;
        job.poll = None;
        if self
            .host_jobs
            .as_ref()
            .is_some_and(|send| send(id, operation.clone()))
        {
            return Ok(());
        }
        self.request(
            Call::Job {
                parent,
                number,
                operation: operation.clone(),
            },
            Pending::Job(id.into(), operation),
        )
    }
    pub(super) fn job_failed(&mut self, id: &str, reason: String) {
        if let Some(job) = self.jobs.get_mut(id) {
            job.live = false;
            job.pending = false;
            job.poll = None;
            job.error = Some(reason);
        }
        self.changed.mark(Changed::THREADS);
    }
    pub(super) fn job_reply(
        &mut self,
        id: &str,
        operation: WorkerAction,
        reply: Result<Response, String>,
    ) {
        let Some(job) = self.jobs.get_mut(id) else {
            return;
        };
        job.pending = false;
        match reply {
            Ok(Response::Snapshot(_))
                if !job.launched_here && job.snapshot.native_session.is_none() =>
            {
                self.job_failed(
                    id,
                    "The recorded worker has no native session identity".into(),
                );
                return;
            }
            Ok(Response::Snapshot(snapshot)) if Self::same_job(job, &snapshot) => {
                job.live = job.launched_here || job.snapshot.native_session.is_some();
                job.snapshot = *snapshot;
                job.error = None;
                job.poll = (job.live && job.snapshot.status == "running")
                    .then(|| Instant::now() + Duration::from_secs(2));
            }
            Ok(Response::Accepted) if matches!(operation, WorkerAction::Steer(_)) => {
                job.error = None;
                job.poll = Some(Instant::now());
            }
            Ok(_) => {
                self.job_failed(id, "The live worker does not match the recorded job".into());
                return;
            }
            Err(error) => {
                self.job_failed(id, error);
                return;
            }
        }
        if let WorkerAction::Steer(text) = operation
            && self.draft(Some(id)) == text
        {
            self.edit(Some(id), String::new());
        }
        self.changed.mark(Changed::THREADS);
    }
    pub(super) fn invalidate_jobs(&mut self) {
        for j in self.jobs.values_mut() {
            j.launched_here = false;
            j.live = false;
            j.pending = false;
            j.poll = None;
        }
    }
}

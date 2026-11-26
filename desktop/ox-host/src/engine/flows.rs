//! Host flows of the mode model: YOLO acceptance and grants, cross-runtime
//! handoffs, and `read_ox_thread`.
use super::*;
use crate::durable::{Grant, HandoffRecord, Outcome};
use crate::workers::{Entry, Marks, Resolved};
use ox_desk_core::{
    handoff::{self, HandoffSource},
    modes::{self, Decision},
    origin::{ParentRef, Relation},
};

/// Longest wait for a handoff source's older history or a read's transcript.
const LOAD_WAIT: Duration = Duration::from_secs(40);
/// Longest wait for the target model's context window before the default is used.
const WINDOW_WAIT: Duration = Duration::from_secs(12);
/// Bytes of item text one read_ox_thread page returns at most.
const PAGE_BYTES: usize = 256 * 1024;

impl Engine {
    /// The grant covering a YOLO child of this session: one stored with the
    /// session or an ancestor (worker parents and handoff sources), or the
    /// project default from `[workers] yolo_grant_projects`.
    pub(super) fn yolo_grant(&mut self, scope: &Scope, session: &str) -> Option<YoloAllowance> {
        let now = crate::durable::unix_now();
        if self.ledger.data.grants.iter().any(|g| !g.live(now)) {
            // Expired grants stop counting and are dropped from the ledger.
            let _ = self.ledger.change(|d| d.grants.retain(|g| g.live(now)));
        }
        let mut current = Some((scope.clone(), session.to_owned()));
        for _ in 0..64 {
            let Some((s, id)) = current.take() else { break };
            if self
                .ledger
                .data
                .grants
                .iter()
                .any(|g| g.scope == s && g.session == id && g.live(now))
            {
                return Some(YoloAllowance::Grant {
                    project: s.project.clone(),
                    session: Some(id),
                });
            }
            current = self.workers.get(&s, &id).and_then(|e| {
                Some((
                    e.origin.parent.scope.clone()?,
                    e.origin.parent.session.clone()?,
                ))
            });
        }
        let by_default = self
            .yolo_projects()
            .iter()
            .any(|p| p == &scope.project || p.canonicalize().ok().as_ref() == Some(&scope.project));
        if by_default {
            self.announce_yolo_default(scope);
        }
        by_default.then(|| YoloAllowance::Grant {
            project: scope.project.clone(),
            session: None,
        })
    }

    /// Tell the operator, once per project and host run, that a session started under the
    /// project's YOLO default, and log it.
    fn announce_yolo_default(&mut self, scope: &Scope) {
        if !self.yolo_announced.insert(scope.project.clone()) {
            return;
        }
        ox_host_service::errlog::record(&format!(
            "YOLO default applied for {}",
            scope.project.display()
        ));
        if let Some(live) = self.running.get_mut(scope) {
            live.store
                .post_notice("YOLO is on for this project by default. Change it in Settings.");
        }
    }

    /// Whether the worker of this job is running or waiting for the operator.
    fn worker_active(&self, job_id: u64) -> bool {
        self.job_status(job_id).is_ok_and(|s| {
            matches!(
                s["status"].as_str(),
                Some("running" | "waiting_for_operator")
            )
        })
    }

    /// Refuse a worker start when this parent already runs the maximum number of workers.
    pub(super) fn check_worker_cap(&self, parent: &ParentRef) -> Result<(), String> {
        self.check_worker_cap_with(
            parent,
            crate::workers::env_limit("OX_WORKER_MAX_PER_PARENT").map(|c| c as usize),
        )
    }

    /// The cap check with the limit given. No limit by default: it is opt-in.
    pub(super) fn check_worker_cap_with(
        &self,
        parent: &ParentRef,
        cap: Option<usize>,
    ) -> Result<(), String> {
        let Some(cap) = cap else {
            return Ok(());
        };
        let started = self
            .workers
            .jobs()
            .filter(|(_, _, e)| &e.origin.parent == parent)
            .filter(|(_, _, e)| self.worker_active(e.origin.job_id))
            .count();
        let starting = self
            .running
            .values()
            .filter_map(|l| l.launching.as_ref())
            .filter(|l| l.origin.job_id != 0 && &l.origin.parent == parent)
            .count()
            + self
                .asking
                .values()
                .filter(|a| &a.origin.parent == parent)
                .count();
        if started + starting >= cap {
            return Err(format!(
                "This session already runs {cap} workers. Wait for one to finish or cancel one. The limit is OX_WORKER_MAX_PER_PARENT."
            ));
        }
        Ok(())
    }

    /// A caution when another running worker can write in the same project.
    pub(super) fn write_overlap(
        &self,
        scope: &Scope,
        level: ox_desk_core::origin::Ceiling,
    ) -> Option<String> {
        if level < ox_desk_core::origin::Ceiling::Write {
            return None;
        }
        self.workers
            .jobs()
            .any(|(worker_scope, _, e)| {
                worker_scope.project == scope.project
                    && e.origin.ceiling >= ox_desk_core::origin::Ceiling::Write
                    && self.worker_active(e.origin.job_id)
            })
            .then(|| {
                "Another worker had write access to this checkout when this job started. Use a separate git worktree for independent edits."
                    .to_owned()
            })
    }

    /// Record how each worker's latest run ended, from the live store, while the
    /// answer is still readable. The record is only the final answer: the transcript
    /// stays in the runtime's own history. A run in progress clears it. Returns whether
    /// any record changed, so the caller can republish the rows that carry it.
    pub(super) fn capture_finals(&mut self, scope: &Scope) -> bool {
        let Some(live) = self.running.get(scope) else {
            return false;
        };
        let ready = live.store.state() == &Connection::Ready;
        let operators = self.operators();
        // The models that answered, recorded as the runtime reports them, so status,
        // the completion record and the worker view survive the process and restarts.
        let answered: Vec<(String, Vec<String>)> = self
            .workers
            .in_scope(scope)
            .filter_map(|(id, entry)| {
                let models = live.store.claude_answering_models(id);
                (!models.is_empty() && !entry.marks.has_models(models))
                    .then(|| (id.to_owned(), models.to_vec()))
            })
            .collect();
        for (id, models) in &answered {
            if let Some(entry) = self.workers.get_mut(scope, id) {
                entry.marks.record_models(models);
            }
        }
        let mut updates: Vec<(String, Option<crate::workers::Last>)> = Vec::new();
        for (id, entry) in self.workers.in_scope(scope) {
            if entry.origin.relation != Relation::Worker || entry.marks.cancelled {
                continue;
            }
            let view = live.state.sessions.iter().find(|v| v.id == id);
            let timeline = live.state.transcripts.get(id);
            let native = id.rsplit(':').next().unwrap_or(id);
            let status =
                crate::workers::status(entry, id, native, view, timeline, ready, operators);
            let next = match status["status"].as_str() {
                Some("completed" | "failed") if timeline.is_some() => {
                    Some(Some(crate::workers::Last {
                        status: status["status"].as_str().unwrap_or("completed").to_owned(),
                        result: status["result"].as_str().map(str::to_owned),
                        error: status["error"].as_str().map(str::to_owned),
                        model: status["actual_model"].as_str().map(str::to_owned),
                    }))
                }
                // A stop the runtime confirmed, with the partial text it left. A route that
                // merely closed is interrupted too, but says nothing about the run's end.
                Some("interrupted")
                    if timeline.is_some_and(|t| t.detail["interrupted"] == true) =>
                {
                    Some(Some(crate::workers::Last {
                        status: "interrupted".to_owned(),
                        result: status["result"].as_str().map(str::to_owned),
                        error: None,
                        model: status["actual_model"].as_str().map(str::to_owned),
                    }))
                }
                Some("running" | "waiting_for_operator") => Some(None),
                _ => None,
            };
            if let Some(next) = next
                && entry.marks.last != next
            {
                updates.push((id.to_owned(), next));
            }
        }
        let changed = !updates.is_empty() || !answered.is_empty();
        for (id, last) in updates {
            if let Some(entry) = self.workers.get_mut(scope, &id) {
                entry.marks.last = last;
            }
        }
        changed
    }

    /// Stop workers that ran past their wall-clock limit.
    pub(super) fn expire_workers(&mut self) -> io::Result<()> {
        let now = crate::durable::unix_now();
        let due: Vec<(Scope, String)> = self
            .workers
            .jobs()
            .filter(|(_, _, e)| {
                let m = &e.marks;
                !m.expired
                    && !m.cancelled
                    && m.max_minutes.is_some_and(|max| {
                        // The current run's start: a continued worker is measured from
                        // its continuation, not from the original launch.
                        let began = if m.run_started_at > 0 {
                            m.run_started_at
                        } else {
                            m.started_at
                        };
                        began > 0 && now - began >= i64::from(max) * 60
                    })
            })
            .filter(|(_, _, e)| self.worker_active(e.origin.job_id))
            .map(|(s, id, _)| (s.clone(), id.to_owned()))
            .collect();
        for (scope, id) in due {
            if let Some(live) = self.running.get_mut(&scope) {
                let _ = live.store.stop(&id);
            }
            if let Some(entry) = self.workers.get_mut(&scope, &id) {
                entry.marks.expired = true;
                entry.marks.cancelled = true;
            }
            self.persist_marks(&scope)?;
        }
        Ok(())
    }

    /// Drop the grants a session gave: they end with the session.
    pub(super) fn drop_grant(&mut self, scope: &Scope, session: &str) -> io::Result<()> {
        if self
            .ledger
            .data
            .grants
            .iter()
            .any(|g| &g.scope == scope && g.session == session)
        {
            self.ledger.change(|d| {
                d.grants
                    .retain(|g| !(&g.scope == scope && g.session == session))
            })?;
        }
        Ok(())
    }

    /// The project default from the settings snapshot. A failed read is retried once, and when
    /// it still fails the last value read is kept, so a transient failure cannot deny a project
    /// the operator already allowed.
    fn yolo_projects(&mut self) -> Vec<PathBuf> {
        if let Some(prepared) = (0..2).find_map(|_| self.config.prepare().ok()) {
            self.yolo_projects = prepared.yolo_grant_projects;
        }
        self.yolo_projects.clone()
    }

    /// Hold a YOLO child for the operator and tell the parent at once.
    pub(super) fn ask_yolo(&mut self, asking: Asking, reply: Replier) -> io::Result<()> {
        let parent = asking.origin.parent.scope.clone();
        let origin = asking.origin.clone();
        self.ledger.change(|d| d.asking.push(origin))?;
        let job = asking.origin.job_id;
        self.asking.insert(job, asking);
        let status = crate::yolo::status(&self.asking[&job], self.operators().connected);
        self.answer(reply, Ok(status))?;
        if let Some(parent) = parent.filter(|p| self.running.contains_key(p)) {
            self.process(&parent, vec![], true)?;
        }
        Ok(())
    }

    /// The operator's answer to a YOLO request. The parent agent has no path here.
    pub(super) fn answer_yolo(
        &mut self,
        client: u64,
        scope: &Scope,
        job: u64,
        choice: usize,
    ) -> io::Result<()> {
        let answer = crate::yolo::Answer::of(choice)
            .ok_or_else(|| io::Error::other("Choose Accept YOLO, a lower mode or Decline."))?;
        let a = self
            .asking
            .get(&job)
            .filter(|a| a.origin.parent.scope.as_ref() == Some(scope))
            .ok_or_else(|| io::Error::other("This YOLO request was already answered."))?;
        let mut origin = a.origin.clone();
        // The worker starts only for a parent that is still live.
        let parent_live = a.origin.parent.session.as_deref().is_some_and(|t| {
            self.running
                .get(scope)
                .is_some_and(|l| l.store.catalog_row(t).is_some() && !l.store.is_ended(t))
        });
        if !parent_live {
            self.withdraw_with(job, ENDED_PARENT)?;
            return Err(io::Error::other(ENDED_PARENT));
        }
        match answer {
            crate::yolo::Answer::Decline => {
                self.asking.remove(&job);
                self.ledger.change(|d| {
                    d.asking.retain(|o| o.job_id != job);
                    d.outcomes.push(Outcome {
                        origin,
                        status: "declined".into(),
                        message: "The operator declined to start this worker in YOLO.".into(),
                    });
                })?;
            }
            accept @ (crate::yolo::Answer::Accept | crate::yolo::Answer::Lower) => {
                if matches!(accept, crate::yolo::Answer::Accept) {
                    origin.yolo = Some(YoloAllowance::Accepted {
                        request: format!("{}{job}", crate::yolo::PREFIX),
                        device: format!("client-{client}"),
                    });
                } else {
                    let level = origin.ceiling.below_yolo();
                    origin.requested = level;
                    origin.ceiling = level;
                    origin.started = Some(level.preset(origin.runtime));
                    origin.decided = Some(Decision::Capped);
                }
                let target = a.target.clone();
                let payload = Payload::Worker(a.start.clone());
                // A busy route keeps the request pending so the operator can retry.
                let mut l = self
                    .prepare_launch(&target, origin, payload)
                    .map_err(io::Error::other)?;
                l.detached = true;
                self.asking.remove(&job);
                self.ledger
                    .change(|d| d.asking.retain(|o| o.job_id != job))?;
                self.running
                    .get_mut(&target)
                    .expect("ensured target")
                    .launching = Some(l);
                self.process(&target, vec![], false)?;
            }
        }
        if self.running.contains_key(scope) {
            self.process(scope, vec![], true)?;
        }
        Ok(())
    }

    /// Store or revoke a session's YOLO grant. Revoking affects new spawns only.
    pub(super) fn set_grant(&mut self, scope: &Scope, session: &str, on: bool) -> io::Result<()> {
        let live = self.running.get_mut(scope).expect("validated live scope");
        if live.store.catalog_row(session).is_none() {
            return Err(io::Error::other(
                "Open the session before changing its YOLO grant.",
            ));
        }
        if !on {
            live.store.annotate_session(
                session,
                None,
                json!({"yolo_grant": false, "yolo_children": []}),
            );
        }
        let grant = Grant {
            scope: scope.clone(),
            session: session.to_owned(),
            granted_at: crate::durable::unix_now(),
        };
        if on && let Some(live) = self.running.get_mut(scope) {
            live.store
                .post_notice("YOLO grant on for this session. It expires after 12 hours or when the session ends.");
        }
        self.ledger.change(|d| {
            d.grants
                .retain(|g| !(g.scope == grant.scope && g.session == grant.session));
            if on {
                d.grants.push(grant);
            }
        })
    }

    /// Why a session cannot be handed off right now, naming what is pending.
    fn handoff_blocker(&self, scope: &Scope, source: &str) -> Option<String> {
        let live = self.running.get(scope)?;
        if live.store.catalog_row(source).is_none() || live.store.transcript(source).is_none() {
            return Some("Open the session before handing it off.".into());
        }
        let mut reasons = Vec::new();
        if live
            .store
            .transcript(source)
            .is_some_and(|t| t.active_turn().is_some())
        {
            reasons.push("it is running a turn".to_owned());
        }
        let asks: Vec<_> = live
            .store
            .asks()
            .iter()
            .filter(|a| a.thread.as_deref() == Some(source))
            .map(|a| a.title().to_owned())
            .chain(
                self.asking
                    .values()
                    .filter(|a| {
                        a.origin.parent.scope.as_ref() == Some(scope)
                            && a.origin.parent.session.as_deref() == Some(source)
                    })
                    .map(|a| crate::yolo::view(a).title),
            )
            .collect();
        if !asks.is_empty() {
            reasons.push(format!(
                "it has {} pending request{}: {}",
                asks.len(),
                if asks.len() == 1 { "" } else { "s" },
                asks.join("; ")
            ));
        }
        if self
            .preparing
            .iter()
            .any(|p| &p.source == scope && p.start.source == source)
        {
            reasons.push("a handoff from it is already starting".into());
        }
        (!reasons.is_empty()).then(|| {
            format!(
                "This session cannot be handed off yet: {}. Stop or finish it first.",
                reasons.join(", and ")
            )
        })
    }

    pub(super) fn start_handoff(
        &mut self,
        client: u64,
        key: &str,
        scope: &Scope,
        start: HandoffStart,
    ) -> io::Result<()> {
        start.validate().map_err(io::Error::other)?;
        if let Some(reason) = self.handoff_blocker(scope, &start.source) {
            return Err(io::Error::other(reason));
        }
        // The helper may run provider model discovery, so the catalog lookup
        // never blocks the engine. A slow or failed lookup uses the default budget.
        let window = std::sync::Arc::new(std::sync::Mutex::new(None));
        let route = start.route();
        let windows = self.ledger.data.windows.clone();
        match &route.billing {
            ox_desk_core::runtime::Billing::Api(alias) => {
                let (slot, alias, config) = (window.clone(), alias.clone(), self.config.clone());
                let route = route.clone();
                let (tx, source) = (self.tx.clone(), scope.clone());
                let generation = self.running.get(scope).map(|l| l.generation);
                thread::spawn(move || {
                    let found = ox_desk_core::ports::ox::request::<BTreeMap<String, u64>>(
                        &config.ox,
                        &config.path,
                        ox_desk_core::settings_protocol::Request::ModelContexts,
                    )
                    .ok()
                    .and_then(|w| w.get(&alias).copied());
                    *slot.lock().unwrap() =
                        Some(crate::durable::handoff_window(&windows, &route, found));
                    if let Some(generation) = generation {
                        let _ = tx.send(Message::Runtime(source, generation));
                    }
                });
            }
            ox_desk_core::runtime::Billing::Subscription => {
                *window.lock().unwrap() =
                    Some(crate::durable::handoff_window(&windows, &route, None))
            }
        }
        self.preparing.push(Preparing {
            source: scope.clone(),
            start,
            reply: Replier::Client(client, key.to_owned()),
            device: format!("client-{client}"),
            surface: self.clients.get(&client).and_then(|c| c.surface),
            deadline: Instant::now() + LOAD_WAIT,
            window,
            window_deadline: Instant::now() + WINDOW_WAIT,
        });
        Ok(())
    }

    /// Resolve, select and open the target. Nothing starts on an error.
    fn launch_handoff(&mut self, prep: Preparing) -> Result<(), (Replier, String)> {
        let Preparing {
            source: src,
            start,
            reply,
            device,
            surface,
            window,
            ..
        } = prep;
        let window = window.lock().unwrap().flatten();
        macro_rules! fail {
            ($e:expr) => {
                return Err((reply, $e))
            };
        }
        if let Some(reason) = self.handoff_blocker(&src, &start.source) {
            fail!(reason);
        }
        let live = &self.running[&src];
        let Some(timeline) = live.state.transcripts.get(&start.source) else {
            fail!("The session's history is not loaded. Open it and try again.".into());
        };
        let items = handoff::items(timeline);
        let title = live
            .store
            .catalog_row(&start.source)
            .map(|r| r.title().to_owned())
            .unwrap_or_default();
        let native = live
            .store
            .terminal_session_id(&start.source)
            .ok()
            .map(str::to_owned)
            .filter(|n| n != &start.source);
        let access = live.store.applied_access(Some(&start.source));
        let ceiling = live.store.applied_ceiling(Some(&start.source));
        let src_origin = self
            .workers
            .get(&src, &start.source)
            .map(|e| e.origin.clone());
        let level = crate::workers::source_level(ceiling, src_origin.as_ref());
        let resolved = match modes::resolve_operator(level, start.mode, start.runtime) {
            Ok(r) => r,
            Err(e) => fail!(e),
        };
        let yolo = if resolved.level.needs_person() {
            match self.yolo_grant(&src, &start.source) {
                Some(grant) => Some(grant),
                None if start.yolo_confirmed => Some(YoloAllowance::Operator {
                    device: device.clone(),
                }),
                None => fail!(
                    "Confirm YOLO for the new session first. It would run without permission prompts."
                        .into()
                ),
            }
        } else {
            None
        };
        let target = match canonical_scope(Scope {
            project: src.project.clone(),
            route: start.route(),
        }) {
            Ok(t) => t,
            Err(e) => fail!(e.to_string()),
        };
        // A source that was itself handed off carries only its own messages;
        // the earlier chain stays readable through read_ox_thread.
        let earlier = self
            .handoff_chain(&src, &start.source)
            .into_iter()
            .map(|h| handoff::Earlier {
                session: h.session,
                title: h.title,
                runtime: h.runtime,
                route: h.route,
            })
            .collect();
        let selection = match handoff::select_chained(items, &start.request, window, earlier) {
            Ok(s) => s,
            Err(e) => fail!(e),
        };
        let source = HandoffSource {
            scope: src.clone(),
            session: start.source.clone(),
            native,
            title,
            runtime: src.route.runtime,
            route: src.route.clone(),
            source_access: access,
            source_level: level,
            decision: resolved.decision,
            selection: selection.refs.clone(),
        };
        let (text, carried) = match start.runtime {
            ox_desk_core::runtime::Runtime::Codex => (
                start.request.clone(),
                handoff::codex_items(&source, &selection),
            ),
            ox_desk_core::runtime::Runtime::Claude => (
                handoff::claude_turn(&source, &selection, &start.request),
                vec![],
            ),
        };
        let origin = Origin {
            relation: Relation::Handoff,
            parent: ParentRef {
                project: src.project.clone(),
                scope: Some(src.clone()),
                session: Some(start.source.clone()),
                runtime: Some(src.route.runtime),
            },
            launch: format!("operator:{device}"),
            by_operator: true,
            runtime: start.runtime,
            route: target.route.clone(),
            mode: WorkerMode::Session,
            job_id: 0,
            requested: resolved.level,
            ceiling: resolved.level,
            source: None,
            started: Some(resolved.access),
            decided: Some(resolved.decision),
            yolo,
            handoff: Some(Box::new(source)),
            requested_model: None,
            pinned_model: None,
        };
        let record = match nonce() {
            Ok(n) => n,
            Err(e) => fail!(e.to_string()),
        };
        let payload = Payload::Handoff {
            text,
            items: carried,
            record: record.clone(),
            from: surface,
        };
        let mut l = match self.prepare_launch(&target, origin.clone(), payload) {
            Ok(l) => l,
            Err(e) => fail!(e),
        };
        if let Err(e) = self.ledger.change(|d| {
            d.handoffs.push(HandoffRecord {
                id: record,
                target: target.clone(),
                origin,
                status: "starting".into(),
                message: None,
            })
        }) {
            fail!(e.to_string());
        }
        l.reply = reply;
        self.running
            .get_mut(&target)
            .expect("ensured target")
            .launching = Some(l);
        if let Err(e) = self.process(&target, vec![], false) {
            // The ledger failed; the engine stops on the next ledger check.
            tracing_error(&e);
        }
        Ok(())
    }

    /// Advance handoffs waiting for history and reads waiting for transcripts.
    pub(super) fn advance_flows(&mut self) -> io::Result<()> {
        if self.in_flows {
            return Ok(());
        }
        self.in_flows = true;
        let result = self.advance_flows_inner();
        self.in_flows = false;
        result
    }
    fn advance_flows_inner(&mut self) -> io::Result<()> {
        let now = Instant::now();
        for prep in std::mem::take(&mut self.preparing) {
            let Some(live) = self.running.get_mut(&prep.source) else {
                self.answer(prep.reply, Err("The source route closed.".into()))?;
                continue;
            };
            let Some(t) = live.store.transcript(&prep.start.source) else {
                self.answer(
                    prep.reply,
                    Err("Open the session before handing it off.".into()),
                )?;
                continue;
            };
            let waiting_window =
                prep.window.lock().unwrap().is_none() && prep.window_deadline > now;
            if t.has_older() || t.is_loading() || waiting_window {
                if prep.deadline <= now {
                    self.answer(
                        prep.reply,
                        Err(
                            "The session's earlier history did not load in time. Try again.".into(),
                        ),
                    )?;
                    continue;
                }
                if t.has_older() && !t.is_loading() {
                    let _ = live
                        .store
                        .dispatch(Action::LoadOlder(prep.start.source.clone()));
                }
                self.preparing.push(prep);
                continue;
            }
            if let Err((reply, e)) = self.launch_handoff(prep) {
                self.answer(reply, Err(e))?;
            }
        }
        for read in std::mem::take(&mut self.reading) {
            match self.advance_read(read, now) {
                Step::Wait(read) => self.reading.push(read),
                Step::Answer(reply, result) => self.answer(reply, result)?,
            }
        }
        Ok(())
    }

    /// The handoff sources of a session, nearest first.
    pub(super) fn handoff_chain(&self, scope: &Scope, session: &str) -> Vec<HandoffSource> {
        let mut chain: Vec<HandoffSource> = Vec::new();
        let mut current = (scope.clone(), session.to_owned());
        for _ in 0..32 {
            let Some(h) = self
                .workers
                .get(&current.0, &current.1)
                .and_then(|e| e.origin.handoff.as_deref())
            else {
                break;
            };
            if chain
                .iter()
                .any(|c| c.scope == h.scope && c.session == h.session)
            {
                break;
            }
            current = (h.scope.clone(), h.session.clone());
            chain.push(h.clone());
        }
        chain
    }

    /// read_ox_thread: a session reads its own handoff sources, read-only and
    /// within its project. Resolved by the same exact-caller rules as every
    /// parent operation.
    pub(super) fn read_thread(
        &mut self,
        caller: &Caller,
        call: ParentCall,
        reply: Replier,
    ) -> io::Result<()> {
        let ParentCall::ReadThread {
            session,
            item,
            after,
            limit,
        } = call
        else {
            return self.answer(reply, Err("Not a read.".into()));
        };
        let found = (|| {
            let Resolved::Hosted { scope, session: me } = self.resolve(caller)? else {
                return Err("read_ox_thread reads the handoff sources of sessions the OX session host runs. This launch has none.".to_owned());
            };
            let chain = self.handoff_chain(&scope, &me);
            if chain.is_empty() {
                return Err("This session was not handed off from another session, so it has no source to read.".into());
            }
            let src = match &session {
                None => chain[0].clone(),
                Some(s) => chain
                    .iter()
                    .find(|h| &h.session == s || h.native.as_ref() == Some(s))
                    .cloned()
                    .ok_or("That session is not in this session's handoff chain.")?,
            };
            if src.scope.project != scope.project {
                return Err("That source is outside this session's project.".into());
            }
            Ok((scope, src))
        })();
        let (_caller_scope, src) = match found {
            Ok(v) => v,
            Err(e) => return self.answer(reply, Err(e)),
        };
        if let Err(e) = self.ensure(&src.scope, None) {
            return self.answer(
                reply,
                Err(format!("The source route cannot be opened: {e}")),
            );
        }
        self.reading.push(Reading {
            scope: src.scope.clone(),
            session: src.session.clone(),
            source: Box::new(src),
            args: ReadArgs {
                item,
                after,
                limit: limit.unwrap_or(20).clamp(1, 100) as usize,
            },
            reply,
            deadline: Instant::now() + LOAD_WAIT,
            selected: false,
        });
        self.advance_flows()
    }

    fn advance_read(&mut self, mut r: Reading, now: Instant) -> Step {
        let late = r.deadline <= now;
        let Some(live) = self.running.get_mut(&r.scope) else {
            return Step::Answer(r.reply, Err("The source route closed.".into()));
        };
        let wait = |r: Reading, why: &str| {
            if late {
                Step::Answer(r.reply, Err(format!("{why} Try again shortly.")))
            } else {
                Step::Wait(r)
            }
        };
        if live.store.state() != &Connection::Ready {
            return wait(r, "The source route is still connecting.");
        }
        let Some(t) = live.store.transcript(&r.session) else {
            if live.store.catalog_row(&r.session).is_some() && !r.selected {
                r.selected = true;
                let _ = live.store.dispatch(Action::SelectThread(r.session.clone()));
            }
            return wait(r, "The source session is not loaded yet.");
        };
        if t.has_older() || t.is_loading() {
            if !t.is_loading() {
                let _ = live.store.dispatch(Action::LoadOlder(r.session.clone()));
            }
            return wait(r, "The source session's history is still loading.");
        }
        match live.state.transcripts.get(&r.session) {
            Some(timeline) => {
                let page = read_page(&r.source, timeline, &r.args);
                Step::Answer(r.reply, page)
            }
            None => wait(r, "The source session is not loaded yet."),
        }
    }

    /// What a parent agent reads for a job that is not (yet) a session.
    pub(super) fn unstarted_job(&self, me: &Resolved, job_id: u64) -> Option<Value> {
        let owned = |origin: &Origin| {
            crate::workers::owns(
                me,
                &Entry {
                    origin: origin.clone(),
                    marks: Marks::default(),
                },
            )
        };
        if let Some(a) = self.asking.get(&job_id).filter(|a| owned(&a.origin)) {
            return Some(crate::yolo::status(a, self.operators().connected));
        }
        if let Some(o) = self
            .ledger
            .data
            .outcomes
            .iter()
            .rev()
            .find(|o| o.origin.job_id == job_id && owned(&o.origin))
        {
            return Some(crate::yolo::outcome(o));
        }
        self.running
            .values()
            .filter_map(|l| l.launching.as_ref())
            .find(|l| l.origin.job_id == job_id && l.origin.job_id != 0 && owned(&l.origin))
            .map(|l| {
                json!({"job_id": job_id, "status": "starting", "runtime": l.origin.runtime,
                    "mode": l.origin.mode, "owner": "host", "can_steer": false,
                    "access_ceiling": l.origin.ceiling, "messages": []})
            })
    }
    /// Job numbers a parent owns that are not sessions yet.
    pub(super) fn unstarted_jobs(&self, me: &Resolved) -> Vec<u64> {
        let mut jobs: Vec<u64> = self
            .asking
            .keys()
            .copied()
            .chain(self.ledger.data.outcomes.iter().map(|o| o.origin.job_id))
            .chain(
                self.running
                    .values()
                    .filter_map(|l| l.launching.as_ref())
                    .map(|l| l.origin.job_id)
                    .filter(|j| *j != 0),
            )
            .filter(|j| self.workers.job(*j).is_none() && self.unstarted_job(me, *j).is_some())
            .collect();
        jobs.sort_unstable();
        jobs.dedup();
        jobs
    }
    /// A parent may withdraw its own YOLO request before the operator answers.
    pub(super) fn withdraw(&mut self, job_id: u64) -> io::Result<Option<Value>> {
        self.withdraw_with(
            job_id,
            "The parent withdrew the request before the operator answered.",
        )
    }
    /// Withdraw a pending YOLO request, recording `message` as why it was cancelled.
    fn withdraw_with(&mut self, job_id: u64, message: &str) -> io::Result<Option<Value>> {
        let Some(a) = self.asking.remove(&job_id) else {
            return Ok(None);
        };
        let origin = a.origin.clone();
        self.ledger.change(|d| {
            d.asking.retain(|o| o.job_id != job_id);
            d.outcomes.push(Outcome {
                origin,
                status: "cancelled".into(),
                message: message.into(),
            });
        })?;
        if let Some(parent) = a
            .origin
            .parent
            .scope
            .clone()
            .filter(|p| self.running.contains_key(p))
        {
            self.process(&parent, vec![], true)?;
        }
        Ok(Some(json!({"job_id": job_id, "status": "cancelled"})))
    }
    /// Withdraw every pending YOLO request whose parent is this session or one of its
    /// workers (at any depth), so an ended session leaves no request to answer.
    pub(super) fn withdraw_asks_under(&mut self, scope: &Scope, thread: &str) -> io::Result<()> {
        let mut parents = vec![(scope.clone(), thread.to_owned())];
        let mut next = 0;
        while next < parents.len() {
            let (s, t) = parents[next].clone();
            let kids: Vec<(Scope, String)> = self
                .workers
                .children_of(&s, &t)
                .map(|(cs, cid)| (cs.clone(), cid.to_owned()))
                .collect();
            for kid in kids {
                if !parents.contains(&kid) {
                    parents.push(kid);
                }
            }
            next += 1;
        }
        let under = |o: &Origin| match (o.parent.scope.as_ref(), o.parent.session.as_deref()) {
            (Some(s), Some(t)) => parents.iter().any(|(ps, pt)| ps == s && pt == t),
            _ => false,
        };
        let jobs: Vec<u64> = self
            .asking
            .iter()
            .filter(|(_, a)| under(&a.origin))
            .map(|(job, _)| *job)
            .collect();
        for job in jobs {
            self.withdraw_with(job, ENDED_PARENT)?;
        }
        // A request recorded in the ledger but not held in memory is cancelled too.
        let stale: Vec<Origin> = self
            .ledger
            .data
            .asking
            .iter()
            .filter(|o| under(o))
            .cloned()
            .collect();
        if !stale.is_empty() {
            self.ledger.change(|d| {
                d.asking.retain(|o| !under(o));
                for origin in stale {
                    d.outcomes.push(Outcome {
                        origin,
                        status: "cancelled".into(),
                        message: ENDED_PARENT.into(),
                    });
                }
            })?;
        }
        Ok(())
    }
}

/// Why a YOLO request is refused or cancelled once the session that asked for it is gone.
pub(super) const ENDED_PARENT: &str = "The session that asked for this worker has ended.";

enum Step {
    Wait(Reading),
    Answer(Replier, Result<Value, String>),
}

fn tracing_error(e: &io::Error) {
    ox_host_service::errlog::record(&e.to_string());
}

/// One page of a handoff source for read_ox_thread.
fn read_page(
    src: &HandoffSource,
    timeline: &wire::Timeline,
    args: &ReadArgs,
) -> Result<Value, String> {
    let items = handoff::items(timeline);
    let view = |i: &handoff::SourceItem| {
        json!({
            "id": i.id,
            "turn": i.turn,
            "role": i.kind,
            "text": i.text,
            "carried": src.selection.items.contains(&i.id),
            "cut_in_handoff": src.selection.cut.contains(&i.id),
        })
    };
    let header = json!({
        "session": src.session,
        "native_session": src.native,
        "title": src.title,
        "runtime": src.runtime,
        "route": src.route.label(),
        "total_items": items.len(),
        "omitted_ranges": src.selection.ranges,
        "cut_items": src.selection.cut,
    });
    if let Some(id) = &args.item {
        let item = items
            .iter()
            .find(|i| &i.id == id)
            .ok_or("No item with that ID in the source session.")?;
        return Ok(json!({"source": header, "items": [view(item)], "next_after": null}));
    }
    let start = match &args.after {
        Some(a) => {
            items
                .iter()
                .position(|i| &i.id == a)
                .ok_or("No item with that ID in the source session.")?
                + 1
        }
        None => 0,
    };
    let mut page = Vec::new();
    let mut bytes = 0;
    for item in &items[start.min(items.len())..] {
        if page.len() >= args.limit || (!page.is_empty() && bytes + item.text.len() > PAGE_BYTES) {
            break;
        }
        bytes += item.text.len();
        page.push(item);
    }
    let next = (start + page.len() < items.len()).then(|| page.last().map(|i| i.id.clone()));
    Ok(json!({
        "source": header,
        "items": page.into_iter().map(view).collect::<Vec<_>>(),
        "next_after": next.flatten(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::host_protocol::TimelineItem;

    fn timeline(n: usize) -> wire::Timeline {
        wire::Timeline {
            items: (0..n)
                .map(|i| TimelineItem {
                    turn: format!("t{i}"),
                    id: format!("i{i}"),
                    role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                    text: format!("text {i}"),
                    revision: 1,
                    detail: Value::Null,
                })
                .collect(),
            detail: Value::Null,
        }
    }

    #[test]
    fn read_pages_by_item_id_and_returns_whole_items() {
        let src = HandoffSource {
            scope: Scope {
                project: "/p".into(),
                route: ox_desk_core::runtime::Route::CODEX_SUBSCRIPTION,
            },
            session: "s".into(),
            native: None,
            title: "t".into(),
            runtime: ox_desk_core::runtime::Runtime::Codex,
            route: ox_desk_core::runtime::Route::CODEX_SUBSCRIPTION,
            source_access: None,
            source_level: None,
            decision: Decision::Inherited,
            selection: handoff::SelectionRefs {
                items: vec!["i0".into(), "i9".into()],
                cut: vec!["i9".into()],
                ..Default::default()
            },
        };
        let t = timeline(10);
        let args = |item: Option<&str>, after: Option<&str>, limit| ReadArgs {
            item: item.map(Into::into),
            after: after.map(Into::into),
            limit,
        };
        let first = read_page(&src, &t, &args(None, None, 4)).unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 4);
        assert_eq!(first["items"][0]["carried"], true);
        assert_eq!(first["next_after"], "i3");
        let next = read_page(&src, &t, &args(None, Some("i3"), 100)).unwrap();
        assert_eq!(next["items"][0]["id"], "i4");
        assert!(next["next_after"].is_null());
        let one = read_page(&src, &t, &args(Some("i9"), None, 1)).unwrap();
        assert_eq!(one["items"][0]["text"], "text 9");
        assert_eq!(one["items"][0]["cut_in_handoff"], true);
        assert!(read_page(&src, &t, &args(Some("nope"), None, 1)).is_err());
        assert!(read_page(&src, &t, &args(None, Some("nope"), 1)).is_err());
    }
}

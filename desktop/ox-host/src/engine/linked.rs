use super::*;
use crate::workers::{Resolved, owns};
use ox_desk_core::origin::LinkedParent;
use ox_host_web::linked_jobs::{Operation, VerifiedJob};
use sha2::{Digest, Sha256};
use std::{fs, io::Write};

pub(super) enum ResultState {
    Preparing,
    Ready(ox_host_web::worker_result::Report),
    Retry { error: String, after: Instant },
}

pub(super) fn terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "interrupted" | "cancelled")
}

impl Engine {
    pub(super) fn recover_worker_results(&mut self) {
        let jobs: Vec<_> = self
            .workers
            .jobs()
            .filter_map(|(_, _, entry)| {
                let origin = &entry.origin;
                let status = self
                    .ledger
                    .data
                    .worker_notices
                    .observed
                    .get(&origin.job_id)?;
                (origin.linked.is_some() && origin.requested != Level::Read && terminal(status))
                    .then(|| (origin.clone(), status.clone()))
            })
            .collect();
        for (origin, status) in jobs {
            self.prepare_worker_result(&origin, &status);
        }
    }

    pub(super) fn prepare_worker_result(&mut self, origin: &Origin, status: &str) {
        let Some(link) = &origin.linked else { return };
        let id = origin.job_id;
        if origin.requested == Level::Read || !terminal(status) {
            return;
        }
        match self.worker_results.get(&id) {
            Some(ResultState::Retry { after, .. }) if Instant::now() >= *after => (),
            Some(_) => return,
            None => (),
        }
        self.worker_results.insert(id, ResultState::Preparing);
        let completed = status == "completed";
        let directory = self
            .state_dir
            .join("linked-work/jobs")
            .join(&link.host)
            .join(id.to_string());
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = ox_host_web::worker_result::finish(&directory, id, completed)
                .map_err(|e| e.to_string());
            let _ = tx.send(Message::WorkerResult(id, Box::new(result)));
        });
    }

    pub(super) fn linked_job(&mut self, job: VerifiedJob, reply: Replier) -> io::Result<()> {
        let result = self.admit_linked(&job);
        match result {
            Ok(Linked::Reply(status)) => self.answer(reply, Ok(status)),
            Ok(Linked::Start(accepted)) => self.start_accepted(Ok(*accepted), reply),
            Err(error) => self.answer(reply, Err(error)),
        }
    }
    fn admit_linked(&mut self, job: &VerifiedJob) -> Result<Linked, String> {
        let r = &job.request;
        let grant = job.grant()?;
        let owner = Resolved::Linked {
            source: r.source.clone(),
            device: job.device.clone(),
            request: r.request_id.clone(),
        };
        let registered = if let Some((_, _, entry)) = self.workers.job(r.job) {
            if !owns(&owner, entry) {
                return Err("This job number belongs to a different worker.".into());
            }
            Some(self.job_status(r.job)?)
        } else {
            self.unstarted_job(&owner, r.job)
        };
        let marker = job.directory.join("worker-start");
        match &r.operation {
            Operation::ReleaseResult { commit } => {
                ox_host_web::worker_result::request_retirement(&job.directory, commit)
                    .map_err(|e| e.to_string())?;
                Ok(Linked::Reply(json!({"released":true})))
            }
            Operation::Control { call } => {
                match call {
                    ParentCall::Status { job_id }
                    | ParentCall::Steer { job_id, .. }
                    | ParentCall::Cancel { job_id }
                        if *job_id == r.job => {}
                    _ => {
                        return Err(
                            "A linked parent can inspect, steer or cancel this worker.".into()
                        );
                    }
                }
                if registered.is_none() {
                    let status = match fs::read_to_string(&marker) {
                        Ok(m) if m == "cancelled" => json!({"job_id":r.job,"status":"cancelled"}),
                        Ok(_) => interrupted(r.job),
                        Err(e)
                            if e.kind() == io::ErrorKind::NotFound
                                && matches!(call, ParentCall::Cancel { .. }) =>
                        {
                            let mut f = fs::File::create_new(&marker).map_err(|e| e.to_string())?;
                            f.write_all(b"cancelled")
                                .and_then(|_| f.sync_all())
                                .and_then(|_| fs::File::open(&job.directory)?.sync_all())
                                .map_err(|e| e.to_string())?;
                            json!({"job_id":r.job,"status":"cancelled"})
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            json!({"job_id":r.job,"status":"not_started"})
                        }
                        Err(e) => return Err(e.to_string()),
                    };
                    return Ok(Linked::Reply(status));
                }
                // Use the same ownership, cancellation and native runtime controls as local parents.
                let mut view = self.call_as(owner, call.clone())?;
                if view["accepted"] == true {
                    view = self.job_status(r.job)?;
                    view["accepted"] = true.into();
                }
                if let Some(status) = view["status"].as_str().filter(|s| terminal(s)) {
                    if let Some((_, _, entry)) = self.workers.job(r.job) {
                        let origin = entry.origin.clone();
                        self.prepare_worker_result(&origin, status);
                    }
                    match self.worker_results.get(&r.job) {
                        Some(ResultState::Preparing) => {
                            view["worker_status"] = view["status"].clone();
                            view["status"] = "returning_result".into();
                            view["worker_changes_pending"] = true.into();
                            return Ok(Linked::Reply(view));
                        }
                        Some(ResultState::Ready(report)) => {
                            view["change_result"] = json!(report);
                            if self
                                .workers
                                .job(r.job)
                                .is_some_and(|(_, _, entry)| entry.marks.continued > 0)
                            {
                                view["worker_followup"] = json!(
                                    "This ref records the original job. Later operator turns remain in the worker checkout on this host."
                                );
                            }
                        }
                        Some(ResultState::Retry { error, .. }) => {
                            view["worker_changes_error"] = json!(error);
                            view["worker_changes_pending"] = true.into();
                        }
                        None => {}
                    }
                }
                self.ledger
                    .change(|d| {
                        d.worker_notices
                            .linked
                            .retain(|n| n.job != r.job || n.id > r.acknowledged)
                    })
                    .map_err(|e| e.to_string())?;
                let mut pending = self
                    .ledger
                    .data
                    .worker_notices
                    .linked
                    .iter()
                    .filter(|n| n.job == r.job);
                view["linked_notices"] = json!(pending.by_ref().take(64).collect::<Vec<_>>());
                view["more_notices"] = pending.next().is_some().into();
                Ok(Linked::Reply(view))
            }
            Operation::Start {
                results,
                memory_brief,
                parent,
                level,
                start,
            } => {
                let digest = format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&r.operation).map_err(|e| e.to_string())?)
                );
                match fs::read_to_string(&marker) {
                    Ok(existing) if existing == "cancelled" => {
                        return Ok(Linked::Reply(json!({"job_id":r.job,"status":"cancelled"})));
                    }
                    Ok(existing) if existing == digest => {
                        return Ok(Linked::Reply(
                            registered.unwrap_or_else(|| interrupted(r.job)),
                        ));
                    }
                    Ok(_) => {
                        return Err("This job already received a different start request.".into());
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.to_string()),
                }
                if registered.is_some() || self.job_in_use(r.job) {
                    return Err("This job number is already in use on the target.".into());
                }
                if !grant.may_launch {
                    return Err("New worker launches are disabled on this link.".into());
                }
                if self.draining.is_some() || self.update_reserved() {
                    return Err(crate::handover::REFUSAL.into());
                }
                if start.host.is_some()
                    || start.needs.is_some()
                    || start.placement.is_some()
                    || start.checkout.is_some()
                    || start.yolo_confirmed
                {
                    return Err("A linked worker starts inside its installed checkout.".into());
                }
                start.validate()?;
                if !self.config.installed_runtimes().contains(&start.runtime) {
                    return Err(format!(
                        "{} is not installed on the target.",
                        start.runtime.name()
                    ));
                }
                let routes = self.config.prepare().map_err(|e| e.to_string())?;
                if !routes.routes.contains(&start.route())
                    || (start.route().is_api() && !routes.has_api_provider())
                {
                    return Err(format!(
                        "{} is unavailable on the target.",
                        start.route().label()
                    ));
                }
                let caller = Caller::Linked {
                    stamp: Box::new(LinkedParent {
                        host: r.source.clone(),
                        device: job.device.clone(),
                        request_id: r.request_id.clone(),
                        parent: parent.clone(),
                        verified_level: *level,
                        link_ceiling: grant.ceiling,
                    }),
                    project: job.project.clone(),
                    job: r.job,
                };
                let mut accepted = self.accept_launch(&caller, *start.clone())?;
                if let Some(brief) = memory_brief
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let capped: String =
                        brief.chars().take(ox_memory::DEFAULT_BRIEF_CHARS).collect();
                    match &mut accepted {
                        Accepted::Launch(_, l) => l.memory_brief = Some(capped),
                        Accepted::Ask(a) => a.memory_brief = Some(capped),
                    }
                }
                let origin = match &accepted {
                    Accepted::Launch(_, l) => &l.origin,
                    Accepted::Ask(a) => &a.origin,
                };
                let policy =
                    if origin.requested != ox_desk_core::modes::Level::Read {
                        Some(results.as_ref().ok_or(
                            "Update the sending host to collect changes from a write worker.",
                        )?)
                    } else {
                        None
                    };
                job.commit_start(origin.ceiling, || {
                    if let Some(p) = policy {
                        ox_host_web::worker_result::configure(&job.directory, p)?;
                    }
                    let mut f = fs::File::create_new(&marker)?;
                    f.write_all(digest.as_bytes())?;
                    f.sync_all()?;
                    fs::File::open(&job.directory)?.sync_all()
                })?;
                Ok(Linked::Start(Box::new(accepted)))
            }
        }
    }
}

enum Linked {
    Reply(Value),
    Start(Box<Accepted>),
}
fn interrupted(job: u64) -> Value {
    json!({"job_id":job,"status":"interrupted","owner":"host","message":"The target accepted this job but has no surviving worker session. Start a new job to retry.","can_steer":false})
}

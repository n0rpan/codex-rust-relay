use super::*;
use crate::workers::{Resolved, owns};
use ox_desk_core::origin::LinkedParent;
use ox_host_web::linked_jobs::{Operation, VerifiedJob};
use sha2::{Digest, Sha256};
use std::{fs, io::Write};

impl Engine {
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
                if start.host.is_some() || start.checkout.is_some() || start.yolo_confirmed {
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
                let accepted = self.accept_launch(&caller, start.clone())?;
                let ceiling = match &accepted {
                    Accepted::Launch(_, l) => l.origin.ceiling,
                    Accepted::Ask(a) => a.origin.ceiling,
                };
                job.commit_start(ceiling, || {
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

//! The source keeps ownership and delivery state. The runtime and its transcript
//! stay on the target. Network and Git work never run on the session actor.
use super::forward_queue::{Pending, path as queue_path};
use super::*;
use crate::workers::{Entry, Marks, Resolved, owns};
use ox_desk_core::modes::ModeRequest;
use ox_host_web::{
    linked_jobs::{self, JobRequest, JobResult, Operation},
    paired_hosts::{self, SavedHost},
    worker_checkout::Delivery,
};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{collections::VecDeque, fs};

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Job {
    pub(super) origin: Origin,
    host: String,
    pub(super) name: String,
    pairing: String,
    request: uuid::Uuid,
    #[serde(default)]
    cursor: u64,
    #[serde(default)]
    awaiting_start: bool,
    #[serde(default)]
    sync_notices: bool,
    status: String,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkout: Option<String>,
    #[serde(default)]
    recovered: bool,
    #[serde(default)]
    cancel_requested: bool,
}
impl Job {
    pub(super) fn owned(&self, me: &Resolved) -> bool {
        owns(
            me,
            &Entry {
                origin: self.origin.clone(),
                marks: Marks::default(),
            },
        )
    }
    pub(super) fn preparing(&self) -> bool {
        self.status == "preparing"
    }
    pub(super) fn active(&self) -> bool {
        !matches!(
            self.status.as_str(),
            "completed" | "failed" | "cancelled" | "interrupted" | "declined" | "detached"
        )
    }
    pub(super) fn summary(&self) -> Value {
        self.decorate(
            json!({"job_id":self.origin.job_id,"status":self.status,"runtime":self.origin.runtime,
            "mode":self.origin.mode,"message":self.message,"can_steer":false,"owner":"host"}),
        )
    }
    fn decorate(&self, mut v: Value) -> Value {
        v["worker_host"] = json!({"id":self.host,"name":self.name});
        if let Some(c) = &self.checkout {
            v["snapshot_commit"] = json!(c);
        }
        if self.recovered {
            v["checkout_recovered"] = true.into();
        }
        if self.cancel_requested {
            v["cancellation_pending"] = true.into();
        }
        v
    }
    fn destination(&self, registry: &Path) -> io::Result<SavedHost> {
        let peers = paired_hosts::worker_links(registry)?;
        peers.into_iter().find(|host|host.id==self.host && host.pairing_id()==self.pairing).ok_or_else(|| {
            io::Error::new(io::ErrorKind::PermissionDenied,"The pairing that owns this worker was removed. Its session remains on the target.")
        })
    }
}
#[derive(Default)]
pub(super) struct State {
    busy: HashSet<u64>,
    preparing: HashMap<u64, Arc<AtomicBool>>,
    next: HashMap<u64, Instant>,
    failures: HashMap<u64, u32>,
    queue: HashMap<u64, VecDeque<(ParentCall, Replier)>>,
}
pub(crate) enum Event {
    Prepared(u64, io::Result<(Delivery, Box<Operation>)>),
    Reply(u64, Kind, Option<Replier>, io::Result<JobResult>),
}
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Start,
    Status,
    Steer,
    Cancel,
}

pub(super) fn recover(ledger: &Ledger, state: &Path) -> io::Result<()> {
    let queue = state.join("linked-work/outgoing/requests");
    let entries = match fs::read_dir(queue) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let keep: HashSet<_> = ledger
        .data
        .forwarded
        .values()
        .filter(|j| j.awaiting_start)
        .map(|j| j.request.to_string())
        .collect();
    for e in entries {
        let e = e?;
        let name = e.file_name();
        let name = name.to_string_lossy();
        if e.file_type()?.is_dir()
            && (name.starts_with(".new-")
                || (uuid::Uuid::parse_str(&name).is_ok() && !keep.contains(&*name)))
        {
            fs::remove_dir_all(e.path())?;
        }
    }
    Ok(())
}
fn retryable(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::NotConnected
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
    )
}
impl Engine {
    pub(super) fn forward_launch(
        &mut self,
        caller: Caller,
        mut start: WorkerStart,
        selector: &str,
    ) -> JobResult {
        start.validate()?;
        let (project, parent, source, external) = self.worker_source(&caller)?;
        let resolved =
            crate::workers::grant(source, external, start.mode_request(), start.runtime)?;
        let parent_ref = crate::workers::parent_ref(&caller, parent.clone());
        self.check_worker_cap(&parent_ref)?;
        let project = ox_desk_core::worker_checkout::resolve(
            &project,
            start.checkout.take().as_deref(),
            source == Some(Level::Yolo),
        )?;
        let peers = paired_hosts::worker_links(&self.state_dir.join("worker-links.json"))
            .map_err(|e| e.to_string())?;
        let host = peers
            .into_iter()
            .find(|h| h.id == selector)
            .ok_or_else(|| {
                format!("Worker host {selector:?} is no longer paired. See list_ox_hosts.")
            })?;
        let level = source.unwrap_or(crate::workers::TERMINAL_CEILING);
        if external && start.mode_request() == ModeRequest::Inherit {
            start.permission = Some(ModeRequest::Level(resolved.level));
        }
        // The target owns the operator's policy decision.
        start.yolo_confirmed = false;
        let job = self.new_job_id()?;
        let origin = crate::workers::origin(&caller, parent, &start, job, resolved);
        let saved = Job {
            origin,
            host: host.id.clone(),
            name: host.name.clone(),
            pairing: host.pairing_id().into(),
            request: uuid::Uuid::new_v4(),
            cursor: 0,
            awaiting_start: true,
            sync_notices: false,
            status: "preparing".into(),
            message: "Preparing the parent checkout.".into(),
            checkout: None,
            recovered: false,
            cancel_requested: false,
        };
        let queue = queue_path(&self.state_dir, &saved.request);
        Pending {
            project,
            operation: Operation::Start {
                parent: parent_ref,
                level,
                start: Box::new(start),
            },
        }
        .save(&queue)
        .map_err(|e| e.to_string())?;
        let response = saved.summary();
        if let Err(e) = self.ledger.change(|d| {
            d.forwarded.insert(job, saved);
        }) {
            let _ = fs::remove_dir_all(&queue);
            return Err(e.to_string());
        }
        self.prepare_remote(job);
        Ok(response)
    }
    fn prepare_remote(&mut self, id: u64) {
        let j = self.ledger.data.forwarded[&id].clone();
        let queue = queue_path(&self.state_dir, &j.request);
        let staging = self.state_dir.join("linked-work/outgoing");
        let registry = self.state_dir.join("worker-links.json");
        let source = self.host_id.to_string();
        let tx = self.tx.clone();
        self.forward.busy.insert(id);
        let stop = Arc::new(AtomicBool::new(false));
        self.forward.preparing.insert(id, stop.clone());
        thread::spawn(move || {
            let prepare = || -> io::Result<_> {
                let p = Pending::load(&queue)?;
                let host = j.destination(&registry)?;
                let Operation::Start { start, level, .. } = &p.operation else {
                    return Err(io::Error::other("Invalid queued worker request."));
                };
                if !queue.join("snapshot").is_dir() {
                    ox_host_web::inventory::check_launch(
                        &host, &source, &p.project, start, *level,
                    )?;
                }
                let sender = ox_host_web::worker_checkout::Sender {
                    host: &host,
                    source: &source,
                    project: &p.project,
                    staging: &staging,
                    job: id,
                    request_id: &j.request.to_string(),
                };
                let delivery = sender.run(Some(&queue), || stop.load(Ordering::Acquire))?;
                Ok((delivery, Box::new(p.operation)))
            };
            let _ = tx.send(Message::Outbound(Box::new(Event::Prepared(id, prepare()))));
        });
    }
    pub(super) fn dispatch_parent_call(
        &mut self,
        caller: &Caller,
        call: ParentCall,
        reply: Replier,
    ) -> io::Result<()> {
        let id = match &call {
            ParentCall::Status { job_id }
            | ParentCall::Steer { job_id, .. }
            | ParentCall::Cancel { job_id }
            | ParentCall::Answer { job_id, .. } => Some(*job_id),
            _ => None,
        };
        let Some((id, j)) = id.and_then(|id| self.ledger.data.forwarded.get(&id).map(|j| (id, j)))
        else {
            let result = self.parent_call(caller, call);
            return self.answer(reply, result);
        };
        if !self.resolve(caller).is_ok_and(|me| j.owned(&me)) {
            return self.answer(
                reply,
                Err("Unknown worker job for this parent session.".into()),
            );
        }
        if j.status == "detached" {
            return self.answer(
                reply,
                match call {
                    ParentCall::Status { .. } | ParentCall::Cancel { .. } => Ok(j.summary()),
                    _ => Err(j.message.clone()),
                },
            );
        }
        if matches!(call, ParentCall::Answer { .. }) {
            return self.answer(
                reply,
                Err(format!("Answer this worker’s requests on {}.", j.name)),
            );
        }
        if j.status == "preparing" || (j.checkout.is_none() && !j.active()) {
            let result = match call {
                ParentCall::Cancel { .. } if j.active() => {
                    if let Some(stop) = self.forward.preparing.get(&id) {
                        stop.store(true, Ordering::Release);
                    }
                    self.record_forward(id, "cancelled", "Cancelled before the worker started.")?;
                    Ok(self.ledger.data.forwarded[&id].summary())
                }
                ParentCall::Steer { .. } => Err("The remote worker has not started.".into()),
                _ => Ok(j.summary()),
            };
            return self.answer(reply, result);
        }
        if matches!(call, ParentCall::Cancel { .. }) && j.active() {
            self.ledger.change(|d| {
                d.forwarded.get_mut(&id).unwrap().cancel_requested = true;
            })?;
        }
        if self.forward.busy.contains(&id) {
            if matches!(call, ParentCall::Status { .. }) {
                return self.answer(reply, Ok(self.ledger.data.forwarded[&id].summary()));
            }
            self.forward
                .queue
                .entry(id)
                .or_default()
                .push_back((call, reply));
        } else {
            self.remote_control(id, call, Some(reply));
        }
        Ok(())
    }
    fn record_forward(&mut self, id: u64, status: &str, message: &str) -> io::Result<()> {
        self.ledger.change(|d| {
            let j = d.forwarded.get_mut(&id).expect("owned remote job");
            j.status = status.into();
            j.message = message.into();
            if !j.active() {
                j.awaiting_start = false;
                j.sync_notices = false;
                j.cancel_requested = false;
            }
            if !matches!(status, "starting" | "preparing") {
                d.worker_notices.observe_on(
                    &j.origin,
                    status,
                    &format!("Worker on {}. {message}", j.name),
                    Some(&j.name),
                );
            }
        })
    }
    fn remote_control(&mut self, id: u64, call: ParentCall, reply: Option<Replier>) {
        let kind = match &call {
            ParentCall::Cancel { .. } => Kind::Cancel,
            ParentCall::Steer { .. } => Kind::Steer,
            _ => Kind::Status,
        };
        self.send_remote(id, Operation::Control { call }, kind, reply);
    }
    fn send_remote(&mut self, id: u64, op: Operation, kind: Kind, reply: Option<Replier>) {
        let j = self.ledger.data.forwarded[&id].clone();
        let registry = self.state_dir.join("worker-links.json");
        let req = JobRequest {
            source: self.host_id.to_string(),
            target: j.host.clone(),
            request_id: j.request.to_string(),
            job: id,
            acknowledged: j.cursor,
            operation: op,
        };
        self.forward.busy.insert(id);
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = j
                .destination(&registry)
                .and_then(|host| linked_jobs::send(&host, &req));
            let _ = tx.send(Message::Outbound(Box::new(Event::Reply(
                id, kind, reply, result,
            ))));
        });
    }
    pub(super) fn forward_deadline(&self) -> Option<Instant> {
        self.ledger
            .data
            .forwarded
            .iter()
            .filter(|(id, j)| (j.active() || j.sync_notices) && !self.forward.busy.contains(id))
            .map(|(id, _)| {
                self.forward
                    .next
                    .get(id)
                    .copied()
                    .unwrap_or_else(Instant::now)
            })
            .min()
    }
    pub(super) fn poll_forwarded(&mut self, now: Instant) -> io::Result<()> {
        let due: Vec<_> = self
            .ledger
            .data
            .forwarded
            .iter()
            .filter(|(id, j)| {
                (j.active() || j.sync_notices)
                    && !self.forward.busy.contains(id)
                    && self.forward.next.get(id).is_none_or(|at| *at <= now)
            })
            .map(|(id, j)| (*id, j.cancel_requested))
            .collect();
        for (job_id, cancel) in due {
            let job = &self.ledger.data.forwarded[&job_id];
            if job.status == "preparing" {
                self.prepare_remote(job_id);
                continue;
            }
            if job.awaiting_start && !cancel {
                match Pending::load(&queue_path(&self.state_dir, &job.request)) {
                    Ok(p) => self.send_remote(job_id, p.operation, Kind::Start, None),
                    Err(e) => self.record_forward(
                        job_id,
                        "failed",
                        &format!("Cannot recover this queued launch: {e}"),
                    )?,
                }
                continue;
            }
            self.remote_control(
                job_id,
                if cancel {
                    ParentCall::Cancel { job_id }
                } else {
                    ParentCall::Status { job_id }
                },
                None,
            );
        }
        Ok(())
    }
    fn accept_remote_reply(&mut self, id: u64, kind: Kind, view: Value) -> io::Result<JobResult> {
        self.forward.failures.remove(&id);
        self.forward
            .next
            .insert(id, Instant::now() + Duration::from_secs(4));
        let Some(status) = view["status"].as_str().filter(|_| view["job_id"] == id) else {
            return Ok(Err("The worker host returned an invalid job status.".into()));
        };
        let message = view["message"]
            .as_str()
            .or_else(|| view["result"].as_str())
            .unwrap_or_default();
        let (status, message) = if status == "not_started" {
            ("interrupted", "The target has no launch for this worker.")
        } else {
            (status, message)
        };
        let notices = match view
            .get("linked_notices")
            .cloned()
            .map(serde_json::from_value::<Vec<crate::worker_notices::LinkedNotice>>)
            .transpose()
        {
            Ok(v) => v.unwrap_or_default(),
            Err(_) => {
                return Ok(Err(
                    "The target returned incompatible worker notices.".into()
                ));
            }
        };
        let job = &self.ledger.data.forwarded[&id];
        if status == "interrupted" && view["status"] == "not_started" {
            if job.awaiting_start && job.active() {
                return Ok(Ok(job.summary()));
            }
            if !job.active() {
                return Ok(Ok(job.summary()));
            }
        }
        self.ledger.change(|d| {
            let j = d.forwarded.get_mut(&id).unwrap();
            let old = j.cursor;
            for n in notices {
                if n.job == id && n.id > j.cursor {
                    d.worker_notices.observe_on(
                        &j.origin,
                        &n.status,
                        &format!("Worker on {}. {}", j.name, n.preview),
                        Some(&j.name),
                    );
                    j.cursor = n.id;
                }
            }
            j.sync_notices = kind == Kind::Start || j.cursor != old || view["more_notices"] == true;
            j.awaiting_start = false;
            j.status = status.into();
            j.message = crate::worker_notices::excerpt(message);
            if !j.active() {
                j.cancel_requested = false;
            }
            if status == "interrupted" && view["linked_notices"].is_null() {
                d.worker_notices
                    .observe_on(&j.origin, status, &j.message, Some(&j.name));
            }
        })?;
        let mut view = view;
        if let Some(o) = view.as_object_mut() {
            o.remove("linked_notices");
            o.remove("more_notices");
        }
        Ok(Ok(self.ledger.data.forwarded[&id].decorate(view)))
    }
    fn backoff(&mut self, id: u64) {
        let failures = self.forward.failures.entry(id).or_default();
        *failures = (*failures + 1).min(5);
        self.forward.next.insert(
            id,
            Instant::now() + Duration::from_secs(2u64.pow(*failures)),
        );
    }
    pub(super) fn outbound(&mut self, event: Event) -> io::Result<()> {
        let id = match event {
            Event::Prepared(id, result) => {
                self.forward.busy.remove(&id);
                self.forward.preparing.remove(&id);
                let queue = queue_path(&self.state_dir, &self.ledger.data.forwarded[&id].request);
                if self.ledger.data.forwarded[&id].status == "cancelled" {
                    let _ = fs::remove_dir_all(&queue);
                    return Ok(());
                }
                match result {
                    Ok((delivery, start)) => {
                        self.ledger.change(|d| {
                            let j = d.forwarded.get_mut(&id).unwrap();
                            j.checkout = Some(delivery.receipt.commit);
                            j.recovered = delivery.recovered;
                            j.status = "starting".into();
                            j.message = "Starting on the worker host.".into();
                        })?;
                        let _ = fs::remove_dir_all(queue.join("snapshot"));
                        self.send_remote(id, *start, Kind::Start, None);
                    }
                    Err(e) if retryable(&e) && queue.join("snapshot").is_dir() => {
                        self.backoff(id);
                    }
                    Err(e) => {
                        self.record_forward(id, "failed", &e.to_string())?;
                        let _ = fs::remove_dir_all(queue);
                    }
                }
                return self.deliver_worker_notices();
            }
            Event::Reply(id, kind, reply, result) => {
                self.forward.busy.remove(&id);
                let result = match result {
                    Ok(Ok(view)) => self.accept_remote_reply(id, kind, view)?,
                    Ok(Err(error)) => {
                        if kind == Kind::Start {
                            self.record_forward(id, "failed", &error)?;
                        }
                        self.forward
                            .next
                            .insert(id, Instant::now() + Duration::from_secs(15));
                        Err(error)
                    }
                    Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                        self.record_forward(id,"detached","The worker link is no longer available. Open the receiving host to inspect or stop the worker.")?;
                        Ok(self.ledger.data.forwarded[&id].summary())
                    }
                    Err(error) => {
                        self.backoff(id);
                        let mut view = self.ledger.data.forwarded[&id].summary();
                        view["connection"] = "unavailable".into();
                        view["connection_error"] = error.to_string().into();
                        if kind == Kind::Steer {
                            Err(format!("The target did not confirm the steer: {error}"))
                        } else {
                            Ok(view)
                        }
                    }
                };
                let j = &self.ledger.data.forwarded[&id];
                if !j.awaiting_start {
                    let _ = fs::remove_dir_all(queue_path(&self.state_dir, &j.request));
                }
                if let Some(reply) = reply {
                    self.answer(reply, result)?;
                }
                id
            }
        };
        if let Some((call, reply)) = self
            .forward
            .queue
            .get_mut(&id)
            .and_then(VecDeque::pop_front)
        {
            self.remote_control(id, call, Some(reply));
        }
        self.deliver_worker_notices()
    }
}

//! Remote worker operations are admitted with a signed worker-device proof.
//! Only this in-process channel can convey a verified remote parent to the engine.
use crate::{
    admin::WorkerGrant,
    paired_hosts::valid_id,
    server::{App, Proof, paired_identity},
    state::State,
    worker_checkout::receiver,
};
use axum::{
    Json,
    extract::State as Extract,
    http::{HeaderMap, StatusCode},
};
use ox_desk_core::{
    modes::Level,
    origin::{ParentCall, ParentRef, WorkerStart},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Start {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        results: Option<crate::worker_result::Policy>,
        parent: ParentRef,
        level: Level,
        start: Box<WorkerStart>,
    },
    ReleaseResult {
        commit: String,
    },
    Control {
        call: ParentCall,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRequest {
    pub source: String,
    pub target: String,
    pub job: u64,
    #[serde(default)]
    pub acknowledged: u64,
    pub request_id: String,
    pub operation: Operation,
}
pub type JobResult = Result<Value, String>;
pub type Dispatch = Arc<dyn Fn(VerifiedJob) -> JobResult + Send + Sync>;
pub struct VerifiedJob {
    pub request: JobRequest,
    pub device: String,
    pub project: PathBuf,
    pub directory: PathBuf,
    state: Arc<Mutex<State>>,
}
impl VerifiedJob {
    /// Commit admission under the lock used by link revocation and ceiling changes.
    pub fn commit_start<T>(
        &self,
        ceiling: Level,
        commit: impl FnOnce() -> io::Result<T>,
    ) -> Result<T, String> {
        let state = self.state.lock().unwrap();
        if !state.worker_grant(&self.device).is_some_and(|g| {
            g.may_launch && g.source == self.request.source && g.ceiling >= ceiling
        }) {
            return Err("Worker link access changed before launch.".into());
        }
        commit().map_err(|e| e.to_string())
    }
    /// Called on the engine thread when the request is actually admitted.
    pub fn grant(&self) -> Result<WorkerGrant, String> {
        self.state
            .lock()
            .unwrap()
            .worker_grant(&self.device)
            .filter(|g| g.source == self.request.source)
            .cloned()
            .ok_or_else(|| "Worker link was revoked.".into())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    request: String,
    proof: Proof,
}

pub(crate) async fn handle(
    Extract(app): Extract<App>,
    headers: HeaderMap,
    Json(body): Json<Envelope>,
) -> Result<Json<JobResult>, StatusCode> {
    let device = paired_identity(&app, &headers)?;
    let request: JobRequest =
        serde_json::from_str(&body.request).map_err(|_| StatusCode::BAD_REQUEST)?;
    if !valid_id(&request.request_id) || request.job == 0 || request.job >= 1 << 52 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let root = {
        let mut state = app.state.lock().unwrap();
        if !state.prove_worker(
            &device,
            &body.proof.nonce,
            &body.proof.signature,
            &body.request,
        ) || !state
            .worker_grant(&device)
            .is_some_and(|g| g.source == request.source)
            || state
                .host_id()
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
                != request.target
        {
            return Err(StatusCode::FORBIDDEN);
        }
        state.worker_directory()
    };
    let Some(dispatch) = app.jobs else {
        return Err(StatusCode::NOT_IMPLEMENTED);
    };
    let slot = app
        .slots
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    Ok(Json(
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let (directory, project) =
                receiver::job_checkout(&root, &request, &device).map_err(|e| e.to_string())?;
            dispatch(VerifiedJob {
                request,
                device,
                directory,
                project,
                state: app.state,
            })
        })
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?,
    ))
}

pub fn send(host: &crate::paired_hosts::SavedHost, request: &JobRequest) -> io::Result<JobResult> {
    if request.target != host.id {
        return Err(io::Error::other("The worker job belongs to another host."));
    }
    let client = host.client()?;
    crate::client::runtime()?.block_on(async {
        let request = serde_json::to_string(request)?;
        let proof = client.proof("worker", Some(&request)).await?;
        let result = client
            .post(
                "worker-job",
                serde_json::json!({"request":request,"proof":proof}),
                "Worker link was revoked or is no longer valid.",
                std::time::Duration::from_secs(75),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "stock Codex on a disposable TLS worker host"]
    fn read_worker() {
        use std::time::{Duration, Instant};
        let dir = PathBuf::from(std::env::var_os("OX_TEST_REMOTE_DIR").unwrap());
        let source = std::fs::read_to_string(dir.join("host-id")).unwrap();
        let hosts = crate::paired_hosts::worker_links(&dir.join("worker-links.json")).unwrap();
        let target = &hosts[0];
        let request = |op| JobRequest {
            source: source.trim().into(),
            target: target.id.clone(),
            job: 42,
            acknowledged: 0,
            request_id: "ddcba183-5073-4e9a-9825-e8730784ce5e".into(),
            operation: op,
        };
        let start = WorkerStart::parse_command(
            "fixture-parent",
            "/worker codex:fixture/demo --mode read Read tracked in your working directory",
        )
        .unwrap()
        .unwrap();
        let parent = ParentRef {
            scope: None,
            project: dir.clone(),
            runtime: Some(start.runtime),
            session: Some("fixture-parent".into()),
        };
        let start = request(Operation::Start {
            results: None,
            parent,
            level: Level::Yolo,
            start: Box::new(start),
        });
        let status = request(Operation::Control {
            call: ParentCall::Status { job_id: 42 },
        });
        assert!(
            send(
                target,
                &request(Operation::Control {
                    call: ParentCall::Status { job_id: 7 }
                })
            )
            .unwrap()
            .is_err()
        );
        let mut wrong = start.clone();
        wrong.request_id = "ddcba183-5073-4e9a-9825-e8730784ce5f".into();
        assert!(send(target, &wrong).unwrap().is_err());
        let mut escalated = start.clone();
        if let Operation::Start { start, .. } = &mut escalated.operation {
            start.permission = Some(ox_desk_core::modes::ModeRequest::Level(Level::Write));
        }
        assert!(send(target, &escalated).unwrap().is_err());
        let first = send(target, &start).unwrap().unwrap();
        assert_eq!(first["job_id"], 42);
        assert_eq!(first["access_ceiling"], "read");
        let until = Instant::now() + Duration::from_secs(45);
        loop {
            let view = send(target, &status).unwrap().unwrap();
            if view["status"] == "completed" {
                assert!(view.to_string().contains("uncommitted on laptop"), "{view}");
                let again = send(target, &start).unwrap().unwrap();
                assert_eq!(again["session"], view["session"]);
                break;
            }
            assert!(Instant::now() < until, "{view}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

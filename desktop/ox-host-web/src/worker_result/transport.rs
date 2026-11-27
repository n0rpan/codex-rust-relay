use super::*;
use crate::{
    client,
    linked_jobs::{JobRequest, Operation},
    paired_hosts::{SavedHost, valid_id},
    server::{App, Proof, paired_identity},
    worker_checkout::{connection::Connection, receiver::job_checkout},
};
use axum::{
    extract::{State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use ox_desk_core::origin::ParentCall;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message as ClientMessage;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    source: String,
    target: String,
    job: u64,
    request_id: String,
    sha256: String,
}
#[derive(Deserialize)]
struct Open {
    request: String,
    proof: Proof,
}
pub(crate) async fn upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    let id = paired_identity(&app, &headers)?;
    if app.state.lock().unwrap().worker_grant(&id).is_none() {
        return Err(StatusCode::FORBIDDEN);
    }
    let slot = app
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    Ok(ws
        .max_message_size(256 * 1024)
        .max_frame_size(256 * 1024)
        .on_upgrade(move |socket| async move {
            let _slot = slot;
            let mut socket = Connection::open(socket);
            if let Err(e) = serve(&app, &id, &mut socket).await {
                let _ = socket
                    .send(Message::text(
                        serde_json::json!({"error":e.to_string()}).to_string(),
                    ))
                    .await;
            }
        }))
}
async fn serve(app: &App, id: &str, socket: &mut Connection<Message>) -> io::Result<()> {
    let Message::Text(first) =
        tokio::time::timeout(Duration::from_secs(8), socket.next()).await??
    else {
        return Err(io::Error::other("Missing result request."));
    };
    let open: Open = serde_json::from_str(&first)?;
    let r: Request = serde_json::from_str(&open.request)?;
    if !valid_id(&r.request_id) || r.job == 0 || r.job >= 1 << 52 {
        return Err(io::Error::other("Invalid result request."));
    }
    let (root, mut changes) = {
        let mut state = app.state.lock().unwrap();
        if !state.prove_worker(id, &open.proof.nonce, &open.proof.signature, &open.request)
            || !state.worker_grant(id).is_some_and(|g| g.source == r.source)
            || state.host_id()? != r.target
        {
            return Err(io::Error::other("Result pairing changed."));
        }
        (state.worker_directory(), state.changed.subscribe())
    };
    let source = r.source.clone();
    let device = id.to_owned();
    let (file, bytes) = tokio::task::spawn_blocking(move || -> io::Result<_> {
        let job = JobRequest {
            source: r.source,
            target: r.target,
            job: r.job,
            request_id: r.request_id,
            acknowledged: 0,
            operation: Operation::Control {
                call: ParentCall::Status { job_id: r.job },
            },
        };
        let (dir, _) = job_checkout(&root, &job, &device)?;
        let saved =
            report(&dir)?.ok_or_else(|| io::Error::other("This result is not available."))?;
        if let Some(inc) = saved.increment.filter(|i| i.sha256 == r.sha256) {
            return Ok((File::open(dir.join("worker-result/increment"))?, inc.bytes));
        }
        if saved.sha256 != r.sha256 {
            return Err(io::Error::other("The result checksum changed."));
        }
        Ok((File::open(dir.join("worker-result/archive"))?, saved.bytes))
    })
    .await??;
    let revoked = async {
        loop {
            if !app
                .state
                .lock()
                .unwrap()
                .worker_grant(id)
                .is_some_and(|g| g.source == source)
                || changes.changed().await.is_err()
            {
                break;
            }
        }
    };
    let stream = async {
        socket
            .send(Message::text(
                serde_json::json!({"bytes":bytes}).to_string(),
            ))
            .await?;
        let mut file = tokio::fs::File::from_std(file);
        let mut buf = vec![0; 256 * 1024];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            socket.send(Message::binary(buf[..n].to_vec())).await?;
        }
        socket.send(Message::text("done")).await
    };
    tokio::select! {biased; _=revoked=>Err(io::Error::other("The result link was revoked.")), result=stream=>result}
}

pub fn collect(
    host: &SavedHost,
    req: &JobRequest,
    report: &Report,
    project: &Path,
    repository: &str,
    staging: &Path,
) -> io::Result<Local> {
    if checkout::repository_id(project).as_deref() != Some(repository) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The source checkout origin changed. The result is retained on the target.",
        ));
    }
    let dir = staging.join(&req.request_id);
    if !dir.try_exists()? {
        crate::state::private_dir(staging)?;
        let stage = tempfile::Builder::new()
            .prefix(".return-")
            .tempdir_in(staging)?;
        let fetch = |bytes: u64, hash: &str| -> io::Result<File> {
            use io::Seek;
            let f = tempfile::tempfile_in(stage.path())?;
            checkout::check_archive_space(&f, bytes)?;
            let mut f = client::runtime()?.block_on(download(host, req, bytes, hash, f))?;
            f.rewind()?;
            Ok(f)
        };
        let full = || -> io::Result<_> {
            let archive = fetch(report.bytes, &report.sha256)?;
            let work =
                checkout::receive_result(stage.path(), req.job, archive, &report.summary.commit)?;
            Ok((work, report.bytes))
        };
        let base = report.increment.as_ref().and_then(|inc| {
            checkout::results::source_with_base(project, &inc.base).map(|root| (inc, root))
        });
        let (work, received_bytes) = if let Some((inc, root)) = base {
            let archive = fetch(inc.bytes, &inc.sha256)?;
            // If the parent pruned a prerequisite, recover using the complete
            // archive. Transport errors retain their own retry schedule.
            checkout::receive_commit(
                stage.path(),
                req.job,
                &root,
                archive,
                &report.summary.commit,
            )
            .map(|work| (work, inc.bytes))
            .or_else(|_| full())?
        } else {
            full()?
        };
        // One directory rename publishes the private checkout. Flush file payloads
        // as well as directory entries so its receipt survives power loss.
        sync_checkout(&work)?;
        fs::rename(work, stage.path().join("checkout"))?;
        write(&stage.path().join("received-bytes"), &received_bytes)?;
        write(&stage.path().join("report.json"), report)?;
        fs::rename(stage.path(), &dir).or_else(|e| if dir.is_dir() { Ok(()) } else { Err(e) })?;
        File::open(staging)?.sync_all()?;
    }
    let saved: Report = read(&dir.join("report.json"))?;
    if saved != *report {
        return Err(io::Error::other("The job already has a different result."));
    }
    let work = dir.join("checkout");
    if checkout::repository_id(project).as_deref() != Some(repository) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The source checkout origin changed while receiving the result.",
        ));
    }
    let reference = checkout::results::import(project, &work, req.job, &saved.summary)?;
    Ok(Local {
        received_bytes: read(&dir.join("received-bytes"))?,
        report: saved,
        reference,
        checkout: work,
    })
}
fn sync_checkout(path: &Path) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_checkout(&entry.path())?;
        } else if kind.is_file() {
            File::open(entry.path())?.sync_all()?;
        }
    }
    File::open(path)?.sync_all()
}

async fn next_data(socket: &mut Connection<ClientMessage>) -> io::Result<ClientMessage> {
    let frame = tokio::time::timeout(Duration::from_secs(90), socket.next()).await??;
    if let ClientMessage::Text(text) = &frame
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(text.as_str())
        && let Some(error) = value["error"].as_str()
    {
        return Err(io::Error::other(format!(
            "Result transfer refused: {error}"
        )));
    }
    Ok(frame)
}

async fn download(
    host: &SavedHost,
    req: &JobRequest,
    bytes: u64,
    sha256: &str,
    f: File,
) -> io::Result<File> {
    let client = host.client()?;
    let request = serde_json::to_string(&Request {
        source: req.source.clone(),
        target: host.id.clone(),
        job: req.job,
        request_id: req.request_id.clone(),
        sha256: sha256.to_owned(),
    })?;
    let proof = client.proof("worker", Some(&request)).await?;
    let socket = client
        .websocket(
            "worker-result",
            serde_json::json!({"proof":proof,"request":request}),
        )
        .await?;
    let mut socket = Connection::open(socket);
    let ClientMessage::Text(header) = next_data(&mut socket).await? else {
        return Err(io::Error::other("Missing result header."));
    };
    let header: serde_json::Value = serde_json::from_str(&header)?;
    if header["bytes"].as_u64() != Some(bytes) {
        return Err(io::Error::other(
            "Result size changed or the result is unavailable.",
        ));
    }
    let mut file = tokio::fs::File::from_std(f);
    let mut left = bytes;
    let mut hash = Sha256::new();
    while left > 0 {
        let ClientMessage::Binary(chunk) = next_data(&mut socket).await? else {
            return Err(io::Error::other("Interrupted result transfer."));
        };
        left = left
            .checked_sub(chunk.len() as u64)
            .filter(|_| !chunk.is_empty() && chunk.len() <= 256 * 1024)
            .ok_or_else(|| io::Error::other("Invalid result chunk."))?;
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    if !matches!(next_data(&mut socket).await?,ClientMessage::Text(m) if m=="done")
        || format!("{:x}", hash.finalize()) != sha256
    {
        return Err(io::Error::other("Worker result checksum differs."));
    }
    file.flush().await?;
    Ok(file.into_std().await)
}

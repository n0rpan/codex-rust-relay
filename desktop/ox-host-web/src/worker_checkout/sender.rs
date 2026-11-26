use super::connection::Connection;
use super::{CHUNK, Checkout, Delivery, Receipt, Reply, Request};
use crate::{client, paired_hosts::SavedHost};
type Socket = Connection<Message>;
use ox_host_service::checkout::{Snapshot, negotiate, repository_id};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{io, path::Path};
use tokio::io::AsyncReadExt;
use tokio_tungstenite::tungstenite::Message;

async fn receive(socket: &mut Socket) -> io::Result<Reply> {
    match socket.next().await? {
        Message::Text(text) => match serde_json::from_str(&text)? {
            Reply::Error { message } => Err(io::Error::other(message)),
            value => Ok(value),
        },
        _ => Err(io::Error::other(
            "The worker host closed the checkout connection.",
        )),
    }
}
async fn send(socket: &mut Socket, request: Request) -> io::Result<()> {
    socket
        .send(Message::text(serde_json::to_string(&request)?))
        .await
}

async fn connect(
    client: &client::PairedClient,
    open: &Checkout,
) -> io::Result<(Socket, Option<Receipt>)> {
    let request = serde_json::to_string(&Request::Open(open.clone()))?;
    let proof = client.proof("worker", Some(&request)).await?;
    let socket = client
        .websocket("worker-checkout", json!({"request":request,"proof":proof}))
        .await?;
    let mut socket = Connection::open(socket);
    match receive(&mut socket).await? {
        Reply::Ready {
            target,
            source,
            existing,
        } if target == open.target && source == open.source => Ok((socket, existing)),
        _ => Err(io::Error::other(
            "The worker host did not confirm its pairing.",
        )),
    }
}
/// Called off the host event loop. `staging` is a private directory on the source.
/// Reuse the same job and request ID to recover an installed checkout after a lost reply.
pub fn transfer(
    host: &SavedHost,
    source: &str,
    project: &Path,
    staging: &Path,
    job: u64,
    request_id: &str,
) -> io::Result<Delivery> {
    Sender {
        host,
        source,
        project,
        staging,
        job,
        request_id,
    }
    .run(None, || false)
}

pub struct Sender<'a> {
    pub host: &'a SavedHost,
    pub source: &'a str,
    pub project: &'a Path,
    pub staging: &'a Path,
    pub job: u64,
    pub request_id: &'a str,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Frozen {
    repository: String,
    commit: String,
}

impl Sender<'_> {
    /// Keep an exact snapshot for an interrupted upload until the receiver acknowledges it.
    pub fn run(&self, queue: Option<&Path>, cancelled: impl Fn() -> bool) -> io::Result<Delivery> {
        use std::fs::{self, File};
        let Self {
            host,
            source,
            project,
            staging,
            job,
            request_id,
        } = *self;
        let check_cancelled = || {
            if cancelled() {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Worker preparation was cancelled.",
                ))
            } else {
                Ok(())
            }
        };
        check_cancelled()?;
        let frozen = queue.map(|q| q.join("snapshot"));
        let prior = match frozen.as_ref().map(|p| File::open(p.join("metadata.json"))) {
            Some(Ok(f)) => Some(serde_json::from_reader::<_, Frozen>(f)?),
            Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => None,
        };
        let repository = prior
            .as_ref()
            .map(|s| s.repository.clone())
            .or_else(|| repository_id(project))
            .ok_or_else(|| {
                io::Error::other("Remote workers need a Git checkout with a shared network origin.")
            })?;
        let open = Checkout {
            source: source.to_owned(),
            target: host.id.clone(),
            repository: repository.clone(),
            job,
            request_id: request_id.to_owned(),
        };
        let client = host.client()?;
        let rt = client::runtime()?;
        let (mut socket, existing) = rt.block_on(connect(client, &open))?;
        if let Some(receipt) = existing {
            return Ok(Delivery {
                receipt,
                recovered: true,
            });
        }
        let (archive, commit) = if let Some(previous) = prior {
            (
                File::open(frozen.as_ref().unwrap().join("archive"))?,
                previous.commit,
            )
        } else {
            let known = negotiate(project, |commits| {
                check_cancelled()?;
                rt.block_on(async {
                    send(
                        &mut socket,
                        Request::Known {
                            commits: commits.to_vec(),
                        },
                    )
                    .await?;
                    match receive(&mut socket).await? {
                        Reply::Known { commits: known }
                            if known.iter().all(|s| commits.contains(s)) =>
                        {
                            Ok(known)
                        }
                        _ => Err(io::Error::other("Invalid worker mirror reply.")),
                    }
                })
            })?;
            let snapshot = Snapshot::capture(staging, project, &known)?;
            if repository_id(project).as_ref() != Some(&repository) {
                return Err(io::Error::other(
                    "The source checkout's origin changed during capture.",
                ));
            }
            check_cancelled()?;
            if let (Some(queue), Some(dest)) = (queue, &frozen) {
                let stage = tempfile::tempdir_in(queue)?;
                snapshot.retain_archive(&stage.path().join("archive"))?;
                let f = File::create(stage.path().join("metadata.json"))?;
                serde_json::to_writer(
                    &f,
                    &Frozen {
                        repository,
                        commit: snapshot.commit.clone(),
                    },
                )?;
                f.sync_all()?;
                File::open(stage.path())?.sync_all()?;
                fs::rename(stage.path(), dest)?;
                File::open(queue)?.sync_all()?;
            }
            (snapshot.open()?, snapshot.commit)
        };
        let bytes = archive.metadata()?.len();
        rt.block_on(async {
            send(
                &mut socket,
                Request::Archive {
                    bytes,
                    commit: commit.clone(),
                },
            )
            .await?;
            let mut file = tokio::fs::File::from_std(archive);
            let mut buffer = vec![0; CHUNK];
            let mut digest = Sha256::new();
            loop {
                check_cancelled()?;
                let count = file.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
                socket
                    .send(Message::binary(buffer[..count].to_vec()))
                    .await?;
            }
            let sha256 = format!("{:x}", digest.finalize());
            send(
                &mut socket,
                Request::Finish {
                    sha256: sha256.clone(),
                },
            )
            .await?;
            match receive(&mut socket).await? {
                Reply::Installed(delivery)
                    if delivery.recovered
                        || (delivery.receipt.commit == commit
                            && delivery.receipt.archive_bytes == bytes
                            && delivery.receipt.sha256 == sha256) =>
                {
                    Ok(delivery)
                }
                _ => Err(io::Error::other(
                    "The worker did not confirm this checkout.",
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paired_hosts;
    use std::{env, fs, path::PathBuf};
    #[test]
    #[ignore = "controlled by the disposable paired-host fixture"]
    fn interrupted_checkout() {
        let state = PathBuf::from(env::var_os("OX_TEST_REMOTE_DIR").unwrap());
        let source = fs::read_to_string(state.join("host-id")).unwrap();
        let hosts = paired_hosts::worker_links(&state.join("worker-links.json")).unwrap();
        let req = Checkout {
            source: source.trim().into(),
            target: hosts[0].id.clone(),
            repository: repository_id(&state.join("inventory-source/ox")).unwrap(),
            job: 44,
            request_id: "ddcba183-5073-4e9a-9825-e8730784ce5f".into(),
        };
        let client = hosts[0].client().unwrap();
        client::runtime().unwrap().block_on(async {
            let (mut socket, existing) = connect(client, &req).await.unwrap();
            assert!(existing.is_none());
            send(
                &mut socket,
                Request::Archive {
                    bytes: CHUNK as u64,
                    commit: "a".repeat(40),
                },
            )
            .await
            .unwrap();
            socket.send(Message::binary(vec![1; 100])).await.unwrap();
            fs::write(state.join("upload-started"), []).unwrap();
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(10), receive(&mut socket))
                    .await
                    .unwrap();
            assert!(
                result.is_err(),
                "disabled link should close an unfinished upload"
            );
        });
    }
}

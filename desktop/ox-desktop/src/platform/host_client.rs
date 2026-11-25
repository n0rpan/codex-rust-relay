//! The desktop owns only this subscription. Dropping it closes its socket and
//! leaves the daemon, native runtimes and outstanding requests alive.
use ox_desk_core::{
    actions::Action,
    host_protocol::{Client, ProcessView, Scope, Server, VERSION},
    links::LinkEvent,
    store::{Profile, Route, Store, ThreadRow, replica},
};
use ox_host_service::{Paths, ensure_running};
use std::{
    collections::HashSet,
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
const MAX_FRAME: u64 = 64 * 1024 * 1024;

fn write_frame(out: &mut impl Write, request: &Client) -> io::Result<()> {
    serde_json::to_writer(&mut *out, request)?;
    out.write_all(b"\n")?;
    out.flush()
}
fn read_frame(input: &mut BufReader<UnixStream>) -> io::Result<Server> {
    let mut bytes = Vec::new();
    let n = input.take(MAX_FRAME).read_until(b'\n', &mut bytes)?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Session host disconnected",
        ));
    }
    if bytes.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Incomplete or oversized host frame",
        ));
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn connect() -> io::Result<(BufReader<UnixStream>, UnixStream)> {
    handshake().map(|(input, socket, _)| (input, socket))
}
fn handshake() -> io::Result<(BufReader<UnixStream>, UnixStream, Vec<String>)> {
    let paths = Paths::discover()?;
    ensure_running(&paths)?;
    let mut socket = UnixStream::connect(&paths.socket)?;
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    write_frame(&mut socket, &Client::Hello { version: VERSION })?;
    let mut input = BufReader::new(socket.try_clone()?);
    let caps = match read_frame(&mut input)? {
        Server::Hello {
            version: VERSION,
            capabilities,
            ..
        } => capabilities,
        Server::Hello { version, .. } => {
            return Err(io::Error::other(
                ox_desk_core::host_protocol::version_mismatch(version, VERSION),
            ));
        }
        Server::Error { message, .. } => return Err(io::Error::other(message)),
        _ => {
            return Err(io::Error::other(
                "The installed OX host uses an incompatible protocol",
            ));
        }
    };
    socket.set_read_timeout(None)?;
    Ok((input, socket, caps))
}

pub(crate) fn open_store(
    project: PathBuf,
    route: Route,
    receive: impl Fn(LinkEvent) + Send + Sync + 'static,
) -> io::Result<Store> {
    let project = project.canonicalize()?;
    let scope = Scope {
        project: project.clone(),
        route: route.clone(),
    };
    let (mut input, mut socket) = connect()?;
    write_frame(
        &mut socket,
        &Client::Subscribe {
            scope: Some(scope.clone()),
            after: None,
        },
    )?;
    let client = getrandom::u64().map_err(io::Error::other)?;
    let (tx, rx) = mpsc::sync_channel::<(String, Action, Option<String>)>(256);
    let receive = Arc::new(receive);
    let writer_scope = scope.clone();
    let fail = receive.clone();
    thread::Builder::new()
        .name("ox-host-write".into())
        .spawn(move || {
            let mut out = BufWriter::new(socket);
            for (id, action, approval) in rx {
                if let Err(e) = write_frame(
                    &mut out,
                    &Client::Act {
                        id,
                        scope: writer_scope.clone(),
                        action,
                        approval,
                    },
                ) {
                    fail(LinkEvent::Closed {
                        reason: e.to_string(),
                        stderr_tail: vec![],
                    });
                    break;
                }
            }
            let _ = out.get_ref().shutdown(Shutdown::Both);
        })?;
    thread::Builder::new()
        .name("ox-host-read".into())
        .spawn(move || {
            loop {
                match read_frame(&mut input) {
                    Ok(frame) => receive(LinkEvent::Host(Box::new(frame))),
                    Err(error) => {
                        receive(LinkEvent::Closed {
                            reason: error.to_string(),
                            stderr_tail: vec![],
                        });
                        break;
                    }
                }
            }
        })?;
    let seq = AtomicU64::new(0);
    Ok(Store::remote_client(
        Profile {
            project,
            route,
            provider: String::new(),
        },
        move |action, approval| {
            let id = format!(
                "desktop-{client:016x}-{}",
                seq.fetch_add(1, Ordering::Relaxed)
            );
            tx.try_send((id.clone(), action, approval))
                .map_err(|e| io::Error::other(e.to_string()))?;
            Ok(id)
        },
    ))
}
pub(crate) type CatalogResult = (Vec<(Scope, ThreadRow)>, Vec<String>);
pub(crate) fn discover() -> io::Result<CatalogResult> {
    let (mut input, mut socket) = connect()?;
    write_frame(
        &mut socket,
        &Client::Discover {
            id: "catalog".into(),
        },
    )?;
    let mut rows = Vec::new();
    loop {
        match read_frame(&mut input)? {
            Server::Discovered {
                id,
                scope,
                rows: page,
            } if id == "catalog" => {
                rows.extend(
                    page.iter()
                        .filter_map(replica::row)
                        .map(|row| (scope.clone(), row)),
                );
            }
            Server::DiscoveredEnd { id, errors } if id == "catalog" => return Ok((rows, errors)),
            Server::Error { message, .. } => return Err(io::Error::other(message)),
            _ => (),
        }
    }
}

/// The live processes the host runs, newest first. A one-shot request on its own socket.
pub(crate) fn list_processes() -> io::Result<Vec<ProcessView>> {
    let (mut input, mut socket) = connect()?;
    write_frame(
        &mut socket,
        &Client::ListProcesses {
            id: "processes".into(),
        },
    )?;
    loop {
        match read_frame(&mut input)? {
            Server::Processes { id, processes } if id == "processes" => return Ok(processes),
            Server::Error { id, message } if id.as_deref().is_none_or(|i| i == "processes") => {
                return Err(io::Error::other(message));
            }
            _ => (),
        }
    }
}

/// End the live process of each thread in `scope`. Acts need a subscription to the
/// scope on the same socket, so this subscribes first, then waits for every reply.
pub(crate) fn end_threads(scope: Scope, threads: Vec<String>) -> io::Result<()> {
    if threads.is_empty() {
        return Ok(());
    }
    let (mut input, mut socket) = connect()?;
    write_frame(
        &mut socket,
        &Client::Subscribe {
            scope: Some(scope.clone()),
            after: None,
        },
    )?;
    let nonce = getrandom::u64().map_err(io::Error::other)?;
    let mut waiting = HashSet::new();
    for (i, thread) in threads.into_iter().enumerate() {
        let id = format!("end-{nonce:016x}-{i}");
        write_frame(
            &mut socket,
            &Client::Act {
                id: id.clone(),
                scope: scope.clone(),
                action: Action::EndSession(thread),
                approval: None,
            },
        )?;
        waiting.insert(id);
    }
    let mut failure = None;
    while !waiting.is_empty() {
        match read_frame(&mut input)? {
            Server::Ack { id } => {
                waiting.remove(&id);
            }
            Server::Error {
                id: Some(id),
                message,
            } if waiting.remove(&id) => {
                failure.get_or_insert(message);
            }
            Server::Error { id: None, message } => return Err(io::Error::other(message)),
            _ => (),
        }
    }
    failure.map_or(Ok(()), |message| Err(io::Error::other(message)))
}

mod catalog;
pub(crate) use catalog::{CatalogEvent, CatalogWatch, watch_catalog};

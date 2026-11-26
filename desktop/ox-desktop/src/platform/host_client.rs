//! The desktop owns only this subscription. Dropping it closes its socket and
//! leaves the daemon, native runtimes and outstanding requests alive.
use ox_desk_core::{
    actions::Action,
    host_protocol::{Client, ProcessView, Scope, Server, VERSION},
    links::LinkEvent,
    store::{Profile, Route, Store, ThreadRow, replica},
};
use ox_host_service::{Paths, ensure_running, is_running};
use std::{
    collections::HashSet,
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc, Mutex,
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
/// The socket owns the reservation. Passing it to the installer keeps the host
/// gated even if the window closes while the package is switching.
pub(crate) fn reserve_idle_update() -> io::Result<Option<UnixStream>> {
    let (mut input, mut socket, caps) = handshake()?;
    if !caps
        .iter()
        .any(|c| c == ox_desk_core::host_protocol::IDLE_UPDATE)
    {
        return Err(io::Error::other(
            "This session host cannot confirm when agents are idle. Choose Restart to update when you’re ready.",
        ));
    }
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    write_frame(&mut socket, &Client::ReserveIdleUpdate)?;
    match read_frame(&mut input)? {
        Server::IdleUpdate { ready } => Ok(ready.then_some(socket)),
        Server::Error { message, .. } => Err(io::Error::other(message)),
        _ => Err(io::Error::other(
            "The session host could not confirm whether agents are idle.",
        )),
    }
}
/// Shown when the host is not up. Polls treat it as a quiet state, not an error.
pub(crate) const HOST_NOT_RUNNING: &str = "Session host not running";

fn handshake() -> io::Result<(BufReader<UnixStream>, UnixStream, Vec<String>)> {
    let paths = Paths::discover()?;
    ensure_running(&paths)?;
    open_handshake(&paths)
}
/// Connects to a host that is already up and never starts one, so a background poll
/// cannot revive a host the user stopped.
fn connect_existing(paths: &Paths) -> io::Result<BufReader<UnixStream>> {
    if !is_running(paths)? {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            HOST_NOT_RUNNING,
        ));
    }
    open_handshake(paths).map(|(input, _, _)| input)
}
fn open_handshake(paths: &Paths) -> io::Result<(BufReader<UnixStream>, UnixStream, Vec<String>)> {
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
    declare_surface(&mut socket, &caps)?;
    Ok((input, socket, caps))
}
/// Tell a host that negotiates it that a person on OX Desktop uses this connection. An
/// older host rejects the frame, so it is sent only when the hello lists the capability.
fn declare_surface(out: &mut impl Write, caps: &[String]) -> io::Result<()> {
    if caps.iter().any(|c| c == ox_desk_core::surface::CAPABILITY) {
        write_frame(
            out,
            &Client::Surface {
                surface: ox_desk_core::surface::Surface::Desktop,
            },
        )?;
    }
    Ok(())
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
            terminal: true,
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

/// A sidebar reuses its process-list connection. Each new protocol handshake reads
/// the operator's route configuration, which an ordinary process poll does not need.
#[derive(Default)]
pub(crate) struct ProcessMonitor {
    socket: Mutex<Option<BufReader<UnixStream>>>,
}

impl ProcessMonitor {
    pub(crate) fn list(&self) -> io::Result<Vec<ProcessView>> {
        self.list_at(&Paths::discover()?)
    }

    fn list_at(&self, paths: &Paths) -> io::Result<Vec<ProcessView>> {
        let mut socket = self.socket.lock().unwrap();
        if let Some(mut connection) = socket.take()
            && let Ok(rows) = read_processes(&mut connection)
        {
            *socket = Some(connection);
            return Ok(rows);
        }
        // A host restart closes the old stream. Reconnect only if a host is running.
        let mut connection = connect_existing(paths)?;
        connection
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(15)))?;
        let rows = read_processes(&mut connection)?;
        *socket = Some(connection);
        Ok(rows)
    }
}

fn read_processes(input: &mut BufReader<UnixStream>) -> io::Result<Vec<ProcessView>> {
    write_frame(
        input.get_mut(),
        &Client::ListProcesses {
            id: "processes".into(),
        },
    )?;
    loop {
        match read_frame(input)? {
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
pub(crate) fn end_threads(scope: Scope, threads: Vec<String>, force: bool) -> io::Result<()> {
    if threads.is_empty() {
        return Ok(());
    }
    let (mut input, mut socket) = connect()?;
    write_frame(
        &mut socket,
        &Client::Subscribe {
            scope: Some(scope.clone()),
            after: None,
            terminal: false,
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
                action: if force {
                    Action::ForceEndSession(thread)
                } else {
                    Action::EndSession(thread)
                },
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

#[cfg(test)]
mod process_monitor_tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn the_desktop_declares_itself_only_to_a_host_that_negotiates_it() {
        let mut out = Vec::new();
        declare_surface(&mut out, &["snapshot".into()]).unwrap();
        assert!(out.is_empty(), "an older host would reject the frame");
        declare_surface(&mut out, &[ox_desk_core::surface::CAPABILITY.into()]).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"type\":\"surface\",\"surface\":\"desktop\"}\n"
        );
    }

    #[test]
    fn polls_reuse_the_handshake_and_reconnect_after_host_exit() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            socket: root.path().join("host.sock"),
            state_dir: root.path().to_owned(),
        };
        let monitor = ProcessMonitor::default();
        let start = |polls| {
            let listener = UnixListener::bind(&paths.socket).unwrap();
            thread::spawn(move || {
                // connect_existing checks that the socket belongs to a live host.
                drop(listener.accept().unwrap());
                let (mut out, _) = listener.accept().unwrap();
                out.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut input = BufReader::new(out.try_clone().unwrap());
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                assert!(matches!(
                    serde_json::from_str(&line),
                    Ok(Client::Hello { version: VERSION })
                ));
                writeln!(
                    out,
                    r#"{{"type":"hello","version":1,"capabilities":[],"routes":[],"projects":[]}}"#
                )
                .unwrap();
                for _ in 0..polls {
                    line.clear();
                    input.read_line(&mut line).unwrap();
                    assert!(matches!(
                        serde_json::from_str(&line),
                        Ok(Client::ListProcesses { .. })
                    ));
                    writeln!(
                        out,
                        r#"{{"type":"processes","id":"processes","processes":[]}}"#
                    )
                    .unwrap();
                }
            })
        };
        let first = start(2);
        assert!(monitor.list_at(&paths).unwrap().is_empty());
        assert!(monitor.list_at(&paths).unwrap().is_empty());
        first.join().unwrap();
        std::fs::remove_file(&paths.socket).unwrap();
        assert_eq!(
            monitor.list_at(&paths).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        let next = start(1);
        assert!(monitor.list_at(&paths).unwrap().is_empty());
        next.join().unwrap();
    }
}

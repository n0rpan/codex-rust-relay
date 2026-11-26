//! The desktop owns only this subscription. Dropping it closes its socket and
//! leaves the daemon, native runtimes and outstanding requests alive.
pub(super) use ox_desk_core::host_protocol::{read_frame, write_frame};
use ox_desk_core::{
    actions::Action,
    host_protocol::{Client, ProcessView, Scope, Server, VERSION},
    links::LinkEvent,
    store::{Profile, Route, Store, ThreadRow, replica},
};
use ox_host_service::{Paths, ensure_running, is_running};
#[cfg(test)]
use std::io::BufRead as _;
use std::{
    collections::HashSet,
    io::{self, BufReader, BufWriter, Write},
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
#[derive(Clone, Default)]
pub(crate) enum HostLink {
    #[default]
    Local,
    Remote(super::hosts::SavedHost),
}
type StreamHello = (BufReader<UnixStream>, UnixStream, Vec<String>);

impl HostLink {
    pub fn remote(&self) -> Option<&super::hosts::SavedHost> {
        match self {
            Self::Local => None,
            Self::Remote(host) => Some(host),
        }
    }
    pub fn open_shell(&self, project: PathBuf, cx: &mut gpui::App) -> Result<(), String> {
        match self {
            Self::Remote(host) => crate::terminal::open_remote(
                host.clone(),
                ox_host_web::terminal_protocol::OpenTerminal::Shell { project },
                cx,
            ),
            Self::Local => crate::terminal::open(project, None, |_, _| {}, cx),
        }
    }
    pub fn settings(&self) -> Result<crate::settings::Settings, String> {
        self.remote()
            .map_or_else(crate::settings::load, |h| h.snapshot())
    }
    pub fn models(&self) -> Result<Vec<String>, String> {
        self.remote()
            .map_or_else(crate::settings::discover_models, |h| h.models())
    }
    pub fn request<T: serde::de::DeserializeOwned>(
        &self,
        request: crate::settings::protocol::Request,
    ) -> Result<T, String> {
        match self.remote() {
            Some(host) => serde_json::from_value(
                host.settings(serde_json::to_value(request).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string()),
            None => crate::settings::request(request),
        }
    }
    pub fn apply(
        &self,
        request: &crate::settings::protocol::Request,
    ) -> Result<Result<crate::settings::Settings, String>, String> {
        use crate::settings::model::Outcome;
        let Some(host) = self.remote() else {
            return crate::settings::apply(request);
        };
        let reply = host
            .settings(serde_json::to_value(request).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        match serde_json::from_value(reply).map_err(|e| e.to_string())? {
            Outcome::Applied => self.settings().map(Ok),
            Outcome::ChangedOnDisk { revision } => Ok(Err(revision)),
        }
    }
    pub fn state_file(&self, file: &str) -> String {
        match self.remote() {
            Some(host) => format!("{}-{file}", host.id),
            None => file.to_owned(),
        }
    }
    pub(crate) fn worker_hosts(&self, project: PathBuf) -> io::Result<Option<serde_json::Value>> {
        let (mut input, mut socket, caps) = self.handshake()?;
        if !caps.iter().any(|c| c == "worker_placement") {
            return Ok(None);
        }
        socket.set_read_timeout(Some(Duration::from_secs(20)))?;
        write_frame(
            &mut socket,
            &Client::WorkerHosts {
                id: "sheet".into(),
                project,
            },
        )?;
        match read_frame(&mut input)? {
            Server::WorkerReply {
                result: Some(value),
                ..
            } => Ok(Some(value)),
            Server::WorkerReply {
                error: Some(error), ..
            }
            | Server::Error { message: error, .. } => Err(io::Error::other(error)),
            _ => Err(io::Error::other(
                "Could not read this project's worker hosts.",
            )),
        }
    }
    fn handshake(&self) -> io::Result<StreamHello> {
        match self {
            Self::Local => {
                let paths = Paths::discover()?;
                ensure_running(&paths)?;
                open_handshake(&paths)
            }
            Self::Remote(host) => handshake_stream(host.connect()?, Some(&host.id)),
        }
    }
}
pub(crate) enum IdleReservation {
    Running(UnixStream),
    Stopped(ox_host_service::HostLock),
}

/// Probe the package lock first so normal agent requests continue throughout a
/// background download. The installer rechecks it before committing the switch.
pub(crate) fn reserve_idle_update(store: &std::path::Path) -> io::Result<Option<IdleReservation>> {
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.join("update.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    let paths = Paths::discover()?;
    if !is_running(&paths)? {
        return Ok(ox_host_service::reserve_stopped_host(&paths)?.map(IdleReservation::Stopped));
    }
    let (mut input, mut socket, caps) = open_handshake(&paths)?;
    if !caps
        .iter()
        .any(|c| c == ox_desk_core::host_protocol::IDLE_UPDATE)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "This session host cannot confirm when agents are idle. Choose Restart to update when you’re ready.",
        ));
    }
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    write_frame(&mut socket, &Client::ReserveIdleUpdate)?;
    match read_frame(&mut input)? {
        Server::IdleUpdate { ready } => Ok(ready.then_some(IdleReservation::Running(socket))),
        Server::Error { message, .. } => Err(io::Error::other(message)),
        _ => Err(io::Error::other(
            "The session host could not confirm whether agents are idle.",
        )),
    }
}
/// Shown when the host is not up. Polls treat it as a quiet state, not an error.
pub(crate) const HOST_NOT_RUNNING: &str = "Session host not running";

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
    handshake_stream(UnixStream::connect(&paths.socket)?, None)
}
fn handshake_stream(mut socket: UnixStream, remote_id: Option<&str>) -> io::Result<StreamHello> {
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    write_frame(&mut socket, &Client::Hello { version: VERSION })?;
    let mut input = BufReader::new(socket.try_clone()?);
    let caps = match read_frame(&mut input)? {
        Server::Hello {
            version,
            capabilities,
            host,
            ..
        } if ox_desk_core::host_protocol::supported(version) => {
            if let Some(id) = remote_id
                && host.as_ref().and_then(|h| h.id.as_deref()) != Some(id)
            {
                return Err(io::Error::other(
                    "Host identity changed. Pair this host again.",
                ));
            }
            capabilities
        }
        Server::Hello { version, .. } => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                ox_desk_core::host_protocol::version_mismatch(version, VERSION),
            ));
        }
        Server::Error { message, .. } => return Err(io::Error::other(message)),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The installed OX host uses an incompatible protocol",
            ));
        }
    };
    socket.set_read_timeout(None)?;
    if remote_id.is_none() {
        declare_surface(&mut socket, &caps)?;
    }
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

#[cfg(test)]
pub(crate) fn open_store(
    project: PathBuf,
    route: Route,
    receive: impl Fn(LinkEvent) + Send + Sync + 'static,
) -> io::Result<Store> {
    HostLink::Local.open_store(project, route, receive)
}
impl HostLink {
    pub(crate) fn open_store(
        &self,
        project: PathBuf,
        route: Route,
        receive: impl Fn(LinkEvent) + Send + Sync + 'static,
    ) -> io::Result<Store> {
        let project = match self {
            Self::Local => project.canonicalize()?,
            Self::Remote(host) if project.is_absolute() => host.choose(&project, &route)?,
            Self::Remote(_) => {
                return Err(io::Error::other(
                    "Select an absolute project path on the host.",
                ));
            }
        };
        let scope = Scope {
            project: project.clone(),
            route: route.clone(),
        };
        let (mut input, mut socket, caps) = self.handshake()?;
        let placement = caps.iter().any(|c| c == "worker_placement");
        write_frame(
            &mut socket,
            &Client::Subscribe {
                scope: Some(scope.clone()),
                after: None,
                terminal: self.remote().is_none(),
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
                for (id, mut action, approval) in rx {
                    if !placement
                        && let Action::StartWorker(s) = &mut action
                        && s.host.as_deref() == Some("parent")
                        && s.needs.is_none()
                    {
                        s.host = None;
                    }
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
        let remote = self.remote().is_some();
        Ok(Store::remote_client(
            Profile {
                project,
                route,
                provider: String::new(),
            },
            move |action, approval| {
                // The native path revalidates slash commands on the host and cannot
                // fall through to a local terminal handoff.
                let action = match action {
                    Action::PaletteCommand(owner, text) if remote => {
                        Action::NativeCommand(owner, text)
                    }
                    other => other,
                };
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
}
pub(crate) type CatalogResult = (Vec<(Scope, ThreadRow)>, Vec<String>);
impl HostLink {
    pub(crate) fn discover(&self) -> io::Result<CatalogResult> {
        let (mut input, mut socket, _) = self.handshake()?;
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
                Server::DiscoveredEnd { id, errors } if id == "catalog" => {
                    return Ok((rows, errors));
                }
                Server::Error { message, .. } => return Err(io::Error::other(message)),
                _ => (),
            }
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
    pub(crate) fn list(&self, link: &HostLink) -> io::Result<Vec<ProcessView>> {
        self.poll(|| match link {
            HostLink::Local => connect_existing(&Paths::discover()?),
            HostLink::Remote(_) => link.handshake().map(|(input, _, _)| input),
        })
    }

    #[cfg(test)]
    fn list_at(&self, paths: &Paths) -> io::Result<Vec<ProcessView>> {
        self.poll(|| connect_existing(paths))
    }
    fn poll(
        &self,
        connect: impl FnOnce() -> io::Result<BufReader<UnixStream>>,
    ) -> io::Result<Vec<ProcessView>> {
        let mut socket = self.socket.lock().unwrap();
        if let Some(mut connection) = socket.take()
            && let Ok(rows) = read_processes(&mut connection)
        {
            *socket = Some(connection);
            return Ok(rows);
        }
        // A host restart closes the old stream. Reconnect only if a host is running.
        let mut connection = connect()?;
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
impl HostLink {
    pub(crate) fn end_threads(
        &self,
        scope: Scope,
        threads: Vec<String>,
        force: bool,
    ) -> io::Result<()> {
        if threads.is_empty() {
            return Ok(());
        }
        let (mut input, mut socket, _) = self.handshake()?;
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
}
mod catalog;
pub(crate) use catalog::{CatalogEvent, CatalogWatch};

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
    fn adjacent_releases_exchange_hello_then_session_frames_on_the_selected_protocol() {
        // The previous release used v1 and returned no stable ID. A future release
        // selects our maximum and may advertise new capabilities and metadata.
        let hosts = [
            (
                None,
                serde_json::json!({"version":"0.1", "epoch":"old"}),
                vec!["snapshot"],
            ),
            (
                Some("stable-id"),
                serde_json::json!({"version":"next", "epoch":"new", "id":"stable-id", "load":0.5}),
                vec!["snapshot", "future_feature"],
            ),
        ];
        for (id, info, caps) in hosts {
            let (client, mut host) = UnixStream::pair().unwrap();
            let peer = thread::spawn(move || {
                let mut input = BufReader::new(host.try_clone().unwrap());
                let read = |input: &mut BufReader<UnixStream>| {
                    let mut line = String::new();
                    input.read_line(&mut line).unwrap();
                    serde_json::from_str::<Client>(&line).unwrap()
                };
                let Client::Hello { version } = read(&mut input) else {
                    panic!("expected hello")
                };
                let maximum = if id.is_some() { VERSION + 1 } else { 1 };
                writeln!(
                    host,
                    "{}",
                    serde_json::json!({"type":"hello", "version":maximum.min(version),
                    "routes":[], "projects":[], "capabilities":caps,"host":info})
                )
                .unwrap();
                assert!(matches!(
                    read(&mut input),
                    Client::Subscribe {
                        scope: None,
                        terminal: false,
                        ..
                    }
                ));
                writeln!(host, "{}", serde_json::json!({"type":"snapshot","cursor":{"epoch":"e","seq":0},"scopes":[]})).unwrap();
            });
            let (mut input, mut out, _) = handshake_stream(client, id).unwrap();
            write_frame(
                &mut out,
                &Client::Subscribe {
                    scope: None,
                    after: None,
                    terminal: false,
                },
            )
            .unwrap();
            assert!(matches!(
                read_frame(&mut input).unwrap(),
                Server::Snapshot { .. }
            ));
            peer.join().unwrap();
        }
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

//! A slow subscriber can reconnect from its cursor. It never blocks another client or a runtime.
use crate::engine::Message;
use ox_desk_core::host_protocol::{Client, Server, supports_route};
use std::{
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    net::Shutdown,
    os::unix::{io::AsRawFd, net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::Duration,
};

pub(crate) type Packet = Arc<[u8]>;
const MAX_FRAME: usize = 64 * 1024 * 1024;
const QUEUE_BYTES: usize = 16 * 1024 * 1024;
const QUEUE_FRAMES: usize = 256;
pub(crate) fn encode(frame: &Server) -> io::Result<Packet> {
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    Ok(bytes.into())
}

pub(crate) fn encode_for(mut frame: Server, version: u32) -> io::Result<Option<Packet>> {
    match &mut frame {
        Server::Hello { routes, .. } => routes.retain(|route| supports_route(version, route)),
        Server::Snapshot { scopes, .. } => {
            scopes.retain(|state| supports_route(version, &state.scope.route));
        }
        Server::DiscoveredEnd {
            directory: Some(directory),
            ..
        } => {
            directory
                .routes
                .retain(|route| supports_route(version, route));
        }
        Server::Processes { processes, .. } => {
            processes.retain(|process| supports_route(version, &process.scope.route));
        }
        Server::Opened { scope, .. }
        | Server::Discovered { scope, .. }
        | Server::Delta { scope, .. }
        | Server::TerminalPreview { scope, .. }
        | Server::Handoff { scope, .. }
            if !supports_route(version, &scope.route) =>
        {
            return Ok(None);
        }
        _ => (),
    }
    encode(&frame).map(Some)
}

pub(crate) struct Peer {
    writer: SyncSender<Packet>,
    queued: Arc<AtomicUsize>,
    stop: UnixStream,
}
impl Peer {
    pub(crate) fn send(&self, packet: Packet) -> bool {
        self.send_for(packet, "reply")
    }
    /// Queue a packet; a subscriber too slow to take it is cut off and told to
    /// reconnect from its cursor. The eviction goes to the host error log
    /// (shown by `ox host doctor`), so a desktop that reloads its catalog can
    /// be traced to it.
    pub(crate) fn send_for(&self, packet: Packet, what: &str) -> bool {
        let n = packet.len();
        let old = self.queued.fetch_add(n, Ordering::Relaxed);
        let over_bytes = old != 0 && old.saturating_add(n) > QUEUE_BYTES;
        if !over_bytes && self.writer.try_send(packet).is_ok() {
            return true;
        }
        self.queued.fetch_sub(n, Ordering::Relaxed);
        let _ = self.stop.shutdown(Shutdown::Both);
        let reason = if over_bytes {
            format!("queue over {} MiB", QUEUE_BYTES >> 20)
        } else {
            format!("queue over {QUEUE_FRAMES} frames")
        };
        ox_host_service::errlog::record(&format!(
            "evicted a slow subscriber ({what}): {reason}, {old} bytes queued; it must reconnect"
        ));
        false
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.stop.shutdown(Shutdown::Both);
    }
}

pub(crate) fn same_user(stream: &UnixStream) -> io::Result<()> {
    let uid;
    #[cfg(target_os = "linux")]
    {
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut peer as *mut libc::ucred).cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        uid = peer.uid;
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (mut user, mut group) = (0, 0);
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut user, &mut group) } != 0 {
            return Err(io::Error::last_os_error());
        }
        uid = user;
    }
    if uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Host peer is not the current user",
        ));
    }
    Ok(())
}

pub(crate) fn attach(stream: UnixStream, id: u64, events: SyncSender<Message>) -> io::Result<()> {
    same_user(&stream)?;
    let read = stream.try_clone()?;
    let write = stream.try_clone()?;
    let (writer, rx) = mpsc::sync_channel::<Packet>(QUEUE_FRAMES);
    let queued = Arc::new(AtomicUsize::new(0));
    let backlog = queued.clone();
    events
        .send(Message::Connect(
            id,
            Peer {
                writer,
                queued,
                stop: stream,
            },
        ))
        .map_err(|_| io::Error::other("Host stopped"))?;
    let lost = events.clone();
    thread::spawn(move || {
        let mut output = BufWriter::new(write);
        for data in rx {
            if output
                .write_all(&data)
                .and_then(|_| output.flush())
                .is_err()
            {
                break;
            }
            backlog.fetch_sub(data.len(), Ordering::Relaxed);
        }
        let _ = lost.send(Message::Disconnect(id));
    });
    thread::spawn(move || {
        let _ = read.set_read_timeout(Some(Duration::from_secs(10)));
        let mut input = BufReader::new(read);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match Read::by_ref(&mut input)
                .take((MAX_FRAME + 1) as u64)
                .read_until(b'\n', &mut bytes)
            {
                Ok(0) | Err(_) => break,
                Ok(n) if n > MAX_FRAME || !bytes.ends_with(b"\n") => break,
                _ => (),
            }
            let Ok(frame) = serde_json::from_slice::<Client>(&bytes) else {
                break;
            };
            if events.send(Message::Request(id, Box::new(frame))).is_err() {
                break;
            }
            let _ = input.get_ref().set_read_timeout(None);
        }
        let _ = events.send(Message::Disconnect(id));
    });
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_peer() -> (Peer, mpsc::Receiver<Packet>) {
    let (writer, rx) = mpsc::sync_channel(256);
    let (stop, _) = UnixStream::pair().expect("socket pair");
    (
        Peer {
            writer,
            queued: Arc::new(AtomicUsize::new(0)),
            stop,
        },
        rx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::{
        host_protocol::{Cursor, ProjectDirectory, Scope, ScopeState},
        runtime::{Route, Runtime},
    };

    #[test]
    fn a_subscriber_that_never_reads_is_evicted_at_the_frame_limit() {
        let (peer, _frames) = test_peer();
        let packet: Packet = vec![b'x'; 8].into();
        for _ in 0..QUEUE_FRAMES {
            assert!(peer.send_for(packet.clone(), "test"));
        }
        assert!(!peer.send_for(packet, "test"), "frame 257 evicts");
    }

    #[test]
    fn v1_replies_omit_antigravity_routes_and_scopes() {
        let codex = Route::CODEX_SUBSCRIPTION;
        let antigravity = Runtime::Antigravity.route(None);
        let cursor = Cursor {
            epoch: "boot".into(),
            seq: 0,
        };
        let scope = |route| Scope {
            project: "/project".into(),
            route,
        };
        let state = |route| ScopeState {
            scope: scope(route),
            connection: "ready".into(),
            capabilities: vec![],
            sessions: vec![],
            asks: vec![],
            transcripts: Default::default(),
            detail: serde_json::Value::Null,
        };
        let routes = vec![codex.clone(), antigravity.clone()];
        let reply = |frame, version| {
            let packet = encode_for(frame, version).unwrap().unwrap();
            serde_json::from_slice::<Server>(&packet).unwrap()
        };

        let hello = Server::Hello {
            version: 1,
            capabilities: vec![],
            routes: routes.clone(),
            projects: vec![],
            host: None,
        };
        assert!(
            matches!(reply(hello, 1), Server::Hello { routes, .. } if routes == [codex.clone()])
        );

        let snapshot = Server::Snapshot {
            cursor: cursor.clone(),
            scopes: routes.iter().cloned().map(state).collect(),
        };
        assert!(
            matches!(reply(snapshot.clone(), 1), Server::Snapshot { scopes, .. }
            if scopes.len() == 1 && scopes[0].scope.route == codex)
        );
        assert!(matches!(reply(snapshot, 2), Server::Snapshot { scopes, .. }
            if scopes.len() == 2 && scopes[1].scope.route == antigravity));

        let directory = Server::DiscoveredEnd {
            id: "directory".into(),
            errors: vec![],
            directory: Some(ProjectDirectory {
                checkouts: vec![],
                routes,
            }),
        };
        assert!(
            matches!(reply(directory, 1), Server::DiscoveredEnd { directory: Some(directory), .. }
            if directory.routes == [codex])
        );

        let delta = Server::Delta {
            cursor,
            scope: scope(antigravity),
            changes: vec![],
        };
        assert!(encode_for(delta.clone(), 1).unwrap().is_none());
        assert!(encode_for(delta, 2).unwrap().is_some());
    }
}

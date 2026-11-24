//! A slow subscriber can reconnect from its cursor. It never blocks another client or a runtime.
use crate::engine::Message;
use ox_desk_core::host_protocol::{Client, Server};
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
pub(crate) fn encode(frame: &Server) -> io::Result<Packet> {
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    Ok(bytes.into())
}

pub(crate) struct Peer {
    writer: SyncSender<Packet>,
    queued: Arc<AtomicUsize>,
    stop: UnixStream,
}
impl Peer {
    pub(crate) fn send(&self, packet: Packet) -> bool {
        let n = packet.len();
        let old = self.queued.fetch_add(n, Ordering::Relaxed);
        if (old == 0 || old.saturating_add(n) <= QUEUE_BYTES)
            && self.writer.try_send(packet).is_ok()
        {
            return true;
        }
        self.queued.fetch_sub(n, Ordering::Relaxed);
        let _ = self.stop.shutdown(Shutdown::Both);
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
    let (writer, rx) = mpsc::sync_channel::<Packet>(256);
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

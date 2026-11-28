//! The explicitly enabled performance probe can read counters while a window is hidden.
//! Waiting for the client and sending its response both happen off the GPUI thread.
use std::{
    env, fs, io,
    os::unix::{fs::PermissionsExt, net::UnixDatagram},
    path::PathBuf,
    sync::mpsc,
    time::Duration,
};

/// In-process input for tests and dogfood reviews: no OS pointer or keyboard involved.
pub enum Input {
    /// `press <id-or-label>`: click a marked control at its centre.
    Press(String),
    /// `key <keystroke>`: a GPUI keystroke such as `escape` or `ctrl-n`.
    Key(String),
    #[cfg(test)]
    MovePreview(String),
}
pub struct Sample {
    pub input: Option<Input>,
    pub reset: bool,
    /// Redraw every window after replying, so the client can wait for a frame
    /// that reflects the state it just observed (`frame` in each window's entry).
    pub refresh: bool,
    pub reply: mpsc::Sender<String>,
}
pub struct Socket {
    path: PathBuf,
}
impl Socket {
    pub fn bind() -> io::Result<Option<(Self, flume::Receiver<Sample>)>> {
        let Some(path) = env::var_os("OX_DESK_STATS_SOCKET").map(PathBuf::from) else {
            return Ok(None);
        };
        Self::bind_at(path).map(Some)
    }

    fn bind_at(path: PathBuf) -> io::Result<(Self, flume::Receiver<Sample>)> {
        // The probe can press buttons: create a missing parent directory 0700.
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
            && !dir.exists()
        {
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        let socket = UnixDatagram::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        // Darwin defaults to 2 KiB per outgoing Unix datagram. A single
        // window's controls can already take several KiB in a snapshot.
        socket2::SockRef::from(&socket).set_send_buffer_size(1 << 20)?;
        socket.set_write_timeout(Some(Duration::from_millis(100)))?;
        let (send, receive) = flume::unbounded();
        std::thread::spawn(move || {
            let mut command = [0; 256];
            while let Ok((n, address)) = socket.recv_from(&mut command) {
                let Some(client) = address.as_pathname() else {
                    break;
                };
                let text = String::from_utf8_lossy(&command[..n]);
                let (reset, refresh, input) = match &*text {
                    "snapshot" => (false, false, None),
                    "reset" => (true, false, None),
                    "frame" => (false, true, None),
                    other => match other.split_once(' ') {
                        Some(("press", q)) if !q.trim().is_empty() => {
                            (false, true, Some(Input::Press(q.trim().into())))
                        }
                        Some(("key", k)) if !k.trim().is_empty() => {
                            (false, true, Some(Input::Key(k.trim().into())))
                        }
                        #[cfg(test)]
                        Some(("move-preview", phase)) => {
                            (false, true, Some(Input::MovePreview(phase.into())))
                        }
                        _ => continue,
                    },
                };
                let (reply, result) = mpsc::channel();
                if send
                    .send(Sample {
                        input,
                        reset,
                        refresh,
                        reply,
                    })
                    .is_err()
                {
                    break;
                }
                if let Ok(payload) = result.recv()
                    && let Err(e) = socket.send_to(payload.as_bytes(), client)
                {
                    eprintln!(
                        "Could not send {}-byte desk probe reply: {e}",
                        payload.len()
                    );
                }
            }
        });
        Ok((Self { path }, receive))
    }
}
impl Drop for Socket {
    fn drop(&mut self) {
        if let Ok(s) = UnixDatagram::unbound() {
            let _ = s.send_to(b"", &self.path);
        }
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_larger_than_darwins_default_datagram_limit_reach_the_client() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let server_path = dir.path().join("stats");
        let (_server, requests) = Socket::bind_at(server_path.clone()).unwrap();
        let client = UnixDatagram::bind(dir.path().join("client")).unwrap();
        socket2::SockRef::from(&client)
            .set_recv_buffer_size(1 << 20)
            .unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client.send_to(b"snapshot", server_path).unwrap();
        let payload = "x".repeat(16 * 1024);
        let sample = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        sample.reply.send(payload.clone()).unwrap();
        let mut received = vec![0; payload.len()];
        let len = client.recv(&mut received).unwrap();
        assert_eq!(&received[..len], payload.as_bytes());
    }
}

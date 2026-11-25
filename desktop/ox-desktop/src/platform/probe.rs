//! The explicitly enabled performance probe can read counters while a window is hidden.
//! Waiting for the client and sending its response both happen off the GPUI thread.
use std::{
    env, fs, io,
    os::unix::{fs::PermissionsExt, net::UnixDatagram},
    path::PathBuf,
    sync::mpsc,
    time::Duration,
};

pub struct Sample {
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
        let socket = UnixDatagram::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        socket.set_write_timeout(Some(Duration::from_millis(100)))?;
        let (send, receive) = flume::unbounded();
        std::thread::spawn(move || {
            let mut command = [0; 16];
            while let Ok((n, address)) = socket.recv_from(&mut command) {
                let Some(client) = address.as_pathname() else {
                    break;
                };
                let (reset, refresh) = match &command[..n] {
                    b"snapshot" => (false, false),
                    b"reset" => (true, false),
                    b"frame" => (false, true),
                    _ => continue,
                };
                let (reply, result) = mpsc::channel();
                if send
                    .send(Sample {
                        reset,
                        refresh,
                        reply,
                    })
                    .is_err()
                {
                    break;
                }
                if let Ok(payload) = result.recv() {
                    let _ = socket.send_to(payload.as_bytes(), client);
                }
            }
        });
        Ok(Some((Self { path }, receive)))
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

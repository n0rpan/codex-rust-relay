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
        // The probe can press buttons: create a missing parent directory 0700.
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
            && !dir.exists()
        {
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        let socket = UnixDatagram::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
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

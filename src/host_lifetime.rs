//! The host drops the peer socket even on SIGKILL. Each host-owned OX helper
//! is its group's leader and terminates that group if the owner disappears.
use serde_json::{Value, json};
use std::{
    env,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    thread,
    time::Duration,
};
static LEASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
const PARENT_LEASE: &str = "OX_HOST_PARENT_LEASE";
const ENDPOINT: &str = "OX_HOST_LIFETIME_SOCKET";

fn request(addr: &std::ffi::OsStr, body: Value) -> io::Result<UnixStream> {
    let mut socket = UnixStream::connect(Path::new(addr))?;
    socket.set_read_timeout(Some(Duration::from_secs(4)))?;
    socket.set_write_timeout(Some(Duration::from_secs(4)))?;
    let body = serde_json::to_vec(&body)?;
    socket.write_all(&(body.len() as u32).to_be_bytes())?;
    socket.write_all(&body)?;
    let mut ack = [0];
    socket.read_exact(&mut ack)?;
    if ack[0] != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The session host refused this OX helper.",
        ));
    }
    Ok(socket)
}
pub(crate) fn supervise() -> io::Result<()> {
    let Some(addr) = env::var_os(ENDPOINT) else {
        return Ok(());
    };
    if unsafe { libc::getpgrp() != libc::getpid() } {
        return Ok(());
    } // internal probes can share the launch group.
    let id = crate::random_token().map_err(|e| io::Error::other(e.to_string()))?;
    let mut socket = request(
        &addr,
        json!({"type":"lease","id":id,"parent":env::var(PARENT_LEASE).ok()}),
    )?;
    LEASE
        .set(id)
        .map_err(|_| io::Error::other("This helper already has a host lease"))?;
    socket.set_read_timeout(None)?;
    thread::Builder::new()
        .name("ox-host-lifetime".into())
        .spawn(move || {
            let _ = socket.read_exact(&mut [0u8; 1]);
            // Signal this helper's private group. No PID comes from disk or input.
            unsafe {
                libc::kill(0, libc::SIGKILL);
            }
        })?;
    Ok(())
}
pub(crate) fn hosted() -> bool {
    env::var_os(ENDPOINT).is_some()
}

pub(crate) fn isolate_stock(command: &mut tokio::process::Command) {
    for key in [
        ENDPOINT,
        PARENT_LEASE,
        "OX_HOST_WORKER_ROUTE",
        "OX_HOST_WORKER_PARENT",
    ] {
        command.env_remove(key);
    }
}

/// A gateway's way to the session host, if one serves this launch. A helper the
/// host started uses its private lifetime endpoint. Any other helper, such as
/// `ox claude` in a terminal, uses the per-user host socket when a host is running.
pub(crate) enum HostLink {
    Hosted {
        addr: std::ffi::OsString,
        scope: Value,
    },
    External {
        socket: std::path::PathBuf,
    },
}

impl HostLink {
    /// `policy` applies only to a helper the host did not start: a helper the host
    /// runs always hands its workers to that host.
    pub(crate) fn find(
        cwd: &Path,
        policy: crate::provider::WorkersHost,
    ) -> Result<Option<Self>, String> {
        if let Some(addr) = env::var_os(ENDPOINT) {
            if LEASE.get().is_none() {
                return Err("This OX worker gateway is not attached to a live host".into());
            }
            let project = cwd.canonicalize().map_err(|e| e.to_string())?;
            let route: Value = serde_json::from_str(
                &env::var("OX_HOST_WORKER_ROUTE")
                    .map_err(|_| "This helper is not attached to a host billing route")?,
            )
            .map_err(|e| e.to_string())?;
            return Ok(Some(Self::Hosted {
                addr,
                scope: json!({"project": project, "route": route}),
            }));
        }
        if policy == crate::provider::WorkersHost::Never {
            return Ok(None);
        }
        let socket = host_socket();
        Ok(socket
            .filter(|s| UnixStream::connect(s).is_ok())
            .map(|socket| Self::External { socket }))
    }

    /// Send one launch (`call` is None) or parent call and wait for the host's answer.
    /// `thread` is the calling native thread reported by the stock client in the
    /// MCP request metadata. A session-scoped helper's own session takes precedence.
    pub(crate) async fn send(
        self,
        launch: String,
        caller: Value,
        thread: Option<String>,
        start: Option<Value>,
        call: Option<Value>,
    ) -> Result<Value, String> {
        let hint = env::var("OX_HOST_WORKER_PARENT").ok().or(thread);
        let wait = Duration::from_secs(if start.is_some() { 80 } else { 15 });
        tokio::task::spawn_blocking(move || match self {
            Self::Hosted { addr, scope } => {
                let body = match (start, call) {
                    (Some(mut start), _) => {
                        start["parent"] = hint.unwrap_or_default().into();
                        json!({"type":"launch","scope":scope,"launch":launch,"start":start})
                    }
                    (None, call) => json!({"type":"call","scope":scope,"launch":launch,
                        "parent":hint,"call":call}),
                };
                let mut socket =
                    UnixStream::connect(Path::new(&addr)).map_err(|e| e.to_string())?;
                socket
                    .set_read_timeout(Some(wait))
                    .map_err(|e| e.to_string())?;
                let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
                socket
                    .write_all(&(body.len() as u32).to_be_bytes())
                    .map_err(|e| e.to_string())?;
                socket.write_all(&body).map_err(|e| e.to_string())?;
                let mut size = [0; 4];
                socket
                    .read_exact(&mut size)
                    .map_err(|_| "The session host closed the worker request.")?;
                let n = u32::from_be_bytes(size) as usize;
                if n > 16 * 1024 * 1024 {
                    return Err("The session host sent an oversized worker reply.".into());
                }
                let mut data = vec![0; n];
                socket.read_exact(&mut data).map_err(|e| e.to_string())?;
                decode(serde_json::from_slice(&data).map_err(|e| e.to_string())?)
            }
            Self::External { socket } => {
                let mut stream = UnixStream::connect(&socket).map_err(|e| e.to_string())?;
                stream
                    .set_read_timeout(Some(wait))
                    .map_err(|e| e.to_string())?;
                let id = format!(
                    "worker-{}",
                    crate::random_token().map_err(|e| e.to_string())?
                );
                let frame = match (start, call) {
                    (Some(start), _) => {
                        json!({"type":"worker_launch","id":id,"caller":caller,"start":start})
                    }
                    (None, call) => {
                        json!({"type":"worker_call","id":id,"caller":caller,"call":call})
                    }
                };
                for line in [json!({"type":"hello","version":1}), frame] {
                    let mut bytes = serde_json::to_vec(&line).map_err(|e| e.to_string())?;
                    bytes.push(b'\n');
                    stream.write_all(&bytes).map_err(|e| e.to_string())?;
                }
                let reader = io::BufReader::new(stream);
                for line in io::BufRead::lines(reader) {
                    let line =
                        line.map_err(|_| "The session host did not answer the worker request.")?;
                    let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    match frame["type"].as_str() {
                        Some("worker_reply") if frame["id"] == id.as_str() => {
                            return decode(frame);
                        }
                        Some("error") if frame["id"].is_null() || frame["id"] == id.as_str() => {
                            return Err(frame["message"]
                                .as_str()
                                .unwrap_or("The session host refused the worker request.")
                                .into());
                        }
                        _ => continue,
                    }
                }
                Err("The session host closed the worker request.".into())
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

fn decode(reply: Value) -> Result<Value, String> {
    match reply.get("error").and_then(Value::as_str) {
        Some(error) => Err(error.to_owned()),
        None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// The per-user host socket, computed exactly as the host computes it.
fn host_socket() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let absolute = |name: &str| {
        env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    if let Some(path) = absolute("OX_HOST_SOCKET") {
        return Some(path);
    }
    let state_dir = match absolute("OX_HOST_STATE_DIR") {
        Some(root) => root,
        None => absolute("XDG_STATE_HOME")
            .or_else(|| {
                absolute("HOME").map(|h| {
                    h.join(if cfg!(target_os = "macos") {
                        "Library/Application Support"
                    } else {
                        ".local/state"
                    })
                })
            })?
            .join("ox/host"),
    };
    let hash = state_dir
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |v, b| {
            (v ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
    let name = format!("ox-host-{}-{hash:016x}", unsafe { libc::geteuid() });
    absolute("XDG_RUNTIME_DIR")
        .map(|p| p.join(&name).join("host.sock"))
        .filter(|p| p.as_os_str().as_encoded_bytes().len() < 100)
        .or_else(|| Some(Path::new("/tmp").join(name).join("host.sock")))
}

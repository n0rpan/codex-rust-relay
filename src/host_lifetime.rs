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
const CHILD_MODE: &str = "OX_HOST_WORKER_CEILING";

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
            "Worker access exceeds its active parent’s permission ceiling.",
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
pub(crate) struct Grant {
    socket: UnixStream,
    pub parent: String,
}
impl Grant {
    pub(crate) async fn register(
        mut self,
        port: u16,
        token: String,
        snapshot: Value,
    ) -> Result<(), String> {
        tokio::task::spawn_blocking(move || {
            let data = serde_json::to_vec(&json!({"port":port,"token":token,"snapshot":snapshot}))?;
            self.socket.write_all(&(data.len() as u32).to_be_bytes())?;
            self.socket.write_all(&data)?;
            let mut ack = [0];
            self.socket.read_exact(&mut ack)?;
            if ack[0] != 1 {
                return Err(io::Error::other("Host refused the worker registration"));
            }
            Ok::<_, io::Error>(())
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
    }
}
pub(crate) async fn authorize(
    cwd: &Path,
    write: bool,
    claude: bool,
) -> Result<Option<Grant>, String> {
    let Some(addr) = env::var_os(ENDPOINT) else {
        return Ok(None);
    };
    if let Some(ceiling) = env::var_os(CHILD_MODE)
        && write
        && (ceiling != "write" || claude)
    {
        return Err("A nested OX worker cannot expand its parent’s sandbox.".into());
    }
    if LEASE.get().is_none() {
        return Err("This OX worker gateway is not attached to a live host".into());
    }
    let project = cwd.canonicalize().map_err(|e| e.to_string())?;
    let route: Value = serde_json::from_str(
        &env::var("OX_HOST_WORKER_ROUTE")
            .map_err(|_| "This helper is not attached to a host billing route")?,
    )
    .map_err(|e| e.to_string())?;
    let parent = env::var("OX_HOST_WORKER_PARENT").ok();
    tokio::task::spawn_blocking(move||{
        let mut socket=request(&addr,json!({"type":"worker","scope":{"project":project,"route":route},"parent":parent,"write":write,"claude":claude}))?;
        let mut size=[0;4];socket.read_exact(&mut size)?;
        let n=u32::from_be_bytes(size) as usize;if n>4096{return Err(io::Error::other("Invalid host parent identity"));}
        let mut data=vec![0;n];socket.read_exact(&mut data)?;
        Ok::<_,io::Error>(Some(Grant{socket,parent:String::from_utf8(data).map_err(io::Error::other)?}))
    }).await.map_err(|e|e.to_string())?.map_err(|e|e.to_string())
}
pub(crate) fn inherit(command: &mut tokio::process::Command, write: bool) {
    if hosted() {
        command.env(CHILD_MODE, if write { "write" } else { "read" });
        if let Some(lease) = LEASE.get() {
            command.env(PARENT_LEASE, lease);
        }
    }
}
pub(crate) fn hosted() -> bool {
    env::var_os(ENDPOINT).is_some()
}

pub(crate) fn isolate_stock(command: &mut tokio::process::Command) {
    for key in [
        ENDPOINT,
        CHILD_MODE,
        PARENT_LEASE,
        "OX_HOST_WORKER_ROUTE",
        "OX_HOST_WORKER_PARENT",
    ] {
        command.env_remove(key);
    }
}

//! Stop, restart and purge a running host from the command line.
//!
//! The running host is found through its socket: the peer process of a
//! connection to it, checked to belong to the current user. `restart` sends
//! SIGUSR1, which a host with drain support answers by refusing new turns and
//! stopping once its running turns finish or the bound passes; it then starts
//! the installed host. An older host that predates drain support has no
//! `host` field in its hello, and is stopped with SIGTERM instead, which
//! interrupts its running turns.
use crate::{handover, transport};
use ox_desk_core::host_protocol::{self as wire, Client, HostInfo, Server};
use ox_host_service::{Paths, ensure_running, is_running};
use std::{
    fs,
    io::{self, BufRead, BufReader, Write},
    os::unix::{io::AsRawFd, net::UnixStream},
    path::Path,
    thread,
    time::{Duration, Instant},
};

const BOUND_FILE: &str = "drain-bound";
pub(crate) const DEFAULT_BOUND: Duration = Duration::from_secs(600);
/// No drain waits longer than a day.
pub(crate) const MAX_BOUND: Duration = Duration::from_secs(86_400);

/// The bound a running host uses for a drain: what `restart` wrote, or the default.
pub(crate) fn drain_bound(state_dir: &Path) -> Duration {
    let bound = fs::read_to_string(state_dir.join(BOUND_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .map_or(DEFAULT_BOUND, Duration::from_secs)
        .min(MAX_BOUND);
    let _ = fs::remove_file(state_dir.join(BOUND_FILE));
    bound
}

pub(crate) fn timeout(args: &[String]) -> io::Result<Duration> {
    match args {
        [] => Ok(DEFAULT_BOUND),
        [flag, secs] if flag == "--timeout" => secs
            .parse()
            .map(Duration::from_secs)
            .ok()
            .filter(|d| *d <= MAX_BOUND)
            .ok_or_else(|| io::Error::other("--timeout takes 0 to 86400 seconds")),
        _ => Err(io::Error::other("Usage: ox host restart [--timeout SECS]")),
    }
}

/// A short-lived protocol connection to the running host.
pub(crate) struct Probe {
    pub pid: libc::pid_t,
    pub version: u32,
    pub host: Option<HostInfo>,
    pub capabilities: Vec<String>,
    /// The host refused this client's hello, for example over a protocol
    /// version. It still runs, and can still be stopped by its pid.
    pub refused: Option<String>,
}

pub(crate) fn probe(paths: &Paths) -> io::Result<Option<Probe>> {
    if !is_running(paths)? {
        return Ok(None);
    }
    let mut stream = UnixStream::connect(&paths.socket)?;
    transport::same_user(&stream)?;
    let pid = peer_pid(&stream)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut hello = serde_json::to_vec(&Client::Hello {
        version: wire::VERSION,
    })?;
    hello.push(b'\n');
    stream.write_all(&hello)?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let (version, host, capabilities, refused) = match serde_json::from_str(&line) {
        Ok(Server::Hello {
            version,
            host,
            capabilities,
            ..
        }) => (version, host, capabilities, None),
        Ok(Server::Error { message, .. }) => (0, None, vec![], Some(message)),
        _ => (
            0,
            None,
            vec![],
            Some("the host's hello could not be read".into()),
        ),
    };
    Ok(Some(Probe {
        pid,
        version,
        host,
        capabilities,
        refused,
    }))
}

fn peer_pid(stream: &UnixStream) -> io::Result<libc::pid_t> {
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
        Ok(peer.pid)
    }
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut size = std::mem::size_of_val(&pid) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                0, // SOL_LOCAL
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(pid)
    }
}

fn signal(pid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    if pid <= 1 || unsafe { libc::kill(pid, signal) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether a process has exited. A zombie has exited: its parent just has not
/// reaped it yet.
fn exited(pid: libc::pid_t) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    #[cfg(target_os = "linux")]
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        return stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'));
    }
    false
}

/// Wait until the socket is closed and the process has exited, so the service
/// manager sees the unit stopped before anything starts it again.
fn wait_stopped(paths: &Paths, pid: libc::pid_t, within: Duration) -> io::Result<bool> {
    let until = Instant::now() + within.min(MAX_BOUND + Duration::from_secs(60));
    while Instant::now() < until {
        if !is_running(paths)? && exited(pid) {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(false)
}

pub(crate) fn stop(paths: &Paths) -> io::Result<()> {
    let Some(host) = probe(paths)? else {
        println!("OX host is not running.");
        return Ok(());
    };
    signal(host.pid, libc::SIGTERM)?;
    if !wait_stopped(paths, host.pid, Duration::from_secs(15))? {
        return Err(io::Error::other(format!(
            "OX host (pid {}) did not stop within 15 seconds",
            host.pid
        )));
    }
    println!("Stopped the OX host. Running turns were interrupted.");
    Ok(())
}

pub(crate) fn restart_if_updated(paths: &Paths) -> io::Result<()> {
    let marker = paths.state_dir.join("pending-update-restart");
    let expected = handover::version();
    match probe(paths)? {
        Some(host)
            if host
                .host
                .as_ref()
                .is_none_or(|host| host.version != expected) =>
        {
            fs::write(&marker, expected)?;
            restart(paths, DEFAULT_BOUND)?;
        }
        None if fs::read_to_string(&marker).ok().as_deref() == Some(expected) => {
            ox_host_service::refresh_update_service()?;
            ensure_running(paths)?;
        }
        None => return Ok(()),
        Some(_) => {}
    }
    if probe(paths)?
        .and_then(|p| p.host)
        .is_none_or(|h| h.version != expected)
    {
        return Err(io::Error::other(
            "The running host version differs from the installed update. Run ox host doctor for details.",
        ));
    }
    match fs::remove_file(marker) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

pub(crate) fn restart(paths: &Paths, bound: Duration) -> io::Result<()> {
    match probe(paths)? {
        None => println!("OX host was not running."),
        Some(host) if host.host.is_some() && host.refused.is_none() => {
            fs::write(
                paths.state_dir.join(BOUND_FILE),
                bound.as_secs().to_string(),
            )?;
            signal(host.pid, libc::SIGUSR1)?;
            println!(
                "Asked OX host {} to finish its running turns (at most {}s). New turns wait for the new host.",
                host.host.as_ref().map_or("", |h| h.version.as_str()),
                bound.as_secs()
            );
            if !wait_stopped(paths, host.pid, bound + Duration::from_secs(15))? {
                return Err(io::Error::other(
                    "The OX host did not stop after its drain bound. Run ox host stop.",
                ));
            }
            if let Some(last) = handover::peek(&paths.state_dir)
                && last.running > 0
            {
                println!(
                    "{} turn(s) were still running at the bound and are reported as interrupted.",
                    last.running
                );
            }
        }
        Some(host) => {
            println!(
                "The running OX host {}; stopping it interrupts its running turns.",
                host.refused.as_deref().map_or_else(
                    || "predates drain support".to_owned(),
                    |why| format!("cannot drain for this version ({why})")
                )
            );
            signal(host.pid, libc::SIGTERM)?;
            if !wait_stopped(paths, host.pid, Duration::from_secs(15))? {
                return Err(io::Error::other("The old OX host did not stop."));
            }
        }
    }
    ox_host_service::refresh_update_service()?;
    ensure_running(paths)?;
    let version = probe(paths)?
        .and_then(|p| p.host)
        .map_or_else(|| "?".into(), |h| h.version);
    println!("OX host {version} is running.");
    Ok(())
}

/// Remove the login service and delete OX host state. Stock client data
/// (CODEX_HOME, CLAUDE_CONFIG_DIR, native transcripts) is never touched.
pub(crate) fn purge(paths: &Paths) -> io::Result<()> {
    let dir = &paths.state_dir;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    // Only a directory the host has run in: it holds the session ledger that
    // only `ox-host run` writes. Checked before anything below creates files.
    let owned = dir.join(crate::durable::LEDGER).is_file()
        && dir.components().count() > 2
        && home.is_none_or(|h| !h.starts_with(dir));
    if dir.exists() && !owned {
        return Err(io::Error::other(format!(
            "{} does not look like an OX host state directory; not deleting it",
            dir.display()
        )));
    }
    ox_host_service::manage("uninstall", paths)?;
    if probe(paths)?.is_some() {
        stop(paths)?;
    }
    match fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => {
            fs::remove_dir_all(dir)?;
            println!(
                "Deleted OX host state at {} (session ledger, pairings, push keys).",
                dir.display()
            );
        }
        Ok(_) => {
            return Err(io::Error::other(format!(
                "{} is not a directory; not deleting it",
                dir.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => println!("No OX host state to delete."),
        Err(e) => return Err(e),
    }
    let _ = fs::remove_file(&paths.socket);
    if let Some(dir) = paths.socket.parent() {
        let _ = fs::remove_dir(dir); // only if empty
    }
    println!("Codex and Claude Code sessions and sign-ins were left as they are.");
    Ok(())
}

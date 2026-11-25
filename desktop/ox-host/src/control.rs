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

/// The bound a running host uses for a drain: what `restart` wrote, or the default.
pub(crate) fn drain_bound(state_dir: &Path) -> Duration {
    let bound = fs::read_to_string(state_dir.join(BOUND_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .map_or(DEFAULT_BOUND, Duration::from_secs);
    let _ = fs::remove_file(state_dir.join(BOUND_FILE));
    bound
}

pub(crate) fn timeout(args: &[String]) -> io::Result<Duration> {
    match args {
        [] => Ok(DEFAULT_BOUND),
        [flag, secs] if flag == "--timeout" => secs
            .parse()
            .map(Duration::from_secs)
            .map_err(|_| io::Error::other("--timeout takes a number of seconds")),
        _ => Err(io::Error::other("Usage: ox host restart [--timeout SECS]")),
    }
}

/// A short-lived protocol connection to the running host.
pub(crate) struct Probe {
    pub pid: libc::pid_t,
    pub version: u32,
    pub host: Option<HostInfo>,
    pub capabilities: Vec<String>,
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
    let (version, host, capabilities) = match serde_json::from_str(&line) {
        Ok(Server::Hello {
            version,
            host,
            capabilities,
            ..
        }) => (version, host, capabilities),
        Ok(Server::Error { message, .. }) => return Err(io::Error::other(message)),
        _ => (0, None, vec![]),
    };
    Ok(Some(Probe {
        pid,
        version,
        host,
        capabilities,
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

fn wait_stopped(paths: &Paths, within: Duration) -> io::Result<bool> {
    let until = Instant::now() + within;
    while Instant::now() < until {
        if !is_running(paths)? {
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
    if !wait_stopped(paths, Duration::from_secs(15))? {
        return Err(io::Error::other(format!(
            "OX host (pid {}) did not stop within 15 seconds",
            host.pid
        )));
    }
    println!("Stopped the OX host. Running turns were interrupted.");
    Ok(())
}

pub(crate) fn restart(paths: &Paths, bound: Duration) -> io::Result<()> {
    match probe(paths)? {
        None => println!("OX host was not running."),
        Some(host) if host.host.is_some() => {
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
            if !wait_stopped(paths, bound + Duration::from_secs(15))? {
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
                "The running OX host predates drain support; stopping it interrupts its running turns."
            );
            signal(host.pid, libc::SIGTERM)?;
            if !wait_stopped(paths, Duration::from_secs(15))? {
                return Err(io::Error::other("The old OX host did not stop."));
            }
        }
    }
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
    ox_host_service::manage("uninstall", paths)?;
    if probe(paths)?.is_some() {
        stop(paths)?;
    }
    let dir = &paths.state_dir;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    // Only a directory the host created: it holds the host's startup lock,
    // and it is not the home directory or one of its ancestors.
    let owned = dir.join("startup.lock").is_file()
        && dir.components().count() > 2
        && home.is_none_or(|h| !h.starts_with(dir));
    match fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() && !owned => {
            return Err(io::Error::other(format!(
                "{} does not look like an OX host state directory; not deleting it",
                dir.display()
            )));
        }
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

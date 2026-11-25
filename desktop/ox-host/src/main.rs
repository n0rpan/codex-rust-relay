mod control;
mod doctor;
mod durable;
mod engine;
mod handover;
mod lease;
mod policy;
mod receipts;
mod replay;
mod runtime;
mod transport;
mod workers;
mod yolo;

use engine::{Engine, Message};
use ox_host_service::{Paths, SocketGuard};
use std::{
    env,
    io::{self, Read},
    os::unix::{io::AsRawFd, net::UnixStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicI32, Ordering},
        mpsc,
    },
    thread,
};

static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);
extern "C" fn stop_signal(signal: libc::c_int) {
    // write is async-signal-safe. The reader wakes the event-driven actor.
    unsafe {
        libc::write(
            SIGNAL_FD.load(Ordering::Relaxed),
            [signal as u8].as_ptr().cast(),
            1,
        );
    }
}
fn run(paths: Paths) -> io::Result<()> {
    ox_host_service::configure_allocator();
    let guard = SocketGuard::bind(&paths)?;
    ox_host_service::errlog::init(&paths.state_dir);
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ox_host_service::errlog::record(&format!("panic: {info}"));
        default_hook(info);
    }));
    let (tx, rx) = mpsc::sync_channel(512);
    let lease = lease::Endpoint::bind(&paths.socket)?;
    // Only the host's own runtime children receive this endpoint. Set it before starting threads.
    unsafe {
        env::set_var("OX_HOST_LIFETIME_SOCKET", &lease.path);
    }
    let mut engine = Engine::open(
        runtime::RuntimeConfig::discover()?,
        &paths.state_dir,
        tx.clone(),
    )?;
    let (mut signal_input, signal_output) = UnixStream::pair()?;
    signal_output.set_nonblocking(true)?;
    SIGNAL_FD.store(signal_output.as_raw_fd(), Ordering::Relaxed);
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = stop_signal as *const () as usize;
        libc::sigemptyset(&mut act.sa_mask);
        // SIGUSR1 asks for an upgrade drain (see `ox host restart`).
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGUSR1] {
            if libc::sigaction(signal, &act, std::ptr::null_mut()) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    let signals = tx.clone();
    let drain_bound = paths.state_dir.clone();
    thread::spawn(move || {
        let mut byte = [0];
        while signal_input.read_exact(&mut byte).is_ok() {
            if i32::from(byte[0]) == libc::SIGUSR1 {
                let bound = control::drain_bound(&drain_bound);
                let _ = signals.send(Message::Drain(bound));
            } else {
                let _ = signals.send(Message::Stop);
                break;
            }
        }
    });
    lease.listen(tx.clone())?;
    let listener = guard.listener.try_clone()?;
    thread::spawn(move || {
        for (id, next) in listener.incoming().enumerate() {
            match next {
                Ok(stream) => {
                    let _ = transport::attach(stream, id as u64, tx.clone());
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    thread::spawn(move || {
        if let Err(e) = ox_host_web::run(paths.state_dir, paths.socket) {
            ox_host_service::errlog::record(&format!("OX remote service: {e}"));
        }
    });
    let result = engine.serve(rx);
    SIGNAL_FD.store(-1, Ordering::Relaxed);
    if let Err(e) = &result {
        ox_host_service::errlog::record(&format!("stopped: {e}"));
    }
    result
}
const USAGE: &str = "Usage: ox-host [--socket PATH --state-dir PATH] COMMAND
  run                       serve sessions (what the login service runs)
  install                   start the host at login (launchd or systemd user service)
  status                    show whether the host and its login service run
  stop                      stop the host now; running turns are interrupted
  restart [--timeout SECS]  finish running turns (default bound 600s), then start the installed host
  doctor [--errors N]       check the host, Tailscale Serve, paired devices and stock clients
  uninstall [--purge]       remove the login service; --purge also deletes OX host state
                            (session ledger, pairings, push keys). Codex and Claude Code data is never touched.";
fn entry() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    let op = args.next().unwrap_or_else(|| "run".into());
    if op == "--version" || op == "-V" {
        println!(
            "ox-host {}",
            option_env!("OX_BUILD_VERSION")
                .filter(|s| !s.is_empty())
                .unwrap_or(env!("CARGO_PKG_VERSION"))
        );
        return Ok(());
    }
    let mut paths = Paths::discover()?;
    let mut args = args.peekable();
    while let Some(flag) = args.next_if(|a| a == "--socket" || a == "--state-dir") {
        let dest = if flag == "--socket" {
            &mut paths.socket
        } else if flag == "--state-dir" {
            &mut paths.state_dir
        } else {
            return Err(io::Error::other(
                "Expected --socket PATH or --state-dir PATH",
            ));
        };
        *dest = PathBuf::from(
            args.next()
                .ok_or_else(|| io::Error::other("Missing host path"))?,
        );
    }
    let rest: Vec<String> = args
        .by_ref()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match (op.to_str(), rest.as_slice()) {
        (Some("run"), []) => run(paths),
        (Some(action @ ("install" | "status")), []) => ox_host_service::manage(action, &paths),
        (Some("uninstall"), []) => ox_host_service::manage("uninstall", &paths),
        (Some("uninstall"), [purge]) if purge == "--purge" => control::purge(&paths),
        (Some("stop"), []) => control::stop(&paths),
        (Some("restart"), rest) => control::restart(&paths, control::timeout(rest)?),
        (Some("doctor"), rest) => doctor::run(&paths, rest),
        _ => Err(io::Error::other(USAGE)),
    }
}
fn main() -> std::process::ExitCode {
    match entry() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ox-host: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

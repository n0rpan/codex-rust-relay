mod durable;
mod engine;
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
extern "C" fn stop_signal(_: libc::c_int) {
    // write is async-signal-safe. The reader wakes the event-driven actor.
    unsafe {
        libc::write(SIGNAL_FD.load(Ordering::Relaxed), [1u8].as_ptr().cast(), 1);
    }
}
fn run(paths: Paths) -> io::Result<()> {
    ox_host_service::configure_allocator();
    let guard = SocketGuard::bind(&paths)?;
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
        for signal in [libc::SIGINT, libc::SIGTERM] {
            if libc::sigaction(signal, &act, std::ptr::null_mut()) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    let signals = tx.clone();
    thread::spawn(move || {
        let mut byte = [0];
        if signal_input.read_exact(&mut byte).is_ok() {
            let _ = signals.send(Message::Stop);
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
            eprintln!("OX remote service: {e}");
        }
    });
    let result = engine.serve(rx);
    SIGNAL_FD.store(-1, Ordering::Relaxed);
    result
}
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
    while let Some(flag) = args.next() {
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
    match op.to_str() {
        Some("run") => run(paths),
        Some(action @ ("install" | "uninstall" | "status")) => {
            ox_host_service::manage(action, &paths)
        }
        _ => Err(io::Error::other(
            "Usage: ox-host run|install|status|uninstall [--socket PATH --state-dir PATH]",
        )),
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

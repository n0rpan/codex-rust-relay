//! The host stages the release in the background, takes its normal idle reservation,
//! then execs the verified generation in place. The login service keeps the same PID.
use crate::engine::Message;
use ox_desk_core::host_protocol::{self as wire, Client, Server};
use ox_host_service::{
    Paths,
    managed_install::{active_binary, package_store},
};
use ox_host_web::host_updates::{Controller, Mode, Preferences, UPDATES};
use serde::Deserialize;
use std::{
    env,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::{Command, Stdio},
    sync::mpsc::SyncSender,
    thread,
    time::Duration,
};

#[derive(Deserialize)]
struct Candidate {
    version: String,
    digest: String,
}

pub fn start(paths: Paths, tx: SyncSender<Message>) -> io::Result<()> {
    let store = env::current_exe().ok().and_then(|p| package_store(&p));
    let controller = Controller::open(
        &paths.state_dir,
        store.as_deref(),
        crate::handover::version(),
    )?;
    UPDATES
        .set(controller)
        .map_err(|_| io::Error::other("Update service already initialized"))?;
    let Some(store) = store else { return Ok(()) };
    thread::Builder::new()
        .name("ox-host-updates".into())
        .spawn(move || {
            let controller = UPDATES.get().unwrap();
            let (initial, prefs) = controller.current();
            if !prefs.scheduled {
                controller.wait(initial, Duration::from_secs(60));
            }
            loop {
                let (rev, prefs) = controller.current();
                let mut delay = Duration::from_secs(1800);
                if prefs.scheduled || (prefs.mode != Mode::Manual && background_allowed(&store)) {
                    match check(controller, &store, &paths, rev, prefs, &tx) {
                        Ok(true) => break,
                        Ok(false) => (),
                        Err(e) => {
                            controller.message(rev, format!("Update check failed: {e}"));
                            delay = Duration::from_secs(300);
                        }
                    }
                }
                controller.wait(rev, delay);
            }
        })?;
    Ok(())
}

fn background_allowed(store: &Path) -> bool {
    env::var_os("OX_NO_BACKGROUND_UPDATE").is_none()
        && (store.join("headless").is_file()
            || ox_desk_core::desktop_behavior::read().is_ok_and(|s| s.behavior.background_updates))
}

fn check(
    controller: &Controller,
    store: &Path,
    paths: &Paths,
    rev: u64,
    prefs: Preferences,
    tx: &SyncSender<Message>,
) -> io::Result<bool> {
    controller.message(
        rev,
        format!("Checking the {} channel", prefs.channel.name()),
    );
    let active = active_binary(store, "ox-host");
    let running = env::current_exe()?.canonicalize()?;
    let cli = running.with_file_name("ox");
    let already_switched = active.canonicalize()? != running;
    let pending: Option<Candidate> = if already_switched {
        None
    } else {
        let out = Command::new(&cli)
            .args(["update", "--stage-host-update", prefs.channel.name()])
            .stdin(Stdio::null())
            .output()?;
        if !out.status.success() {
            return Err(failure(&out));
        }
        serde_json::from_slice(&out.stdout)?
    };
    if pending.is_none() && !already_switched {
        controller.finish(rev, "Up to date", || Ok(()))?;
        return Ok(false);
    }
    let version = pending
        .as_ref()
        .map_or("installed release", |c| c.version.as_str());
    if !prefs.scheduled && prefs.mode != Mode::Automatic {
        controller.message(rev, format!("Ready to install {version}"));
        return Ok(false);
    }
    controller.message(rev, format!("Waiting for idle to install {version}"));
    let mut retry = Duration::from_secs(3);
    while controller.current().0 == rev && (prefs.scheduled || background_allowed(store)) {
        if let Some(reservation) = reserve(paths)? {
            let result = controller.finish(rev, "Restarting on the selected release", || {
                if let Some(candidate) = &pending {
                    let out = Command::new(&cli)
                        .args([
                            "update",
                            "--try-select-host-update",
                            &candidate.version,
                            &candidate.digest,
                        ])
                        .stdin(Stdio::null())
                        .output()?;
                    if !out.status.success() {
                        return Err(failure(&out));
                    }
                    if out.stdout == b"busy\n" {
                        return Err(io::ErrorKind::WouldBlock.into());
                    }
                }
                Ok(())
            });
            match result {
                Ok(Some(())) => {
                    tx.send(Message::Replace(active, reservation))
                        .map_err(|_| io::Error::other("Host stopped"))?;
                    return Ok(true);
                }
                Ok(None) => break,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                Err(e) => return Err(e),
            }
        }
        controller.wait(rev, retry);
        retry = (retry * 2).min(Duration::from_secs(60));
    }
    Ok(false)
}
fn failure(out: &std::process::Output) -> io::Error {
    let detail = String::from_utf8_lossy(&out.stderr);
    io::Error::other(if detail.trim().is_empty() {
        format!("Updater exited with {}", out.status)
    } else {
        detail.trim().chars().take(500).collect()
    })
}
fn reserve(paths: &Paths) -> io::Result<Option<UnixStream>> {
    let mut socket = UnixStream::connect(&paths.socket)?;
    crate::transport::same_user(&socket)?;
    socket.set_read_timeout(Some(Duration::from_secs(10)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    fn exchange(socket: &mut UnixStream, frame: &Client) -> io::Result<Server> {
        serde_json::to_writer(&mut *socket, frame)?;
        socket.write_all(b"\n")?;
        let mut bytes = Vec::new();
        BufReader::new(socket.take(8 * 1024 * 1024)).read_until(b'\n', &mut bytes)?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    if !matches!(exchange(&mut socket,&Client::Hello{version:wire::VERSION})?,Server::Hello{capabilities,..} if capabilities.iter().any(|c|c==wire::IDLE_UPDATE))
    {
        return Err(io::Error::other("Host cannot reserve an idle update"));
    }
    match exchange(&mut socket, &Client::ReserveIdleUpdate)? {
        Server::IdleUpdate { ready } => Ok(ready.then_some(socket)),
        _ => Err(io::Error::other("Host could not confirm its idle state")),
    }
}
pub fn cli(paths: &Paths, args: &[String]) -> io::Result<()> {
    use ox_host_web::{
        access, admin,
        host_updates::{Channel, Command, Mode},
    };
    let action = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["-h" | "--help"] => {
            println!(
                "Usage: ox host update [status|when-idle|automatic|download|manual|stable|dev|cancel]"
            );
            return Ok(());
        }
        [] | ["when-idle"] => Command::UpdateWhenIdle,
        ["status"] => Command::Status,
        ["cancel"] => Command::Cancel,
        ["automatic"] => Command::SetMode {
            mode: Mode::Automatic,
        },
        ["manual"] => Command::SetMode { mode: Mode::Manual },
        ["download"] => Command::SetMode {
            mode: Mode::Download,
        },
        ["stable"] => Command::SetChannel {
            channel: Channel::Stable,
        },
        ["dev"] => Command::SetChannel {
            channel: Channel::Dev,
        },
        _ => {
            return Err(io::Error::other(
                "Usage: ox host update [status|when-idle|automatic|download|manual|stable|dev|cancel]",
            ));
        }
    };
    access::status(paths).map_err(io::Error::other)?;
    let s = admin::request(
        &paths.state_dir,
        &admin::Request::Update { command: action },
    )?
    .update
    .ok_or_else(|| io::Error::other("Update the host to configure updates"))?;
    println!(
        "OX {} · {} · {:?}\n{}",
        s.installed,
        s.channel.name(),
        s.mode,
        s.message
    );
    Ok(())
}

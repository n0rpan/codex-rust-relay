//! Delegate desktop and session host commands to packaged companion binaries.
use crate::provider::RelayError;
use std::{
    env,
    ffi::OsString,
    io,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::PathBuf,
    process::{Command, Stdio},
};

pub(crate) fn run(args: &[OsString]) -> Result<u8, RelayError> {
    let usage = "Usage: ox desktop [--] [directory] | ox desktop --client-only";
    let client_only = matches!(args,[flag] if flag=="--client-only");
    let project = match args {
        [] => env::current_dir()?,
        [_] if client_only => env::current_dir()?,
        [flag] if flag == "-h" || flag == "--help" => {
            println!(
                "{usage}\nOpen the project in OX Desktop. With no directory, use the current directory."
            );
            return Ok(0);
        }
        [path] if !path.as_encoded_bytes().starts_with(b"-") => PathBuf::from(path),
        [dash, path] if dash == "--" => PathBuf::from(path),
        _ => return Err(usage.into()),
    };
    let directory = project
        .canonicalize()
        .map_err(|e| format!("Cannot open {}: {e}", project.display()))?;
    if !directory.is_dir() {
        return Err(format!("{} is not a directory", directory.display()).into());
    }
    let binary = companion("ox-desktop")?;
    let mut command = Command::new(&binary);
    if client_only {
        command.arg("--client-only");
    } else {
        command.arg(&directory);
    }
    command
        .current_dir(&directory)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Could not launch {}: {e}", binary.display()),
            )
        })?;
    Ok(0)
}

fn companion(program: &str) -> Result<PathBuf, RelayError> {
    let sibling = env::current_exe()?.with_file_name(program);
    let mut candidates = vec![sibling];
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(
            env::split_paths(&path)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(program)),
        );
    }
    #[cfg(target_os = "macos")]
    {
        let bundle = PathBuf::from("Applications/OX Desktop.app/Contents/MacOS").join(program);
        if let Some(home) = env::var_os("HOME") {
            candidates.push(PathBuf::from(home).join(&bundle));
        }
        candidates.push(PathBuf::from("/").join(&bundle));
    }
    candidates
        .into_iter()
        .find(|path| {
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or_else(|| {
            format!("Install the OX desktop package containing {program} using the installer’s --desktop option.").into()
        })
}

const HOST_USAGE: &str = "Usage: ox host COMMAND
  install                   start the session host at login
  status                    show whether the host and its login service run
  pair [--terminal] [--port HTTPS_PORT] [--listen-port LOCAL_PORT]
                            enable private Serve access and print a device invitation
  pair --worker-host [--port HTTPS_PORT] [--listen-port LOCAL_PORT]
                            invite another OX host to launch workers here
  update [setting]          install when idle or set automatic, download, manual, stable or dev
  name [NAME]               show or change this host’s display name
  project [list | add DIR | remove DIR]  manage registered checkouts
  labels [set LABEL…]       show or replace placement labels
  policy [set POLICY]       show or change the host mode policy
  devices                   list paired devices and their IDs
  terminal <device> on|off  change a paired device’s terminal grant
  revoke <device-id>        disconnect a paired device
  doctor [--errors N]       check the host, Tailscale Serve, paired devices and stock clients
  restart [--timeout SECS]  let running turns finish, then start the installed host (after an update)
  stop                      stop the host now; running turns are interrupted
  uninstall [--purge]       remove the login service; --purge also deletes OX host state.
                            Codex and Claude Code sessions and sign-ins are never touched.";

pub(crate) fn host(args: &[OsString]) -> Result<u8, RelayError> {
    const COMMANDS: &[&str] = &[
        "install",
        "status",
        "doctor",
        "pair",
        "devices",
        "policy",
        "name",
        "labels",
        "project",
        "update",
        "revoke",
        "terminal",
        "restart",
        "stop",
        "uninstall",
    ];
    match args.first().and_then(|a| a.to_str()) {
        Some(command) if COMMANDS.contains(&command) => {
            Err(Command::new(companion("ox-host")?).args(args).exec().into())
        }
        None | Some("-h" | "--help" | "help") => {
            println!("{HOST_USAGE}");
            Ok(0)
        }
        _ => Err(HOST_USAGE.into()),
    }
}

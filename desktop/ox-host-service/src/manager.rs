//! Install login startup using the native per-user service manager.
use super::{Paths, binary, home, is_running, lock_file};
use std::{
    env,
    ffi::OsStr,
    fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
};
const MARKER: &str = "Managed by OX session host";
#[cfg(target_os = "linux")]
const UNIT: &str = "ox-host.service";
#[cfg(target_os = "macos")]
const LABEL: &str = "dev.ox.session-host";

fn unit_path() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Ok(super::absolute_env("XDG_CONFIG_HOME")
            .unwrap_or(home()?.join(".config"))
            .join("systemd/user")
            .join(UNIT))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(home()?
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }
}
pub(super) fn installed() -> io::Result<bool> {
    Ok(owned_contents(&unit_path()?)?.is_some())
}

fn run(tool: &str, args: &[&OsStr]) -> io::Result<()> {
    let result = Command::new(tool).args(args).output()?;
    if result.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "{tool}: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    )))
}
fn stable_binary() -> io::Result<PathBuf> {
    let real = binary()?.canonicalize()?;
    Ok(env::var_os("PATH")
        .and_then(|s| {
            env::split_paths(&s)
                .filter(|p| p.is_absolute())
                .map(|p| p.join("ox-host"))
                .find(|p| p.canonicalize().is_ok_and(|v| v == real))
        })
        .unwrap_or(real))
}
fn text(s: &OsStr) -> io::Result<&str> {
    s.to_str()
        .filter(|s| !s.chars().any(char::is_control))
        .ok_or_else(|| io::Error::other("Service paths must contain printable UTF-8."))
}
fn environment() -> Vec<(&'static str, std::ffi::OsString)> {
    [
        "PATH",
        "CODEX_HOME",
        "CODEX_SQLITE_HOME",
        "CLAUDE_CONFIG_DIR",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ]
    .into_iter()
    .filter_map(|key| env::var_os(key).map(|v| (key, v)))
    .collect()
}
#[cfg(any(target_os = "linux", test))]
fn systemd_quote(value: &str, exec: bool) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!(
        "\"{}\"",
        if exec {
            escaped.replace('$', "$$")
        } else {
            escaped
        }
    )
}
#[cfg(any(target_os = "linux", test))]
fn systemd_unit(path: &Path, paths: &Paths) -> io::Result<String> {
    let args = [
        path.as_os_str(),
        "run".as_ref(),
        "--socket".as_ref(),
        paths.socket.as_os_str(),
        "--state-dir".as_ref(),
        paths.state_dir.as_os_str(),
    ];
    let args = args
        .iter()
        .map(|s| text(s).map(|s| systemd_quote(s, true)))
        .collect::<io::Result<Vec<_>>>()?
        .join(" ");
    let mut unit = format!(
        "# {MARKER}\n[Unit]\nDescription=OX session host\n\n[Service]\nType=simple\nExecStart={args}\nRestart=on-failure\nRestartSec=2\n"
    );
    for (key, val) in environment() {
        unit.push_str(&format!(
            "Environment={}\n",
            systemd_quote(&format!("{key}={}", text(&val)?), false)
        ));
    }
    unit.push_str("\n[Install]\nWantedBy=default.target\n");
    Ok(unit)
}
#[cfg(any(target_os = "macos", test))]
fn plist(path: &Path, paths: &Paths) -> io::Result<String> {
    fn xml(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    let mut doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- {MARKER} -->
<plist version="1.0"><dict>
<key>Label</key><string>dev.ox.session-host</string>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
<key>ProgramArguments</key><array>
"#
    );
    for v in [
        path.as_os_str(),
        "run".as_ref(),
        "--socket".as_ref(),
        paths.socket.as_os_str(),
        "--state-dir".as_ref(),
        paths.state_dir.as_os_str(),
    ] {
        doc.push_str(&format!("<string>{}</string>\n", xml(text(v)?)));
    }
    doc.push_str("</array><key>EnvironmentVariables</key><dict>\n");
    for (k, v) in environment() {
        doc.push_str(&format!(
            "<key>{k}</key><string>{}</string>\n",
            xml(text(&v)?)
        ));
    }
    doc.push_str("</dict></dict></plist>\n");
    Ok(doc)
}
#[cfg(target_os = "linux")]
fn service(args: &[&str]) -> io::Result<()> {
    let mut words = vec![OsStr::new("--user")];
    words.extend(args.iter().map(OsStr::new));
    run("systemctl", &words)
}
/// launchd domains to try, in order: the login session's GUI domain, then the
/// per-user background domain, which exists for accounts without a GUI session
/// (SSH, a CI user, before first login).
#[cfg(target_os = "macos")]
fn domains() -> [String; 2] {
    let uid = super::uid();
    [format!("gui/{uid}"), format!("user/{uid}")]
}
#[cfg(target_os = "macos")]
fn loaded(domain: &str) -> io::Result<bool> {
    Ok(Command::new("launchctl")
        .args(["print", &format!("{domain}/{LABEL}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?
        .success())
}
/// The domain the agent is loaded in, if any.
#[cfg(target_os = "macos")]
fn loaded_domain() -> io::Result<Option<String>> {
    for domain in domains() {
        if loaded(&domain)? {
            return Ok(Some(domain));
        }
    }
    Ok(None)
}

pub(super) fn start() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        service(&["start", UNIT])
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(domain) = loaded_domain()? {
            let name = format!("{domain}/{LABEL}");
            return run("launchctl", &["kickstart".as_ref(), name.as_ref()]);
        }
        let plist = unit_path()?;
        let mut last = None;
        for domain in domains() {
            let _ = run(
                "launchctl",
                &["enable".as_ref(), format!("{domain}/{LABEL}").as_ref()],
            );
            match run(
                "launchctl",
                &["bootstrap".as_ref(), domain.as_ref(), plist.as_os_str()],
            ) {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }
        }
        let detail = last.map_or_else(|| "launchctl bootstrap failed".into(), |e| e.to_string());
        Err(io::Error::other(format!(
            "launchd would not start the OX host agent ({detail}). A login agent needs this account's desktop session: run `ox host install` from Terminal in a logged-in desktop session, or start the host with `ox desktop .`."
        )))
    }
}
fn owned_contents(file: &Path) -> io::Result<Option<String>> {
    match fs::symlink_metadata(file) {
        Ok(m) if !m.is_file() => Err(io::Error::other(format!(
            "Service definition is not a regular file: {}",
            file.display()
        ))),
        Ok(_) => {
            let doc = fs::read_to_string(file)?;
            if doc.lines().take(4).any(|l| l.contains(MARKER)) {
                Ok(Some(doc))
            } else {
                Err(io::Error::other(format!(
                    "Service definition is user-managed: {}",
                    file.display()
                )))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
fn install(paths: &Paths, file: &Path) -> io::Result<()> {
    let executable = stable_binary()?;
    #[cfg(target_os = "linux")]
    let content = systemd_unit(&executable, paths)?;
    #[cfg(target_os = "macos")]
    let content = plist(&executable, paths)?;
    if owned_contents(file)?.as_deref() != Some(&content) {
        let parent = file.parent().unwrap();
        fs::create_dir_all(parent)?;
        let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        let update = (|| {
            output.write_all(content.as_bytes())?;
            output.sync_all()?;
            fs::rename(&tmp, file)
        })();
        if update.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        update?;
    }
    #[cfg(target_os = "linux")]
    {
        service(&["daemon-reload"])?;
        service(&["enable", UNIT])?;
    }
    // Clear any earlier `launchctl disable`, even when a host started on demand
    // is already running, so the agent loads at the next login.
    #[cfg(target_os = "macos")]
    for domain in domains() {
        let name = format!("{domain}/{LABEL}");
        let _ = run("launchctl", &["enable".as_ref(), name.as_ref()]);
    }
    if !is_running(paths)? {
        start()?;
    }
    println!("Installed OX host login service at {}", file.display());
    Ok(())
}
fn uninstall(file: &Path) -> io::Result<()> {
    if owned_contents(file)?.is_none() {
        println!("OX host login service is already uninstalled.");
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    service(&["disable", "--now", UNIT])?;
    #[cfg(target_os = "macos")]
    if let Some(domain) = loaded_domain()? {
        let name = format!("{domain}/{LABEL}");
        run("launchctl", &["bootout".as_ref(), name.as_ref()])?;
    }
    fs::remove_file(file)?;
    #[cfg(target_os = "linux")]
    service(&["daemon-reload"])?;
    println!("Removed the OX host login service.");
    Ok(())
}
pub fn manage(action: &str, paths: &Paths) -> io::Result<()> {
    let unit = unit_path()?;
    if action == "status" {
        println!(
            "OX host: {}\nLogin service: {}\nSocket: {}\nState: {}",
            if is_running(paths)? {
                "running"
            } else {
                "stopped"
            },
            if installed()? {
                "installed"
            } else {
                "not installed"
            },
            paths.socket.display(),
            paths.state_dir.display()
        );
        return Ok(());
    }
    paths.prepare()?;
    let lock = lock_file(&paths.state_dir.join("startup.lock"))?;
    lock.lock()?;
    match action {
        "install" => install(paths, &unit),
        "uninstall" => uninstall(&unit),
        _ => Err(io::Error::other("Usage: ox host install|status|uninstall")),
    }
}

#[cfg(test)]
mod tests {
    use super::{Paths, plist, systemd_unit};
    use std::path::Path;
    #[test]
    fn native_services_encode_literal_program_arguments_without_a_shell() {
        let paths = Paths {
            socket: "/private/One & Two/socket".into(),
            state_dir: "/private/One & Two".into(),
        };
        let binary = Path::new("/Applications/100% $OX & test/ox-host");
        let unit = systemd_unit(binary, &paths).unwrap();
        assert!(unit.contains(r#"ExecStart="/Applications/100%% $$OX & test/ox-host" "run" "--socket" "/private/One & Two/socket""#));
        let agent = plist(binary, &paths).unwrap();
        assert!(agent.contains("<string>/Applications/100% $OX &amp; test/ox-host</string>"));
        assert!(agent.contains("<string>/private/One &amp; Two/socket</string>"));
    }
}

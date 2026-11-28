//! Explicit, local browser verification for humans and tools in a worktree.

use crate::provider::RelayError;
use reqwest::{StatusCode, Url};
use ring::digest;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

const VERSION: &str = "0.45.0";
const LOCAL_HOSTS: &str = "localhost,127.0.0.1,[::1]";
const INSTALL_HINT: &str = "fix: ox browser install";
const DOWNLOAD_ENV: &[&str] = &[
    "SSL_CERT_FILE",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

const USAGE: &str = "\
Verify a local app with a shared browser session.

Usage: ox browser [--session NAME] <command>

  install                         Install the browser component on this host
  open http://localhost:3000      Open your local app
  snapshot [--json]               Read the page and get element references
  fill @e1 TEXT                   Fill a field from the latest snapshot
  click @e2                       Click an element from the latest snapshot
  preview                         Print the live preview link for the same session
  screenshot FILE.png             Save a screenshot to the given path
  close                           Close the browser session

Run commands from the same Git worktree or, outside Git, the same directory.
Use --session NAME to keep two conversations in that worktree separate.
Set OX_BROWSER_EXECUTABLE to use a specific Chromium executable.";

pub(crate) fn print_usage() {
    println!("{USAGE}");
}

enum Action<'a> {
    Help,
    Install,
    Open(Url),
    Snapshot { json: bool },
    Fill(&'a OsStr, &'a str),
    Click(&'a OsStr),
    Screenshot(PathBuf),
    Preview,
    Close,
}

fn parse(args: &[OsString]) -> Result<(&str, Action<'_>), RelayError> {
    let (label, args) = if let [flag, name, rest @ ..] = args
        && flag == "--session"
    {
        let name = name
            .to_str()
            .ok_or("Browser session names must be UTF-8.")?;
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(
                "Use a browser session name with 1–64 letters, digits, hyphens or underscores."
                    .into(),
            );
        }
        (name, rest)
    } else {
        ("default", args)
    };

    let action = match args {
        [] => Action::Help,
        [arg] if matches!(arg.to_str(), Some("-h" | "--help" | "help")) => Action::Help,
        [arg] if arg == "install" => Action::Install,
        [arg, url] if arg == "open" => {
            Action::Open(local_url(url.to_str().ok_or("The URL must be UTF-8.")?)?)
        }
        [arg] if arg == "snapshot" => Action::Snapshot { json: false },
        [arg, flag] if arg == "snapshot" && flag == "--json" => Action::Snapshot { json: true },
        [arg, reference, text] if arg == "fill" => Action::Fill(
            element_ref(reference)?,
            text.to_str().ok_or("The field text must be UTF-8.")?,
        ),
        [arg, reference] if arg == "click" => Action::Click(element_ref(reference)?),
        [arg, path]
            if arg == "screenshot" && Path::new(path).extension() == Some(OsStr::new("png")) =>
        {
            Action::Screenshot(env::current_dir()?.join(path))
        }
        [arg] if arg == "preview" => Action::Preview,
        [arg] if arg == "close" => Action::Close,
        _ => return Err("Invalid browser command or arguments. See ox browser --help.".into()),
    };
    Ok((label, action))
}

fn element_ref(reference: &OsStr) -> Result<&OsStr, RelayError> {
    if reference
        .to_str()
        .and_then(|r| r.strip_prefix("@e"))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    {
        Ok(reference)
    } else {
        Err("Use an element reference such as @e1 from ox browser snapshot.".into())
    }
}

fn local_url(input: &str) -> Result<Url, RelayError> {
    if let Ok(url) = Url::parse(input)
        && matches!(url.scheme(), "http" | "https")
        && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
        && url.username().is_empty()
        && url.password().is_none()
    {
        return Ok(url);
    }
    Err(
        "Enter a local HTTP or HTTPS URL for your dev server, such as http://localhost:3000."
            .into(),
    )
}

struct Asset {
    name: &'static str,
    size: u64,
    sha256: &'static str,
}

fn asset() -> Result<Asset, RelayError> {
    // SHA-256 digests from the GitHub v0.45.0 release metadata.
    let (name, size, sha256) = match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => (
            "agent-browser-linux-x64",
            23_180_784,
            "5eff2baaabb337ec728e4571ccfb228ad1fb6babce529fcaf7a6062fcdec6087",
        ),
        ("linux", "aarch64") => (
            "agent-browser-linux-arm64",
            20_192_928,
            "dcdab91ae7ce8ee3bd6a189e3e5ce41b8f713e4b07c25cb18506b984960cd224",
        ),
        ("macos", "x86_64") => (
            "agent-browser-darwin-x64",
            22_157_960,
            "4696167af4c90394c1cd1da78d461f39bd074d3e0d686ac57cf67b29e28b2171",
        ),
        ("macos", "aarch64") => (
            "agent-browser-darwin-arm64",
            20_087_232,
            "bdd665cddefcc61488e0ffe383d7aee33d47b7a6b5b36dc75ae10d1e827826f7",
        ),
        _ => return Err("OX Browser is available on x86-64 and ARM64 Linux and macOS.".into()),
    };
    Ok(Asset { name, size, sha256 })
}

fn absolute_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn component_dir() -> Result<PathBuf, RelayError> {
    let base = absolute_env("XDG_CACHE_HOME")
        .or_else(|| {
            absolute_env("HOME").map(|home| {
                home.join(if cfg!(target_os = "macos") {
                    "Library/Caches"
                } else {
                    ".cache"
                })
            })
        })
        .ok_or("Set HOME or XDG_CACHE_HOME to an absolute directory.")?;
    Ok(base.join("ox/browser"))
}

fn worktree_root(cwd: &Path) -> &Path {
    cwd.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .unwrap_or(cwd)
}

fn session_name(worktree: &Path, label: &str) -> String {
    let mut sha = digest::Context::new(&digest::SHA256);
    sha.update(worktree.as_os_str().as_encoded_bytes());
    sha.update(b"\0");
    sha.update(label.as_bytes());
    format!("ox-{}", hex::encode(&sha.finish().as_ref()[..12]))
}

fn system_browser() -> Result<Option<PathBuf>, RelayError> {
    if let Some(path) = env::var_os("OX_BROWSER_EXECUTABLE").map(PathBuf::from) {
        if path.is_absolute() && is_executable(&path) {
            return Ok(Some(path));
        }
        return Err(
            "OX_BROWSER_EXECUTABLE must point to an absolute, executable Chromium path.".into(),
        );
    }
    if cfg!(target_os = "macos") {
        let path = "Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
        let system = Path::new("/").join(path);
        return Ok([
            Some(system),
            absolute_env("HOME").map(|home| home.join(path)),
        ]
        .into_iter()
        .flatten()
        .find(|path| is_executable(path)));
    }
    let Some(path) = env::var_os("PATH") else {
        return Ok(None);
    };
    Ok(env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .flat_map(|dir| {
            [
                "chromium",
                "chromium-browser",
                "google-chrome",
                "google-chrome-stable",
            ]
            .map(|name| dir.join(name))
        })
        .find(|path| is_executable(path)))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o100 != 0)
}

fn managed_cli(root: &Path) -> PathBuf {
    root.join(format!("agent-browser-{VERSION}"))
}

fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(path)
}

fn user_id() -> u32 {
    // SAFETY: geteuid reads the current process user ID.
    unsafe { libc::geteuid() }
}

fn private_directory(path: &Path) -> Result<bool, RelayError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    if !metadata.is_dir() || metadata.uid() != user_id() || metadata.mode() & 0o777 != 0o700 {
        return Err(format!(
            "The OX Browser directory must be private and owned by you: {}",
            path.display()
        )
        .into());
    }
    Ok(true)
}

fn private_executable(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.is_file() && meta.uid() == user_id() && meta.mode() & 0o777 == 0o700),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn socket_dir(root: &Path, create: bool) -> Result<Option<PathBuf>, RelayError> {
    // Keep the path below the Unix socket path limit, even in long home paths.
    let uid = user_id();
    let namespace = session_name(root, VERSION);
    let dir = Path::new("/tmp").join(format!("ox-browser-{uid}-{namespace}"));
    if create {
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(private_directory(&dir)?.then_some(dir))
}

fn child_command(root: &Path) -> Command {
    let mut command = Command::new(managed_cli(root));
    // The browser keeps its own agent-browser settings and inherits no provider credentials.
    command.env_clear().env("HOME", root).current_dir(root);
    for key in ["PATH", "LANG", "LC_ALL", "LC_CTYPE"] {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
    command
}

fn download_client() -> Result<reqwest::Client, RelayError> {
    let mut builder = crate::update::release_client_builder();
    // Honor the user's system CA bundle, for example on a managed network.
    // The asset must independently match the embedded release digest.
    if let Some(pem) = env::var_os("SSL_CERT_FILE").filter(|value| !value.is_empty()) {
        for certificate in reqwest::Certificate::from_pem_bundle(&fs::read(pem)?)? {
            builder = builder.add_root_certificate(certificate);
        }
    }
    Ok(builder.build()?)
}

fn verify_binary(path: &Path, asset: &Asset) -> io::Result<bool> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if file.metadata()?.len() != asset.size {
        return Ok(false);
    }
    let mut sha = digest::Context::new(&digest::SHA256);
    let mut buf = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        sha.update(&buf[..read]);
    }
    Ok(hex::encode(sha.finish()) == asset.sha256)
}

async fn install_cli(root: &Path) -> Result<(), RelayError> {
    let asset = asset()?;
    let dest = managed_cli(root);
    if private_executable(&dest)? && verify_binary(&dest, &asset)? {
        return Ok(());
    }
    println!("Downloading agent-browser {VERSION}…");
    let url = format!(
        "https://github.com/vercel-labs/agent-browser/releases/download/v{VERSION}/{}",
        asset.name
    );
    let mut response =
        download_client()?.get(url).send().await.map_err(|error| {
            format!("Could not download agent-browser: {error}.\n  {INSTALL_HINT}")
        })?;
    if response.status() != StatusCode::OK {
        return Err(format!(
            "agent-browser download returned HTTP {}.\n  {INSTALL_HINT}",
            response.status()
        )
        .into());
    }
    let mut stage = tempfile::NamedTempFile::new_in(root)?;
    let mut sha = digest::Context::new(&digest::SHA256);
    let mut bytes = 0;
    while let Some(chunk) = response.chunk().await? {
        bytes += chunk.len() as u64;
        if bytes > asset.size {
            return Err("agent-browser download exceeds its published size.".into());
        }
        stage.write_all(&chunk)?;
        sha.update(&chunk);
    }
    if bytes != asset.size || hex::encode(sha.finish()) != asset.sha256 {
        return Err("agent-browser download failed SHA-256 verification.".into());
    }
    stage
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o700))?;
    stage.as_file().sync_all()?;
    stage.persist(dest)?;
    Ok(())
}

pub(crate) async fn run(args: &[OsString]) -> Result<u8, RelayError> {
    let (label, action) = parse(args)?;
    if matches!(action, Action::Help) {
        print_usage();
        return Ok(0);
    }
    let root = component_dir()?;
    if matches!(action, Action::Install) {
        private_dir(&root)?;
        if !private_directory(&root)? {
            return Err("The OX Browser directory became unavailable. Retry installation.".into());
        }
        let _lock = crate::update::store_lock(&root, false)?;
        let browser = system_browser()?;
        install_cli(&root).await?;
        if let Some(browser) = browser {
            println!(
                "agent-browser {VERSION} is ready with {}.",
                browser.display()
            );
        } else {
            if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
                return Err("Install Chromium from your system package manager, then run ox browser install again.".into());
            }
            println!("Installing Chrome for Testing…");
            let mut command = child_command(&root);
            for key in DOWNLOAD_ENV {
                if let Some(value) = env::var_os(key) {
                    command.env(key, value);
                }
            }
            let status = command.arg("install").status()?;
            if !status.success() {
                return Ok(exit_code(status));
            }
            println!("agent-browser {VERSION} is ready with Chrome for Testing.");
        }
        return Ok(0);
    }
    if !private_directory(&root)? || !private_executable(&managed_cli(&root))? {
        let message = format!("OX Browser is not installed.\n  {INSTALL_HINT}");
        if matches!(action, Action::Preview) {
            eprintln!("ox: {message}");
            return Ok(2);
        }
        return Err(message.into());
    }
    let cwd = env::current_dir()?.canonicalize()?;
    let session = session_name(worktree_root(&cwd), label);
    let require_existing = !matches!(action, Action::Open(_));
    let sockets = socket_dir(&root, !require_existing)?;
    let Some(sockets) = sockets.filter(|dir| {
        // The pinned CLI can otherwise start a daemon during inspection even
        // though the user's browser has closed or hasn't opened yet.
        !require_existing || UnixStream::connect(dir.join(format!("{session}.sock"))).is_ok()
    }) else {
        return match action {
            Action::Preview => Ok(preview_unavailable()),
            Action::Close => Ok(0),
            _ => Err("Open your app with ox browser open before using this session.".into()),
        };
    };
    let mut command = child_command(&root);
    command.env("AGENT_BROWSER_SOCKET_DIR", sockets);
    command.args([
        "--session",
        &session,
        "--allowed-domains",
        LOCAL_HOSTS,
        "--idle-timeout",
        "10m",
        "--content-boundaries",
    ]);
    if matches!(action, Action::Open(_))
        && let Some(browser) = system_browser()?
    {
        command.arg("--executable-path").arg(
            browser
                .to_str()
                .ok_or("The Chromium executable path must be UTF-8.")?,
        );
    }
    match action {
        Action::Open(url) => {
            command.args(["open", url.as_str()]);
        }
        Action::Snapshot { json } => {
            command.args(["snapshot", "--compact"]);
            if json {
                command.arg("--json");
            }
        }
        Action::Fill(reference, text) => {
            command.arg("fill").arg(reference).arg("--").arg(text);
        }
        Action::Click(reference) => {
            command.arg("click").arg(reference);
        }
        Action::Screenshot(path) => {
            command
                .arg("screenshot")
                .arg(path.to_str().ok_or("The screenshot path must be UTF-8.")?);
        }
        Action::Preview => return preview(&mut command),
        Action::Close => {
            command.arg("close");
        }
        Action::Help | Action::Install => unreachable!(),
    }
    // Use the caller's process for ordinary actions so the wrapper does not
    // keep an additional process alive while Chromium handles the command.
    Err(format!("Could not start agent-browser: {}.", command.exec()).into())
}

fn preview(command: &mut Command) -> Result<u8, RelayError> {
    let output = command.args(["stream", "status", "--json"]).output()?;
    if !output.status.success() {
        std::io::stderr().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        return Ok(exit_code(output.status));
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let state = &json["data"];
    if state["connected"].as_bool() == Some(true)
        && let Some(port) = state["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
        && port != 0
    {
        println!("http://127.0.0.1:{port}");
        return Ok(0);
    }
    Ok(preview_unavailable())
}

fn preview_unavailable() -> u8 {
    eprintln!("ox: Open your app with ox browser open before opening its preview.");
    3
}

fn exit_code(status: ExitStatus) -> u8 {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_urls_are_http_loopback_without_credentials() {
        for url in [
            "http://localhost:3000/path",
            "https://127.0.0.1:8443",
            "http://[::1]:8000",
        ] {
            assert!(local_url(url).is_ok(), "{url}");
        }
        for url in [
            "localhost:3000",
            "https://localhost.example",
            "http://127.0.0.2:3000",
            "https://user:pass@localhost",
            "ftp://localhost/",
            "file:///tmp/page.html",
        ] {
            assert!(local_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn scopes_follow_worktrees_and_keep_named_sessions_separate() {
        let root = tempfile::tempdir().unwrap();
        let left = root.path().join("left");
        let right = root.path().join("right");
        let child = left.join("src");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir(&right).unwrap();
        fs::write(left.join(".git"), "gitdir: /elsewhere/worktrees/left").unwrap();
        fs::create_dir(right.join(".git")).unwrap();

        assert_eq!(worktree_root(&child), left);
        assert_eq!(worktree_root(&right), right);
        assert_ne!(
            session_name(&left, "default"),
            session_name(&right, "default"),
        );
        assert_ne!(
            session_name(&left, "default"),
            session_name(&left, "review"),
        );
    }

    #[test]
    fn existing_component_must_match_the_size_and_hash_at_install() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("browser");
        let bytes = b"verified release";
        let asset = Asset {
            name: "browser-test",
            size: bytes.len() as u64,
            sha256: "1ce4572138ddacf54f7b7834f96aef9b61cc975676daa26fef2fbdf5c7a2d4bf",
        };
        assert!(!verify_binary(&path, &asset).unwrap());
        fs::write(&path, bytes).unwrap();
        assert!(verify_binary(&path, &asset).unwrap());
        fs::write(&path, b"tampered release").unwrap();
        assert!(!verify_binary(&path, &asset).unwrap());
    }

    #[test]
    fn managed_paths_must_be_private_and_exclude_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        assert!(!private_directory(&root).unwrap());
        private_dir(&root).unwrap();
        assert!(private_directory(&root).unwrap());
        let binary = managed_cli(&root);
        fs::write(&binary, "component").unwrap();
        assert!(!private_executable(&binary).unwrap());
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(private_executable(&binary).unwrap());
        let link = root.join("link");
        std::os::unix::fs::symlink(binary, &link).unwrap();
        assert!(!private_executable(&link).unwrap());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_directory(&root).is_err());
    }
}

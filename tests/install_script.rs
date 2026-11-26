//! Runs the real `install.sh` against a local directory of release files.
//!
//! No network: `OX_INSTALL_BASE_URL` points at a `file://` directory holding a
//! fake `ox` archive, so this checks the download, checksum, unpack, atomic
//! install and messages of the installer on the machine running the tests.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE_OX: &str = "#!/bin/sh\necho \"ox 9.9.9-dev.1\"\n";

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let path = std::env::temp_dir().join(format!("ox-{name}-{}", hex::encode(bytes)));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn has(tool: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {tool} >/dev/null 2>&1")])
        .status()
        .is_ok_and(|status| status.success())
}

/// The target triple install.sh will choose on this machine.
fn target() -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return None,
    };
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-gnu",
        "macos" => "apple-darwin",
        _ => return None,
    };
    Some(format!("{arch}-{os}"))
}

fn sha256(path: &Path) -> String {
    let output = if has("sha256sum") {
        Command::new("sha256sum").arg(path).output()
    } else {
        Command::new("shasum")
            .args(["-a", "256"])
            .arg(path)
            .output()
    }
    .expect("hash the archive");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)[..64].to_owned()
}

/// Writes `ox-<target>.tar.gz` and its `.sha256` file into `directory`.
fn write_release(directory: &Path, target: &str) {
    let source = directory.join("source");
    fs::create_dir_all(&source).unwrap();
    let binary = source.join("ox");
    fs::write(&binary, FAKE_OX).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let archive = directory.join(format!("ox-{target}.tar.gz"));
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&source)
        .arg("ox")
        .status()
        .expect("run tar");
    assert!(status.success());
    fs::remove_dir_all(&source).unwrap();
    fs::write(
        directory.join(format!("ox-{target}.tar.gz.sha256")),
        format!("{} *ox-{target}.tar.gz\n", sha256(&archive)),
    )
    .unwrap();
}

fn install(base: &Path, install_dir: &Path, home: &Path, arguments: &[&str]) -> Output {
    Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .args(arguments)
        .env("OX_INSTALL_BASE_URL", format!("file://{}", base.display()))
        .env("OX_INSTALL_DIR", install_dir)
        .env("OX_INSTALL_SKIP_GLIBC_CHECK", "1")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("share"))
        .env("SHELL", "/bin/zsh")
        .env_remove("OX_CHANNEL")
        .output()
        .expect("run install.sh")
}

fn ready() -> Option<String> {
    let target = target()?;
    let tools = has("curl") && has("tar") && (has("sha256sum") || has("shasum"));
    if !tools {
        eprintln!("skipping: install.sh tests need curl, tar and sha256sum or shasum");
        return None;
    }
    Some(target)
}

#[test]
fn installs_and_upgrades_from_a_verified_archive() {
    let Some(target) = ready() else { return };
    let root = TempDir::new("install-ok");
    let release = root.0.join("release");
    fs::create_dir_all(&release).unwrap();
    write_release(&release, &target);
    let bin = root.0.join("home/.local/bin");

    for round in ["fresh install", "upgrade"] {
        let output = install(&release, &bin, &root.0.join("home"), &[]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{round}: {stdout}\n{stderr}");
        assert!(
            stdout.contains("Installed ox 9.9.9-dev.1"),
            "{round}: {stdout}"
        );
        assert!(stdout.contains("ox setup"), "{round}: {stdout}");
        // The install directory is not on PATH, so the zsh hint is printed.
        assert!(stdout.contains("is not on your PATH"), "{round}: {stdout}");
        assert!(stdout.contains(">> ~/.zshrc"), "{round}: {stdout}");
        // Output is not a terminal here, so it must not contain colour codes.
        assert!(!stdout.contains('\u{1b}'), "{round}: {stdout}");
        assert_eq!(fs::read_to_string(bin.join("ox")).unwrap(), FAKE_OX);
        let mode = fs::metadata(bin.join("ox")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "{round}");
        // No staging file is left next to the binary.
        assert!(bin.join(".ox-cli-update").is_dir());
        assert_eq!(fs::read_dir(&bin).unwrap().count(), 2, "{round}");
    }
}

#[test]
fn refuses_a_wrong_checksum_a_missing_build_and_unknown_options() {
    let Some(target) = ready() else { return };
    let root = TempDir::new("install-bad");
    let release = root.0.join("release");
    fs::create_dir_all(&release).unwrap();
    write_release(&release, &target);
    fs::write(
        release.join(format!("ox-{target}.tar.gz.sha256")),
        format!("{} *ox-{target}.tar.gz\n", "0".repeat(64)),
    )
    .unwrap();
    let bin = root.0.join("bin");
    let home = root.0.join("home");

    let output = install(&release, &bin, &home, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("checksum mismatch"), "{stderr}");
    assert!(!bin.join("ox").exists());

    let empty = root.0.join("empty");
    fs::create_dir_all(&empty).unwrap();
    let output = install(&empty, &bin, &home, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("could not download ox"), "{stderr}");
    assert!(!bin.join("ox").exists());

    let output = install(&release, &bin, &home, &["--nightly"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("unknown option: --nightly"), "{stderr}");

    let output = install(&release, &bin, &home, &["--help"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: install.sh [--stable|--dev]"));
}

#[cfg(target_os = "linux")]
#[test]
fn optional_desktop_installs_launches_and_preserves_existing_profile() {
    if ready().is_none() {
        return;
    }
    let root = TempDir::new("desktop-install");
    let release = root.0.join("release");
    let source = release.join("ox-desktop");
    let home = root.0.join("home");
    let bin = home.join("bin with space %");
    let project = home.join("project with $ and ;");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&project).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_ox"), source.join("ox")).unwrap();
    let host = source.join("ox-host");
    fs::write(&host, "#!/bin/sh\nif [ \"$1\" = --version ]; then \"$(dirname \"$0\")/ox\" --version | sed 's/^ox /ox-host /'; fi\n").unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
    let gui = source.join("ox-desktop");
    fs::write(&gui, "#!/bin/sh\nif [ \"$1\" = --version ]; then \"$(dirname \"$0\")/ox\" --version | sed 's/^ox /ox-desktop /'; else printf '%s' \"$1\" > \"$HOME/opened\"; fi\n").unwrap();
    fs::set_permissions(&gui, fs::Permissions::from_mode(0o755)).unwrap();
    for file in ["dev.ox.desktop.desktop", "ox-desktop.png"] {
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("desktop/packaging")
                .join(file),
            source.join(file),
        )
        .unwrap();
    }
    let asset = format!("ox-desktop-linux-{}.tar.gz", std::env::consts::ARCH);
    let archive = release.join(&asset);
    let checksum = release.join(format!("{asset}.sha256"));
    let pack = || {
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&release)
                .arg("ox-desktop")
                .status()
                .unwrap()
                .success()
        );
        fs::write(&checksum, format!("{}  {asset}\n", sha256(&archive))).unwrap();
    };
    pack();
    let run = || install(&release, &bin, &home, &["--desktop"]);
    let installed = run();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let installed_gui = fs::read_link(bin.join("ox-desktop")).unwrap();
    let entry = home.join("share/applications/dev.ox.desktop.desktop");
    assert!(
        fs::read_to_string(&entry)
            .unwrap()
            .contains("bin with space %%/ox-desktop")
    );
    if has("desktop-file-validate") {
        assert!(
            Command::new("desktop-file-validate")
                .arg(entry)
                .status()
                .unwrap()
                .success()
        );
    }
    let config = home.join(".codex/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "# existing user settings\n").unwrap();
    assert!(run().status.success());
    assert_eq!(
        fs::read_link(bin.join("ox-desktop")).unwrap(),
        installed_gui
    );
    assert!(
        Command::new(bin.join("ox"))
            .arg("desktop")
            .arg(&project)
            .env("HOME", &home)
            .env_remove("CODEX_HOME")
            .status()
            .unwrap()
            .success()
    );
    let marker = home.join("opened");
    for _ in 0..100 {
        if fs::read_to_string(&marker).ok().as_deref() == project.to_str() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        fs::read_to_string(marker).unwrap(),
        project.to_str().unwrap()
    );
    fs::write(&checksum, format!("{}  {asset}\n", "0".repeat(64))).unwrap();
    assert!(!run().status.success());
    fs::write(&gui, "#!/bin/sh\necho ox-desktop 0.0.0-incompatible\n").unwrap();
    pack();
    let mismatch = run();
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("versions differ"));
    assert_eq!(
        fs::read_link(bin.join("ox-desktop")).unwrap(),
        installed_gui
    );
    assert_eq!(
        fs::read_to_string(config).unwrap(),
        "# existing user settings\n"
    );
    // Stage the next verified generation. The public entry points must continue
    // resolving to the old package until explicit, offline CLI consent.
    let store = bin.join(".ox-desktop");
    let current = store.join("current").canonicalize().unwrap();
    let digest = "a".repeat(64);
    let next = store.join(format!("release-{digest}"));
    let next_bin = next.join("ox-desktop");
    fs::create_dir_all(&next_bin).unwrap();
    for name in ["ox", "ox-host", "ox-desktop"] {
        let file = next_bin.join(name);
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in --version) echo '{name} 9.9.9';; restart-if-updated) [ -f \"$HOME/fail-restart\" ] && exit 1; touch \"$HOME/restarted\";; esac\n"
        );
        fs::write(&file, script).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(bin.join(name).canonicalize().unwrap().starts_with(&current));
    }
    fs::write(next.join(".ready"), "9.9.9").unwrap();
    let staged = serde_json::json!({"version": "9.9.9", "digest": digest});
    fs::write(
        store.join("pending.json"),
        serde_json::to_vec(&(current.clone(), &staged)).unwrap(),
    )
    .unwrap();
    let update = |args: &[&str]| {
        Command::new(bin.join("ox"))
            .arg("update")
            .args(args)
            .env("HOME", &home)
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .output()
            .unwrap()
    };
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&update(&["--stage-desktop"]).stdout).unwrap(),
        staged
    );
    assert_eq!(store.join("current").canonicalize().unwrap(), current);
    assert!(!home.join("restarted").exists());
    assert!(
        !update(&["--apply-desktop", "9.9.9", "../bad"])
            .status
            .success()
    );
    fs::write(home.join("fail-restart"), "").unwrap();
    let failed = update(&["--apply"]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("OX 9.9.9 is installed"));
    assert!(store.join("pending.json").is_file());
    assert_eq!(store.join("current").canonicalize().unwrap(), next);
    fs::remove_file(home.join("fail-restart")).unwrap();
    let applied = Command::new(current.join("ox-desktop/ox"))
        .args(["update", "--apply-desktop", "9.9.9", &digest])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(home.join("restarted").exists());
    for name in ["ox", "ox-host", "ox-desktop"] {
        assert_eq!(bin.join(name).canonicalize().unwrap(), next_bin.join(name));
    }
}

#[test]
fn standalone_applies_a_staged_binary_offline_and_rejects_cache_corruption() {
    let dir = TempDir::new("cli-staged");
    let cli = dir.0.join("ox");
    fs::copy(env!("CARGO_BIN_EXE_ox"), &cli).unwrap();
    let version_line = Command::new(&cli).arg("--version").output().unwrap().stdout;
    let installed = std::str::from_utf8(&version_line)
        .unwrap()
        .trim()
        .strip_prefix("ox ")
        .unwrap();
    let cache = dir.0.join(".ox-cli-update");
    fs::create_dir(&cache).unwrap();
    let stage = || {
        fs::write(cache.join("download"), FAKE_OX).unwrap();
        let row = serde_json::json!({"installed": installed, "version": "9.9.9-dev.1",
            "size": FAKE_OX.len(), "digest": sha256(&cache.join("download"))});
        fs::write(cache.join("pending.json"), row.to_string()).unwrap();
    };
    let apply = || {
        Command::new(&cli)
            .args(["update", "--apply"])
            .output()
            .unwrap()
    };
    stage();
    fs::write(cache.join("download"), "x".repeat(FAKE_OX.len())).unwrap();
    assert!(!apply().status.success());
    assert_eq!(
        Command::new(&cli).arg("--version").output().unwrap().stdout,
        version_line
    );
    stage();
    assert!(apply().status.success());
    assert_eq!(
        Command::new(&cli).arg("--version").output().unwrap().stdout,
        b"ox 9.9.9-dev.1\n"
    );
    assert!(!cache.join("pending.json").exists());
}

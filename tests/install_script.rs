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
        assert_eq!(fs::read_dir(&bin).unwrap().count(), 1, "{round}");
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

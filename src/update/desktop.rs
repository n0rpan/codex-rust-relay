//! Stage a verified whole release in its final filesystem before asking to activate it.
use super::*;
use crate::managed_install::{BIN_DIR, active_binary};
use serde::{Deserialize, Serialize};
use std::{fs, os::unix::fs::symlink, process::Command as Process};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Candidate {
    version: String,
    digest: String,
}

impl Candidate {
    fn generation(&self, store: &Path) -> Result<PathBuf, RelayError> {
        if parse_version(&self.version).is_none()
            || self.digest.len() != 64
            || !self.digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("Invalid staged desktop release.".into());
        }
        Ok(store.join(format!("release-{}", self.digest)))
    }
}

fn archive(target: &str) -> Result<&'static str, RelayError> {
    Ok(match target {
        "aarch64-apple-darwin" => "ox-desktop-macos-arm64.zip",
        "x86_64-apple-darwin" => "ox-desktop-macos-x86_64.zip",
        "aarch64-unknown-linux-gnu" => "ox-desktop-linux-aarch64.tar.gz",
        "x86_64-unknown-linux-gnu" => "ox-desktop-linux-x86_64.tar.gz",
        _ => return Err("Unsupported OX Desktop platform.".into()),
    })
}

fn pending(store: &Path) -> Option<Candidate> {
    let (base, candidate): (PathBuf, Candidate) =
        serde_json::from_reader(File::open(store.join("pending.json")).ok()?).ok()?;
    let current = store.join("current").canonicalize().ok()?;
    if current != base && current != candidate.generation(store).ok()? {
        return None;
    }
    let generation = candidate.generation(store).ok()?;
    (fs::read_to_string(generation.join(".ready")).ok()? == candidate.version).then_some(candidate)
}

pub(super) fn pending_version(store: &Path) -> Option<String> {
    pending(store).map(|c| c.version)
}

pub(super) async fn public_command(store: &Path, action: &str) -> Result<u8, RelayError> {
    if action == "--apply" {
        let _lock = store_lock(store, false)?;
        let candidate =
            pending(store).ok_or("No staged desktop update. Run ox update --stage first.")?;
        activate(store, &candidate, None)?;
        println!("Installed OX {}.", candidate.version);
        return Ok(0);
    }
    let _lock = store_lock(store, action == "--background")?;
    if let Some(candidate) = stage_for_caller(store, action == "--background").await? {
        if candidate.digest == "current" {
            println!(
                "OX {} is installed. Reopen OX Desktop to use it.",
                candidate.version
            );
        } else {
            println!(
                "OX {} is downloaded and verified. Run ox update to install it.",
                candidate.version
            );
        }
    } else {
        println!("OX {CURRENT} is up to date.");
    }
    Ok(0)
}

fn active_caller(store: &Path) -> Result<bool, RelayError> {
    Ok(active_binary(store, "ox").canonicalize()? == std::env::current_exe()?.canonicalize()?)
}

fn host_restart(store: &Path, version: &str, stopped_host: Option<File>) -> Result<(), RelayError> {
    // The desktop passed the daemon's ownership lock on stdin. Release it
    // after switching, before marker recovery can start the newly selected host.
    if let Some(lock) = stopped_host {
        lock.unlock()?;
    }

    let output = Process::new(active_binary(store, "ox-host"))
        .arg("restart-if-updated")
        .output().map_err(|e| format!("OX {version} is installed. Could not start the session host: {e}. Retry the restart."))?;
    if !output.status.success() {
        return Err(format!(
            "OX {version} is installed. Retry the session host restart or run ox host restart. {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(())
}

pub(super) fn select_installer_package(executable: &Path) -> Result<u8, RelayError> {
    let generation = crate::managed_install::generation_dir(executable)
        .filter(|p| p.join(".ready").is_file())
        .ok_or("Expected a verified OX installer package.")?;
    let store = generation.parent().ok_or("Missing OX package store.")?;
    let _lock = store_lock(store, false)?;
    switch_generation(store, &generation)?;
    // Only a package selected by a lease-aware installer is eligible for
    // cleanup. In-flight extractions and legacy, unleased packages stay intact.
    if generation.join(".in-use").is_file() {
        File::create(generation.join(".installer-managed"))?;
    }
    prune_unused(store, &generation);
    Ok(0)
}

fn switch_generation(store: &Path, generation: &Path) -> Result<(), RelayError> {
    let current = store.join("current");
    let staging = tempfile::tempdir_in(store)?;
    if let Ok(old) = current.canonicalize() {
        if old == generation {
            return Ok(());
        }
        let previous = staging.path().join("previous");
        symlink(old, &previous)?;
        fs::rename(previous, store.join("previous"))?;
    }
    let link = staging.path().join("current");
    symlink(generation, &link)?;
    fs::rename(link, current)?;
    Ok(())
}

fn activate(
    store: &Path,
    candidate: &Candidate,
    stopped_host: Option<File>,
) -> Result<(), RelayError> {
    let generation = candidate.generation(store)?;
    if fs::read_to_string(generation.join(".ready"))? != candidate.version {
        return Err("The staged OX release is incomplete.".into());
    }
    let current = store.join("current");
    if current.canonicalize()? != generation {
        if !active_caller(store)? || pending(store).as_ref() != Some(candidate) {
            return Err("The active or staged OX release changed. Check for updates again.".into());
        }
        verify_package(&generation, &candidate.version)?;
        switch_generation(store, &generation)?;
    }
    // Keep the pending record on failure. A retry can finish an interrupted host drain.
    host_restart(store, &candidate.version, stopped_host)?;
    prune_unused(store, &generation);
    if pending(store).as_ref() == Some(candidate) {
        fs::remove_file(store.join("pending.json"))?;
    }
    Ok(())
}

fn prune_unused(store: &Path, active: &Path) {
    let Ok(previous) = store.join("previous").canonicalize() else {
        return;
    };
    let Ok(entries) = fs::read_dir(store) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let digest = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
        let owned = name.to_str().is_some_and(|s| {
            s.strip_prefix("release-").is_some_and(digest)
                || s.split_once('.').is_some_and(|(hash, suffix)| {
                    digest(hash)
                        && suffix.len() == 6
                        && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
                        && path.join(".installer-managed").is_file()
                })
        });
        if !owned
            || path == active
            || path == previous
            || !entry.file_type().is_ok_and(|t| t.is_dir())
        {
            continue;
        }
        if let Ok(lease) = File::open(path.join(".in-use"))
            && lease.try_lock().is_ok()
        {
            // Remove the published name atomically before recursive cleanup.
            // An interrupted deletion can never be mistaken for a reusable release.
            if let Ok(trash) = tempfile::tempdir_in(store) {
                let _ = fs::rename(path, trash.path().join("retired"));
            }
        }
    }
}

async fn stage(
    store: &Path,
    client: &Client,
    url: &str,
    update: &Update,
) -> Result<Candidate, RelayError> {
    let base = store.join("current").canonicalize()?;
    let candidate = Candidate {
        version: update.version.clone(),
        digest: hex::encode(update.sha256),
    };
    let generation = candidate.generation(store)?;
    if fs::read_to_string(generation.join(".ready"))
        .ok()
        .as_deref()
        != Some(&candidate.version)
        || verify_package(&generation, &candidate.version).is_err()
    {
        let mut archive =
            tempfile::NamedTempFile::new_in(store).map_err(|e| write_error(store, e))?;
        download_verified(client, url, update, archive.as_file_mut()).await?;
        let unpack = tempfile::tempdir_in(store)?;
        #[cfg(target_os = "macos")]
        {
            checked(
                Process::new("/usr/bin/ditto")
                    .args(["-x", "-k"])
                    .arg(archive.path())
                    .arg(unpack.path()),
            )
            .map_err(|e| format!("Could not unpack OX {}. {e}", update.version))?;
        }
        #[cfg(not(target_os = "macos"))]
        checked(
            Process::new("tar")
                .arg("-xzf")
                .arg(archive.path())
                .arg("-C")
                .arg(unpack.path()),
        )
        .map_err(|e| format!("Could not unpack OX {}. {e}", update.version))?;
        verify_package(unpack.path(), &update.version)?;
        File::create(unpack.path().join(".in-use"))?;
        fs::write(unpack.path().join(".ready"), &update.version)?;
        if generation.exists() {
            if generation == base {
                return Err("Cannot replace the running OX package. Rerun the installer.".into());
            }
            let lease = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(generation.join(".in-use"))?;
            lease
                .try_lock()
                .map_err(|_| "The damaged OX package is in use. Close its sessions and retry.")?;
            let retired = tempfile::tempdir_in(store)?;
            fs::rename(&generation, retired.path().join("damaged"))?;
        }
        fs::rename(unpack.path(), &generation)?;
    }
    if fs::read_to_string(generation.join(".ready"))? != candidate.version {
        return Err("Cached OX package has an unexpected version.".into());
    }
    let mut record = tempfile::NamedTempFile::new_in(store)?;
    serde_json::to_writer(record.as_file_mut(), &(&base, &candidate))?;
    record.persist(store.join("pending.json"))?;
    Ok(candidate)
}

fn verify_package(root: &Path, version: &str) -> Result<(), RelayError> {
    #[cfg(target_os = "macos")]
    checked(
        Process::new("/usr/bin/codesign")
            .args(["--verify", "--strict", "--deep"])
            .arg(root.join("OX Desktop.app")),
    )
    .map_err(|_| format!("OX {version} failed macOS signature verification."))?;
    let bin = root.join(BIN_DIR);
    for name in ["ox", "ox-desktop", "ox-host"] {
        let reported = checked(Process::new(bin.join(name)).arg("--version"))?;
        if reported.trim() != format!("{name} {version}") {
            return Err(format!("The packaged {name} does not match OX {version}.").into());
        }
    }
    Ok(())
}

fn checked(command: &mut Process) -> Result<String, RelayError> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "{} failed: {}",
            command.get_program().to_string_lossy(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

async fn available(client: &Client, channel: Channel) -> Result<Option<Update>, RelayError> {
    let running = parse_version(CURRENT).ok_or("Invalid installed OX version.")?;
    let target = target_triple(std::env::consts::OS, std::env::consts::ARCH)
        .ok_or("Unsupported OX target.")?;
    let release = release_metadata(client, &channel.metadata_url(), channel).await?;
    select_update(
        release,
        channel,
        channel != Channel::of(running),
        CURRENT,
        target,
        Some(archive(target)?),
    )
}

async fn prepare(store: &Path, channel: Channel) -> Result<Option<Candidate>, RelayError> {
    let running = parse_version(CURRENT).ok_or("Invalid installed OX version.")?;
    if let Some(candidate) = pending(store)
        && let Some(version) = parse_version(&candidate.version)
        && Channel::of(version) == channel
        && (version >= running || channel != Channel::of(running))
    {
        if verify_package(&candidate.generation(store)?, &candidate.version).is_ok() {
            return Ok(Some(candidate));
        }
        fs::remove_file(store.join("pending.json"))?;
    }
    let client = release_client()?;
    let Some(update) = available(&client, channel).await? else {
        return Ok(None);
    };
    Ok(Some(
        stage(store, &client, &channel.download_url(&update), &update).await?,
    ))
}

pub(super) async fn update(
    store: &Path,
    channel: Channel,
    check_only: bool,
    json: bool,
) -> Result<u8, RelayError> {
    if check_only {
        let version = available(&release_client()?, channel)
            .await?
            .map(|u| u.version);
        if json {
            println!(
                "{}",
                serde_json::json!({"installed": CURRENT, "available": version})
            );
        } else if let Some(version) = version {
            println!(
                "OX {version} is available. Run ox update {} to install the complete package.",
                channel.flag()
            );
        } else {
            println!("OX {CURRENT} is up to date.");
        }
        return Ok(0);
    }
    let _lock = store_lock(store, false)?;
    if !active_caller(store)? {
        return Err("Run the installed ox launcher to update the active package.".into());
    }
    if let Some(candidate) = record_check(store, prepare(store, channel).await)? {
        activate(store, &candidate, None)?;
        println!(
            "Installed OX {}. Reopen any running OX Desktop windows.",
            candidate.version
        );
    } else {
        println!("OX {CURRENT} is up to date.");
    }
    Ok(0)
}

async fn stage_for_caller(store: &Path, background: bool) -> Result<Option<Candidate>, RelayError> {
    if !active_caller(store)? {
        // A terminal or another window already switched releases.
        let output = checked(Process::new(active_binary(store, "ox")).arg("--version"))?;
        let version = output
            .trim()
            .strip_prefix("ox ")
            .ok_or("Invalid installed OX version.")?;
        if parse_version(version).ok_or("Invalid installed OX version.")?
            <= parse_version(CURRENT).ok_or("Invalid running OX version.")?
        {
            return Ok(None);
        }
        return Ok(Some(Candidate {
            version: version.into(),
            digest: "current".into(),
        }));
    }
    if background && recent_check(store) && pending(store).is_none() {
        return Ok(None);
    }
    let channel = Channel::of(parse_version(CURRENT).ok_or("Invalid installed OX version.")?);
    if background && retry_later(store) && pending(store).is_none() {
        return Err("The last update check failed. OX will retry automatically.".into());
    }
    record_check(store, prepare(store, channel).await)
}

pub(super) async fn command(
    store: &Path,
    command: &std::ffi::OsStr,
    args: &[OsString],
) -> Result<u8, RelayError> {
    let background = args.len() == 1 && args[0] == "--background";
    let opportunistic = command == "--try-apply-desktop";
    let _lock = if opportunistic {
        let file = open_lock_file(store)?;
        match file.try_lock() {
            Ok(()) => file,
            Err(std::fs::TryLockError::WouldBlock) => {
                println!("busy");
                return Ok(0);
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    } else {
        store_lock(store, background)?
    };
    let (args, stopped_host) =
        if opportunistic && args.last().is_some_and(|s| s == "--host-stopped") {
            use std::os::fd::AsFd;
            (
                &args[..args.len() - 1],
                Some(File::from(std::io::stdin().as_fd().try_clone_to_owned()?)),
            )
        } else {
            (args, None)
        };
    if background && !automatic_enabled() {
        println!("null");
        return Ok(0);
    }
    if command == "--stage-desktop" && (args.is_empty() || background) {
        println!(
            "{}",
            serde_json::to_string(&stage_for_caller(store, background).await?)?
        );
    } else if (command == "--apply-desktop" || opportunistic)
        && let [version, digest] = args
    {
        if digest == "current" {
            let reported = checked(Process::new(active_binary(store, "ox")).arg("--version"))?;
            if reported.trim() != format!("ox {}", version.to_string_lossy()) {
                return Err("The active OX version changed. Check for updates again.".into());
            }
            host_restart(store, &version.to_string_lossy(), stopped_host)?;
            if let Some(candidate) = pending(store)
                && candidate.generation(store)? == store.join("current").canonicalize()?
            {
                fs::remove_file(store.join("pending.json"))?;
            }
        } else {
            activate(
                store,
                &Candidate {
                    version: version.to_string_lossy().into(),
                    digest: digest.to_string_lossy().into(),
                },
                stopped_host,
            )?;
        }
    } else {
        return Err(USAGE.into());
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_published_archive_for_every_supported_target() {
        for (target, expected) in [
            ("aarch64-apple-darwin", "ox-desktop-macos-arm64.zip"),
            ("x86_64-apple-darwin", "ox-desktop-macos-x86_64.zip"),
            (
                "aarch64-unknown-linux-gnu",
                "ox-desktop-linux-aarch64.tar.gz",
            ),
            ("x86_64-unknown-linux-gnu", "ox-desktop-linux-x86_64.tar.gz"),
        ] {
            assert_eq!(archive(target).unwrap(), expected);
        }
    }

    #[test]
    fn prune_preserves_active_previous_and_leased_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().canonicalize().unwrap();
        let packages: Vec<_> = (0..4)
            .map(|n| {
                let path = store.join(format!("release-{n:064x}"));
                fs::create_dir(&path).unwrap();
                File::create(path.join(".in-use")).unwrap();
                path
            })
            .collect();
        let installer = store.join(format!("{:064x}.aB12Cd", 42));
        fs::create_dir(&installer).unwrap();
        File::create(installer.join(".in-use")).unwrap();
        symlink(&packages[1], store.join("previous")).unwrap();
        let lease = File::open(packages[2].join(".in-use")).unwrap();
        lease.lock_shared().unwrap();
        prune_unused(&store, &packages[0]);
        assert!(packages[..3].iter().all(|p| p.exists()));
        assert!(!packages[3].exists());
        assert!(installer.exists());
        File::create(installer.join(".installer-managed")).unwrap();
        let installer_user = File::open(installer.join(".in-use")).unwrap();
        installer_user.lock_shared().unwrap();
        prune_unused(&store, &packages[0]);
        assert!(installer.exists());
        drop(installer_user);
        drop(lease);
        prune_unused(&store, &packages[0]);
        assert!(!packages[2].exists());
        assert!(!installer.exists());
    }

    #[cfg(target_os = "linux")]
    fn archive_fixture(dir: &Path) -> Vec<u8> {
        use std::os::unix::fs::PermissionsExt;
        let package = dir.join(BIN_DIR);
        fs::create_dir(&package).unwrap();
        for name in ["ox", "ox-desktop", "ox-host"] {
            let file = package.join(name);
            fs::write(&file, format!("#!/bin/sh\necho {name} 2.0.0\n")).unwrap();
            fs::set_permissions(file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let output = Process::new("tar")
            .args(["-czf", "-", "-C"])
            .arg(dir)
            .arg(BIN_DIR)
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    }

    #[cfg(target_os = "macos")]
    fn archive_fixture(dir: &Path) -> Vec<u8> {
        let bin = dir.join(BIN_DIR);
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            bin.parent().unwrap().join("Info.plist"),
            include_bytes!("../../desktop/packaging/Info.plist"),
        )
        .unwrap();
        let source = dir.join("version.c");
        fs::write(
            &source,
            r#"#include <stdio.h>
#include <libgen.h>
int main(int argc, char **argv) { printf("%s 2.0.0\n", basename(argv[0])); return 0; }
"#,
        )
        .unwrap();
        for name in ["ox", "ox-desktop", "ox-host"] {
            let binary = bin.join(name);
            checked(Process::new("cc").arg(&source).arg("-o").arg(&binary)).unwrap();
            checked(
                Process::new("codesign")
                    .args(["--force", "--sign", "-"])
                    .arg(binary),
            )
            .unwrap();
        }
        let bundle = dir.join("OX Desktop.app");
        checked(
            Process::new("codesign")
                .args(["--force", "--sign", "-"])
                .arg(&bundle),
        )
        .unwrap();
        let zip = dir.join("fixture.zip");
        checked(
            Process::new("ditto")
                .args(["-c", "-k", "--keepParent"])
                .arg(bundle)
                .arg(&zip),
        )
        .unwrap();
        fs::read(zip).unwrap()
    }

    #[tokio::test]
    async fn staged_package_is_verified_and_offline_ready_before_any_switch() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().canonicalize().unwrap().join(".ox-desktop");
        let old = store.join("initial");
        fs::create_dir_all(&old).unwrap();
        symlink(&old, store.join("current")).unwrap();
        let bytes = Box::leak(archive_fixture(dir.path()).into_boxed_slice());
        let (base, server) = super::super::tests::mock_release(bytes).await;
        let client = Client::builder().no_proxy().build().unwrap();
        let mut update = Update {
            version: "2.0.0".into(),
            asset: archive("aarch64-unknown-linux-gnu").unwrap().into(),
            size: bytes.len() as u64,
            sha256: [0; 32],
        };
        assert!(stage(&store, &client, &base, &update).await.is_err());
        assert!(pending(&store).is_none());
        update
            .sha256
            .copy_from_slice(digest::digest(&digest::SHA256, bytes).as_ref());
        update.version = "2.0.1".into();
        assert!(stage(&store, &client, &base, &update).await.is_err());
        assert!(pending(&store).is_none());
        update.version = "2.0.0".into();
        let staged = stage(&store, &client, &base, &update).await.unwrap();
        let staged_host = staged
            .generation(&store)
            .unwrap()
            .join(BIN_DIR)
            .join("ox-host");
        fs::remove_file(&staged_host).unwrap();
        assert_eq!(
            stage(&store, &client, &base, &update).await.unwrap(),
            staged
        );
        assert!(staged_host.is_file());
        server.abort();
        assert_eq!(pending(&store), Some(staged.clone()));
        assert_eq!(
            stage(&store, &client, &base, &update).await.unwrap(),
            staged
        );
        assert_eq!(fs::read_link(store.join("current")).unwrap(), old);
        // A concurrent installer selecting a different generation invalidates the prompt.
        let next = store.join("other");
        fs::create_dir(&next).unwrap();
        symlink(&next, store.join("new-link")).unwrap();
        fs::rename(store.join("new-link"), store.join("current")).unwrap();
        assert!(pending(&store).is_none());
    }
}

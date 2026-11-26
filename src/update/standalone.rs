//! Only CLI installations created by install.sh opt into background downloads.
use super::*;
use serde::{Deserialize, Serialize};
use std::{fs, io::Read};

pub(super) fn store(exe: &Path) -> Option<PathBuf> {
    let exe = exe.canonicalize().ok()?;
    let cache = exe.with_file_name(".ox-cli-update");
    (exe.ends_with("ox") && cache.is_dir() && !exe.with_file_name("ox-desktop").is_file())
        .then_some(cache)
}

#[derive(Deserialize, Serialize)]
struct Staged {
    installed: String,
    version: String,
    digest: String,
    size: u64,
}

fn pending(cache: &Path) -> Option<Staged> {
    let s: Staged = serde_json::from_reader(File::open(cache.join("pending.json")).ok()?).ok()?;
    (s.installed == CURRENT
        && parse_version(&s.version)? > parse_version(CURRENT)?
        && (1..=MAX_BINARY_BYTES).contains(&s.size)
        && fs::metadata(cache.join("download")).ok()?.len() == s.size)
        .then_some(s)
}

pub(super) fn pending_version(cache: &Path) -> Option<String> {
    pending(cache).map(|s| s.version)
}

async fn prepare(cache: &Path) -> Result<Option<Staged>, RelayError> {
    if let Some(s) = pending(cache) {
        return Ok(Some(s));
    }
    let channel = Channel::of(parse_version(CURRENT).ok_or("Invalid OX version.")?);
    let client = release_client()?;
    let target = target_triple(std::env::consts::OS, std::env::consts::ARCH)
        .ok_or("Unsupported OX platform.")?;
    let Some(update) = latest(
        &client,
        &channel.metadata_url(),
        channel,
        false,
        CURRENT,
        target,
    )
    .await?
    else {
        return Ok(None);
    };
    let mut binary = tempfile::NamedTempFile::new_in(cache).map_err(|e| write_error(cache, e))?;
    download_verified(
        &client,
        &channel.download_url(&update),
        &update,
        binary.as_file_mut(),
    )
    .await?;
    let staged = Staged {
        installed: CURRENT.into(),
        version: update.version,
        digest: hex::encode(update.sha256),
        size: update.size,
    };
    let mut record = tempfile::NamedTempFile::new_in(cache).map_err(|e| write_error(cache, e))?;
    serde_json::to_writer(record.as_file_mut(), &staged)?;
    binary.persist(cache.join("download"))?;
    record.persist(cache.join("pending.json"))?;
    Ok(Some(staged))
}

fn apply(cache: &Path, executable: &Path, staged: &Staged) -> Result<(), RelayError> {
    let installation = Installation::lock(executable)?;
    let path = cache.join("download");
    let mut file = File::open(&path)?;
    let mut sha = digest::Context::new(&digest::SHA256);
    let mut buf = [0; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
    }
    if hex::encode(sha.finish()) != staged.digest {
        fs::remove_file(cache.join("pending.json"))?;
        return Err("The staged CLI failed SHA-256 verification. Run ox update again.".into());
    }
    replace_cli(&path, &installation)?;
    fs::remove_file(cache.join("pending.json"))?;
    Ok(())
}

pub(super) async fn command(
    cache: &Path,
    executable: &Path,
    action: &str,
) -> Result<u8, RelayError> {
    let _lock = store_lock(cache, action == "--background")?;
    if action == "--background" && (recent_check(cache) || retry_later(cache)) {
        return Ok(0);
    }
    let staged = if action == "--apply" {
        Some(pending(cache).ok_or("No staged CLI update. Run ox update --stage first.")?)
    } else {
        record_check(cache, prepare(cache).await)?
    };
    match staged {
        Some(s) if action == "--apply" || action == "update" => {
            apply(cache, executable, &s)?;
            println!(
                "Installed ox {}. Restart running CLI sessions to use it.",
                s.version
            );
        }
        Some(s) => println!(
            "OX {} is downloaded. Run ox update to install it.",
            s.version
        ),
        None => println!("ox {CURRENT} is up to date."),
    }
    Ok(0)
}

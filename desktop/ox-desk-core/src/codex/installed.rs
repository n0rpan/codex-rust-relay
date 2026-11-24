//! Read the installed Codex command schema away from the window. The executable's
//! identity keys the cache so an upgrade can be discovered in a new connection.
use crate::slash::Capabilities;
use std::{
    fs, io,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

type Identity = (PathBuf, u64, u64, i64, i64);
static CACHE: Mutex<Option<(Identity, Capabilities)>> = Mutex::new(None);

pub fn installed(path: &std::ffi::OsStr) -> io::Result<Capabilities> {
    let bin = std::env::split_paths(path)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("codex"))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Install Codex to browse its commands",
            )
        })?;
    let meta = fs::metadata(&bin)?;
    let key = (
        bin.clone(),
        meta.dev(),
        meta.ino(),
        meta.mtime(),
        meta.mtime_nsec(),
    );
    let mut cache = CACHE.lock().unwrap();
    if let Some((cached, caps)) = &*cache
        && cached == &key
    {
        return Ok(caps.clone());
    }
    let dir = tempfile::tempdir()?;
    let mut child = Command::new(bin)
        .env("PATH", path)
        .args([
            "app-server",
            "generate-json-schema",
            "--experimental",
            "--out",
        ])
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Err(io::Error::other(
                    "Codex could not generate its command schema",
                ));
            }
            break;
        }
        if start.elapsed() >= Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Codex command discovery timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    let mut caps = Capabilities::default();
    caps.read_schema(&serde_json::from_slice(&fs::read(
        dir.path().join("ClientRequest.json"),
    )?)?);
    *cache = Some((key, caps.clone()));
    Ok(caps)
}

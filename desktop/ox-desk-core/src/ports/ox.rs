//! The OX helper owns provider storage, configuration locks and credentials.
use crate::settings_protocol::{Request, VERSION, model::Snapshot};
use serde::de::DeserializeOwned;
use std::{
    env,
    ffi::OsStr,
    io::{self, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

type FileIdentity = (u64, u64, u64, i64, i64, i64, i64);
pub type RouteInputs = [Option<FileIdentity>; 4];

/// File identities behind the settings snapshot’s provider aliases and favorites.
/// An unreadable input produces no stamp so callers bypass their cache.
pub fn route_inputs() -> Option<RouteInputs> {
    let home = env::var_os("CODEX_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".codex")))?;
    let mut ids = [None; 4];
    for (i, name) in [
        "codeox.toml",
        "config.toml",
        "ox-api-keys.json",
        "ox-favorites.json",
    ]
    .into_iter()
    .enumerate()
    {
        match std::fs::metadata(home.join(name)) {
            Ok(m) => {
                ids[i] = Some((
                    m.dev(),
                    m.ino(),
                    m.len(),
                    m.mtime(),
                    m.mtime_nsec(),
                    m.ctime(),
                    m.ctime_nsec(),
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(_) => return None,
        }
    }
    Some(ids)
}
pub fn request<T: DeserializeOwned>(
    ox: &Path,
    path: &OsStr,
    request: Request,
) -> Result<T, String> {
    request_ref(ox, path, &request)
}
pub fn request_ref<T: DeserializeOwned>(
    ox: &Path,
    path: &OsStr,
    request: &Request,
) -> Result<T, String> {
    let mut child = process(ox, path)
        .arg("--ox-desktop-settings-v1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not open the OX settings service: {e}"))?;
    let mut input = child.stdin.take().expect("stdin is piped");
    let write = serde_json::to_writer(&mut input, &request)
        .and_then(|_| input.flush().map_err(serde_json::Error::io));
    drop(input);
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    write.map_err(|e| e.to_string())?;
    serde_json::from_slice(&output.stdout)
        .map_err(|_| "The OX executable returned an incompatible settings response.".into())
}

pub fn snapshot(ox: &Path, path: &OsStr) -> Result<Snapshot, String> {
    let settings: Snapshot = request(ox, path, Request::ReadRows)?;
    if settings.version != VERSION {
        return Err(format!(
            "The OX helper speaks settings protocol {}. This desktop requires version {VERSION}.",
            settings.version
        ));
    }
    Ok(settings)
}

/// `base` (the settings snapshot's static and starred aliases, starred first) followed by every
/// other alias the providers list. Stars only order the list; a discovered alias that nobody
/// starred is as usable as any other.
pub fn merge_catalog(base: &[String], discovered: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut all = base.to_vec();
    let mut seen: std::collections::HashSet<String> = all.iter().cloned().collect();
    all.extend(discovered.into_iter().filter(|m| seen.insert(m.clone())));
    all
}

/// The full catalog behind `base`. Discovery asks the providers, so a failure keeps `base`.
pub fn catalog(ox: &Path, path: &OsStr, base: &[String]) -> Vec<String> {
    let discovered: Vec<String> = request(ox, path, Request::Models).unwrap_or_default();
    merge_catalog(base, discovered)
}

/// Both the settings port and native runtime jobs use the same companion and child PATH.
pub(crate) fn process(ox: &Path, path: &OsStr) -> Command {
    let mut cmd = Command::new(ox);
    cmd.env("PATH", path);
    cmd
}

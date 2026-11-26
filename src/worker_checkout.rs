//! Resolve the worker directory before starting a process or stamping its lineage.
use std::path::{Path, PathBuf};

pub fn resolve(
    parent: &Path,
    checkout: Option<&Path>,
    unrestricted: bool,
) -> Result<PathBuf, String> {
    let Some(checkout) = checkout else {
        return Ok(parent.to_owned());
    };
    if checkout.as_os_str().is_empty() {
        return Err("Provide a worker checkout directory.".into());
    }
    let root = parent
        .canonicalize()
        .map_err(|e| format!("Cannot open the parent workspace: {e}"))?;
    let path = root
        .join(checkout)
        .canonicalize()
        .map_err(|e| format!("Cannot open the worker checkout: {e}"))?;
    if !path.is_dir() {
        return Err("The worker checkout must be a directory.".into());
    }
    if !unrestricted && !path.starts_with(&root) {
        return Err("Choose a worker checkout inside the parent's workspace.".into());
    }
    Ok(path)
}

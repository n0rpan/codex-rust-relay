use std::{env, path::PathBuf};

pub fn directory() -> Option<PathBuf> {
    let state = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            env::var_os("HOME").map(|p| {
                PathBuf::from(p).join(if cfg!(target_os = "macos") {
                    "Library/Application Support"
                } else {
                    ".local/state"
                })
            })
        })?;
    Some(state.join("ox/desktop"))
}

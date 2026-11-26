//! Layout written by install.sh --desktop. Public launchers all follow current.
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
pub const BIN_DIR: &str = "OX Desktop.app/Contents/MacOS";
#[cfg(not(target_os = "macos"))]
pub const BIN_DIR: &str = "ox-desktop";

pub fn generation_dir(executable: &Path) -> Option<PathBuf> {
    let executable = executable.canonicalize().ok()?;
    let bin = executable.parent()?;
    if !bin.ends_with(BIN_DIR) {
        return None;
    }
    let generation = bin
        .ancestors()
        .nth(Path::new(BIN_DIR).components().count())?;
    generation
        .parent()?
        .ends_with(".ox-desktop")
        .then(|| generation.to_owned())
}

pub fn package_store(executable: &Path) -> Option<PathBuf> {
    let generation = generation_dir(executable)?;
    let store = generation.parent()?;
    (generation.join(".ready").is_file() && store.join("current").is_symlink())
        .then(|| store.to_owned())
}

pub fn active_binary(store: &Path, name: &str) -> PathBuf {
    store.join("current").join(BIN_DIR).join(name)
}

/// Keep this immutable package available to helpers started later by this process.
/// An updater may retire an older generation only after its last user exits.
pub fn hold_generation() -> Option<std::fs::File> {
    let generation = generation_dir(&std::env::current_exe().ok()?)?;
    let file = std::fs::File::open(generation.join(".in-use")).ok()?;
    file.lock_shared().ok()?;
    Some(file)
}

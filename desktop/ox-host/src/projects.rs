//! Checkouts an operator keeps available on a headless host, across sessions and restarts.
use ox_desk_core::state_file::update_preference_at;
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

pub(crate) fn registered(dir: &Path) -> io::Result<BTreeSet<PathBuf>> {
    match fs::File::open(dir.join("projects.json")) {
        Ok(f) => Ok(serde_json::from_reader(f)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(e),
    }
}
pub(crate) fn cli(dir: &Path, args: &[String]) -> io::Result<()> {
    match args {
        [] => (),
        [action] if action == "list" => (),
        [action, path] if matches!(action.as_str(), "add" | "remove") => {
            let path = std::path::absolute(path)?;
            let path = if action == "add" {
                let p = path.canonicalize()?;
                if !p.is_dir() {
                    return Err(io::Error::other("Choose a project directory."));
                }
                p
            } else {
                path.canonicalize().unwrap_or(path)
            };
            update_preference_at(&dir.join("projects.json"), BTreeSet::new, |mut projects| {
                if action == "add" {
                    projects.insert(path);
                } else {
                    projects.remove(&path);
                }
                projects
            })?;
        }
        _ => {
            return Err(io::Error::other(
                "Usage: ox host project [list | add PATH | remove PATH]",
            ));
        }
    }
    for path in registered(dir)? {
        println!("{}", path.display());
    }
    Ok(())
}

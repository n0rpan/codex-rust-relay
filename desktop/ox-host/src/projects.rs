//! Checkouts an operator keeps available on a headless host, across sessions and restarts.
use ox_desk_core::state_file::update_preference_at;
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

/// Existing operator registrations and workspace history, plus scopes known to the engine.
pub(crate) fn known(
    config: &crate::runtime::RuntimeConfig,
    state_dir: &Path,
    mut scopes: BTreeSet<PathBuf>,
) -> Vec<PathBuf> {
    scopes.extend(config.projects());
    scopes.extend(
        registered(state_dir)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.is_dir()),
    );
    scopes.into_iter().collect()
}
pub(crate) fn directory(
    config: &crate::runtime::RuntimeConfig,
    state_dir: &Path,
    scopes: BTreeSet<PathBuf>,
    checkout: Option<&Path>,
) -> ox_desk_core::host_protocol::ProjectDirectory {
    use ox_desk_core::host_protocol::{ProjectCheckout, ProjectDirectory};
    let checkouts = known(config, state_dir, scopes)
        .into_iter()
        .filter(|path| checkout.is_none_or(|selected| selected == path) && path.is_dir())
        .map(|path| {
            let repository = ox_host_service::checkout::repository_id(&path);
            ProjectCheckout { path, repository }
        })
        .collect();
    let installed = config.installed_runtimes();
    let routes = config
        .prepare()
        .map(|p| {
            p.routes
                .into_iter()
                .filter(|r| installed.contains(&r.runtime))
                .collect()
        })
        .unwrap_or_default();
    ProjectDirectory { checkouts, routes }
}

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

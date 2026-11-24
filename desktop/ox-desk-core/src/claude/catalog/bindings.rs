//! Billing identity recorded by OX at launch or on explicit confirmation.
//! Read and write only the index path supplied by the desktop.
use super::valid_session_id;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Bindings {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    project: PathBuf,
    profile: PathBuf,
    id: String,
    alias: String,
}

// On the first API launch OX creates ox-clients/claude in its child process.
// Resolve the existing parent without creating or writing any stock directory.
fn profile_identity(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Expected an absolute Claude profile",
        ));
    }
    for ancestor in path.ancestors() {
        match ancestor.canonicalize() {
            Ok(root) => {
                let suffix = path.strip_prefix(ancestor).map_err(io::Error::other)?;
                if suffix
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Invalid Claude profile directory",
                    ));
                }
                return Ok(root.join(suffix));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Claude profile root unavailable",
    ))
}

impl Bindings {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn entries(&self) -> io::Result<Vec<Entry>> {
        match File::open(&self.path) {
            Ok(file) => Ok(serde_json::from_reader(file)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn aliases(
        &self,
        relay_profile: &Path,
    ) -> io::Result<std::collections::HashMap<(PathBuf, String), String>> {
        let profile = profile_identity(relay_profile)?;
        Ok(self
            .entries()?
            .into_iter()
            .filter(|r| r.profile == profile)
            .map(|r| ((r.project, r.id), r.alias))
            .collect())
    }

    pub fn alias(
        &self,
        project: &Path,
        relay_profile: &Path,
        id: &str,
    ) -> io::Result<Option<String>> {
        let project = project.canonicalize()?;
        let profile = profile_identity(relay_profile)?;
        Ok(self
            .entries()?
            .into_iter()
            .find(|r| r.project == project && r.profile == profile && r.id == id)
            .map(|r| r.alias))
    }

    pub fn bind(
        &self,
        project: &Path,
        relay_profile: &Path,
        id: &str,
        alias: &str,
    ) -> io::Result<()> {
        if !valid_session_id(id)
            || alias.trim() != alias
            || alias.is_empty()
            || alias.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid Claude session ID or API alias",
            ));
        }
        let project = project.canonicalize()?;
        let profile = profile_identity(relay_profile)?;
        let dir = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("Missing OX state directory"))?;
        fs::create_dir_all(dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path.with_extension("lock"))?;
        lock.lock()?;
        let mut rows = self.entries()?;
        if let Some(existing) = rows
            .iter()
            .find(|r| r.project == project && r.profile == profile && r.id == id)
        {
            return if existing.alias == alias {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Claude session is bound to a different API alias",
                ))
            };
        }
        rows.push(Entry {
            project,
            profile,
            id: id.into(),
            alias: alias.into(),
        });
        // The adjacent lock serializes all writers, including other desktop windows.
        let tmp = self.path.with_extension("tmp");
        // An interrupted write may leave a temporary file: never reuse its inode.
        match fs::remove_file(&tmp) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        serde_json::to_writer(&mut file, &rows)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(tmp, &self.path)?;
        File::open(dir)?.sync_all()
    }
}

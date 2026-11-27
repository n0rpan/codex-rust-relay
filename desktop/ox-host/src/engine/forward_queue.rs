//! An unfinished request may contain the user's prompt or private inputs.
//! Keep it in the source host's owner-only queue, outside the ownership ledger.
use ox_host_web::linked_jobs::Operation;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
pub(super) struct Pending {
    #[serde(default)]
    pub policy: Option<ox_host_web::worker_result::Policy>,
    pub project: PathBuf,
    pub operation: Operation,
}
pub(super) fn path(state: &Path, request: &uuid::Uuid) -> PathBuf {
    state
        .join("linked-work/outgoing/requests")
        .join(request.to_string())
}
impl Pending {
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let parent = dir.parent().unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(parent)?;
        let stage = tempfile::Builder::new()
            .prefix(".new-")
            .tempdir_in(parent)?;
        let f = File::create(stage.path().join("start.json"))?;
        serde_json::to_writer(&f, self)?;
        f.sync_all()?;
        File::open(stage.path())?.sync_all()?;
        fs::rename(stage.path(), dir)?;
        File::open(parent)?.sync_all()
    }
    pub fn load(dir: &Path) -> io::Result<Self> {
        Ok(serde_json::from_reader(File::open(
            dir.join("start.json"),
        )?)?)
    }
}

//! Durable, host-owned preparation and delivery of linked Git results.
mod cleanup;
pub(crate) mod transport;
pub use cleanup::{archive, request_retirement, sweep};
use ox_host_service::checkout::{
    self,
    results::{Baseline, ChangeSummary, Destination, Frozen},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};
pub use transport::collect;

#[derive(Serialize, Deserialize)]
struct Base {
    inputs: Baseline,
    registered: Vec<u8>,
    mirror: Vec<u8>,
    repository: String,
}
fn path(bytes: Vec<u8>) -> PathBuf {
    std::ffi::OsString::from_vec(bytes).into()
}
fn write<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer(&mut file, value)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    File::open(path.parent().unwrap())?.sync_all()
}
fn read<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    Ok(serde_json::from_reader(File::open(path)?)?)
}
pub(crate) fn save_base(
    job: &Path,
    inputs: Baseline,
    registered: &Path,
    mirror: &Path,
    repository: &str,
) -> io::Result<()> {
    write(
        &job.join("result-base.json"),
        &Base {
            inputs,
            registered: registered.as_os_str().as_bytes().to_vec(),
            mirror: mirror.as_os_str().as_bytes().to_vec(),
            repository: repository.into(),
        },
    )
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub destination: Destination,
    pub max_age_days: u32,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            destination: Destination::Branch,
            max_age_days: 30,
        }
    }
}
pub fn discard_stages(dir: &Path, prefixes: &[&str]) -> io::Result<()> {
    if !dir.try_exists()? {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|n| prefixes.iter().any(|p| n.starts_with(p)))
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

pub fn configure(job: &Path, policy: &Policy) -> io::Result<()> {
    if policy.max_age_days == 0 {
        return Err(io::Error::other(
            "Worker result retention must be at least one day.",
        ));
    }
    // Legacy installations have no immutable private-input manifest.
    File::open(job.join("result-base.json")).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            io::Error::other(
                "This checkout predates write results. Launch a new worker to return changes.",
            )
        } else {
            err
        }
    })?;
    write(&job.join("result-policy.json"), policy)
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Increment {
    pub base: String,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub increment: Option<Increment>,
    pub summary: ChangeSummary,
    pub remote_ref: Option<String>,
    pub note: Option<String>,
    pub bytes: u64,
    pub sha256: String,
}
pub fn report(job: &Path) -> io::Result<Option<Report>> {
    match read(&job.join("worker-result/report.json")) {
        Ok(r) => Ok(Some(r)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
#[derive(Serialize, Deserialize)]
struct Artifact {
    #[serde(default)]
    increment: Option<Increment>,
    summary: ChangeSummary,
    bytes: u64,
    sha256: String,
}
pub(crate) fn digest(file: &mut File) -> io::Result<(u64, String)> {
    let mut hash = Sha256::new();
    let n = io::copy(file, &mut hash)?;
    Ok((n, format!("{:x}", hash.finalize())))
}
/// Freeze before publishing. A retry after an interrupted push reuses precisely
/// the same object ID and bytes, including an automatically recorded final edit.
pub fn finish(job: &Path, id: u64, completed: bool) -> io::Result<Report> {
    if let Some(done) = report(job)? {
        return Ok(done);
    }
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(job.join("result.lock"))?;
    lock.lock()?;
    if let Some(done) = report(job)? {
        return Ok(done);
    }
    discard_stages(job, &[".freezing-", ".publishing-"])?;
    let base: Base = read(&job.join("result-base.json"))?;
    let policy: Policy = read(&job.join("result-policy.json"))?;
    let dir = job.join("worker-result");
    let registered = path(base.registered);
    let mirror = path(base.mirror);
    let artifact = if dir.try_exists()? {
        read::<Artifact>(&dir.join("artifact.json"))?
    } else {
        let stage = tempfile::Builder::new()
            .prefix(".freezing-")
            .tempdir_in(job)?;
        let frozen = Frozen::capture(stage.path(), &job.join("checkout"), &base.inputs, &[])?;
        frozen.archive(&stage.path().join("archive"))?;
        let (bytes, sha256) = digest(&mut frozen.packet.open()?)?;
        let incremental = stage.path().join("increment");
        let increment = frozen
            .incremental_archive(&incremental)?
            .map(|base| -> io::Result<_> {
                let (bytes, sha256) = digest(&mut File::open(incremental)?)?;
                Ok(Increment {
                    base,
                    bytes,
                    sha256,
                })
            })
            .transpose()?;
        let artifact = Artifact {
            increment,
            summary: frozen.summary.clone(),
            bytes,
            sha256,
        };
        write(&stage.path().join("artifact.json"), &artifact)?;
        drop(frozen);
        fs::rename(stage.path(), &dir)?;
        File::open(job)?.sync_all()?;
        artifact
    };
    let mut result = Report {
        increment: artifact.increment,
        summary: artifact.summary,
        bytes: artifact.bytes,
        sha256: artifact.sha256,
        remote_ref: None,
        note: None,
    };
    if !completed {
        result.note = Some("The worker stopped before completion. Its partial changes were retained over the link.".into());
    }
    if completed
        && policy.destination != Destination::Tailnet
        && result.summary.commit != result.summary.snapshot
    {
        let publish = || -> io::Result<String> {
            if checkout::repository_id(&registered).as_deref() != Some(&base.repository) {
                return Err(io::Error::other("The registered origin changed."));
            }
            let stage = tempfile::Builder::new()
                .prefix(".publishing-")
                .tempdir_in(job)?;
            let work = checkout::receive_commit(
                stage.path(),
                id,
                &mirror,
                File::open(dir.join("archive"))?,
                &result.summary.commit,
            )?;
            checkout::results::publish_checkout(
                &work,
                &registered,
                &base.inputs,
                id,
                policy.destination,
                &result.summary,
            )
        };
        match publish() {
            Ok(fingerprint) => {
                cleanup::published(&dir, &fingerprint)?;
                result.remote_ref = policy.destination.reference(id);
            }
            Err(e) => result.note = Some(format!("Returning over the host link. {e}")),
        }
    }
    write(&dir.join("report.json"), &result)?;
    Ok(result)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Local {
    pub received_bytes: u64,
    pub report: Report,
    pub reference: String,
    pub checkout: PathBuf,
}

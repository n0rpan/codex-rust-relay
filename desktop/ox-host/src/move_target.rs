//! Destination-owned checkout and transcript, verified before fencing the source.
use crate::{
    move_archive::{Archive, checksum},
    move_ledger::{Ledger, Plan, Prepared, Stage},
};
use ox_host_service::checkout::receive_result;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{self, Seek},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BundlePlan {
    pub(crate) plan: Plan,
    pub(crate) checkout_commit: String,
}

/// Received from the authenticated upload adapter. A receipt for these bytes
/// allows verification here, but confers no native write authority.
pub(crate) struct Files<'a> {
    pub(crate) transcript: &'a Path,
    pub(crate) transcript_sha256: [u8; 32],
    pub(crate) checkout: &'a Path,
    pub(crate) checkout_sha256: [u8; 32],
    pub(crate) manifest_sha256: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    bundle: BundlePlan,
    manifest_sha256: [u8; 32],
    transcript_sha256: [u8; 32],
    checkout_sha256: [u8; 32],
}

pub(crate) struct VerifiedTarget {
    directory: PathBuf,
    saved: Saved,
}

impl VerifiedTarget {
    /// Each move receives a fresh checkout in the destination state directory.
    /// The host actor serializes this step and reserves the scope in preflight.
    pub(crate) fn install(
        workspace: &Path,
        ledger: &Ledger,
        authenticated_source: Uuid,
        bundle: BundlePlan,
        files: Files<'_>,
    ) -> io::Result<Self> {
        let plan = &bundle.plan;
        let prepared = ledger.prepared_receiver(authenticated_source, plan)?;
        if prepared
            .as_ref()
            .is_some_and(|receipt| receipt.stage.digest != files.manifest_sha256)
        {
            return Err(invalid(
                "The destination is already prepared for other files.",
            ));
        }
        let workspace = workspace.canonicalize()?;
        let directory = workspace.join(plan.id.to_string());
        let project = plan
            .target_scope
            .project
            .strip_prefix(directory.join("1"))
            .map_err(|_| invalid("The checkout is outside the reserved move directory."))?;
        if !project
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err(invalid("The destination checkout path is invalid."));
        }
        let saved = Saved {
            bundle,
            manifest_sha256: files.manifest_sha256,
            transcript_sha256: files.transcript_sha256,
            checkout_sha256: files.checkout_sha256,
        };
        let target = Self { directory, saved };
        match target.directory.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() => {
                let file = File::open(target.directory.join("receipt.json"))?;
                if serde_json::from_reader::<_, Saved>(file)? != target.saved {
                    return Err(invalid("This directory belongs to another move."));
                }
                target.history()?;
                target.check_project()?;
                File::open(workspace)?.sync_all()?;
                return Ok(target);
            }
            Ok(_) => return Err(invalid("The reserved move directory is already in use.")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if prepared.is_some() {
            return Err(invalid("The prepared destination files are missing."));
        }
        let stage = tempfile::Builder::new()
            .prefix(".receiving-")
            .tempdir_in(&workspace)?;
        let transcript = stage.path().join("transcript.archive");
        let mut source = File::open(files.transcript)?;
        let mut retained = File::create_new(&transcript)?;
        io::copy(&mut source, &mut retained)?;
        retained.sync_all()?;
        Archive::open(
            File::open(transcript)?,
            &target.saved.bundle.plan,
            files.transcript_sha256,
        )?;

        let mut checkout = File::open(files.checkout)?;
        if checksum(&mut checkout)? != files.checkout_sha256 {
            return Err(invalid("The checkout failed its integrity check."));
        }
        checkout.rewind()?;
        let installed = receive_result(
            stage.path(),
            1,
            checkout,
            &target.saved.bundle.checkout_commit,
        )?;
        let relative = installed
            .strip_prefix(stage.path())
            .map_err(|_| invalid("The checkout is outside the move directory."))?;
        if target.directory.join(relative) != target.saved.bundle.plan.target_scope.project {
            return Err(invalid(
                "The checkout project differs from the reserved destination.",
            ));
        }
        sync_checkout(&stage.path().join("1"))?;
        let receipt = File::create_new(stage.path().join("receipt.json"))?;
        serde_json::to_writer(&receipt, &target.saved)?;
        receipt.sync_all()?;
        File::open(stage.path())?.sync_all()?;
        fs::rename(stage.path(), &target.directory)?;
        File::open(workspace)?.sync_all()?;
        Ok(target)
    }

    pub(crate) fn history(&self) -> io::Result<Archive> {
        Archive::open(
            File::open(self.directory.join("transcript.archive"))?,
            &self.saved.bundle.plan,
            self.saved.transcript_sha256,
        )
    }

    /// Call after reserving an idle native session in the verified checkout.
    /// Recording Prepared still keeps that native session fenced until activation.
    pub(crate) fn prepare(
        &self,
        ledger: &mut Ledger,
        authenticated_source: Uuid,
        native: String,
    ) -> io::Result<Prepared> {
        self.check_project()?;
        ledger.prepare_receiver(
            authenticated_source,
            &Prepared {
                plan: self.saved.bundle.plan.clone(),
                stage: Stage {
                    native,
                    digest: self.saved.manifest_sha256,
                },
            },
        )
    }

    fn check_project(&self) -> io::Result<()> {
        let project = &self.saved.bundle.plan.target_scope.project;
        if project.canonicalize()? == *project && project.is_dir() {
            return Ok(());
        }
        Err(invalid("The reserved destination checkout has moved."))
    }
}

// Keep directory entries as well as file contents durable before replying.
// The checkout may contain symlinks, which belong to the directory entry.
fn sync_checkout(path: &Path) -> io::Result<()> {
    let mut directories = vec![path.to_path_buf()];
    let mut seen = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                directories.push(entry.path());
            } else if kind.is_file() {
                File::open(entry.path())?.sync_all()?;
            }
        }
        seen.push(directory);
    }
    for directory in seen.into_iter().rev() {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

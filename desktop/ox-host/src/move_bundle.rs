//! Capture a frozen source's complete transcript and working tree for staging.
use crate::{
    move_archive::{Archive, checksum},
    move_ledger::Plan,
    move_target::BundlePlan,
};
use ox_desk_core::host_protocol::Timeline;
use ox_host_service::checkout::Snapshot;
use ox_host_web::move_manifest::{CHECKOUT, FileSpec, Manifest, PLAN, TRANSCRIPT};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Write},
    path::Path,
};

pub(crate) struct Bundle {
    directory: tempfile::TempDir,
    pub(crate) manifest: Manifest,
}

impl Bundle {
    pub(crate) fn capture(staging: &Path, plan: &Plan, timeline: &Timeline) -> io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix(".move-export-")
            .tempdir_in(staging)?;
        let transcript = directory.path().join(TRANSCRIPT);
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(transcript)?;
        let transcript = FileSpec {
            sha256: Archive::capture(&mut file, plan, timeline)?,
            bytes: file.metadata()?.len(),
        };
        // The destination might need to run without reaching the source origin.
        let snapshot = Snapshot::capture(directory.path(), &plan.source.scope.project, &[])?;
        snapshot.retain_archive(&directory.path().join(CHECKOUT))?;
        let mut archive = snapshot.open()?;
        let checkout = FileSpec {
            sha256: checksum(&mut archive)?,
            bytes: archive.metadata()?.len(),
        };
        let plan = serde_json::to_vec(&BundlePlan {
            plan: plan.clone(),
            checkout_commit: snapshot.commit.clone(),
        })?;
        let mut file = File::create_new(directory.path().join(PLAN))?;
        file.write_all(&plan)?;
        file.sync_all()?;
        let plan = FileSpec {
            sha256: Sha256::digest(&plan).into(),
            bytes: plan.len() as u64,
        };
        Ok(Self {
            directory,
            manifest: Manifest {
                plan,
                transcript,
                checkout,
            },
        })
    }

    pub(crate) fn directory(&self) -> &Path {
        self.directory.path()
    }
}

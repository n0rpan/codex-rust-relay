//! Retire only the public ref, leaving the private return available offline.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize)]
struct Publication {
    origin_digest: String,
    created: u64,
}

fn now() -> io::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs())
}

pub(super) fn published(dir: &Path, fingerprint: &str) -> io::Result<()> {
    write(
        &dir.join("publication.json"),
        &Publication {
            origin_digest: fingerprint.into(),
            created: now()?,
        },
    )
}

// A positive acknowledgement means the receiver durably owns cleanup, even
// when the forge is offline. The caller's reported commit must match the job.
pub fn request_retirement(job: &Path, commit: &str) -> io::Result<()> {
    let result = report(job)?.ok_or_else(|| io::Error::other("The worker result is not ready."))?;
    if result.summary.commit != commit {
        return Err(io::Error::other(
            "The commit differs from the published job.",
        ));
    }
    write(&job.join("worker-result/released"), &true)
}

pub fn archive(job: &Path) -> io::Result<()> {
    let marker = job.join("result-archived");
    if !marker.try_exists()? {
        write(&marker, &true)?;
    }
    Ok(())
}

pub fn maintain(job: &Path, id: u64) -> io::Result<()> {
    let dir = job.join("worker-result");
    if !dir.join("publication.json").try_exists()? || dir.join("retired").try_exists()? {
        return Ok(());
    }
    let Some(result) = report(job)? else {
        return Ok(());
    };
    if result.remote_ref.is_none() {
        return Ok(());
    }
    let policy: Policy = read(&job.join("result-policy.json"))?;
    let publication: Publication = read(&dir.join("publication.json"))?;
    let age = now()?.saturating_sub(publication.created);
    if !job.join("result-archived").try_exists()?
        && age < u64::from(policy.max_age_days) * 86400
        && !dir.join("released").try_exists()?
    {
        return Ok(());
    }
    let base: Base = read(&job.join("result-base.json"))?;
    let registered = path(base.registered);
    if checkout::repository_id(&registered).as_deref() != Some(&base.repository) {
        return Err(io::Error::other(
            "The registered project for the published ref changed.",
        ));
    }
    checkout::results::retire(
        &registered,
        &path(base.mirror),
        id,
        policy.destination,
        &result.summary.commit,
        &publication.origin_digest,
    )?;
    write(&dir.join("retired"), &true)
}

// Job directories can outlive runtime sessions and registry entries.
pub fn sweep(root: &Path) -> io::Result<()> {
    if !root.try_exists()? {
        return Ok(());
    }
    for host in fs::read_dir(root)? {
        let host = host?;
        if !host.file_type()?.is_dir()
            || !host
                .file_name()
                .to_str()
                .is_some_and(crate::paired_hosts::valid_id)
        {
            continue;
        }
        for job in fs::read_dir(host.path())? {
            let job = job?;
            if job.file_type()?.is_dir()
                && let Some(id) = job.file_name().to_str().and_then(|s| s.parse::<u64>().ok())
                && let Err(e) = maintain(&job.path(), id)
            {
                eprintln!("ox host: cleanup of worker ref {id} will retry: {e}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@localhost",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim_end().into()
    }

    #[test]
    fn a_ref_is_retired_on_release_archive_or_expiry_and_the_private_return_survives() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let registered = root.join("registered");
        let remote = root.join("origin.git");
        git(root, &["init", "-q", "registered"]);
        fs::write(registered.join("tracked"), "base").unwrap();
        git(&registered, &["add", "."]);
        git(&registered, &["commit", "-qm", "base"]);
        git(
            root,
            &["clone", "--quiet", "--bare", "registered", "origin.git"],
        );
        git(
            &registered,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/owner/project",
            ],
        );
        git(
            &registered,
            &["config", "remote.origin.pushurl", remote.to_str().unwrap()],
        );
        let repository = checkout::repository_id(&registered).unwrap();
        let snapshot = checkout::Snapshot::capture(root, &registered, &[]).unwrap();
        for id in 1..=6 {
            let job = root.join(format!("job-{id}"));
            fs::create_dir(&job).unwrap();
            let work = checkout::receive_commit(
                &job,
                id,
                &remote,
                snapshot.open().unwrap(),
                &snapshot.commit,
            )
            .unwrap();
            let baseline = Baseline::read_checkout(&work, &snapshot.commit).unwrap();
            save_base(&job, baseline, &registered, &remote, &repository).unwrap();
            fs::rename(work, job.join("checkout")).unwrap();
            fs::write(job.join("checkout/tracked"), format!("edit {id}")).unwrap();
            let dest = match id {
                2 => Destination::Ref,
                4 => Destination::Tailnet,
                _ => Destination::Branch,
            };
            configure(
                &job,
                &Policy {
                    destination: dest,
                    max_age_days: 10,
                },
            )
            .unwrap();
            let reference = dest.reference(id);
            if id == 5 {
                git(
                    &remote,
                    &["update-ref", reference.as_ref().unwrap(), &snapshot.commit],
                );
            }
            if id == 6 {
                let pointer = format!(
                    "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 42\n",
                    "1".repeat(64)
                );
                fs::write(job.join("checkout/tracked"), pointer).unwrap();
            }
            let result = finish(&job, id, true).unwrap();
            assert_eq!(result, finish(&job, id, true).unwrap());
            if id >= 4 {
                assert!(result.remote_ref.is_none());
                if id == 5 {
                    assert!(result.note.as_deref().unwrap().contains("different commit"));
                }
                if id == 6 {
                    assert!(result.note.as_deref().unwrap().contains("LFS"));
                }
                continue;
            }
            let reference = reference.unwrap();
            assert_eq!(result.remote_ref.as_deref(), Some(reference.as_str()));
            let dir = job.join("worker-result");
            maintain(&job, id).unwrap();
            assert_eq!(
                git(&remote, &["rev-parse", &reference]),
                result.summary.commit
            );
            match id {
                1 => {
                    assert!(request_retirement(&job, &snapshot.commit).is_err());
                    request_retirement(&job, &result.summary.commit).unwrap();
                }
                2 => archive(&job).unwrap(),
                _ => {
                    let mut p: Publication = read(&dir.join("publication.json")).unwrap();
                    p.created -= 11 * 86400;
                    write(&dir.join("publication.json"), &p).unwrap();
                }
            }
            maintain(&job, id).unwrap();
            assert!(dir.join("retired").exists() && dir.join("archive").exists());
            assert_eq!(
                git(
                    &remote,
                    &["for-each-ref", "--format=%(refname)", &reference]
                ),
                ""
            );
        }
    }
}

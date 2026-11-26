//! A Git bundle doesn't record shallow boundaries. Carry the source's reachable
//! boundaries alongside it, retaining any fuller history already on the target.
use super::{files, git, git_path, oid, run};
use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
};

pub(super) fn prepare(root: &Path, repo: &Path) -> io::Result<Vec<String>> {
    let path = git_path(
        root,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "shallow",
        ],
    )?;
    let contents = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let ids = contents
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .map(|id| oid(id).map(str::to_owned))
        .collect::<io::Result<Vec<_>>>()?;
    fs::write(repo.join("shallow"), contents)?;
    Ok(ids)
}

pub(super) fn pack(
    repo: &Path,
    boundaries: &[String],
    commit: &str,
    output: &mut impl Write,
) -> io::Result<()> {
    for id in boundaries {
        let status = git(repo)
            .args(["merge-base", "--is-ancestor", id, commit])
            .output()?
            .status;
        match status.code() {
            Some(0) => files::write_field(output, id.as_bytes())?,
            Some(1) => (),
            _ => {
                return Err(io::Error::other(
                    "Git could not verify the shallow history boundary",
                ));
            }
        }
    }
    files::write_field(output, &[])
}

pub(super) fn unpack(work: &Path, input: &mut impl Read, hash_len: usize) -> io::Result<()> {
    let mut shallow = None;
    loop {
        let boundary = files::read_field(input)?;
        if boundary.is_empty() {
            return Ok(());
        }
        let id = oid(&boundary)?;
        if id.len() != hash_len {
            return Err(io::Error::other(
                "Invalid object format for shallow history",
            ));
        }
        // Existing objects keep the target's own ancestry and shallow status.
        if run(git(work).args(["cat-file", "-e", &format!("{id}^{{commit}}")])).is_ok() {
            continue;
        }
        if shallow.is_none() {
            shallow = Some(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(work.join(".git/shallow"))?,
            );
        }
        writeln!(shallow.as_mut().unwrap(), "{id}")?;
    }
}

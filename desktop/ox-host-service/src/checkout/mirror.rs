//! The target opens mirrors from its own registered checkout. Source requests
//! never supply a local path, command, remote helper or target credential.
use super::{bare, git, line, oid, run};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Seek, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

pub struct Mirror {
    path: PathBuf,
}

impl Mirror {
    /// Seed the cache from the target's checkout, which works while its origin
    /// is offline. The cache key is local and never contains a URL in the clear.
    pub fn from_checkout(cache: &Path, checkout: &Path) -> io::Result<Self> {
        let checkout = checkout.canonicalize()?;
        let origin = line(run(git(&checkout).args(["remote", "get-url", "origin"]))?)?;
        let hash_size = super::object_format(&checkout)?;
        // Resolve local relative origins against the target's checkout before
        // moving the URL into a bare repository. Remote URLs stay byte-exact.
        let url = local_origin(&origin, &checkout);
        let key = format!("{:x}", Sha256::digest(url.as_bytes()));
        let path = cache.join(key);
        if path.try_exists()? {
            return Ok(Self { path });
        }
        let temp = tempfile::Builder::new()
            .prefix(".mirror-")
            .tempdir_in(cache)?;
        bare(temp.path(), hash_size)?;
        run(git(temp.path()).args(["remote", "add", "origin"]).arg(&url))?;
        // Fetch locally so the cache owns its objects, even if the selected
        // checkout is itself shared or is removed later. Keep checked-out HEAD
        // as well as the origin-tracking tips (a worktree may be detached).
        let mut seed = git(temp.path());
        seed.args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-recurse-submodules",
            "--",
        ])
        .arg(&checkout);
        if super::head(&checkout)?.is_some() {
            seed.arg("+HEAD:refs/ox/seed");
        }
        run(seed.arg("+refs/remotes/origin/*:refs/remotes/origin/*"))?;
        match fs::rename(temp.path(), &path) {
            Ok(()) => (),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
                ) && path.is_dir() => {}
            Err(error) => return Err(error),
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A failed refresh leaves the seed and previously fetched objects usable.
    /// Only remote-tracking branches are pruned, never job results or seed refs.
    pub fn refresh(&self) -> io::Result<()> {
        run(git(&self.path).args([
            "fetch",
            "--atomic",
            "--prune",
            "--no-tags",
            "--no-recurse-submodules",
            "origin",
            "+refs/heads/*:refs/remotes/origin/*",
        ]))?;
        Ok(())
    }

    /// Intersect advertised source commits with the target object store in one
    /// Git process. A linked host only receives IDs it already named.
    pub fn known(&self, candidates: &[String]) -> io::Result<Vec<String>> {
        known_commits(&self.path, candidates)
    }
}

pub(super) fn known_commits(repo: &Path, candidates: &[String]) -> io::Result<Vec<String>> {
    let mut revisions = tempfile::tempfile()?;
    for candidate in candidates {
        oid(candidate.as_bytes())?;
        writeln!(revisions, "{candidate}")?;
    }
    revisions.rewind()?;
    let output = run(git(repo)
        .arg("cat-file")
        .arg("--batch-check=%(objecttype)")
        .stdin(revisions))?;
    let types: Vec<_> = output
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    if types.len() != candidates.len() {
        return Err(io::Error::other("Incomplete Git object negotiation"));
    }
    Ok(types
        .into_iter()
        .zip(candidates)
        .filter(|(kind, _)| *kind == b"commit")
        .map(|(_, id)| id.clone())
        .collect())
}

/// A local clone of a mirror has its objects, but Git only maps its local
/// heads. Install the target's actual upstream tracking refs for normal diffs.
pub(super) fn copy_tracking_refs(mirror: &Path, work: &Path) -> io::Result<()> {
    let refs = run(git(mirror).args([
        "for-each-ref",
        "--format=%(objectname) %(refname) %(symref)",
        "refs/remotes/origin/",
    ]))?;
    let mut updates = tempfile::tempfile()?;
    let mut symbolic = Vec::new();
    for row in refs.split(|b| *b == b'\n').filter(|r| !r.is_empty()) {
        let mut fields = row.split(|b| *b == b' ');
        let id = oid(fields.next().unwrap_or_default())?;
        let name = fields
            .next()
            .ok_or_else(|| io::Error::other("Invalid tracking ref"))?;
        if let Some(target) = fields.next().filter(|p| !p.is_empty()) {
            symbolic.push((name, target));
        } else {
            updates.write_all(b"update ")?;
            updates.write_all(name)?;
            writeln!(updates, " {id}")?;
        }
    }
    if updates.stream_position()? != 0 {
        updates.rewind()?;
        run(git(work).args(["update-ref", "--stdin"]).stdin(updates))?;
    }
    for (name, target) in symbolic {
        run(git(work)
            .arg("symbolic-ref")
            .arg(OsStr::from_bytes(name))
            .arg(OsStr::from_bytes(target)))?;
    }
    Ok(())
}

pub(super) fn local_origin(origin: &OsStr, checkout: &Path) -> std::ffi::OsString {
    let raw = origin.as_bytes();
    if raw.starts_with(b"/") {
        return origin.to_owned();
    }
    let colon = raw.iter().position(|b| *b == b':');
    let slash = raw.iter().position(|b| *b == b'/');
    if colon.is_some_and(|c| slash.is_none_or(|s| c < s)) {
        return origin.to_owned();
    }
    checkout.join(origin).into_os_string()
}

/// Find the common boundary of the parent's HEAD and the target mirror. Stop
/// traversing each ancestry path as soon as the target already has its commit.
/// Only object IDs cross this negotiation; no ref names or user remotes do.
pub fn negotiate(
    project: &Path,
    mut known: impl FnMut(&[String]) -> io::Result<Vec<String>>,
) -> io::Result<Vec<String>> {
    use std::{collections::HashSet, io::BufRead, process::Stdio};
    let Some(head) = super::head(project)? else {
        return Ok(Vec::new());
    };
    if known(std::slice::from_ref(&head))?.contains(&head) {
        return Ok(vec![head]);
    }
    let mut child = git(project)
        .args(["rev-list", "--parents", "--topo-order", &head])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let read = || -> io::Result<Vec<String>> {
        let mut lines = io::BufReader::new(child.stdout.take().unwrap()).lines();
        let mut pending = HashSet::from([head]);
        let mut common = Vec::new();
        while !pending.is_empty() {
            let batch = (&mut lines).take(128).collect::<io::Result<Vec<_>>>()?;
            if batch.is_empty() {
                return Err(io::Error::other(
                    "Git ancestry is incomplete. Fetch the missing history before launching remotely.",
                ));
            }
            let mut commits = Vec::new();
            for row in &batch {
                let (id, _) = row.split_once(' ').unwrap_or((row, ""));
                commits.push(oid(id.as_bytes())?.to_owned());
            }
            let found: HashSet<_> = known(&commits)?.into_iter().collect();
            for (row, id) in batch.iter().zip(commits) {
                if !pending.remove(&id) {
                    continue;
                }
                if found.contains(&id) {
                    common.push(id);
                } else {
                    pending.extend(row.split_ascii_whitespace().skip(1).map(str::to_owned));
                }
            }
        }
        Ok(common)
    };
    let result = read();
    // Traversing older history is unnecessary once all branches have a base.
    let _ = child.kill();
    child.wait()?;
    result
}

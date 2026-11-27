//! The target opens mirrors from its own registered checkout. Source requests
//! never supply a local path, command, remote helper or target credential.
use super::{bare, git, line, oid, run};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    io::{self, Seek, Write},
    os::unix::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

pub struct Mirror {
    path: PathBuf,
    registered: PathBuf,
}

impl Mirror {
    /// Seed the cache from the target's checkout, which works while its origin
    /// is offline. The cache key is local and never contains a URL in the clear.
    pub fn from_checkout(cache: &Path, checkout: &Path) -> io::Result<Self> {
        Self::seed(cache, checkout, None)
    }

    /// Check the very URL used to seed the mirror when fulfilling a paired-host request.
    pub fn for_repository(cache: &Path, checkout: &Path, repository: &str) -> io::Result<Self> {
        Self::seed(cache, checkout, Some(repository))
    }

    /// Prefer a complete registered checkout, falling back to a shallow or partial one.
    pub fn from_registered(
        cache: &Path,
        projects: &[PathBuf],
        private: &Path,
        repository: &str,
    ) -> io::Result<Self> {
        let private = private.canonicalize()?;
        let mut fallback = None;
        for project in projects {
            let Ok(path) = project.canonicalize() else {
                continue;
            };
            if path.starts_with(&private)
                || super::repository_id(&path).as_deref() != Some(repository)
            {
                continue;
            }
            if complete(&path)? {
                return Self::for_repository(cache, &path, repository);
            }
            fallback.get_or_insert(path);
        }
        let path = fallback.ok_or_else(|| io::Error::other("The target has no registered checkout of this origin. Use ox host project add DIR on the target."))?;
        Self::for_repository(cache, &path, repository)
    }

    fn seed(cache: &Path, checkout: &Path, expected: Option<&str>) -> io::Result<Self> {
        let checkout = checkout.canonicalize()?;
        let origin = line(run(git(&checkout).args(["remote", "get-url", "origin"]))?)?;
        if expected.is_some()
            && origin
                .to_str()
                .and_then(super::identity::identity)
                .as_deref()
                != expected
        {
            return Err(io::Error::other(
                "The registered checkout's origin changed.",
            ));
        }
        let hash_size = super::object_format(&checkout)?;
        // Resolve local relative origins against the target's checkout before
        // moving the URL into a bare repository. Remote URLs stay byte-exact.
        let url = local_origin(&origin, &checkout);
        let key = format!("{:x}", Sha256::digest(url.as_bytes()));
        let path = cache.join(key);
        if path.try_exists()? {
            let mirror = Self {
                path,
                registered: checkout.clone(),
            };
            if let Ok(_lock) = mirror.refresh_lock() {
                // A network outage never prevents a launch from reusing the registered clone.
                let copy_tracking = !mirror.path.join("ox-origin-refreshed").exists();
                let _ = local_fetch(&mirror.path, &checkout, copy_tracking);
            }
            return Ok(mirror);
        }
        let temp = tempfile::Builder::new()
            .prefix(".mirror-")
            .tempdir_in(cache)?;
        bare(temp.path(), hash_size)?;
        run(git(temp.path()).args(["remote", "add", "origin"]).arg(&url))?;
        // Fetch locally so the cache owns its objects, even if the selected
        // checkout is itself shared or is removed later. Keep checked-out HEAD
        // as well as the origin-tracking tips (a worktree may be detached).
        local_fetch(temp.path(), &checkout, true)?;
        match fs::rename(temp.path(), &path) {
            Ok(()) => (),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
                ) && path.is_dir() => {}
            Err(error) => return Err(error),
        }
        Ok(Self {
            path,
            registered: checkout,
        })
    }

    pub fn registered(&self) -> &Path {
        &self.registered
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn refresh_lock(&self) -> io::Result<fs::File> {
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.join("ox-refresh.lock"))?;
        f.try_lock()?;
        Ok(f)
    }

    /// Best-effort upstream refresh. The bound applies only to the optional Git
    /// fetch. The source still supplies all missing objects when the origin is slow or offline.
    pub fn refresh(&self) -> io::Result<()> {
        self.fetch(false)
    }
    pub fn refresh_if_stale(&self) -> io::Result<()> {
        self.fetch(true)
    }
    fn fetch(&self, cached: bool) -> io::Result<()> {
        let _lock = self.refresh_lock()?;
        let stamp = self.path.join("ox-last-fetch");
        if cached
            && stamp
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age < Duration::from_secs(30))
        {
            return Ok(());
        }
        fs::write(stamp, [])?;
        let mut child = git(&self.path)
            .args([
                "fetch",
                "--atomic",
                "--prune",
                "--no-tags",
                "--no-recurse-submodules",
                "origin",
                "+refs/heads/*:refs/remotes/origin/*",
            ])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait()? {
                if status.success() {
                    return fs::write(self.path.join("ox-origin-refreshed"), []);
                }
                return Err(io::Error::other("Git mirror origin refresh failed."));
            }
            if Instant::now() >= until {
                // The unreaped child still owns its PID and its new process group.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                child.wait()?;
                return Err(io::ErrorKind::TimedOut.into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Intersect advertised source commits with the target object store in one
    /// Git process. A linked host only receives IDs it already named.
    pub fn known(&self, candidates: &[String]) -> io::Result<Vec<String>> {
        if !complete(&self.path)? {
            return Ok(Vec::new());
        }
        known_commits(&self.path, candidates)
    }
}

fn complete(repo: &Path) -> io::Result<bool> {
    if run(git(repo).args(["rev-parse", "--is-shallow-repository"]))? != b"false\n" {
        return Ok(false);
    }
    let partial = git(repo)
        .args([
            "config",
            "--get-regexp",
            r"^(extensions\.partialclone|remote\..*\.promisor)$",
        ])
        .output()?;
    Ok(partial.status.code() == Some(1))
}

fn local_fetch(mirror: &Path, checkout: &Path, copy_tracking: bool) -> io::Result<()> {
    // Remember what this checkout advertised before fetching it again. A stale
    // local clone must not rewind a fresher tip fetched directly from the origin.
    let previous = if copy_tracking {
        BTreeMap::new()
    } else {
        local_tips(mirror)?
    };
    let mut cmd = git(mirror);
    cmd.args([
        "fetch",
        "--atomic",
        "--quiet",
        "--no-tags",
        "--no-recurse-submodules",
        "--",
    ])
    .arg(checkout);
    if super::head(checkout)?.is_some() {
        cmd.arg("+HEAD:refs/ox/seed");
    }
    cmd.arg("+refs/remotes/origin/*:refs/ox/local/*");
    if copy_tracking {
        cmd.arg("+refs/remotes/origin/*:refs/remotes/origin/*");
    }
    run(&mut cmd)?;
    if !copy_tracking {
        for (name, tip) in local_tips(mirror)? {
            if previous.get(&name) == Some(&tip) {
                continue;
            }
            let name = [b"refs/remotes/origin/".as_slice(), &name].concat();
            let reference = OsStr::from_bytes(&name);
            let current = git(mirror)
                .args(["rev-parse", "--verify", "--quiet"])
                .arg(reference)
                .output()?;
            if current.status.success() {
                let old = oid(current.stdout.trim_ascii_end())?;
                if old == tip
                    || !git(mirror)
                        .args(["merge-base", "--is-ancestor", old, &tip])
                        .status()?
                        .success()
                {
                    continue;
                }
            } else if current.status.code() != Some(1) {
                return Err(io::Error::other(
                    "Cannot read a worker mirror tracking ref.",
                ));
            }
            run(git(mirror).arg("update-ref").arg(reference).arg(tip))?;
        }
    }
    Ok(())
}

fn local_tips(mirror: &Path) -> io::Result<BTreeMap<Vec<u8>, String>> {
    let refs = run(git(mirror).args([
        "for-each-ref",
        "--format=%(objectname) %(refname:strip=3)",
        "refs/ox/local/",
    ]))?;
    refs.split(|b| *b == b'\n')
        .filter(|r| !r.is_empty())
        .map(|row| {
            let split = row
                .iter()
                .position(|b| *b == b' ')
                .ok_or_else(|| io::Error::other("Invalid local tracking ref."))?;
            Ok((row[split + 1..].to_vec(), oid(&row[..split])?.to_owned()))
        })
        .collect()
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

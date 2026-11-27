//! Freeze a linked worker's result and import its exact commit into the parent.
//! Git objects and working files live in an isolated checkout until the operator merges.
use super::*;
use serde::{Deserialize, Serialize};

/// Captured by the receiving host after installation and before native code runs.
/// Paths use Unix bytes so a non-UTF-8 private input retains its identity.
#[derive(Serialize, Deserialize)]
pub struct Baseline {
    snapshot: String,
    repositories: Vec<PrivatePaths>,
}
#[derive(Serialize, Deserialize)]
struct PrivatePaths {
    directory: Vec<u8>,
    names: Vec<Vec<u8>>,
}

impl Baseline {
    pub fn read_checkout(root: &Path, snapshot: &str) -> io::Result<Self> {
        if object(root, "HEAD")? != oid(snapshot.as_bytes())? {
            return Err(io::Error::other(
                "Installed result base differs from its checkout receipt.",
            ));
        }
        let mut base = Self {
            snapshot: snapshot.to_owned(),
            repositories: Vec::new(),
        };
        base.record(root, Path::new(""), 0)?;
        Ok(base)
    }
    fn record(&mut self, root: &Path, relative: &Path, depth: usize) -> io::Result<()> {
        submodules::check_depth(depth)?;
        let dir = root.join(relative);
        let mut reader = io::BufReader::new(File::open(dir.join(files::BASELINE))?);
        let mut names = Vec::new();
        loop {
            let name = files::read_field(&mut reader)?;
            if name.is_empty() {
                break;
            }
            files::validate(Path::new(OsStr::from_bytes(&name)))?;
            let mut details = [0; 35];
            reader.read_exact(&mut details)?;
            names.push(name);
        }
        self.repositories.push(PrivatePaths {
            directory: relative.as_os_str().as_bytes().to_vec(),
            names,
        });
        for path in submodules::gitlinks(&mut git(&dir))?.into_keys() {
            if dir.join(&path).join(".git").is_dir() {
                self.record(root, &relative.join(path), depth + 1)?;
            }
        }
        Ok(())
    }
}
pub(super) struct Inputs<'a> {
    base: &'a Baseline,
    relative: PathBuf,
}
impl<'a> Inputs<'a> {
    pub(super) fn child(&self, path: &Path) -> Self {
        Self {
            base: self.base,
            relative: self.relative.join(path),
        }
    }
    pub(super) fn paths(&self) -> Vec<&'a Path> {
        self.base
            .repositories
            .iter()
            .find(|r| r.directory == self.relative.as_os_str().as_bytes())
            .into_iter()
            .flat_map(|r| r.names.iter().map(|v| Path::new(OsStr::from_bytes(v))))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    #[default]
    Branch,
    Ref,
    Tailnet,
}
impl Destination {
    pub fn reference(self, job: u64) -> Option<String> {
        match self {
            Self::Branch => Some(format!("refs/heads/ox/{job}")),
            Self::Ref => Some(format!("refs/ox/jobs/{job}/result")),
            Self::Tailnet => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSummary {
    pub snapshot: String,
    pub commit: String,
    pub diffstat: String,
    pub private_files: u64,
}

pub struct Frozen {
    pub packet: Snapshot,
    pub summary: ChangeSummary,
}
impl Frozen {
    pub fn capture(
        staging: &Path,
        checkout: &Path,
        base: &Baseline,
        known: &[String],
    ) -> io::Result<Self> {
        let packet = Snapshot::capture_inner(
            staging,
            checkout,
            known,
            0,
            Some(Inputs {
                base,
                relative: PathBuf::new(),
            }),
        )?;
        let repo = packet.result_objects.as_ref().unwrap().path().join("repo");
        ancestry(&repo, &base.snapshot, &packet.commit)?;
        let diff = run(git(&repo).env("LC_ALL", "C").args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--shortstat",
            &base.snapshot,
            &packet.commit,
            "--",
        ]))?;
        let summary = ChangeSummary {
            snapshot: base.snapshot.clone(),
            commit: packet.commit.clone(),
            diffstat: String::from_utf8_lossy(&diff).trim().to_owned(),
            private_files: packet.untracked.files,
        };
        Ok(Self { packet, summary })
    }
    /// Reuse the frozen private-file stream byte for byte. The parent's HEAD at
    /// launch is normally either the snapshot or its first parent when dirty.
    pub fn incremental_archive(&self, destination: &Path) -> io::Result<Option<String>> {
        use io::{Seek, SeekFrom};
        let repo = self
            .packet
            .result_objects
            .as_ref()
            .unwrap()
            .path()
            .join("repo");
        let base = object(&repo, &format!("{}^1", self.summary.snapshot))
            .unwrap_or_else(|_| self.summary.snapshot.clone());
        if base == self.summary.commit || self.packet.bundle_bytes < 128 * 1024 {
            return Ok(None);
        }
        let tmp = tempfile::tempdir_in(destination.parent().unwrap())?;
        let bundle = tmp.path().join("increment.bundle");
        run(git(&repo)
            .args(["bundle", "create"])
            .arg(&bundle)
            .arg(SNAPSHOT_REF)
            .arg(format!("^{base}")))?;
        let mut bundle = File::open(bundle)?;
        let size = bundle.metadata()?.len();
        if size >= self.packet.bundle_bytes {
            return Ok(None);
        }
        let mut frozen = self.packet.open()?;
        let mut output = io::BufWriter::new(File::create(destination)?);
        frozen.seek(SeekFrom::Start(MAGIC.len() as u64))?;
        output.write_all(MAGIC)?;
        for _ in 0..2 {
            files::write_field(&mut output, &files::read_field(&mut frozen)?)?;
        }
        frozen.seek(SeekFrom::Current(8))?;
        output.write_all(&size.to_be_bytes())?;
        loop {
            let boundary = files::read_field(&mut frozen)?;
            files::write_field(&mut output, &boundary)?;
            if boundary.is_empty() {
                break;
            }
        }
        frozen.seek(SeekFrom::Current(
            i64::try_from(self.packet.bundle_bytes).map_err(io::Error::other)?,
        ))?;
        io::copy(&mut bundle, &mut output)?;
        io::copy(&mut frozen, &mut output)?;
        output.flush()?;
        output.get_ref().sync_all()?;
        Ok(Some(base))
    }

    /// The private archive also carries submodule and LFS worktrees and remaining untracked inputs.
    pub fn archive(&self, destination: &Path) -> io::Result<()> {
        self.packet.retain_archive(destination)
    }
}

/// Use only the target's registered upstream and credentials. The result
/// checkout is unpacked from frozen bytes with host-generated Git metadata.
pub fn publish_checkout(
    repo: &Path,
    registered: &Path,
    base: &Baseline,
    job: u64,
    dest: Destination,
    summary: &ChangeSummary,
) -> io::Result<String> {
    ancestry(repo, &base.snapshot, &summary.commit)?;
    let reference = dest
        .reference(job)
        .ok_or_else(|| io::Error::other("Tailnet delivery was selected."))?;
    check_public_history(repo, base, &summary.commit)?;
    let remote = push_destination(registered)?;
    let fingerprint = destination_digest(&remote);
    let spec = format!("{}:{reference}", summary.commit);
    // A job owns a new remote ref. A retry may find the same commit there.
    let current = run(git(repo).args(["ls-remote", "--refs", "--"]).arg(&remote))?;
    let mut tips = std::collections::BTreeSet::new();
    for line in current.split(|b| *b == b'\n').filter(|s| !s.is_empty()) {
        let tab = line
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| io::Error::other("Invalid origin ref listing."))?;
        let id = oid(&line[..tab])?;
        if line[tab + 1..] == *reference.as_bytes() {
            if id == summary.commit {
                return Ok(fingerprint);
            }
            return Err(io::Error::other(
                "A different commit already owns the result ref.",
            ));
        }
        tips.insert(id.to_owned());
    }
    check_dependencies(
        repo,
        registered,
        &summary.commit,
        &tips.into_iter().collect::<Vec<_>>(),
    )?;
    run(git(repo)
        .args(["push", "--no-follow-tags", "--recurse-submodules=no"])
        .arg(format!("--force-with-lease={reference}:"))
        .arg("--")
        .arg(remote)
        .arg(spec))
    .map_err(|e| io::Error::other(format!("Origin refused the result push: {e}")))?;
    Ok(fingerprint)
}

fn push_destination(registered: &Path) -> io::Result<OsString> {
    let remote = line(run(
        git(registered).args(["remote", "get-url", "--push", "--all", "origin"])
    )?)?;
    if remote.as_bytes().contains(&b'\n') {
        return Err(io::Error::other(
            "The origin has several push destinations.",
        ));
    }
    Ok(mirror::local_origin(&remote, registered))
}

fn destination_digest(remote: &OsStr) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(remote.as_bytes()))
}

/// A changed URL or moved ref stays under its new owner's control. The private
/// result archive survives cleanup of its public Git ref.
pub fn retire(
    registered: &Path,
    repo: &Path,
    job: u64,
    dest: Destination,
    tip: &str,
    fingerprint: &str,
) -> io::Result<()> {
    oid(tip.as_bytes())?;
    let Some(reference) = dest.reference(job) else {
        return Ok(());
    };
    let remote = push_destination(registered)?;
    if destination_digest(&remote) != fingerprint {
        return Err(io::Error::other(
            "The result's original push destination is no longer configured.",
        ));
    }
    let current = run(git(repo)
        .args(["ls-remote", "--refs", "--"])
        .arg(&remote)
        .arg(&reference))?;
    let expected = format!("{tip}\t{reference}");
    if !current
        .split(|b| *b == b'\n')
        .any(|line| line == expected.as_bytes())
    {
        return Ok(());
    }
    run(git(repo)
        .args(["push", "--no-follow-tags", "--recurse-submodules=no"])
        .arg(format!("--force-with-lease={reference}:{tip}"))
        .arg("--")
        .arg(&remote)
        .arg(format!(":{reference}")))?;
    Ok(())
}

pub fn source_with_base(project: &Path, commit: &str) -> Option<PathBuf> {
    oid(commit.as_bytes()).ok()?;
    let root = git_path(project, &["rev-parse", "--show-toplevel"]).ok()?;
    run(git(&root).args(["cat-file", "-e", &format!("{commit}^{{commit}}")])).ok()?;
    Some(root)
}

pub fn source_branch(project: &Path) -> Option<String> {
    let branch = run(git(project).args(["symbolic-ref", "--quiet", "HEAD"])).ok()?;
    let branch = std::str::from_utf8(&branch).ok()?.trim_end();
    branch.starts_with("refs/heads/").then(|| branch.to_owned())
}

pub fn merged(project: &Path, branch: &str, commit: &str) -> io::Result<bool> {
    if !branch.starts_with("refs/heads/") {
        return Ok(false);
    }
    oid(commit.as_bytes())?;
    let output = git(project)
        .args(["merge-base", "--is-ancestor", commit, branch])
        .output()?;
    Ok(output.status.success())
}

// A normal Git push transfers neither nested repositories nor LFS objects.
fn check_dependencies(
    repo: &Path,
    registered: &Path,
    tip: &str,
    advertised: &[String],
) -> io::Result<()> {
    use std::io::Seek;
    let mut revs = tempfile::tempfile()?;
    writeln!(revs, "{tip}")?;
    for id in mirror::known_commits(repo, advertised)? {
        writeln!(revs, "^{id}")?;
    }
    revs.rewind()?;
    let changes = run(git(repo)
        .args([
            "log",
            "--stdin",
            "--format=",
            "--no-abbrev",
            "--raw",
            "--no-renames",
            "--root",
            "-m",
            "-r",
            "-z",
        ])
        .stdin(revs.try_clone()?))?;
    let mut records = changes.split(|b| *b == 0).filter(|s| !s.is_empty());
    while let Some(header) = records.next() {
        let name = records
            .next()
            .ok_or_else(|| io::Error::other("Missing Git change path."))?;
        let mut fields = header.trim_ascii().split(|b| *b == b' ');
        if fields.nth(1) == Some(&b"160000"[..]) {
            let path = Path::new(OsStr::from_bytes(name));
            files::validate(path)?;
            let commit = oid(fields.nth(1).unwrap_or_default())?;
            let module = registered.join(path);
            if !module.join(".git").exists()
                || run(git(&module).args([
                    "for-each-ref",
                    "--count=1",
                    "--format=%(refname)",
                    "--contains",
                    commit,
                    "refs/remotes/origin/",
                ]))
                .unwrap_or_default()
                .is_empty()
            {
                return Err(io::Error::other(
                    "The result references a submodule commit that the target cannot verify on its origin.",
                ));
            }
        }
    }
    revs.rewind()?;
    let new = run(git(repo)
        .args([
            "rev-list",
            "--objects",
            "--no-object-names",
            "--filter=object:type=blob",
            "--filter=blob:limit=1025",
            "--stdin",
        ])
        .stdin(revs))?;
    let mut ids = tempfile::tempfile()?;
    ids.write_all(&new)?;
    ids.rewind()?;
    let types = run(git(repo)
        .args([
            "cat-file",
            "--batch-check=%(objecttype) %(objectname) %(objectsize)",
        ])
        .stdin(ids))?;
    let mut blobs = Vec::new();
    for line in types.split(|b| *b == b'\n') {
        if let Some(fields) = line.strip_prefix(b"blob ") {
            let mut parts = fields.split(|b| *b == b' ');
            let id = oid(parts.next().unwrap_or_default())?;
            if parts
                .next()
                .and_then(|n| std::str::from_utf8(n).ok()?.parse::<u64>().ok())
                .is_some_and(|n| n <= 1024)
            {
                blobs.push(id);
            }
        }
    }
    for group in blobs.chunks(256) {
        let mut ids = tempfile::tempfile()?;
        for id in group {
            writeln!(ids, "{id}")?;
        }
        ids.rewind()?;
        let contents = run(git(repo).args(["cat-file", "--batch"]).stdin(ids))?;
        let mut bytes = &contents[..];
        while !bytes.is_empty() {
            let end = bytes
                .iter()
                .position(|b| *b == b'\n')
                .ok_or_else(|| io::Error::other("Incomplete Git blob header."))?;
            let size = std::str::from_utf8(&bytes[..end])
                .ok()
                .and_then(|s| s.split_whitespace().last()?.parse::<usize>().ok())
                .ok_or_else(|| io::Error::other("Invalid Git blob size."))?;
            bytes = &bytes[end + 1..];
            let (blob, rest) = bytes
                .split_at_checked(size)
                .ok_or_else(|| io::Error::other("Incomplete Git blob."))?;
            if super::lfs::is_pointer(blob) {
                return Err(io::Error::other(
                    "The result adds an LFS pointer whose content is not verified on the origin.",
                ));
            }
            bytes = rest
                .strip_prefix(b"\n")
                .ok_or_else(|| io::Error::other("Incomplete Git blob response."))?;
        }
    }
    Ok(())
}

fn ancestry(repo: &Path, snapshot: &str, tip: &str) -> io::Result<()> {
    oid(snapshot.as_bytes())?;
    oid(tip.as_bytes())?;
    if !git(repo)
        .args(["merge-base", "--is-ancestor", snapshot, tip])
        .output()?
        .status
        .success()
    {
        return Err(io::Error::other(
            "The result no longer descends from the installed snapshot. The worker checkout is retained for recovery.",
        ));
    }
    Ok(())
}
fn check_public_history(repo: &Path, base: &Baseline, tip: &str) -> io::Result<()> {
    // Check all new history, including changes later reverted or merged. Paths
    // transferred privately may only be returned over the host link.
    let inputs = Inputs {
        base,
        relative: PathBuf::new(),
    };
    for paths in inputs.paths().chunks(64) {
        let changes = run(git(repo)
            .args([
                "--literal-pathspecs",
                "rev-list",
                "--full-history",
                "-1",
                tip,
            ])
            .arg(format!("^{}", base.snapshot))
            .arg("--")
            .args(paths))?;
        if !changes.is_empty() {
            return Err(io::Error::other(
                "Worker history includes a transferred private input. Returning it over the link.",
            ));
        }
    }
    Ok(())
}

/// Only installs the named result ref. Receiving objects never writes FETCH_HEAD,
/// checks out a branch, stages a change, or replaces an existing different result.
pub fn import(
    parent: &Path,
    received: &Path,
    job: u64,
    summary: &ChangeSummary,
) -> io::Result<String> {
    let tip = object(received, "HEAD^{commit}")?;
    if tip != summary.commit {
        return Err(io::Error::other(
            "Received worker checkout does not match the result commit.",
        ));
    }
    ancestry(received, &summary.snapshot, &tip)?;
    let reference = format!("refs/ox/jobs/{job}/result");
    run(git(parent)
        .args([
            "-c",
            "fetch.writeCommitGraph=false",
            "fetch",
            "--no-auto-maintenance",
            "--no-write-fetch-head",
            "--no-tags",
            "--no-recurse-submodules",
            "--refmap=",
            "--",
        ])
        .arg(received)
        .arg(&tip))?;
    let existing = git(parent)
        .args(["rev-parse", "--verify", "--quiet", &reference])
        .output()?;
    if existing.status.success() && existing.stdout.trim_ascii_end() == tip.as_bytes() {
        return Ok(reference);
    }
    let zeros = "0".repeat(tip.len());
    run(git(parent).args(["update-ref", &reference, &tip, &zeros])).or_else(|e| {
        if object(parent, &reference).ok().as_ref() == Some(&tip) {
            Ok(Vec::new())
        } else {
            if object(parent, &reference).is_ok() {
                Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("The parent already has a different {reference}. The worker commit is {tip} and its checkout is at {}.", received.display())))
            } else { Err(e) }
        }
    })?;
    Ok(reference)
}

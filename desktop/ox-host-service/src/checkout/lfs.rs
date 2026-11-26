//! A bundle contains LFS pointers. Copy materialized content from the source
//! worktree separately, and verify the target's clean filter round-trips it
//! before exposing the job. Hosts that both use pointer-only worktrees need
//! no LFS installation to transfer those pointers.
use super::{
    files::{self, Tree},
    git, line, run,
};
use rustix::fs::{Mode, OFlags, openat};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read, Seek, Write},
    os::unix::ffi::OsStrExt,
    path::Path,
    process::Command,
};

struct Pointer {
    sha: String,
    size: u64,
}
fn pointer(text: &[u8]) -> Option<Pointer> {
    let mut lines = std::str::from_utf8(text).ok()?.lines();
    if lines.next()? != "version https://git-lfs.github.com/spec/v1" {
        return None;
    }
    let mut sha = None;
    let mut size = None;
    for line in lines {
        if let Some(id) = line.strip_prefix("oid sha256:") {
            if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            sha = Some(id.to_ascii_lowercase());
        }
        if let Some(n) = line.strip_prefix("size ") {
            size = n.parse().ok();
        }
    }
    Some(Pointer {
        sha: sha?,
        size: size?,
    })
}
fn open(tree: &Tree, path: &Path, flags: OFlags) -> io::Result<File> {
    let (dir, name) = tree.parent(path, false)?;
    let file = File::from(openat(
        dir,
        name,
        flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("Expected a regular LFS worktree file"));
    }
    Ok(file)
}

pub(super) fn pack(
    root: &Path,
    source: impl Fn() -> Command,
    output: &mut impl Write,
) -> io::Result<()> {
    // Path names from Git remain NUL-framed throughout.
    let mut paths = tempfile::tempfile()?;
    let status = source()
        .args(["ls-files", "--cached", "-z"])
        .stdout(paths.try_clone()?)
        .stderr(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other("Git could not list tracked files"));
    }
    paths.rewind()?;
    let attributes = run(source()
        .args(["check-attr", "--cached", "-z", "--stdin", "filter"])
        .stdin(paths))?;
    let tree = Tree::open(root)?;
    let mut fields = attributes.split(|b| *b == 0);
    while let (Some(bytes), Some(_), Some(driver)) = (fields.next(), fields.next(), fields.next()) {
        if driver != b"lfs" {
            continue;
        }
        let path = Path::new(OsStr::from_bytes(bytes));
        files::validate(path)?;
        let mut indexed = std::ffi::OsString::from(":");
        indexed.push(path);
        // A repository with no configured filter can already store the actual
        // bytes as a regular blob. Never load such a blob into memory.
        let len = line(run(source().args(["cat-file", "-s"]).arg(&indexed))?)?;
        let len: usize = len
            .to_str()
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| io::Error::other("Invalid Git blob length"))?;
        if len > 1024 {
            continue;
        }
        let content = run(source().args(["cat-file", "blob"]).arg(&indexed))?;
        let Some(pointer) = pointer(&content) else {
            continue;
        };
        let mut file = match open(&tree, path, OFlags::RDONLY) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let size = file.metadata()?.len();
        if size == content.len() as u64 {
            let mut actual = Vec::new();
            Read::by_ref(&mut file)
                .take(size)
                .read_to_end(&mut actual)?;
            if actual == content {
                continue;
            }
            file.rewind()?;
        }
        if size != pointer.size {
            return Err(io::Error::other(
                "An LFS file changed during capture. Retry the launch.",
            ));
        }
        files::write_field(output, bytes)?;
        output.write_all(&size.to_be_bytes())?;
        files::copy_exact(&mut file, output, size)?;
    }
    files::write_field(output, &[])
}

pub(super) fn unpack(root: &Path, input: &mut impl Read) -> io::Result<()> {
    let tree = Tree::open(root)?;
    loop {
        let path = files::read_field(input)?;
        if path.is_empty() {
            return Ok(());
        }
        let path = Path::new(OsStr::from_bytes(&path));
        let mut file = open(&tree, path, OFlags::RDWR)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file).take(1025).read_to_end(&mut bytes)?;
        let target = pointer(&bytes).ok_or_else(|| {
            io::Error::other("Transferred content does not match a tracked LFS pointer")
        })?;
        let mut size = [0; 8];
        input.read_exact(&mut size)?;
        if u64::from_be_bytes(size) != target.size {
            return Err(io::Error::other(
                "Transferred LFS size does not match the snapshot",
            ));
        }
        file.rewind()?;
        file.set_len(0)?;
        files::copy_exact(input, &mut file, target.size)?;
        file.rewind()?;
        let mut hash = Sha256::new();
        io::copy(&mut file, &mut hash)?;
        if format!("{:x}", hash.finalize()) != target.sha {
            return Err(io::Error::other(
                "Transferred LFS content does not match its Git pointer",
            ));
        }
        // The destination must be able to edit and commit this file as LFS;
        // otherwise git add could store its entire contents as a normal blob.
        file.rewind()?;
        let actual = line(
            run(git(root)
                .arg("hash-object")
                .arg("--path")
                .arg(path)
                .arg("--stdin")
                .stdin(file))
            .map_err(|_| {
                io::Error::other("Configure Git LFS on the receiving host for this repository")
            })?,
        )?;
        let mut indexed = std::ffi::OsString::from(":");
        indexed.push(path);
        if actual != line(run(git(root).arg("rev-parse").arg(indexed))?)? {
            return Err(io::Error::other(
                "Configure Git LFS on the receiving host so edited files retain their LFS pointers",
            ));
        }
    }
}

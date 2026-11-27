//! Program fixtures for tests. The desktop crates include this file too.

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

/// Write a file that the test is about to run.
///
/// `cp` writes it, so the test process never holds it open for writing. A child
/// forked by a parallel test keeps a copy of such a descriptor until its own
/// exec, and running the file meanwhile fails with "Text file busy".
pub fn write(path: &Path, contents: &str) {
    let source = path.with_extension("source");
    fs::write(&source, contents).unwrap();
    let copied = Command::new("cp").arg(&source).arg(path).status().unwrap();
    assert!(copied.success(), "cp to {}", path.display());
    fs::remove_file(source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

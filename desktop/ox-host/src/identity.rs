//! Stable even if the host temporarily runs an older release that rewrites its ledger.
use std::{
    fs::{self, File},
    io::{self, Write},
    path::Path,
};
use uuid::Uuid;
pub(crate) fn load(dir: &Path) -> io::Result<Uuid> {
    let path = dir.join("host-id");
    match fs::read_to_string(&path) {
        Ok(id) => {
            Uuid::parse_str(id.trim()).map_err(|_| io::Error::other("Invalid OX host-id file"))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let id = Uuid::new_v4();
            let mut file = tempfile::NamedTempFile::new_in(dir)?;
            writeln!(file, "{id}")?;
            file.as_file().sync_all()?;
            match file.persist_noclobber(&path) {
                Ok(_) => {
                    File::open(dir)?.sync_all()?;
                    Ok(id)
                }
                Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => load(dir),
                Err(e) => Err(e.error),
            }
        }
        Err(e) => Err(e),
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn identity_outlives_restarts_and_legacy_ledger_rewrites() {
        let root = tempfile::tempdir().unwrap();
        let first = super::load(root.path()).unwrap();
        std::fs::write(
            root.path().join(crate::durable::LEDGER),
            r#"{"scopes":[],"reserved":0}"#,
        )
        .unwrap();
        assert_eq!(super::load(root.path()).unwrap(), first);
        assert_ne!(
            super::load(tempfile::tempdir().unwrap().path()).unwrap(),
            first
        );
        std::fs::write(root.path().join("host-id"), "broken").unwrap();
        assert!(super::load(root.path()).is_err());
    }
}

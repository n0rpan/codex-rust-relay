//! Operator-selected name and placement labels shared by all devices.
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

#[derive(Clone, Default, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostProfile {
    pub name: String,
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub before: HostProfile,
    pub after: HostProfile,
}

impl HostProfile {
    pub fn read(dir: &Path) -> io::Result<Self> {
        match fs::File::open(dir.join("host-profile.json")) {
            Ok(file) => Ok(serde_json::from_reader(file)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }
    pub fn normalized(mut self) -> io::Result<Self> {
        self.name = self.name.trim().to_owned();
        if self.name.len() > 128 || self.name.chars().any(char::is_control) {
            return Err(io::Error::other(
                "Use a single-line host name of up to 128 bytes.",
            ));
        }
        if self.labels.len() > 64
            || self.labels.iter().any(|s| {
                s.is_empty()
                    || s.len() > 64
                    || s.chars()
                        .any(|c| c.is_whitespace() || c.is_control() || c == ',')
            })
        {
            return Err(io::Error::other(
                "Use up to 64 single-word labels, each up to 64 bytes.",
            ));
        }
        self.labels.sort();
        self.labels.dedup();
        Ok(self)
    }
}
#[cfg(feature = "server")]
impl Edit {
    pub(crate) fn save(self, dir: &Path) -> io::Result<HostProfile> {
        if self.before != HostProfile::read(dir)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Host details changed. Reopen them and try again.",
            ));
        }
        let profile = self.after.normalized()?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(file.as_file_mut(), &profile)?;
        file.as_file().sync_all()?;
        file.persist(dir.join("host-profile.json"))?;
        fs::File::open(dir)?.sync_all()?;
        Ok(profile)
    }
}

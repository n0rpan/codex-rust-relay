//! User-owned Desktop appearance and input settings shared by native and paired settings clients.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThemeSource {
    #[default]
    System,
    Light,
    Dark,
}
/// Who answers the approvals and questions of a worker an agent started.
#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerApprovals {
    /// The parent agent answers what the host's policy lets it; the rest waits
    /// for a person.
    #[default]
    Parent,
    /// Every request waits for a person.
    Operator,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Behavior {
    pub enter_sends: bool,
    pub theme: ThemeSource,
    pub font_size: u8,
    pub reduce_motion: bool,
    /// Show the weather from MET Norway on the header horizon.
    pub live_weather: bool,
    pub background_updates: bool,
    pub worker_approvals: WorkerApprovals,
}
impl Default for Behavior {
    fn default() -> Self {
        Self {
            enter_sends: true,
            theme: ThemeSource::System,
            font_size: 16,
            reduce_motion: false,
            live_weather: true,
            background_updates: true,
            worker_approvals: WorkerApprovals::Parent,
        }
    }
}
#[derive(Serialize)]
pub struct Snapshot {
    pub revision: String,
    pub behavior: Behavior,
}
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Applied,
    ChangedOnDisk { revision: String },
}

#[path = "../../../src/desktop_state.rs"]
mod desktop_state;
pub use desktop_state::directory;
pub fn path() -> io::Result<PathBuf> {
    directory()
        .map(|p| p.join("preferences.json"))
        .ok_or_else(|| io::Error::other("Desktop state directory unavailable"))
}
fn document(path: &Path) -> io::Result<Value> {
    match fs::File::open(path) {
        Ok(file) => serde_json::from_reader(file).map_err(io::Error::from),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e),
    }
}
fn snapshot(value: Value) -> io::Result<Snapshot> {
    let mut behavior: Behavior = serde_json::from_value(value)?;
    behavior.font_size = behavior.font_size.clamp(12, 24);
    Ok(Snapshot {
        revision: revision(behavior),
        behavior,
    })
}
pub fn revision(value: Behavior) -> String {
    serde_json::to_string(&value).expect("Scalar desktop preferences are serializable")
}
pub fn read() -> io::Result<Snapshot> {
    snapshot(document(&path()?)?)
}
pub fn write(expected: &str, next: Behavior) -> io::Result<Outcome> {
    write_at(&path()?, expected, next)
}
fn write_at(path: &Path, expected: &str, next: Behavior) -> io::Result<Outcome> {
    if !(12..=24).contains(&next.font_size) {
        return Err(io::Error::other("Font size must be between 12 and 24"));
    }
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("Desktop state directory unavailable"))?;
    fs::create_dir_all(dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join(".preferences.lock"))?;
    lock.lock()?;
    let mut doc = document(path)?;
    let now = snapshot(doc.clone())?;
    if now.revision != expected {
        return Ok(Outcome::ChangedOnDisk {
            revision: now.revision,
        });
    }
    let object = doc
        .as_object_mut()
        .ok_or_else(|| io::Error::other("Invalid desktop preferences"))?;
    let Value::Object(fields) = serde_json::to_value(next)? else {
        unreachable!()
    };
    object.extend(fields);
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut temp, &doc)?;
    temp.flush()?;
    temp.persist(path)?;
    Ok(Outcome::Applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revision_checked_edits_preserve_extension_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("preferences.json");
        fs::write(&path, r#"{"future":{"tint":"amber"}}"#).unwrap();
        let original = Behavior::default();
        let changed = Behavior {
            font_size: 20,
            ..original
        };
        assert!(matches!(
            write_at(&path, &revision(original), changed).unwrap(),
            Outcome::Applied
        ));
        assert!(matches!(
            write_at(&path, &revision(original), original).unwrap(),
            Outcome::ChangedOnDisk { .. }
        ));
        assert_eq!(document(&path).unwrap()["future"]["tint"], "amber");
    }
}

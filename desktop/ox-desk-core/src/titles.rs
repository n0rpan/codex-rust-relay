//! Generated display names live alongside workbench state. Stock names always win.
use crate::{codex::history::ThreadRow, runtime::Runtime};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::DirBuilderExt;
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Key {
    pub project: PathBuf,
    pub runtime: Runtime,
    pub provider: String,
    pub alias: Option<String>,
    pub id: String,
}
impl Key {
    pub(crate) fn row(row: &ThreadRow) -> Self {
        let runtime = if row.id.starts_with("claude:") {
            Runtime::Claude
        } else {
            Runtime::Codex
        };
        let subscription = matches!(
            row.model_provider.as_str(),
            "openai" | crate::store::claude::SUBSCRIPTION_PROVIDER
        );
        Self {
            project: row.cwd.clone(),
            runtime,
            provider: row.model_provider.clone(),
            alias: if subscription {
                None
            } else {
                row.model.clone()
            },
            id: row.id.clone(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Title {
    pub key: Key,
    pub name: String,
}
#[derive(Clone, Default)]
pub struct Index(Arc<RwLock<HashMap<Key, String>>>);
impl Index {
    pub fn load(dir: &Path) -> io::Result<Self> {
        let index = Self::default();
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(index),
            Err(e) => return Err(e),
        };
        for file in entries.flatten() {
            let p = file.path();
            if p.extension().is_some_and(|e| e == "json")
                && file.file_type().is_ok_and(|m| m.is_file())
                && file.metadata().is_ok_and(|m| m.len() <= 32 * 1024)
                && let Ok(data) = fs::read(p)
                && let Ok(t) = serde_json::from_slice::<Title>(&data)
            {
                index.insert(t);
            }
        }
        Ok(index)
    }
    pub(crate) fn insert(&self, t: Title) {
        self.0.write().unwrap().insert(t.key, t.name);
    }
    pub fn decorate(&self, row: &mut ThreadRow) {
        row.generated_title = self.0.read().unwrap().get(&Key::row(row)).cloned();
    }
}
impl Title {
    pub(crate) fn persist(&self, dir: &Path) -> io::Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(dir)?;
        // FNV-1a supplies a stable filename independent of Unicode, path syntax and length.
        // Verify the full identity when a file already exists.
        let hash = serde_json::to_vec(&self.key)?
            .into_iter()
            .fold(0xcbf29ce484222325u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
            });
        let path = dir.join(format!("{hash:016x}.json"));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut tmp, self)?;
        match tmp.persist_noclobber(&path) {
            Ok(_) => Ok(self.clone()),
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {
                let prior: Self = serde_json::from_slice(&fs::read(path)?)?;
                if prior.key != self.key {
                    return Err(io::Error::other("Session title belongs to another project"));
                }
                Ok(prior)
            }
            Err(e) => Err(e.error),
        }
    }
}
pub(crate) fn concise(text: &str) -> Option<String> {
    let name = text
        .trim()
        .trim_matches(|c| matches!(c, '"' | '“' | '”'))
        .trim();
    (!name.is_empty()
        && name.chars().count() <= 80
        && name.split_whitespace().count() <= 12
        && !name.chars().any(char::is_control))
    .then(|| name.to_owned())
}

mod inference;
pub(crate) use inference::generate;
pub struct Launcher {
    pub ox: PathBuf,
    pub path: std::ffi::OsString,
    pub directory: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn titles_survive_reopen_but_only_for_their_own_session_and_route() {
        let dir = tempfile::tempdir().unwrap();
        let mut row: ThreadRow = serde_json::from_value(json!({
            "id":"same-id","cwd":"/project","modelProvider":"codeox","model":"vendor/a",
            "preview":"Original first user message"
        }))
        .unwrap();
        let key = Key::row(&row);
        let suggestion = Title {
            key: key.clone(),
            name: "First saved title".into(),
        };
        suggestion.persist(dir.path()).unwrap();
        let second = Title {
            key,
            name: "Overlapping title".into(),
        }
        .persist(dir.path())
        .unwrap();
        assert_eq!(second.name, suggestion.name);
        let index = Index::load(dir.path()).unwrap();
        index.decorate(&mut row);
        assert_eq!(row.title(), suggestion.name);
        assert_eq!(row.preview, "Original first user message");
        row.name = Some("Operator rename".into());
        assert_eq!(row.title(), "Operator rename");
        row.name = None;
        for (project, provider, model, id) in [
            ("/other", "codeox", "vendor/a", "same-id"),
            ("/project", "openai", "vendor/a", "same-id"),
            ("/project", "codeox", "vendor/b", "same-id"),
            ("/project", "codeox", "vendor/a", "claude:same-id"),
        ] {
            row.cwd = project.into();
            row.model_provider = provider.into();
            row.model = Some(model.into());
            row.id = id.into();
            index.decorate(&mut row);
            assert_eq!(row.title(), "Original first user message");
        }
    }

    #[test]
    fn display_title_rejects_multiline_and_oversized_output() {
        assert_eq!(
            concise("  “Connection pool timeouts”  ").as_deref(),
            Some("Connection pool timeouts")
        );
        assert!(concise("Title\nfollowed by commentary").is_none());
        assert!(concise(&"x".repeat(81)).is_none());
        assert!(concise("").is_none());
    }
}

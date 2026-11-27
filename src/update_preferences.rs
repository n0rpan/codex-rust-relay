use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Automatic,
    Download,
    Manual,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Stable,
    Dev,
}
impl Channel {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Dev => "dev",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Preferences {
    pub mode: Mode,
    pub channel: Channel,
    #[serde(default)]
    pub scheduled: bool,
    // A channel switch is requested for one installed generation. A later explicit
    // installation is authoritative, even when it switches to an older version.
    #[serde(default)]
    pub generation: Option<String>,
}

pub fn read(store: &std::path::Path) -> std::io::Result<Option<Preferences>> {
    match std::fs::File::open(store.join("update-preferences.json")) {
        Ok(file) => Ok(Some(serde_json::from_reader(file)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn generation(store: &std::path::Path) -> Option<String> {
    store
        .join("current")
        .canonicalize()
        .ok()?
        .file_name()?
        .to_str()
        .map(str::to_owned)
}
pub fn effective_channel(prefs: &Preferences, store: &std::path::Path, version: &str) -> Channel {
    if prefs.generation == generation(store) {
        prefs.channel
    } else {
        installed_channel(version)
    }
}
pub fn installed_channel(version: &str) -> Channel {
    if version.contains("-dev.") {
        Channel::Dev
    } else {
        Channel::Stable
    }
}

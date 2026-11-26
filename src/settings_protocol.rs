//! Local stdin/stdout protocol between the desktop and its matching ox executable.
use serde::{Deserialize, Serialize};
#[path = "settings_protocol/model.rs"]
pub mod model;

pub const VERSION: u32 = 1;
fn legacy_version() -> u32 {
    1
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Document {
    pub id: String,
    pub title: String,
    pub text: String,
    pub revision: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Credential {
    pub name: String,
    pub scope: String,
    pub saved: bool,
    pub environment: bool,
    pub configured: bool,
}

#[derive(Default, Deserialize, Serialize)]
pub struct Settings {
    #[serde(default = "legacy_version")]
    pub version: u32,
    pub documents: Vec<Document>,
    pub credentials: Vec<Credential>,
    pub models: Vec<String>,
    pub api_provider: Option<String>,
    pub chatgpt_account: Option<String>,
    pub claude_account: bool,
    pub presets: Vec<(String, String)>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    Read,
    ReadRows,
    ClaudeProfiles,
    CodexProfiles,
    ReadAdvanced,
    SaveAdvanced {
        id: String,
        revision: String,
        original: String,
        text: String,
    },
    Edit {
        revision: String,
        edit: model::Edit,
    },
    TestProvider {
        id: String,
        revision: String,
    },
    EditKey {
        revision: String,
        name: String,
        scope: String,
        value: Option<model::Secret>,
    },
    LegacyWorkspace,
    Save {
        id: String,
        revision: String,
        text: String,
    },
    Key {
        name: String,
        scope: String,
        value: Option<String>,
    },
    Models,
    /// Context windows of launchable API aliases, from OX's model catalog.
    ModelContexts,
    /// Provider workarounds the relay recorded, with the safe configuration fix.
    Advice,
    /// Apply one recorded advice by id, then forget it.
    ApplyAdvice {
        id: String,
    },
}

/// One recorded provider workaround, as shown by OX Desktop.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderAdvice {
    pub id: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub message: String,
    /// Human-readable description of the change the Fix button applies.
    pub fix: String,
}

/// Directories in which stock Claude Code persists sessions for each OX launch.
#[derive(Debug, Deserialize, Serialize)]
pub struct ClaudeProfiles {
    pub subscription: std::path::PathBuf,
    pub api: std::path::PathBuf,
}

/// Existing Codex session homes, including signed-out profiles. The stock server owns their contents.
#[derive(Debug, Deserialize, Serialize)]
pub struct CodexProfiles {
    pub subscription: Option<std::path::PathBuf>,
    pub api: Option<std::path::PathBuf>,
}

/// Metadata from the desktop layout used before the app had its own state directory.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProjectRoute {
    pub project: std::path::PathBuf,
    pub api_model: Option<String>,
}

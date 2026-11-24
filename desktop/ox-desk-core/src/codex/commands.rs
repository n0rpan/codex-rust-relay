//! Small typed replies for the installed command catalog and thread commands.
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub data: Vec<T>,
    pub next_cursor: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct Feature {
    pub name: String,
    pub enabled: bool,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    #[serde(default)]
    pub display_name: String,
    pub input_modalities: Option<Vec<String>>,
    #[serde(default)]
    pub supported_reasoning_efforts: Vec<crate::store::routes_thread::Effort>,
    pub default_reasoning_effort: Option<String>,
    pub id: String,
    pub model: Option<String>,
    #[serde(default)]
    pub service_tiers: Vec<crate::slash::Tier>,
    #[serde(default)]
    pub additional_speed_tiers: Vec<String>,
    #[serde(default)]
    pub is_default: bool,
    pub default_service_tier: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct Config {
    pub model: Option<String>,
    pub service_tier: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct ProjectConfig {
    pub config: Config,
}

#[derive(Debug, Deserialize)]
pub struct Goal {
    pub objective: String,
    pub status: String,
}
#[derive(Debug, Deserialize)]
pub struct ThreadGoal {
    pub goal: Option<Goal>,
}

#[derive(Debug, Deserialize)]
pub struct Cleared {
    pub cleared: bool,
}

#[derive(Debug, Deserialize)]
pub struct Unsubscribed {
    pub status: UnsubscribeStatus,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UnsubscribeStatus {
    NotLoaded,
    NotSubscribed,
    Unsubscribed,
}

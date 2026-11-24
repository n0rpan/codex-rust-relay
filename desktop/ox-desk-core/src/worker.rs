//! Runtime-independent data and actions consumed by worker views. Runtime adapters
//! decide which controls are available for each specific worker.
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
pub struct WorkerMessage {
    pub role: String,
    pub text: String,
}

#[derive(Clone, Debug, serde::Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkerAction {
    Refresh,
    Steer(String),
    Stop,
}

pub struct WorkerView<'a> {
    pub title: &'a str,
    pub parent: &'a str,
    pub runtime: &'a str,
    pub model: &'a str,
    pub status: &'a str,
    pub messages: &'a [WorkerMessage],
    pub progress: &'a str,
    pub result: Option<&'a str>,
    pub error: Option<&'a str>,
    pub can_refresh: bool,
    pub can_steer: bool,
    pub can_stop: bool,
}

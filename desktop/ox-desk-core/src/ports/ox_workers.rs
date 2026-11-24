//! Snapshots returned by the OX worker service.
use crate::worker::WorkerMessage;
use serde::Deserialize;
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
pub struct Snapshot {
    pub job_id: u64,
    pub runtime: String,
    pub model: String,
    pub native_session: Option<String>,
    pub status: String,
    #[serde(default)]
    pub can_steer: bool,
    #[serde(default)]
    pub messages: Vec<WorkerMessage>,
    #[serde(default)]
    pub progress: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

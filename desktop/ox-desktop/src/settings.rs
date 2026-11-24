use ox_desk_core::ports::ox;
pub use ox_desk_core::settings_protocol::{self as protocol, Credential, model};
pub type Settings = model::Snapshot;
use protocol::Request;
use serde::de::DeserializeOwned;

pub fn request<T: DeserializeOwned>(request: Request) -> Result<T, String> {
    ox::request(
        &crate::platform::ox(),
        &crate::platform::child_path(),
        request,
    )
}
pub fn load() -> Result<Settings, String> {
    ox::snapshot(&crate::platform::ox(), &crate::platform::child_path())
}
pub fn discover_models() -> Result<Vec<String>, String> {
    request(Request::Models)
}
pub fn previous_workspace() -> Result<Option<protocol::ProjectRoute>, String> {
    request(Request::LegacyWorkspace)
}

pub fn apply(request: &Request) -> Result<Result<Settings, String>, String> {
    match ox::request_ref(
        &crate::platform::ox(),
        &crate::platform::child_path(),
        request,
    )? {
        model::Outcome::Applied => load().map(Ok),
        model::Outcome::ChangedOnDisk { revision } => Ok(Err(revision)),
    }
}

use ox_desk_core::ports::ox;
pub use ox_desk_core::settings_protocol::{self as protocol, Credential, model};
use ox_host_service::{RouteInputs, route_inputs};
pub type Settings = model::Snapshot;
use protocol::Request;
use serde::de::DeserializeOwned;
use std::sync::Mutex;

#[derive(Clone)]
pub struct PickerSettings {
    pub favorites: Vec<String>,
    pub aliases: Vec<String>,
    pub providers: bool,
}

// The picker only needs model names and provider presence. Keep that projection
// in memory while the backing files are unchanged, including edits from another client.
static PICKER: Mutex<Option<(RouteInputs, PickerSettings)>> = Mutex::new(None);

impl From<&Settings> for PickerSettings {
    fn from(settings: &Settings) -> Self {
        Self {
            favorites: settings.favorites.clone(),
            aliases: settings.models.clone(),
            providers: settings.api_provider.is_some(),
        }
    }
}

pub fn picker_settings() -> Result<PickerSettings, String> {
    if let Some(files) = route_inputs()
        && let Some((_, cached)) = PICKER
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(seen, _)| *seen == files)
    {
        return Ok(cached.clone());
    }
    load().map(|s| PickerSettings::from(&s))
}

pub fn request<T: DeserializeOwned>(request: Request) -> Result<T, String> {
    ox::request(
        &crate::platform::ox(),
        &crate::platform::child_path(),
        request,
    )
}
pub fn load() -> Result<Settings, String> {
    let before = route_inputs();
    let snapshot = ox::snapshot(&crate::platform::ox(), &crate::platform::child_path())?;
    if let Some(files) = before
        && Some(&files) == route_inputs().as_ref()
    {
        *PICKER.lock().unwrap() = Some((files, PickerSettings::from(&snapshot)));
    }
    Ok(snapshot)
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

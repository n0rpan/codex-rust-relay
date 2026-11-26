//! Same-user, paired-device settings. Separate from session actions and their durable receipts:
//! configuration documents, credentials and pairing codes must never enter the event journal.
use super::{App, DENY, ErrCode, Proof, device, login};
use axum::{
    Json,
    extract::State as Extract,
    http::{HeaderMap, StatusCode},
};
use ox_desk_core::{
    ports::ox,
    settings_protocol::{self as protocol, Request},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::sync::Semaphore;
pub(super) static HELPERS: Semaphore = Semaphore::const_new(3);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Envelope {
    proof: Proof,
    request: protocol::model::Secret,
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum DeviceAction {
    HostModePolicy {
        set: Option<ox_desk_core::host_policy::HostPolicy>,
    },
    HostProfile {
        edit: Option<crate::host_profile::Edit>,
    },
    Devices,
    PairDevice,
    HostUpdate {
        command: crate::host_updates::Command,
    },
    RevokeDevice {
        id: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum BehaviorAction {
    ReadBehavior,
    SaveBehavior {
        revision: String,
        behavior: ox_desk_core::desktop_behavior::Behavior,
    },
}
pub(super) async fn settings(
    Extract(app): Extract<App>,
    headers: HeaderMap,
    body: Result<Json<Envelope>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ErrCode> {
    let id = device(&app, &headers)?;
    let Json(body) = body.map_err(|_| StatusCode::BAD_REQUEST)?;
    let text = body.request.expose();
    let identity = login(&headers).ok_or(DENY)?;
    let device_action = serde_json::from_str::<DeviceAction>(text).ok();
    {
        let mut state = app.state.lock().unwrap();
        if !state.prove_settings(&id, &body.proof.nonce, &body.proof.signature, text) {
            return Err(DENY);
        }
        if let Some(op) = &device_action {
            let data = match op {
                DeviceAction::HostUpdate { .. } => None,
                DeviceAction::HostModePolicy { set } => Some(
                    json!({"policy":state.mode_policy(*set).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?}),
                ),
                DeviceAction::HostProfile { edit } => {
                    Some(json!(state.host_profile(edit.clone()).map_err(|e| {
                        if e.kind() == std::io::ErrorKind::AlreadyExists {
                            StatusCode::CONFLICT
                        } else {
                            StatusCode::BAD_REQUEST
                        }
                    })?))
                }
                DeviceAction::Devices => {
                    let reply = state.reply(None);
                    let devices = reply
                        .devices
                        .into_iter()
                        .filter(|d| d.login == identity)
                        .collect::<Vec<_>>();
                    Some(json!({"devices":devices,"self":id,"origin":reply.origin}))
                }
                DeviceAction::PairDevice => Some(
                    serde_json::to_value(
                        state.device_invite(&id).map_err(|_| StatusCode::CONFLICT)?,
                    )
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                ),
                DeviceAction::RevokeDevice { id: target } => {
                    if !state
                        .reply(None)
                        .devices
                        .iter()
                        .any(|d| &d.id == target && d.login == identity)
                    {
                        return Err(DENY);
                    }
                    state
                        .revoke(target)
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                    Some(json!({"status":"applied"}))
                }
            };
            if let Some(data) = data {
                return Ok(Json(data));
            }
        }
    }
    if let Ok(action) = serde_json::from_str::<super::key_transfer::KeyAction>(text) {
        return super::key_transfer::apply(app, id, identity.to_owned(), action).await;
    }
    if let Some(DeviceAction::HostUpdate { command }) = device_action {
        let active = app.state.clone();
        return tokio::task::spawn_blocking(move || {
            if !active.lock().unwrap().alive(&id) {
                return Err(DENY);
            }
            let status = crate::host_updates::control(command).map_err(|_| StatusCode::CONFLICT)?;
            Ok(Json(
                serde_json::to_value(status).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
            ))
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    // Type-check and allowlist before a helper can open files. In particular raw v1 Read/Save/Key
    // and profile-directory requests are unavailable through the phone.
    if let Ok(action) = serde_json::from_str::<super::project_picker::ProjectAction>(text) {
        let _slot = HELPERS
            .try_acquire()
            .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
        return super::project_picker::apply(&app, &id, action).await;
    }
    let behavior = serde_json::from_str::<BehaviorAction>(text).ok();
    let request = if behavior.is_some() {
        None
    } else {
        let request: Request = serde_json::from_str(text).map_err(|_| StatusCode::BAD_REQUEST)?;
        if !matches!(
            &request,
            Request::ReadRows
                | Request::ReadAdvanced
                | Request::SaveAdvanced { .. }
                | Request::Edit { .. }
                | Request::EditKey { .. }
                | Request::ImportKeys { .. }
                | Request::Models
                | Request::TestProvider { .. }
        ) {
            return Err(DENY);
        }
        Some(request)
    };
    let slot = HELPERS
        .try_acquire()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    let active = app.state.clone();
    let requester = id.clone();
    let data = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        if !active.lock().unwrap().alive(&requester) {
            return Err(DENY);
        }
        let data = match behavior {
            Some(BehaviorAction::ReadBehavior) => ox_desk_core::desktop_behavior::read()
                .and_then(|v| serde_json::to_value(v).map_err(std::io::Error::from))
                .map_err(|e| e.to_string()),
            Some(BehaviorAction::SaveBehavior { revision, behavior }) => {
                ox_desk_core::desktop_behavior::write(&revision, behavior)
                    .and_then(|v| serde_json::to_value(v).map_err(std::io::Error::from))
                    .map_err(|e| e.to_string())
            }
            None => helper(request.expect("typed helper request")),
        };
        Ok(data.unwrap_or_else(|e| json!({"error":safe_error(&e)})))
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    if !app.state.lock().unwrap().alive(&id) {
        return Err(DENY);
    }
    Ok(Json(data))
}

pub(super) fn binary() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("OX_TEST_SETTINGS_OX") {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            return Ok(p);
        }
    }
    std::env::current_exe()
        .map(|path| path.with_file_name("ox"))
        .map_err(|e| e.to_string())
}

fn helper(request: Request) -> Result<Value, String> {
    let companion = binary()?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let encode = |result: serde_json::Result<Value>| {
        result.map_err(|_| "Incompatible settings response".to_owned())
    };
    match request {
        Request::ReadRows => encode(serde_json::to_value(ox::snapshot(&companion, &path)?)),
        Request::ReadAdvanced => {
            let mut docs: Vec<protocol::Document> = ox::request(&companion, &path, request)?;
            // Malformed favorites can contain arbitrary text; the native Favorites form repairs
            // them using the same file revision without disclosing that content to the phone.
            docs.retain(|d| {
                !d.id.contains("favorites") || serde_json::from_str::<Vec<String>>(&d.text).is_ok()
            });
            encode(serde_json::to_value(docs))
        }
        Request::Edit { .. }
        | Request::EditKey { .. }
        | Request::ImportKeys { .. }
        | Request::SaveAdvanced { .. } => {
            let result: protocol::model::Outcome = ox::request(&companion, &path, request)?;
            encode(serde_json::to_value(result))
        }
        Request::Models => {
            let models: Vec<String> = ox::request(&companion, &path, request)?;
            Ok(json!(models))
        }
        Request::TestProvider { .. } => {
            let count: usize = ox::request(&companion, &path, request)?;
            Ok(json!(count))
        }
        _ => Err("Unsupported settings operation".into()),
    }
}

fn safe_error(message: &str) -> &'static str {
    // TOML parser diagnostics, remote provider errors and OS error paths can quote private
    // input. Only fixed messages cross the device boundary.
    let s = message.to_lowercase();
    if s.contains("changed") || s.contains("reload") {
        "Settings changed on the laptop. Reload and review your edits."
    } else if s.contains("credential") || s.contains("api key") {
        "Check the provider credential name and value."
    } else if s.contains("provider") || s.contains("model") {
        "Check the provider and model settings on the laptop."
    } else {
        "The laptop could not apply this request. Review the settings and try again."
    }
}

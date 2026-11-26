//! Read-only weather for the phone horizon. The host runs the sibling `ox weather --json`
//! (which owns the timezone lookup, the MET Norway request and its cache) and passes on
//! only the coarse condition. Coordinates never leave the laptop.
use super::{App, DENY, ErrCode, device};
use axum::{Json, extract::State as Extract, http::HeaderMap};
use serde_json::{Value, json};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

const REUSE: Duration = Duration::from_secs(300);
static LAST: Mutex<Option<(Instant, Value)>> = Mutex::new(None);

/// The fields a phone may see: a condition and when it was last refreshed.
fn public(full: &Value) -> Value {
    let mut out = json!({ "condition": full.get("condition").cloned().unwrap_or(Value::Null) });
    for key in ["night", "temperature", "updated"] {
        if let (Some(v), Some(map)) = (full.get(key), out.as_object_mut()) {
            map.insert(key.into(), v.clone());
        }
    }
    out
}

fn run_ox() -> Value {
    // The browser acceptance fixture runs this code; it must never reach the network.
    #[cfg(test)]
    return json!({"condition": null});
    #[cfg(not(test))]
    run_companion()
}

#[cfg(not(test))]
fn run_companion() -> Value {
    let Ok(binary) = super::remote_settings::binary() else {
        return json!({"condition": null});
    };
    let output = std::process::Command::new(binary)
        .args(["weather", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    output
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .map_or_else(|| json!({"condition": null}), |v| public(&v))
}

pub(super) async fn weather(
    Extract(app): Extract<App>,
    h: HeaderMap,
) -> Result<Json<Value>, ErrCode> {
    let id = device(&app, &h)?;
    if !app.state.lock().unwrap().alive(&id) {
        return Err(DENY);
    }
    if let Some((at, value)) = LAST.lock().unwrap().as_ref()
        && at.elapsed() < REUSE
    {
        return Ok(Json(value.clone()));
    }
    let value = tokio::task::spawn_blocking(run_ox)
        .await
        .unwrap_or_else(|_| json!({"condition": null}));
    *LAST.lock().unwrap() = Some((Instant::now(), value.clone()));
    Ok(Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_never_sees_coordinates_or_zone() {
        let full = json!({"zone":"Europe/Oslo","lat":59.92,"lon":10.75,"condition":"rain",
            "night":false,"temperature":4.0,"updated":"2026-11-25T12:00:00Z","expires":9});
        let out = public(&full);
        assert_eq!(out["condition"], "rain");
        assert!(
            out.get("lat").is_none() && out.get("zone").is_none() && out.get("expires").is_none()
        );
        assert_eq!(public(&json!({"zone":"X"})), json!({"condition": null}));
    }
}

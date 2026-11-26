//! A signed operator can copy selected local keys to a paired host. Only metadata is
//! returned to the browser. The source host signs and sends the destination request.
use super::{App, DENY, ErrCode};
use crate::{
    client::Invitation,
    paired_hosts::{self, KeyReview, SavedHost},
};
use axum::{Json, http::StatusCode};
use ox_desk_core::{ports::ox, settings_protocol::KeySelection};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum KeyAction {
    KeyDestinations,
    PairKeyDestination {
        address: String,
        code: String,
    },
    ReviewKeyTransfer {
        host: String,
    },
    SendKeyTransfer {
        host: String,
        pairing: String,
        source: String,
        target: String,
        selected: Vec<KeySelection>,
    },
}

fn description(h: &SavedHost) -> Value {
    json!({"id":h.id, "name":h.name, "origin":h.origin})
}

pub(super) fn tailnet(origin: &str) -> Option<String> {
    let url = url::Url::parse(origin).ok()?;
    let (_, tailnet) = url.host_str()?.split_once('.')?;
    tailnet.ends_with(".ts.net").then(|| tailnet.to_owned())
}

pub(super) async fn apply(
    app: App,
    device: String,
    login: String,
    action: KeyAction,
) -> Result<Json<Value>, ErrCode> {
    let slot = super::remote_settings::HELPERS
        .try_acquire()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    let source = app.clone();
    let requester = device.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        execute(&source, &requester, &login, action)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !app.state.lock().unwrap().alive(&device) {
        return Err(DENY);
    }
    Ok(Json(result.unwrap_or_else(|error| json!({"error":error}))))
}

fn execute(app: &App, device: &str, login: &str, action: KeyAction) -> Result<Value, &'static str> {
    super::background_keychain()?;
    let (origin, registry) = {
        let state = app.state.lock().unwrap();
        if !state.alive(device) {
            return Err("This device needs to pair again.");
        }
        (
            state.configuration().ok_or("Remote access is disabled.")?.0,
            state.key_destinations_path(),
        )
    };
    let hosts = paired_hosts::destinations(&registry)
        .map_err(|_| "Could not read this host’s paired destinations.")?;
    let host_by_id = |id: &str| {
        hosts
            .iter()
            .find(|h| h.id == id && h.origin != origin)
            .cloned()
            .ok_or("That destination is no longer paired with this host.")
    };
    let prepare = |id: &str| {
        let h = host_by_id(id)?;
        if h.web_destination {
            crate::access::verify_key_destination(&h.origin, login)?;
        }
        let bin = super::remote_settings::binary()
            .map_err(|_| "The source host needs its matching OX CLI.")?;
        let path = std::env::var_os("PATH").unwrap_or_default();
        let local = ox::snapshot(&bin, &path)
            .map_err(|_| "Could not read the source host’s credentials.")?;
        let review = KeyReview::prepare(h, local).map_err(
            |_| "Could not reach the paired destination. Check its connection and pairing. If its desktop keyring is locked, pair the destination from here.",
        )?;
        Ok::<_, &'static str>((review, bin, path))
    };
    match action {
        KeyAction::KeyDestinations => Ok(
            json!({"hosts":hosts.iter().filter(|h| h.origin!=origin).map(description).collect::<Vec<_>>()}),
        ),
        KeyAction::PairKeyDestination { address, code } => {
            let invite = Invitation::parse(&address, &code).map_err(
                |_| "Paste a destination invitation or its HTTPS Serve address and pairing code.",
            )?;
            if invite.origin == origin {
                return Err("Choose a different host.");
            }
            if tailnet(&origin).is_none() || tailnet(&origin) != tailnet(&invite.origin) {
                return Err("Choose a destination on this host’s tailnet.");
            }
            crate::access::verify_key_destination(&invite.origin, login)?;
            let local = paired_hosts::read_hosts(&registry)
                .map_err(|_| "Could not read this host’s paired destinations.")?;
            let previous = local.iter().find(|h| h.origin == invite.origin);
            let accept = |current: &[SavedHost], new: &SavedHost| {
                if !app.state.lock().unwrap().alive(device)
                    || hosts
                        .iter()
                        .any(|h| h.id == new.id && h.origin != new.origin)
                {
                    return false;
                }
                let mut matching = current
                    .iter()
                    .filter(|h| h.id == new.id || h.origin == new.origin);
                let unchanged = match (previous, matching.next()) {
                    (None, None) => true,
                    (Some(expected), Some(now)) => {
                        now.same_pairing(expected) && now.id == new.id && now.origin == new.origin
                    }
                    _ => false,
                };
                unchanged && matching.next().is_none()
            };
            let label: String = format!("OX key transfer from {}", paired_hosts::device_label())
                .chars()
                .take(64)
                .collect();
            let h = paired_hosts::enroll(invite, &label, &registry, true, None, accept).map_err(|_| {
                "Could not save the pairing. Check this host’s credential store and connection, then try a fresh invitation."
            })?;
            Ok(description(&h))
        }
        KeyAction::ReviewKeyTransfer { host } => {
            let (review, _, _) = prepare(&host)?;
            Ok(
                json!({"host":description(&review.host), "pairing":review.host.key_id,
                    "rows":review.rows,"source":review.source,"target":review.target}),
            )
        }
        KeyAction::SendKeyTransfer {
            host,
            pairing,
            source,
            target,
            selected,
        } => {
            if selected.is_empty() {
                return Err("Select at least one API key.");
            }
            let (mut review, bin, path) = prepare(&host)?;
            if review.host.key_id != pairing || review.source != source || review.target != target {
                return Err("The pairing or credentials changed. Review the transfer again.");
            }
            let mut count = 0;
            for row in &mut review.rows {
                row.selected = row.available
                    && selected
                        .iter()
                        .any(|k| k.name == row.key.name && k.scope == row.key.scope);
                count += usize::from(row.selected);
            }
            if count != selected.len() {
                return Err("The selected credentials changed. Review the transfer again.");
            }
            let export = |request| {
                if !app.state.lock().unwrap().alive(device) {
                    return Err("Device revoked".into());
                }
                ox::request(&bin, &path, request)
            };
            let sent = review.send(export).map_err(|_| {
                "The destination did not confirm the transfer. Reopen the review to check its keys."
            })?;
            Ok(json!({"sent":sent}))
        }
    }
}

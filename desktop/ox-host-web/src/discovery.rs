//! Tailnet peers that will accept a pairing right now.
//!
//! A device that has never been paired cannot read this host's catalogue. It
//! can read the tailnet, ask each peer whether a pairing window is open, and
//! offer the reachable ones. Nothing here carries a secret: the window is the
//! operator's own decision on the receiving host.
use crate::client;
use serde_json::Value;
use std::{process::Command, time::Duration};

/// The Serve port OX uses when nobody changed it.
const DEFAULT_PORT: u16 = 8443;

/// Peers from `tailscale status --json`, without this machine.
fn peers() -> Vec<(String, String, Vec<String>)> {
    let Ok(cli) = crate::access::cli() else {
        return Vec::new();
    };
    let Ok(output) = Command::new(cli)
        .args(["status", "--json"])
        .stdin(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let Ok(status) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Vec::new();
    };
    if status["BackendState"] != "Running" {
        return Vec::new();
    }
    status["Peer"]
        .as_object()
        .into_iter()
        .flat_map(|peers| peers.values())
        .filter(|peer| peer["Online"].as_bool() == Some(true))
        .filter_map(|peer| {
            let dns = peer["DNSName"].as_str()?.trim_end_matches('.');
            if dns.is_empty()
                || !dns
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
            {
                return None;
            }
            let tags = peer["Tags"]
                .as_array()
                .map(|tags| {
                    tags.iter()
                        .filter_map(|tag| tag.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            Some((
                format!("https://{dns}:{DEFAULT_PORT}"),
                dns.to_owned(),
                tags,
            ))
        })
        .collect()
}

/// Ask every online peer, in parallel and with a short deadline, which ones
/// are waiting for a device. Best effort: an unreachable or older peer is
/// simply absent.
pub fn open_hosts() -> Vec<client::OpenHost> {
    let peers = peers();
    let Ok(runtime) = client::runtime() else {
        return Vec::new();
    };
    runtime.block_on(async {
        let probes = peers.into_iter().map(|(origin, name, tags)| async move {
            let status =
                tokio::time::timeout(Duration::from_secs(6), client::window_status(&origin))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .flatten()?;
            let (host_name, expires_at) = status;
            Some(client::OpenHost {
                origin,
                name: if host_name.is_empty() {
                    name
                } else {
                    host_name
                },
                tags,
                expires_at,
            })
        });
        futures_util::future::join_all(probes)
            .await
            .into_iter()
            .flatten()
            .collect()
    })
}

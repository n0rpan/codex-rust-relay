//! Authenticated Tailscale peer memory synchronization (`/api/memory/sync`).
//!
//! Uses the same Ed25519 device proof and worker-link authorization as `linked_jobs`.
//! `Sensitivity::LocalOnly` records are never exported or accepted across hosts.

use crate::{
    paired_hosts::valid_id,
    server::{App, Proof, paired_identity},
};
use axum::{
    Json,
    extract::State as Extract,
    http::{HeaderMap, StatusCode},
};
use ox_memory::{MemoryEntry, MemoryStore, MergeStats};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

/// Maximum entries accepted in a single sync request.
pub const MAX_SYNC_ENTRIES: usize = 500;
/// Maximum HTTP body size for `/api/memory/sync` (512 KiB).
pub const MAX_SYNC_BODY_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRequest {
    pub source: String,
    pub target: String,
    pub request_id: String,
    /// Monotonic local sequence cursor previously returned by `target`.
    #[serde(default)]
    pub since_seq: u64,
    /// Caller's non-`LocalOnly` delta to merge into `target`.
    #[serde(default)]
    pub entries: Vec<MemoryEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncResponse {
    pub latest_seq: u64,
    #[serde(default)]
    pub has_more: bool,
    pub entries: Vec<MemoryEntry>,
    pub stats: MergeStats,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    request: String,
    proof: Proof,
}

pub(crate) async fn handle(
    Extract(app): Extract<App>,
    headers: HeaderMap,
    Json(body): Json<Envelope>,
) -> Result<Json<SyncResponse>, StatusCode> {
    let device = paired_identity(&app, &headers)?;
    let request: SyncRequest =
        serde_json::from_str(&body.request).map_err(|_| StatusCode::BAD_REQUEST)?;
    if !valid_id(&request.request_id) || request.entries.len() > MAX_SYNC_ENTRIES {
        return Err(StatusCode::BAD_REQUEST);
    }

    let memory_path = {
        let mut state = app.state.lock().unwrap();
        if !state.prove_worker(
            &device,
            &body.proof.nonce,
            &body.proof.signature,
            &body.request,
        ) || !state
            .worker_grant(&device)
            .is_some_and(|g| g.source == request.source)
            || state
                .host_id()
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
                != request.target
        {
            return Err(StatusCode::FORBIDDEN);
        }
        state.memory_path()
    };

    let slot = app
        .slots
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;

    let response = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        apply_sync(&memory_path, &request)
    })
    .await
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(response))
}

/// Apply an incoming sync request against `memory_path` and return the peer delta.
pub fn apply_sync(memory_path: &Path, request: &SyncRequest) -> Result<SyncResponse, String> {
    apply_sync_bounded(memory_path, request, MAX_SYNC_ENTRIES)
}

fn apply_sync_bounded(
    memory_path: &Path,
    request: &SyncRequest,
    max_entries: usize,
) -> Result<SyncResponse, String> {
    let store = MemoryStore::open(memory_path);
    let stats = store
        .merge_remote(&request.entries)
        .map_err(|e| e.to_string())?;
    let (mut entries, store_seq) = store
        .export_peer_since(request.since_seq)
        .map_err(|e| e.to_string())?;
    let has_more = entries.len() > max_entries;
    if has_more {
        entries.truncate(max_entries);
    }
    let latest_seq = if has_more {
        entries.last().map_or(request.since_seq, |e| e.seq)
    } else {
        store_seq
    };
    Ok(SyncResponse {
        latest_seq,
        has_more,
        entries,
        stats,
    })
}

/// Send a signed memory sync request to a paired Tailscale worker host.
pub fn send(
    host: &crate::paired_hosts::SavedHost,
    request: &SyncRequest,
) -> io::Result<SyncResponse> {
    if request.target != host.id {
        return Err(io::Error::other(
            "The memory sync request belongs to another host.",
        ));
    }
    let client = host.client()?;
    crate::client::runtime()?.block_on(async {
        let payload = serde_json::to_string(request)?;
        let proof = client.proof("worker", Some(&payload)).await?;
        let result = client
            .post(
                "memory/sync",
                serde_json::json!({"request": payload, "proof": proof}),
                "Worker link was revoked or is no longer valid.",
                std::time::Duration::from_secs(30),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SyncCursors {
    #[serde(default)]
    peers: std::collections::BTreeMap<String, PeerCursor>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct PeerCursor {
    remote_seq: u64,
    local_seq: u64,
}

/// Bidirectionally sync non-`LocalOnly` memories with every paired Tailscale worker host.
pub fn sync_all_peers(
    state_dir: &Path,
    local_host_id: &str,
) -> io::Result<Vec<(String, Result<MergeStats, String>)>> {
    let links = crate::paired_hosts::worker_links(&state_dir.join("worker-links.json"))?;
    let memory_path = state_dir.join("memory.json");
    let cursors_path = state_dir.join("memory-sync-cursors.json");
    let mut cursors: SyncCursors = std::fs::read_to_string(&cursors_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let store = MemoryStore::open(&memory_path);
    let mut outcomes = Vec::with_capacity(links.len());

    for host in &links {
        let mut cursor = cursors.peers.get(&host.id).copied().unwrap_or_default();
        let outcome = (|| -> Result<MergeStats, String> {
            let mut total = MergeStats::default();
            let (mut outbound, store_local_seq) = store
                .export_peer_since(cursor.local_seq)
                .map_err(|e| e.to_string())?;
            let outbound_truncated = outbound.len() > MAX_SYNC_ENTRIES;
            if outbound_truncated {
                outbound.truncate(MAX_SYNC_ENTRIES);
            }
            let next_local_seq = if outbound_truncated {
                outbound.last().map_or(cursor.local_seq, |e| e.seq)
            } else {
                store_local_seq
            };
            let mut sent_outbound = false;
            loop {
                let batch = if !sent_outbound {
                    sent_outbound = true;
                    std::mem::take(&mut outbound)
                } else {
                    Vec::new()
                };
                let req = SyncRequest {
                    source: local_host_id.to_string(),
                    target: host.id.clone(),
                    request_id: new_request_id().map_err(|e| e.to_string())?,
                    since_seq: cursor.remote_seq,
                    entries: batch,
                };
                let resp = send(host, &req).map_err(|e| e.to_string())?;
                let merged = store
                    .merge_remote(&resp.entries)
                    .map_err(|e| e.to_string())?;
                total.inserted += merged.inserted;
                total.updated += merged.updated;
                total.ignored += merged.ignored;
                total.rejected_local_only += merged.rejected_local_only;
                cursor.remote_seq = resp.latest_seq;
                cursor.local_seq = next_local_seq;
                if !resp.has_more {
                    break;
                }
            }
            cursors.peers.insert(host.id.clone(), cursor);
            Ok(total)
        })();
        outcomes.push((host.name.clone(), outcome));
    }

    if let Ok(payload) = serde_json::to_vec_pretty(&cursors) {
        let _ = std::fs::write(&cursors_path, payload);
    }
    Ok(outcomes)
}

fn new_request_id() -> io::Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(io::Error::other)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_memory::{Kind, NewMemory, Scope, Sensitivity};

    #[test]
    fn apply_sync_exchanges_deltas_and_never_leaks_local_only() {
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("host-a.json");
        let path_b = dir.path().join("host-b.json");
        let store_a = MemoryStore::open(&path_a);
        let store_b = MemoryStore::open(&path_b);

        store_a
            .upsert(NewMemory {
                id: None,
                scope: Scope::Global,
                kind: Kind::RoutingPolicy,
                sensitivity: Sensitivity::Private,
                text: "Use Claude subscription for confidential work.".into(),
                tags: vec!["routing".into()],
                pin: true,
                source_host: "host-a".into(),
            })
            .unwrap();
        store_a
            .upsert(NewMemory {
                id: None,
                scope: Scope::Host("host-a".into()),
                kind: Kind::Fact,
                sensitivity: Sensitivity::LocalOnly,
                text: "Host A local GPU note.".into(),
                tags: vec![],
                pin: true,
                source_host: "host-a".into(),
            })
            .unwrap();
        store_b
            .upsert(NewMemory {
                id: None,
                scope: Scope::Global,
                kind: Kind::Preference,
                sensitivity: Sensitivity::Public,
                text: "Keep PRs small and focused.".into(),
                tags: vec!["git".into()],
                pin: true,
                source_host: "host-b".into(),
            })
            .unwrap();

        let (delta_a, _seq_a) = store_a.export_peer_since(0).unwrap();
        assert_eq!(delta_a.len(), 1);

        let resp = apply_sync(
            &path_b,
            &SyncRequest {
                source: "host-a".into(),
                target: "host-b".into(),
                request_id: "ddcba183-5073-4e9a-9825-e8730784ce5e".into(),
                since_seq: 0,
                entries: delta_a,
            },
        )
        .unwrap();
        assert_eq!(resp.stats.inserted, 1);
        assert_eq!(resp.stats.rejected_local_only, 0);
        assert_eq!(resp.entries.len(), 2);

        // Merge host B's response back into host A
        let stats_a = store_a.merge_remote(&resp.entries).unwrap();
        assert_eq!(stats_a.inserted, 1);
        let all_b = store_b.list(None, None, Sensitivity::LocalOnly).unwrap();
        assert_eq!(all_b.len(), 2);
        assert!(
            all_b
                .iter()
                .all(|e| e.sensitivity != Sensitivity::LocalOnly)
        );
    }
}

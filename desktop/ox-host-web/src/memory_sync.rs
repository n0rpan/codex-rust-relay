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
    let store = MemoryStore::open(memory_path);
    let stats = store
        .merge_remote(&request.entries)
        .map_err(|e| e.to_string())?;
    let (entries, latest_seq) = store
        .export_peer_since(request.since_seq)
        .map_err(|e| e.to_string())?;
    Ok(SyncResponse {
        latest_seq,
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

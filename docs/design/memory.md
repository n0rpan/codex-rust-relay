# OX Native Memory System

## Goals
- Give Codex and Claude sessions and workers durable memory across sessions and paired Tailscale hosts without modifying stock Codex or Claude binaries.
- Keep token overhead minimal: a tiny, cache-stable Tier-1 brief (~150-250 tokens) at session/worker start plus 2 compact MCP tools for on-demand Tier-2 recall/save.
- Support provider/model routing policies by data sensitivity (`public`, `private`, `local_only`) so workers respect user routing preferences and `local_only` facts never leave a host.
- Stay 100% pure Rust with zero external daemons or C dependencies, fitting `ox`'s single-binary and headless-host architecture.

## Architecture

### Shared Crate: `desktop/ox-memory`
Used by both root `ox` (CLI + `ox_workers` MCP server) and `desktop/` (`ox-host`, `ox-desk-core`, `ox-host-web`).

- **Storage**: `memory.json` inside `ox`'s state directory (`OX_MEMORY_PATH` override, else `OX_HOST_STATE_DIR/memory.json`, else `$XDG_STATE_HOME/ox/host/memory.json` or `~/Library/Application Support/ox/host/memory.json`).
- **Concurrency & durability**: `flock` advisory lock (`memory.lock`), `0o600` file permissions, atomic write via temp file + `sync_all` + `rename`.
- **Entry schema**:
  - `id`: 12-char lowercase hex identifier
  - `scope`: `global` | `project:<canonical_id>` | `host:<host_id>`
  - `kind`: `routing_policy` | `preference` | `fact`
  - `sensitivity`: `public` | `private` | `local_only`
  - `text`: concise atomic statement (max 500 chars)
  - `tags`: normalized lowercase tags (max 12)
  - `pin`: eligible for Tier-1 brief
  - `created_at_ms`, `updated_at_ms`, `source_host`, `deleted` (tombstone for LWW sync)
- **Retrieval**: Pure-Rust BM25 over `text` + `tags` + `kind` combined with scope affinity (`project` > `host` > `global`), `pin` boost, `routing_policy` boost for provider/model/data queries, and recency tie-break. Filters tombstones and entries above `max_sensitivity`.
- **Tier-1 Brief**: Deterministic, sensitivity-filtered summary capped at `max_chars` (default 900 chars ≈ 220 tokens). Ordered: pinned `routing_policy` -> pinned `preference` -> pinned `fact` (project before global).
- **Sync primitives**: `export_since(since_ms, include_local_only=false)` and conflict-free LWW `merge_remote` ordered by `(updated_at_ms, source_host, id)`. `local_only` entries are never exported or overwritten by remote sync. `/api/memory/sync` also requires the receiving host's per-link `may_sync_memory` grant; a worker link alone never permits memory exchange.

## Rollout Phases (Separate PRs)
1. **PR 1 — Core engine + CLI + MCP**: `desktop/ox-memory`, `ox memory` CLI (`add`, `list`, `search`, `pin`, `unpin`, `rm`, `brief`), and `recall_ox_memory` / `save_ox_memory` on `ox_workers` MCP (with secret scrubbing).
2. **PR 2 — Tier-1 injection & routing policy enforcement**: Inject budgeted, sensitivity-filtered Tier-1 brief at Codex/Claude session and worker start via `ox-host` / `surface.rs`, respecting route sensitivity.
3. **PR 3 — Multi-host Tailscale sync & remote worker brief**: `/api/memory/sync` on `ox-host-web` for paired hosts + filtered Tier-1 brief attached to `/api/worker-job` for headless remote workers.

//! A worker gateway's parent is never chosen by an MCP tool argument. The host
//! uses the verified launch route and session, rejecting ambiguous concurrent
//! origins before a child can start.
use ox_desk_core::{actions::Action, origin::Origin, store::Store};
use std::{collections::HashSet, io};

/// Hosted sessions in this scope that could have made a gateway call: those
/// with an active turn, narrowed to the launch's own session when it has one.
pub(crate) fn active_parents(store: &Store, hint: Option<&str>) -> Vec<String> {
    let mut found: Vec<String> = store
        .threads()
        .iter()
        .filter(|r| {
            hint.is_none_or(|id| {
                r.id == id
                    || r.id
                        .strip_prefix(id)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            }) && store
                .transcript(&r.id)
                .is_some_and(|t| t.active_turn().is_some())
        })
        .map(|r| match hint {
            // A Claude helper serves one session. Its native subagents act for it.
            Some(id) => id.to_owned(),
            None => r.id.clone(),
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Exactly one verified parent, or a refusal.
pub(crate) fn single_parent(store: &Store, hint: Option<&str>) -> Result<String, String> {
    let mut parents = active_parents(store, hint);
    match parents.len() {
        1 => Ok(parents.pop().unwrap()),
        0 => Err("No active parent session was found for this OX helper.".into()),
        // Codex may multiplex unrelated running threads on the same app-server.
        // Until it attributes an MCP call, choosing one would invent parentage.
        _ => Err(
            "Several sessions on this route are running. Retry when only the parent is active."
                .into(),
        ),
    }
}

fn descends_from<'a>(store: &'a Store, id: &'a str, root: &str) -> bool {
    let mut current = id;
    let mut seen = HashSet::new();
    while seen.insert(current) {
        match store
            .catalog_row(current)
            .and_then(|r| r.parent_thread_id.as_deref())
        {
            Some(p) if p == root => return true,
            Some(p) => current = p,
            None => return false,
        }
    }
    false
}

/// `origin` is the target's stamp. `active_workers` says whether host-owned
/// workers launched from the target are still running.
pub(crate) fn check(
    store: &Store,
    action: &Action,
    origin: Option<&Origin>,
    active_workers: bool,
) -> io::Result<()> {
    if let Action::SetAccess(id, mode) = action {
        if let Some(origin) = origin {
            crate::workers::operator_access(origin, *mode).map_err(io::Error::other)?;
        } else if store
            .catalog_row(id)
            .is_some_and(|r| r.parent_thread_id.is_some())
        {
            return Err(io::Error::other(
                "Child permissions are inherited from the parent.",
            ));
        }
        if store.applied_access(Some(id)) != Some(*mode)
            && (active_workers
                || store.threads().iter().any(|r| {
                    r.parent_thread_id.is_some()
                        && descends_from(store, &r.id, id)
                        && (r.status.as_ref().is_some_and(|s| s.kind == "active")
                            || store
                                .transcript(&r.id)
                                .is_some_and(|t| t.active_turn().is_some()))
                }))
        {
            return Err(io::Error::other(
                "Stop active children before changing parent permissions.",
            ));
        }
    }
    Ok(())
}

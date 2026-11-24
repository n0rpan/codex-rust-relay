//! The native OX worker service checks here before creating a process. MCP tool
//! arguments cannot choose the parent. Use the verified launch route and
//! session, rejecting ambiguous concurrent origins before a child can start.
use ox_desk_core::{actions::Action, runtime::Access, store::Store};
use std::{collections::HashSet, io};
fn permits(access: Option<Access>, write: bool, claude: bool) -> bool {
    match access {
        Some(Access::Yolo | Access::BypassPermissions) => true,
        Some(Access::Workspace | Access::AutoReview) => !write || !claude,
        Some(Access::AcceptEdits) => !write || claude,
        Some(Access::ReadOnly | Access::Plan | Access::Default | Access::Auto) => !write,
        _ => false,
    }
}
pub(crate) fn owner(
    store: &Store,
    parent: Option<&str>,
    write: bool,
    claude: bool,
) -> Option<String> {
    if let Some(id) = parent
        && let Some(job) = store.job(id)
    {
        // The helper independently inherits the exact worker's immutable read/write cap.
        if !job.live || job.snapshot.status != "running" {
            return None;
        }
        return Some(id.to_owned());
    }
    let mut candidates = store.threads().iter().filter(|r| {
        parent.is_none_or(|id| {
            r.id == id
                || r.id
                    .strip_prefix(id)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) && store
            .transcript(&r.id)
            .is_some_and(|t| t.active_turn().is_some())
    });
    let row = candidates.next()?;
    // Codex may multiplex unrelated running threads on the same app-server.
    // Until it attributes an MCP call, choosing one would invent parentage.
    if candidates.next().is_some() || !permits(store.applied_access(Some(&row.id)), write, claude) {
        return None;
    }
    Some(row.id.clone())
}
fn descends_from<'a>(store: &'a Store, id: &'a str, root: &str) -> bool {
    let mut current = id;
    let mut seen = HashSet::new();
    while seen.insert(current) {
        let parent = store.job(current).map(|j| j.parent.as_str()).or_else(|| {
            store
                .catalog_row(current)
                .and_then(|r| r.parent_thread_id.as_deref())
        });
        match parent {
            Some(p) if p == root => return true,
            Some(p) => current = p,
            None => return false,
        }
    }
    false
}
pub(crate) fn check(store: &Store, action: &Action) -> io::Result<()> {
    if let Action::SetAccess(id, mode) = action {
        if store.job(id).is_some()
            || store
                .catalog_row(id)
                .is_some_and(|r| r.parent_thread_id.is_some())
        {
            return Err(io::Error::other(
                "Child permissions are inherited from the parent.",
            ));
        }
        if store.applied_access(Some(id)) != Some(*mode)
            && (store.jobs().any(|(child, j)| {
                j.snapshot.status == "running" && descends_from(store, child, id)
            }) || store.threads().iter().any(|r| {
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_cannot_inherit_unverified_or_broader_permissions() {
        for mode in [
            Access::ReadOnly,
            Access::Plan,
            Access::Default,
            Access::Auto,
        ] {
            assert!(!permits(Some(mode), true, false));
            assert!(!permits(Some(mode), true, true));
        }
        assert!(!permits(None, false, false));
        assert!(!permits(Some(Access::Configured), true, false));
        assert!(permits(Some(Access::Workspace), true, false));
        assert!(!permits(Some(Access::Workspace), true, true));
        assert!(!permits(Some(Access::AcceptEdits), true, false));
    }
}

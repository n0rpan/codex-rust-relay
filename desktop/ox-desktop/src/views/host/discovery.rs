//! Paged native metadata and the merged navigation index. Opening a row is the only
//! operation here that creates a project workbench.
use super::*;
use ox_desk_core::runtime::Runtime;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Scope {
    pub runtime: Runtime,
    pub api: bool,
}
impl Scope {
    pub(super) fn saved_route(
        self,
        row: &ThreadRow,
        aliases: &[String],
        provider: Option<&str>,
    ) -> Option<Route> {
        match self.runtime {
            Runtime::Claude => row.saved_runtime_route(aliases).filter(|r| self.matches(r)),
            Runtime::Codex if !self.api && row.model_provider == "openai" => {
                Some(Route::CODEX_SUBSCRIPTION)
            }
            Runtime::Codex if self.api && provider == Some(&row.model_provider) => row
                .model
                .as_ref()
                .filter(|m| aliases.contains(m))
                .cloned()
                .map(Route::codex_api),
            _ => None,
        }
    }

    pub fn matches(self, route: &Route) -> bool {
        self.runtime == route.runtime && self.api == route.is_api()
    }
}

#[derive(Clone, Copy)]
pub(super) enum Location {
    Live(usize, usize),
    Saved(usize),
    Hosted(usize),
}

#[derive(PartialEq)]
pub(super) struct Saved {
    pub row: ThreadRow,
    pub scope: Scope,
}

#[derive(PartialEq)]
struct Hosted {
    saved: Saved,
    route: Route,
    status: String,
}
impl Hosted {
    fn from_view(
        scope: &ox_desk_core::host_protocol::Scope,
        view: &ox_desk_core::host_protocol::SessionView,
    ) -> Option<Self> {
        if view.detail["owned"] != true {
            return None;
        }
        let mut row = ox_desk_core::store::replica::row(view)?;
        if row.cwd != scope.project {
            return None;
        }
        row.parent_thread_id = view.parent.clone().or(row.parent_thread_id);
        Some(Self {
            saved: Saved {
                row,
                scope: Scope {
                    runtime: scope.route.runtime,
                    api: scope.route.is_api(),
                },
            },
            route: scope.route.clone(),
            status: view.status.clone(),
        })
    }
}
#[derive(Default)]
pub(super) struct Discovery {
    pub rows: Vec<Saved>,
    hosted: Vec<Hosted>,
    archived: HashMap<ox_desk_core::host_protocol::Scope, HashSet<String>>,
    watch: Option<platform::CatalogWatch>,
    pub revision: usize,
    pub indexed: Option<(usize, String)>,
    pub pending: usize,
    /// The last discovery failure, so a later success can clear the banner it raised.
    pub error: Option<String>,
    removed: HashMap<Scope, HashMap<PathBuf, HashSet<String>>>,
    /// Parents whose child rows are folded away, by project and thread id.
    pub collapsed: HashSet<(PathBuf, String)>,
    /// Each indexed child's parent, by project and thread id, from the last index.
    pub parents: HashMap<(PathBuf, String), (PathBuf, String)>,
}

/// What a parent session has under it, counted over all descendants.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Tree {
    /// Direct children.
    pub children: usize,
    pub running: usize,
    pub waiting: usize,
    /// The first running descendant, in catalog order.
    pub first_running: Option<String>,
}

/// Tree counts for every parent in the navigation index, keyed by project and thread id.
/// Set by `index_catalog`; read by the workbench activity line and the sidebar rows.
#[derive(Default)]
pub(crate) struct TreeIndex(pub HashMap<(PathBuf, String), Tree>);
impl gpui::Global for TreeIndex {}

/// Unfolds every collapsed ancestor of `start`. Returns whether any fold was removed.
/// Bounded by the parent map's size, so a malformed parent cycle cannot spin.
fn unfold_ancestors(
    parents: &HashMap<(PathBuf, String), (PathBuf, String)>,
    collapsed: &mut HashSet<(PathBuf, String)>,
    start: (PathBuf, String),
) -> bool {
    let mut unfolded = false;
    let mut at = start;
    for _ in 0..parents.len() {
        let Some(parent) = parents.get(&at).cloned() else {
            break;
        };
        unfolded |= collapsed.remove(&parent);
        at = parent;
    }
    unfolded
}

/// A child's parent link survives a merge with a richer row that has none.
fn keep_parent(known: Option<String>, incoming: Option<String>) -> Option<String> {
    incoming.or(known)
}

/// The identity of one catalog row: project, scope, thread id and provider.
type Key = (PathBuf, Scope, String, String);

impl Discovery {
    pub(super) fn all(&self) -> impl Iterator<Item = &Saved> {
        self.hosted.iter().map(|h| &h.saved).chain(&self.rows)
    }
    /// Fold or unfold a parent's children in the sidebar.
    pub(super) fn toggle(&mut self, project: PathBuf, id: String) {
        let key = (project, id);
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.revision += 1;
    }
    fn live_event(&mut self, event: platform::CatalogEvent) -> bool {
        use platform::CatalogEvent;
        match event {
            CatalogEvent::Reset => {
                let changed = !self.hosted.is_empty() || !self.archived.is_empty();
                self.archived.clear();
                self.hosted.clear();
                changed
            }
            CatalogEvent::Archived(scope, ids) => {
                if ids.is_empty() {
                    return self.archived.remove(&scope).is_some();
                }
                if self.archived.get(&scope) == Some(&ids) {
                    return false;
                }
                self.archived.insert(scope, ids);
                true
            }
            CatalogEvent::Replace(scope, views) => {
                let rows: Vec<_> = views
                    .iter()
                    .filter_map(|v| Hosted::from_view(&scope, v))
                    .collect();
                let matches =
                    |r: &Hosted| r.route == scope.route && r.saved.row.cwd == scope.project;
                if self.hosted.iter().filter(|r| matches(r)).eq(&rows) {
                    return false;
                }
                self.hosted.retain(|r| !matches(r));
                self.hosted.extend(rows);
                true
            }
            CatalogEvent::Update(scope, view) => {
                let Some(row) = Hosted::from_view(&scope, &view) else {
                    return false;
                };
                if let Some(old) = self.hosted.iter_mut().find(|r| {
                    r.route == scope.route
                        && r.saved.row.cwd == scope.project
                        && r.saved.row.id == view.id
                }) {
                    if *old == row {
                        return false;
                    }
                    *old = row;
                } else {
                    self.hosted.push(row)
                }
                true
            }
        }
    }
    pub(super) fn route_status(
        &self,
        project: &Path,
        scope: Scope,
        row: &ThreadRow,
    ) -> Option<(Route, Option<&'static str>)> {
        let h = self.hosted.iter().find(|h| {
            h.saved.scope == scope
                && h.saved.row.cwd == project
                && h.saved.row.id == row.id
                && h.saved.row.model_provider == row.model_provider
        })?;
        let status = match h.status.as_str() {
            "needs_input" => Some("Needs you"),
            "running" => Some("Running"),
            "failed" => Some("Error"),
            "interrupted" => Some("Interrupted"),
            _ => None,
        };
        Some((h.route.clone(), status))
    }

    fn replace(&mut self, source: Option<bool>, rows: Vec<ThreadRow>) {
        self.rows.retain(|saved| {
            let belongs = match source {
                None => saved.scope.runtime == Runtime::Claude,
                Some(api) => saved.scope.runtime == Runtime::Codex && saved.scope.api == api,
            };
            if belongs {
                self.removed
                    .entry(saved.scope)
                    .or_default()
                    .entry(saved.row.cwd.clone())
                    .or_default()
                    .insert(saved.row.id.clone());
            }
            !belongs
        });
        for row in rows.into_iter().filter(|r| r.cwd.is_absolute()) {
            let scope = match source {
                Some(api) => Scope {
                    runtime: Runtime::Codex,
                    api,
                },
                None => Scope {
                    runtime: Runtime::Claude,
                    api: row.saved_claude_api().unwrap_or(false),
                },
            };
            if let Some(ids) = self
                .removed
                .get_mut(&scope)
                .and_then(|m| m.get_mut(&row.cwd))
            {
                ids.remove(&row.id);
            }
            self.rows.push(Saved { row, scope });
        }
        self.removed.retain(|_, projects| {
            projects.retain(|_, ids| !ids.is_empty());
            !projects.is_empty()
        });
        self.revision += 1;
    }

    fn removed(&self, project: &Path, scope: Scope, id: &str) -> bool {
        self.removed
            .get(&scope)
            .and_then(|projects| projects.get(project))
            .is_some_and(|ids| ids.contains(id))
    }
}

impl Host {
    pub(super) fn watch_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (watch, events) = match platform::watch_catalog() {
            Ok(pair) => pair,
            Err(error) => {
                self.message = Some(error.to_string());
                return;
            }
        };
        self.discovery.watch = Some(watch);
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(first) = events.recv_async().await {
                if this
                    .update_in(cx, |s, window, cx| {
                        let mut changed = s.discovery.live_event(first);
                        for event in events.try_iter() {
                            changed |= s.discovery.live_event(event)
                        }
                        if changed {
                            s.discovery.revision += 1;
                            if window.is_visible() {
                                cx.notify();
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn start_discovery(&mut self, cx: &mut Context<Self>) {
        if self.discovery.pending != 0 {
            return;
        }
        self.discovery.pending = 1;
        let task = cx.background_spawn(async { platform::discover() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                s.discovery.pending = 0;
                match result {
                    Ok((rows, errors)) => {
                        // A failure the user already saw is over once discovery works again.
                        if s.discovery
                            .error
                            .take()
                            .is_some_and(|e| s.message.as_ref() == Some(&e))
                        {
                            s.message = None;
                        }
                        let mut groups = [vec![], vec![], vec![]];
                        for (scope, row) in rows {
                            let index = match (scope.route.runtime, scope.route.is_api()) {
                                (Runtime::Claude, _) => 0,
                                (_, false) => 1,
                                (_, true) => 2,
                            };
                            groups[index].push(row);
                        }
                        for (source, mut page) in
                            [None, Some(false), Some(true)].into_iter().zip(groups)
                        {
                            // A failed source must not erase previously visible histories.
                            if !errors.is_empty() {
                                for saved in &s.discovery.rows {
                                    let belongs = match source {
                                        None => saved.scope.runtime == Runtime::Claude,
                                        Some(api) => {
                                            saved.scope.runtime == Runtime::Codex
                                                && saved.scope.api == api
                                        }
                                    };
                                    if belongs
                                        && !page
                                            .iter()
                                            .any(|r| r.cwd == saved.row.cwd && r.id == saved.row.id)
                                    {
                                        page.push(saved.row.clone());
                                    }
                                }
                            }
                            s.discovery.replace(source, page);
                        }
                        for message in errors {
                            eprintln!("Could not discover saved sessions: {message}");
                        }
                    }
                    Err(e) => {
                        let message = e.to_string();
                        s.discovery.error = Some(message.clone());
                        s.message = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn archived(&self, project: &Path, scope: Scope, id: &str, cx: &App) -> bool {
        self.discovery.archived.iter().any(|(origin, ids)| {
            origin.project == project && scope.matches(&origin.route) && ids.contains(id)
        }) || (self.discovery.removed(project, scope, id)
            && !self.discovery.hosted.iter().any(|h| {
                h.saved.row.id == id && h.saved.scope == scope && h.saved.row.cwd == project
            }))
            || self.panes.iter().any(|p| {
                let p = p.read(cx);
                p.project == project
                    && scope.matches(&p.route)
                    && p.store
                        .as_ref()
                        .is_some_and(|s| s.archived_threads().contains(id))
            })
    }

    fn located<'a>(
        &'a self,
        loc: Location,
        cx: &'a App,
    ) -> Option<(&'a ThreadRow, &'a Path, Scope)> {
        match loc {
            Location::Live(pane, index) => {
                let pane = self.panes.get(pane)?.read(cx);
                let row = pane.store.as_ref()?.threads().get(index)?;
                let scope = Scope {
                    runtime: pane.route.runtime,
                    api: row.saved_claude_api().unwrap_or(pane.route.is_api()),
                };
                Some((row, &pane.project, scope))
            }
            Location::Hosted(index) => {
                let h = self.discovery.hosted.get(index)?;
                Some((&h.saved.row, &h.saved.row.cwd, h.saved.scope))
            }
            Location::Saved(index) => {
                let saved = self.discovery.rows.get(index)?;
                Some((&saved.row, &saved.row.cwd, saved.scope))
            }
        }
    }

    pub(super) fn located_entry<'a>(
        &'a self,
        entry: CatalogEntry,
        cx: &'a App,
    ) -> Option<(&'a ThreadRow, &'a Path, Scope)> {
        let CatalogEntry::Thread { loc, .. } = entry else {
            return None;
        };
        self.located(loc, cx)
    }

    fn status_at(&self, loc: Location, cx: &App) -> Option<&'static str> {
        let (row, project, scope) = self.located(loc, cx)?;
        self.project_row_state(project, row, scope, cx).1
    }

    /// Aggregate of everything under `key`, recording every parent that has children.
    fn walk_tree(
        &self,
        key: &Key,
        children: &HashMap<&Key, Vec<(&Key, i64, Location)>>,
        cx: &App,
        out: &mut HashMap<(PathBuf, String), Tree>,
    ) -> Tree {
        let mut total = Tree::default();
        for (child, _, loc) in children.get(key).into_iter().flatten() {
            let own = self.status_at(*loc, cx);
            let below = self.walk_tree(child, children, cx, out);
            let running = own == Some("Running");
            total.children += 1;
            total.running += below.running + usize::from(running);
            total.waiting += below.waiting + usize::from(own == Some("Needs you"));
            if running {
                total.first_running.get_or_insert_with(|| child.2.clone());
            }
            if let Some(first) = below.first_running {
                total.first_running.get_or_insert(first);
            }
        }
        if total.children > 0 {
            out.insert((key.0.clone(), key.2.clone()), total.clone());
        }
        total
    }

    /// Open a session from the sidebar index, unfolding its parent if needed.
    pub(super) fn open_thread(
        &mut self,
        owner: &Entity<Workbench>,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (project, selected) = {
            let pane = owner.read(cx);
            (
                pane.project.clone(),
                pane.store
                    .as_ref()
                    .and_then(|s| s.selected())
                    .map(str::to_owned),
            )
        };
        // The target may be a worker folded under the session being viewed, or under
        // any other folded ancestor. Unfold the whole chain so its row shows.
        let mut unfolded = selected.is_some_and(|selected| {
            self.discovery
                .collapsed
                .remove(&(project.clone(), selected))
        });
        unfolded |= unfold_ancestors(
            &self.discovery.parents,
            &mut self.discovery.collapsed,
            (project.clone(), id.clone()),
        );
        if unfolded {
            self.discovery.revision += 1;
            self.index_catalog(cx);
        }
        let found = self.catalog.iter().find_map(|entry| {
            let (row, path, scope) = self.located_entry(*entry, cx)?;
            (row.id == id && path == project.as_path()).then(|| (row.clone(), scope))
        });
        if let Some((row, scope)) = found {
            self.activate_row(project, scope, row, window, cx);
        }
    }

    pub(super) fn index_catalog(&mut self, cx: &mut App) {
        let versions: Vec<_> = self
            .panes
            .iter()
            .enumerate()
            .filter_map(|(i, p)| Some((i, p.read(cx).store.as_ref()?.catalog_revision())))
            .collect();
        let query = self.filter.read(cx).value().trim().to_lowercase();
        let state = (self.discovery.revision, query.clone());
        // `is:running`, `is:waiting` and `is:finished` filter by live status.
        let (status_filter, query) = split_status_filter(&query);
        if versions == self.catalog_versions && self.discovery.indexed.as_ref() == Some(&state) {
            return;
        }
        self.discovery.indexed = Some(state);
        self.catalog_versions = versions;
        let locations = self
            .catalog_versions
            .iter()
            .flat_map(|&(i, _)| {
                (0..self.panes[i]
                    .read(cx)
                    .store
                    .as_ref()
                    .unwrap()
                    .threads()
                    .len())
                    .map(move |j| Location::Live(i, j))
            })
            .chain((0..self.discovery.hosted.len()).map(Location::Hosted))
            .chain((0..self.discovery.rows.len()).map(Location::Saved));
        let mut entries = HashMap::new();
        for loc in locations {
            let Some((r, project, scope)) = self.located(loc, cx) else {
                continue;
            };
            if self.archived(project, scope, &r.id, cx) {
                continue;
            }
            if let Some(wanted) = status_filter {
                let (_, status) = self.project_row_state(project, r, scope, cx);
                let matches = match (wanted, status) {
                    (StatusFilter::Running, Some("Running")) => true,
                    (StatusFilter::Waiting, Some("Needs you")) => true,
                    (StatusFilter::Finished, Some("Running" | "Needs you")) => false,
                    (StatusFilter::Finished, _) => true,
                    _ => false,
                };
                if !matches {
                    continue;
                }
            }
            if !query.is_empty()
                && !r.title().to_lowercase().contains(&query)
                && !r.preview.to_lowercase().contains(&query)
                && !project.to_string_lossy().to_lowercase().contains(&query)
                && !matches!(loc, Location::Live(i, _) if self.panes[i].read(cx).query.to_lowercase() == query)
            {
                continue;
            }
            let key = (
                project.to_owned(),
                scope,
                r.id.clone(),
                r.model_provider.clone(),
            );
            let order = (r.updated_at, r.activity_order);
            let entry = entries.entry(key).or_insert((
                order,
                loc,
                r.parent_thread_id.clone(),
                (false, false, false, false, false, 0),
            ));
            // Prefer authoritative, richer identity for rendering. Keep the newest
            // positive activity date independently when two catalogs describe the same row.
            let quality = (
                r.worker || r.activity_order != 0,
                r.name.is_some() || r.agent_nickname.is_some(),
                r.generated_title.is_some(),
                !r.preview.trim().is_empty(),
                matches!(loc, Location::Live(..) | Location::Hosted(..)),
                r.updated_at,
            );
            entry.0 = entry.0.max(order);
            if quality > entry.3 {
                entry.1 = loc;
                entry.2 = keep_parent(entry.2.take(), r.parent_thread_id.clone());
                entry.3 = quality;
            }
        }
        let mut ordered: Vec<_> = entries.keys().collect();
        ordered.sort_unstable_by(|a, b| {
            entries[b]
                .0
                .cmp(&entries[a].0)
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.runtime.name().cmp(b.1.runtime.name()))
                .then_with(|| a.1.api.cmp(&b.1.api))
                .then_with(|| a.3.cmp(&b.3))
        });
        let mut roots = Vec::new();
        let mut children = HashMap::new();
        let mut parents: HashMap<(PathBuf, String), (PathBuf, String)> = HashMap::new();
        // A host-owned worker runs on its own route. Its origin names a parent
        // session on another route of the same project.
        let by_id: HashMap<_, _> = entries
            .keys()
            .map(|k| ((k.0.as_path(), k.2.as_str()), k))
            .collect();
        for key in ordered {
            let ((time, _), loc, parent, _) = &entries[key];
            let node = (key, *time, *loc);
            if let Some(parent) = parent
                && let Some((key, _)) =
                    entries.get_key_value(&(key.0.clone(), key.1, parent.clone(), key.3.clone()))
            {
                parents.insert(
                    (node.0.0.clone(), node.0.2.clone()),
                    (key.0.clone(), key.2.clone()),
                );
                children.entry(key).or_insert_with(Vec::new).push(node);
            } else if let Some(parent) = parent
                && let Some(owner) = by_id.get(&(key.0.as_path(), parent.as_str()))
                && *owner != key
            {
                parents.insert(
                    (node.0.0.clone(), node.0.2.clone()),
                    (owner.0.clone(), owner.2.clone()),
                );
                children.entry(*owner).or_insert_with(Vec::new).push(node);
            } else {
                roots.push(node);
            }
        }
        // Counts cover the whole tree, folded or not, so a collapsed parent can still
        // say what is running under it.
        let mut tree = HashMap::new();
        for (key, _, _) in &roots {
            self.walk_tree(key, &children, cx, &mut tree);
        }
        cx.set_global(TreeIndex(tree));
        self.discovery.parents = parents;
        let mut catalog = Vec::new();
        let mut previous = None;
        let mut ordinal = 0;
        for (key, timestamp, loc) in roots {
            let day = session_day(timestamp);
            if previous != Some(day) {
                catalog.push(CatalogEntry::Day(day));
            }
            previous = Some(day);
            let mut stack = vec![(key, loc, timestamp, 0)];
            while let Some((key, loc, timestamp, depth)) = stack.pop() {
                catalog.push(CatalogEntry::Thread {
                    loc,
                    timestamp,
                    ordinal,
                    depth,
                });
                ordinal += 1;
                if self
                    .discovery
                    .collapsed
                    .contains(&(key.0.clone(), key.2.clone()))
                {
                    continue;
                }
                if let Some(next) = children.remove(key) {
                    stack.extend(
                        next.into_iter()
                            .rev()
                            .map(|(k, time, loc)| (k, loc, time, depth + 1)),
                    );
                }
            }
        }
        self.catalog = catalog;
    }
}

#[cfg(test)]
mod unfold_tests {
    use super::unfold_ancestors;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    fn key(id: &str) -> (PathBuf, String) {
        (PathBuf::from("/work/app"), id.to_owned())
    }

    #[test]
    fn unfolds_every_collapsed_ancestor_of_a_folded_worker() {
        // grandchild G sits under child C, which sits under parent P.
        let parents = HashMap::from([(key("G"), key("C")), (key("C"), key("P"))]);
        let mut collapsed = HashSet::from([key("C"), key("P")]);
        assert!(unfold_ancestors(&parents, &mut collapsed, key("G")));
        assert!(collapsed.is_empty());
    }

    #[test]
    fn reports_nothing_to_do_without_folded_ancestors() {
        let parents = HashMap::from([(key("G"), key("C"))]);
        let mut collapsed = HashSet::from([key("X")]);
        assert!(!unfold_ancestors(&parents, &mut collapsed, key("G")));
        assert_eq!(collapsed, HashSet::from([key("X")]));
    }

    #[test]
    fn a_parent_cycle_terminates() {
        let parents = HashMap::from([(key("A"), key("B")), (key("B"), key("A"))]);
        let mut collapsed = HashSet::from([key("B")]);
        assert!(unfold_ancestors(&parents, &mut collapsed, key("A")));
        assert!(collapsed.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::{Discovery, Route, Runtime, Scope, ThreadRow};
    use std::path::Path;
    #[test]
    fn native_and_relay_homes_only_open_their_original_provider() {
        let row = |provider, model| {
            serde_json::from_value::<ThreadRow>(serde_json::json!({
                "id":"same","cwd":"/other","modelProvider":provider,"model":model
            }))
            .unwrap()
        };
        let saved = ["vendor/demo".to_owned()];
        let native = Scope {
            runtime: Runtime::Codex,
            api: false,
        };
        let relay = Scope {
            runtime: Runtime::Codex,
            api: true,
        };
        let resolve = |scope: Scope, t: &ThreadRow| scope.saved_route(t, &saved, Some("codeox"));
        assert_eq!(
            resolve(native, &row("openai", None)),
            Some(Route::CODEX_SUBSCRIPTION)
        );
        assert_eq!(
            resolve(relay, &row("codeox", Some("vendor/demo"))),
            Some(Route::codex_api(saved[0].clone()))
        );
        assert_eq!(
            resolve(native, &row("external-vendor", Some("vendor/demo"))),
            None
        );
        assert_eq!(resolve(native, &row("codeox", Some("vendor/demo"))), None);
        assert_eq!(resolve(relay, &row("openai", None)), None);
        assert_eq!(resolve(relay, &row("codeox", None)), None);
    }

    #[test]
    fn global_sessions_arrive_without_discovery_and_keep_their_exact_route() {
        use crate::platform::CatalogEvent;
        use ox_desk_core::host_protocol::{Scope as Origin, SessionView};
        let project = Path::new("/other");
        let route = Route::codex_api("vendor/demo".into());
        let origin = Origin {
            project: project.into(),
            route: route.clone(),
        };
        let detail = serde_json::json!({"owned":true,"row":{"cwd":project,"id":"phone","modelProvider":"codeox","preview":"From my phone"}});
        let mut session = SessionView {
            id: "phone".into(),
            title: "From my phone".into(),
            parent: None,
            status: "running".into(),
            updated_at: 1,
            capabilities: vec![],
            detail,
        };
        let mut library = Discovery::default();
        assert!(library.live_event(CatalogEvent::Update(origin.clone(), session.clone())));
        let saved = &library.hosted[0].saved;
        assert_eq!(
            library.route_status(project, saved.scope, &saved.row),
            Some((route.clone(), Some("Running")))
        );
        assert!(!library.live_event(CatalogEvent::Update(origin.clone(), session.clone())));
        session.status = "needs_input".into();
        assert!(library.live_event(CatalogEvent::Update(origin.clone(), session.clone())));
        library.replace(Some(true), vec![]);
        assert_eq!(library.all().count(), 1);
        assert!(library.live_event(CatalogEvent::Replace(origin, vec![])));
        assert_eq!(library.all().count(), 0);
    }

    #[test]
    fn refreshing_one_home_removes_archived_rows_without_touching_the_other_home() {
        let mut library = Discovery::default();
        let project = Path::new("/work");
        let native = Scope {
            runtime: Runtime::Codex,
            api: false,
        };
        let relay = Scope {
            runtime: Runtime::Codex,
            api: true,
        };
        let row = || {
            serde_json::from_value::<ThreadRow>(
                serde_json::json!({"id":"duplicate","cwd":project,"modelProvider":"openai"}),
            )
            .unwrap()
        };
        library.replace(Some(false), vec![row()]);
        library.replace(Some(true), vec![row()]);
        library.replace(Some(false), vec![]);
        assert_eq!(library.rows.len(), 1);
        assert!(library.removed(project, native, "duplicate"));
        assert!(!library.removed(project, relay, "duplicate"));
        library.replace(Some(false), vec![row()]);
        assert!(!library.removed(project, native, "duplicate"));
        assert_eq!(library.rows.len(), 2);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum StatusFilter {
    Running,
    Waiting,
    Finished,
}

/// Split a leading or trailing `is:` token from the catalog search text.
pub(super) fn split_status_filter(query: &str) -> (Option<StatusFilter>, String) {
    let mut filter = None;
    let rest: Vec<&str> = query
        .split_whitespace()
        .filter(|word| {
            let found = match *word {
                "is:running" => Some(StatusFilter::Running),
                "is:waiting" | "is:approval" => Some(StatusFilter::Waiting),
                "is:finished" | "is:done" => Some(StatusFilter::Finished),
                _ => None,
            };
            filter = filter.or(found);
            found.is_none()
        })
        .collect();
    (filter, rest.join(" "))
}

#[cfg(test)]
mod status_filter_tests {
    use super::{StatusFilter, split_status_filter};
    #[test]
    fn status_tokens_leave_the_text_query() {
        assert_eq!(
            split_status_filter("is:running fix"),
            (Some(StatusFilter::Running), "fix".into())
        );
        assert_eq!(
            split_status_filter("tests is:waiting"),
            (Some(StatusFilter::Waiting), "tests".into())
        );
        assert_eq!(
            split_status_filter("is:finished"),
            (Some(StatusFilter::Finished), String::new())
        );
        assert_eq!(split_status_filter("plain"), (None, "plain".into()));
    }
}

#[cfg(test)]
mod parent_merge_tests {
    use super::keep_parent;
    #[test]
    fn a_richer_row_without_a_parent_keeps_the_known_parent() {
        let p = |s: &str| Some(s.to_owned());
        assert_eq!(keep_parent(p("parent"), None), p("parent"));
        assert_eq!(keep_parent(None, p("parent")), p("parent"));
        assert_eq!(keep_parent(p("old"), p("new")), p("new"));
        assert_eq!(keep_parent(None, None), None);
    }
}

//! Live navigation metadata for a single host, including unopened sessions.
use crate::platform::{CatalogEvent, CatalogWatch, HostLink};
use gpui::*;
use ox_desk_core::host_protocol::Scope;
use std::{
    collections::{HashMap, HashSet},
    io,
};

#[derive(Default)]
struct Project {
    titles: HashMap<String, String>,
    archived: HashSet<String>,
    waiting: HashSet<String>,
}

#[derive(Default)]
pub(super) struct Inventory {
    pub link: HostLink,
    projects: HashMap<Scope, Project>,
    online: bool,
    error: Option<io::ErrorKind>,
    last_seen: Option<chrono::DateTime<chrono::Local>>,
    _watch: Option<CatalogWatch>,
}

impl Inventory {
    pub fn start(link: HostLink, cx: &mut Context<Self>) -> Self {
        let mut catalog = Self {
            link,
            ..Default::default()
        };
        match catalog.link.watch_catalog() {
            Ok((watch, events)) => {
                catalog._watch = Some(watch);
                cx.spawn(async move |this, cx| {
                    while let Ok(first) = events.recv_async().await {
                        if this
                            .update(cx, |s, cx| {
                                s.receive(first);
                                for e in events.try_iter() {
                                    s.receive(e);
                                }
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(e) => catalog.error = Some(e.kind()),
        }
        catalog
    }
    fn receive(&mut self, event: CatalogEvent) {
        if let CatalogEvent::Disconnected(reason) = event {
            if self.online {
                self.last_seen = Some(chrono::Local::now());
            }
            self.error = Some(reason);
            self.online = false;
            return;
        }
        self.online = true;
        self.error = None;
        match event {
            CatalogEvent::Reset => self.projects.clear(),
            CatalogEvent::Archived(scope, ids) => {
                self.projects.entry(scope).or_default().archived = ids
            }
            CatalogEvent::Waiting(scope, ids) => {
                self.projects.entry(scope).or_default().waiting = ids
            }
            CatalogEvent::Replace(scope, rows) => {
                self.projects.entry(scope).or_default().titles =
                    rows.into_iter().map(|r| (r.id, r.title)).collect();
            }
            CatalogEvent::Update(scope, row) => {
                self.projects
                    .entry(scope)
                    .or_default()
                    .titles
                    .insert(row.id, row.title);
            }
            CatalogEvent::Disconnected(_) => unreachable!(),
        }
    }

    pub fn status(&self) -> String {
        if self.online {
            return "Connected".into();
        }
        match self.error {
            Some(io::ErrorKind::Unsupported) => "Needs update".into(),
            Some(io::ErrorKind::PermissionDenied) => "Pairing needs attention".into(),
            Some(_) => self.last_seen.as_ref().map_or_else(
                || "Offline".into(),
                |t| format!("Offline · last seen {}", t.format("%a %H:%M")),
            ),
            None => "Connecting…".into(),
        }
    }
    pub fn rows(&self, waiting_only: bool) -> Vec<(Scope, String, String)> {
        let mut rows = Vec::new();
        for (scope, project) in &self.projects {
            let add = |id: &String| {
                (
                    scope.clone(),
                    id.clone(),
                    project
                        .titles
                        .get(id)
                        .filter(|t| !t.is_empty())
                        .unwrap_or(id)
                        .clone(),
                )
            };
            if waiting_only {
                rows.extend(project.waiting.iter().map(add));
            } else {
                rows.extend(
                    project
                        .titles
                        .keys()
                        .filter(|id| !project.archived.contains(*id))
                        .map(add),
                );
            }
        }
        rows.sort_by(|a, b| (&a.0.project, &a.2, &a.1).cmp(&(&b.0.project, &b.2, &b.1)));
        rows
    }
    pub fn waiting(&self) -> usize {
        self.projects.values().map(|p| p.waiting.len()).sum()
    }
    pub fn online(&self) -> bool {
        self.online
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Sessions,
    Inbox,
}

impl super::Workspace {
    pub(super) fn sync_inventory(&mut self, cx: &mut Context<Self>) {
        let saved = &cx.global::<crate::preferences::HostDirectory>().0;
        let mut links: Vec<_> = saved.iter().cloned().map(HostLink::Remote).collect();
        if !links.is_empty() && self.local.is_some() {
            links.push(HostLink::Local);
        }
        self.inventory.retain(|_, entry| {
            let current = &entry.read(cx).link;
            links.iter().any(|l| match (current.remote(), l.remote()) {
                (Some(a), Some(b)) => a.same_pairing(b),
                (None, None) => true,
                _ => false,
            })
        });
        for link in links {
            let id = link.remote().map(|h| h.id.clone());
            self.inventory.entry(id).or_insert_with(|| {
                let entry = cx.new(|cx| Inventory::start(link, cx));
                cx.observe(&entry, |_, _, cx| cx.notify()).detach();
                entry
            });
        }
    }
    fn open_result(
        &mut self,
        id: Option<String>,
        scope: Scope,
        thread: String,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.inventory.get(&id) else {
            return;
        };
        if !entry.read(cx).online() {
            return;
        }
        if let Some(host) = entry.read(cx).link.remote().cloned()
            && !self.hosts.contains_key(&host.id)
        {
            self.show_project(host, scope.project.clone(), scope.route.clone(), w, cx);
        }
        let host = match &id {
            Some(id) => self.hosts.get(id),
            None => self.local.as_ref(),
        };
        if let Some(view) = host {
            view.update(cx, |host, cx| host.show_thread(scope, thread, w, cx));
            self.set_active(id);
            self.settings = None;
            self.page = None;
            cx.notify();
        }
    }
    pub(super) fn fleet_view(&self, page: Page, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::{appearance::Palette, kit};
        use gpui_component::input::Input;
        let p = Palette::get(cx);
        let query = self.filter.read(cx).value().trim().to_lowercase();
        let mut words: Vec<_> = query.split_whitespace().collect();
        let waiting = page == Page::Inbox || words.contains(&"is:waiting");
        words.retain(|q| *q != "is:waiting");
        let mut panel = kit::col()
            .id("fleet-sessions")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_2()
            .p_3()
            .child(Input::new(&self.filter));
        let mut matches = 0_usize;
        let mut hosts: Vec<_> = self
            .inventory
            .iter()
            .map(|(id, e)| {
                let catalog = e.read(cx);
                let name = catalog
                    .link
                    .remote()
                    .map_or("This computer", |h| h.name.as_str())
                    .to_string();
                (name, id, catalog)
            })
            .collect();
        hosts.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        for (name, id, inventory) in hosts {
            let items = inventory.rows(waiting);
            let mut last_project = None;
            let mut show_header = true;
            for (scope, thread, title) in items {
                let path = scope.project.to_string_lossy();
                let text = format!("{name} {path} {title}").to_lowercase();
                if !words.iter().all(|q| match q.strip_prefix('@') {
                    Some(host) => name.to_lowercase().contains(host),
                    None => text.contains(q),
                }) {
                    continue;
                }
                if show_header {
                    panel = panel.child(format!("{} · {}", name, inventory.status()));
                    show_header = false;
                }
                if last_project.as_ref() != Some(&scope.project) {
                    panel = panel.child(kit::small(p, path.into_owned()));
                    last_project = Some(scope.project.clone());
                }
                let id = id.clone();
                panel = panel.child(kit::button(
                    p,
                    ("fleet-thread", matches),
                    title,
                    false,
                    inventory.online(),
                    cx.listener(move |s, _, w, cx| {
                        s.open_result(id.clone(), scope.clone(), thread.clone(), w, cx)
                    }),
                ));
                matches += 1;
            }
            if show_header && page == Page::Sessions && words.is_empty() {
                panel = panel.child(format!("{} · {}", name, inventory.status()));
            }
        }
        if matches == 0 {
            panel = panel.child(kit::small(
                p,
                if waiting {
                    "No matching sessions need a response."
                } else {
                    "No matching sessions."
                },
            ));
        }
        panel
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::{host_protocol::SessionView, store::Route};
    #[::core::prelude::v1::test]
    fn identical_session_ids_and_paths_stay_on_their_host_across_reconnect() {
        let scope = Scope {
            project: "/work/ox".into(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let mut hosts = [
            Inventory::default(),
            Inventory::default(),
            Inventory::default(),
        ];
        for (index, host) in hosts.iter_mut().enumerate() {
            host.receive(CatalogEvent::Reset);
            host.receive(CatalogEvent::Replace(
                scope.clone(),
                vec![SessionView {
                    id: "same".into(),
                    title: format!("Thread on {index}"),
                    status: "waiting".into(),
                    parent: None,
                    updated_at: 0,
                    capabilities: vec![],
                    detail: Default::default(),
                }],
            ));
            host.receive(CatalogEvent::Waiting(
                scope.clone(),
                HashSet::from(["same".into()]),
            ));
        }
        hosts[1].receive(CatalogEvent::Disconnected(io::ErrorKind::ConnectionReset));
        hosts[0].receive(CatalogEvent::Waiting(scope.clone(), Default::default()));
        assert!(hosts[0].rows(true).is_empty());
        assert!(!hosts[1].online());
        assert!(hosts[1].last_seen.is_some());
        assert_eq!(hosts[1].rows(true)[0].2, "Thread on 1");
        assert!(hosts[2].online());
        assert_eq!(hosts[2].rows(true)[0].2, "Thread on 2");
        hosts[1].receive(CatalogEvent::Reset);
        assert_eq!(hosts[1].waiting(), 0);
        assert_eq!(hosts[2].waiting(), 1);
    }
}

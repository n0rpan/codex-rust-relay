//! Match project checkouts by the repository identity supplied by their owning hosts.
use super::Workspace;
use crate::{appearance::Palette, kit, workspace_state::Checkout};
use gpui::*;
use ox_desk_core::{host_protocol::ProjectDirectory, store::Route};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub(crate) struct NewProjectSession {
    pub project: PathBuf,
    pub route: Route,
}
pub(crate) struct UsedProject(pub PathBuf);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ProjectKey {
    Repository(String),
    Directory(Checkout),
}
impl ProjectKey {
    fn new(location: &Checkout, repo: Option<&String>) -> Self {
        repo.map_or_else(
            || Self::Directory(location.clone()),
            |r| Self::Repository(r.clone()),
        )
    }
}
#[derive(Clone)]
struct Choice {
    checkout: Checkout,
    registration: EntityId,
    host_name: String,
    route: Option<Route>,
}
pub(super) struct Recent(pub crate::workspace_state::ProjectHosts);
impl Global for Recent {}
pub(super) struct Chooser {
    title: String,
    key: ProjectKey,
    choices: Vec<Choice>,
    index: usize,
    request: Option<u64>,
    error: Option<String>,
    previous_focus: Option<FocusHandle>,
}

fn project_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |p| p.to_string_lossy().into_owned(),
    )
}
fn hosts_label(choices: &[Choice]) -> String {
    choices
        .iter()
        .map(|c| &*c.host_name)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ")
}
fn preferred(
    choices: &[Choice],
    remembered: Option<&Checkout>,
    current: Option<&Checkout>,
    available: impl Fn(&Choice) -> bool,
) -> usize {
    for loc in remembered.into_iter().chain(current) {
        if let Some(index) = choices
            .iter()
            .position(|c| &c.checkout == loc && available(c))
            .or_else(|| {
                choices
                    .iter()
                    .position(|c| c.checkout.host == loc.host && available(c))
            })
        {
            return index;
        }
    }
    choices.iter().position(available).unwrap_or(0)
}
impl Workspace {
    fn project_key(&self, loc: &Checkout, cx: &App) -> Option<ProjectKey> {
        let dir = self.inventory.get(&loc.host)?.read(cx).directory.as_ref()?;
        let project = dir.checkouts.iter().find(|p| p.path == loc.path)?;
        Some(ProjectKey::new(loc, project.repository.as_ref()))
    }
    pub(super) fn remember_project(&self, path: &Path, cx: &mut App) {
        let loc = Checkout {
            host: self.active.clone(),
            path: path.into(),
        };
        if let Some(ProjectKey::Repository(repo)) = self.project_key(&loc, cx)
            && cx.global::<Recent>().0.get(&repo) != Some(&loc)
        {
            cx.global_mut::<Recent>()
                .0
                .insert(repo.clone(), loc.clone());
            crate::workspace_state::remember_checkout(repo, loc);
        }
    }
    fn host_name(&self, id: &Option<String>, cx: &App) -> String {
        let entry = self.inventory[id].read(cx);
        let name = entry.display_name();
        if self
            .inventory
            .values()
            .filter(|i| i.read(cx).display_name().eq_ignore_ascii_case(name))
            .nth(1)
            .is_some()
        {
            let origin = entry
                .link
                .remote()
                .map_or("This computer", |h| h.origin.as_str());
            format!("{name} ({origin})")
        } else {
            name.to_owned()
        }
    }
    pub(crate) fn shared_caption(
        &self,
        host: Option<&str>,
        path: &Path,
        cx: &App,
    ) -> Option<String> {
        let host = host.map(str::to_owned);
        let dir = self.inventory.get(&host)?.read(cx).directory.as_ref()?;
        let repo = dir
            .checkouts
            .iter()
            .find(|p| p.path == path)?
            .repository
            .as_ref()?;
        let mut names: Vec<_> = self
            .inventory
            .iter()
            .filter(|(_, entry)| {
                entry.read(cx).directory.as_ref().is_some_and(|dir| {
                    dir.checkouts
                        .iter()
                        .any(|p| p.repository.as_ref() == Some(repo))
                })
            })
            .map(|(id, _)| self.host_name(id, cx))
            .collect();
        names.sort();
        (names.len() > 1).then(|| names.join(", "))
    }
    fn project_groups(
        &self,
        preferred_route: Option<&Route>,
        cx: &App,
    ) -> BTreeMap<ProjectKey, Vec<Choice>> {
        let mut groups: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (host, entry) in &self.inventory {
            let inv = entry.read(cx);
            let Some(dir) = &inv.directory else {
                continue;
            };
            let name = self.host_name(host, cx);
            for project in &dir.checkouts {
                let loc = Checkout {
                    host: host.clone(),
                    path: project.path.clone(),
                };
                let key = ProjectKey::new(&loc, project.repository.as_ref());
                let opened = match host {
                    None => self.local.as_ref(),
                    Some(id) => self.hosts.get(id),
                };
                let remembered = opened
                    .and_then(|view| view.read(cx).project_route(&loc.path))
                    .or_else(|| inv.recent.saved_route(&loc.path));
                let route = preferred_route
                    .into_iter()
                    .chain(remembered.as_ref())
                    .find(|route| dir.routes.contains(route))
                    .or_else(|| dir.routes.first())
                    .cloned();
                groups.entry(key).or_default().push(Choice {
                    checkout: loc,
                    registration: entry.entity_id(),
                    host_name: name.clone(),
                    route,
                });
            }
        }
        for group in groups.values_mut() {
            group.sort_by(|a, b| {
                (&a.checkout.host.is_some(), &a.host_name, &a.checkout).cmp(&(
                    &b.checkout.host.is_some(),
                    &b.host_name,
                    &b.checkout,
                ))
            });
        }
        groups
    }
    pub(super) fn pick_for_workbench(
        &mut self,
        request: &NewProjectSession,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let loc = Checkout {
            host: self.active.clone(),
            path: request.project.clone(),
        };
        if let Some(key) = self.project_key(&loc, cx)
            && let Some(choices) = self.project_groups(Some(&request.route), cx).remove(&key)
            && choices
                .iter()
                .any(|c| c.checkout != loc && self.known(c, &key, cx))
        {
            self.choose_project(key, choices, Some(&loc), w, cx);
            return;
        }
        if let Some(view) = self.visible_host() {
            view.update(cx, |h, cx| {
                h.new_project_draft(request.project.clone(), request.route.clone(), w, cx)
            });
            self.remember_project(&request.project, cx);
        }
    }
    fn choose_project(
        &mut self,
        key: ProjectKey,
        choices: Vec<Choice>,
        current: Option<&Checkout>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let last = match &key {
            ProjectKey::Repository(repo) => cx.global::<Recent>().0.get(repo),
            _ => None,
        };
        let index = preferred(&choices, last, current, |c| self.known(c, &key, cx));
        let title = project_name(current.map_or(&choices[0].checkout.path, |c| &c.path));
        self.chooser = Some(Chooser {
            title,
            key,
            choices,
            index,
            request: None,
            error: None,
            previous_focus: w.focused(cx),
        });
        cx.notify();
    }
    fn known(&self, choice: &Choice, key: &ProjectKey, cx: &App) -> bool {
        let Some(entry) = self.inventory.get(&choice.checkout.host) else {
            return false;
        };
        entry.entity_id() == choice.registration
            && entry.read(cx).online()
            && entry
                .read(cx)
                .directory
                .as_ref()
                .is_some_and(|dir| matches_directory(dir, choice, key))
    }
    fn submit_project(&mut self, index: usize, w: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.chooser.as_ref() else {
            return;
        };
        let Some(choice) = picker.choices.get(index).cloned() else {
            return;
        };
        if picker.request.is_some() {
            return;
        }
        if !self.known(&choice, &picker.key, cx) {
            self.chooser.as_mut().unwrap().error = Some(
                "This checkout or route is unavailable. Choose another location or reopen Projects.".into()
            );
            cx.notify();
            return;
        }
        let entry = self.inventory[&choice.checkout.host].clone();
        let link = entry.read(cx).link.clone();
        self.project_request = self.project_request.wrapping_add(1);
        let request = self.project_request;
        let picker = self.chooser.as_mut().unwrap();
        picker.index = index;
        picker.request = Some(request);
        picker.error = None;
        let confirmed = choice.clone();
        let group = picker.key.clone();
        let task = cx.background_spawn(async move {
            let directory = link.check_project(&confirmed.checkout.path)?;
            if matches_directory(&directory, &confirmed, &group)
                && let Some(remote) = link.remote()
            {
                // A saved native session may be known only on its original route. Register
                // the chosen project and route just as the remote folder picker does.
                let actual =
                    remote.choose(&confirmed.checkout.path, confirmed.route.as_ref().unwrap())?;
                if actual != confirmed.checkout.path {
                    return Err(std::io::Error::other("The checkout path changed."));
                }
            }
            Ok(directory)
        });
        cx.spawn_in(w, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |s, w, cx| {
                s.finish_project(request, choice, result, w, cx);
            });
        })
        .detach();
        cx.notify();
    }
    fn finish_project(
        &mut self,
        request: u64,
        choice: Choice,
        result: std::io::Result<ProjectDirectory>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = self.chooser.as_mut().filter(|p| p.request == Some(request)) else {
            return;
        };
        picker.request = None;
        let result = result.and_then(|directory| {
            let entry = self.inventory.get(&choice.checkout.host)
                .filter(|entry| entry.entity_id()==choice.registration && entry.read(cx).online())
                .ok_or_else(|| std::io::Error::other("The host disconnected or was paired again."))?;
            let valid = matches_directory(&directory, &choice, &picker.key);
            entry.update(cx, |inventory,cx| {
                if let Some(known) = &mut inventory.directory {
                    known.routes = directory.routes;
                    known.checkouts.retain(|p| p.path != choice.checkout.path);
                    known.checkouts.extend(directory.checkouts);
                }
                cx.notify();
            });
            if !valid {
                return Err(std::io::Error::other(
                    "The checkout or billing route changed. Reopen Projects to choose its current location."
                ));
            }
            let remote = entry.read(cx).link.remote();
            remote.map(|previous| {
                cx.global::<crate::preferences::HostDirectory>().0.iter()
                    .find(|host|host.same_pairing(previous) && host.origin==previous.origin).cloned()
                    .ok_or_else(||std::io::Error::other("The host pairing changed. Choose the host again."))
            }).transpose()
        });
        match result {
            Err(error) => picker.error = Some(format!("{}: {error}", choice.host_name)),
            Ok(remote) => {
                let route = choice.route.expect("validated project route");
                let Checkout { host, path } = choice.checkout;
                if let Some(remote) = remote
                    && !self.hosts.contains_key(&remote.id)
                {
                    self.show_project(remote, path.clone(), route.clone(), w, cx);
                }
                let view = match &host {
                    None => self.local.clone(),
                    Some(id) => self.hosts.get(id).cloned(),
                };
                if let Some(view) = view {
                    self.set_active(host);
                    self.page = None;
                    self.settings = None;
                    view.update(cx, |h, cx| h.new_project_draft(path.clone(), route, w, cx));
                    self.remember_project(&path, cx);
                } else if let Some(picker) = &mut self.chooser {
                    picker.error = Some("The host pairing changed. Choose the host again.".into());
                }
            }
        }
        cx.notify();
    }
    fn cancel_project(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if let Some(focus) = self.chooser.take().and_then(|p| p.previous_focus) {
            focus.focus(w, cx);
        }
        cx.notify();
    }
    pub(super) fn chooser_key(
        &mut self,
        key: &str,
        shift: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = self.chooser.as_mut() else {
            return;
        };
        match key {
            "escape" => self.cancel_project(w, cx),
            "enter" => {
                let index = picker.index;
                self.submit_project(index, w, cx);
            }
            "up" | "down" | "tab" => {
                let n = picker.choices.len();
                if n > 0 && picker.request.is_none() {
                    picker.index = (picker.index
                        + if key == "up" || (key == "tab" && shift) {
                            n - 1
                        } else {
                            1
                        })
                        % n;
                    cx.notify();
                }
            }
            _ => return,
        }
        w.prevent_default();
        cx.stop_propagation();
    }
    pub(super) fn project_chooser(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let picker = self.chooser.as_ref().unwrap();
        let p = Palette::get(cx);
        let mut panel = kit::col()
            .id("project-checkouts")
            .flex_1()
            .overflow_y_scroll()
            .p_4()
            .gap_2()
            .child(format!("New session in {}", picker.title))
            .child(kit::small(
                p,
                "Choose a checkout · ↑ ↓ to select, Enter to open",
            ));
        for (index, c) in picker.choices.iter().enumerate() {
            let status = if self
                .inventory
                .get(&c.checkout.host)
                .is_some_and(|e| e.read(cx).online())
            {
                ""
            } else {
                " · Offline"
            };
            panel = panel.child(kit::button(
                p,
                ("project-checkout", index),
                format!(
                    "{} · {} · {}{status}",
                    c.host_name,
                    c.checkout.path.display(),
                    c.route
                        .as_ref()
                        .map_or_else(|| "No available route".to_owned(), |r| r.label())
                ),
                picker.index == index,
                picker.request.is_none() && self.known(c, &picker.key, cx),
                cx.listener(move |s, _, w, cx| s.submit_project(index, w, cx)),
            ));
        }
        if picker.request.is_some() {
            panel = panel.child("Opening checkout…");
        }
        if let Some(error) = &picker.error {
            panel = panel.child(kit::small(p, error.clone()));
        }
        panel.child(kit::button(
            p,
            "cancel-project",
            "Cancel",
            false,
            true,
            cx.listener(|s, _, w, cx| s.cancel_project(w, cx)),
        ))
    }
    pub(super) fn projects_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let mut page = kit::col()
            .id("workspace-projects")
            .flex_1()
            .overflow_y_scroll()
            .gap_2()
            .p_3();
        let mut rows: Vec<_> = self.project_groups(None, cx).into_iter().collect();
        rows.sort_by_cached_key(|(_, group)| project_name(&group[0].checkout.path).to_lowercase());
        if rows.is_empty() {
            page=page.child(kit::small(p,"No projects reported yet. Open a folder on a host or register it with ox host project add."));
        }
        for (index, (key, group)) in rows.into_iter().enumerate() {
            let name = project_name(&group[0].checkout.path);
            let heading = format!("{name} · {}", hosts_label(&group));
            let detail = (group.len() == 1).then(|| {
                let c = &group[0];
                format!(
                    "{} · {}",
                    c.checkout.path.display(),
                    c.route
                        .as_ref()
                        .map_or_else(|| "No available route".into(), |r| r.label().to_string())
                )
            });
            let mut title = kit::col().child(heading);
            if let Some(detail) = detail {
                title = title.child(kit::small(p, detail));
            }
            page = page.child(
                kit::row()
                    .items_center()
                    .gap_2()
                    .child(title)
                    .child(kit::button(
                        p,
                        ("start-project", index),
                        "New session",
                        false,
                        group.iter().any(|c| self.known(c, &key, cx)),
                        cx.listener(move |s, _, w, cx| {
                            let sole = group.len() == 1;
                            s.choose_project(key.clone(), group.clone(), None, w, cx);
                            if sole {
                                s.submit_project(0, w, cx);
                            }
                        }),
                    )),
            );
        }
        for (id, entry) in &self.inventory {
            let inv = entry.read(cx);
            if inv.online() && inv.legacy_projects {
                page = page.child(kit::small(
                    p,
                    format!(
                        "Update {} to list its projects here.",
                        self.host_name(id, cx)
                    ),
                ));
            }
        }
        page
    }
}
fn matches_directory(dir: &ProjectDirectory, choice: &Choice, key: &ProjectKey) -> bool {
    choice
        .route
        .as_ref()
        .is_some_and(|r| dir.routes.contains(r))
        && dir.checkouts.iter().any(|p| {
            p.path == choice.checkout.path
                && ProjectKey::new(&choice.checkout, p.repository.as_ref()) == *key
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::host_protocol::ProjectCheckout;

    fn location(host: Option<&str>, path: &str) -> Checkout {
        Checkout {
            host: host.map(str::to_owned),
            path: path.into(),
        }
    }
    fn choice(checkout: Checkout) -> Choice {
        Choice {
            checkout,
            registration: 1u64.into(),
            host_name: "Build".into(),
            route: Some(Route::CODEX_SUBSCRIPTION),
        }
    }
    #[::core::prelude::v1::test]
    fn local_paths_never_identify_shared_projects() {
        let laptop = location(None, "/work/ox");
        let studio = location(Some("studio"), "/work/ox");
        assert_ne!(
            ProjectKey::new(&laptop, None),
            ProjectKey::new(&studio, None)
        );
        assert_ne!(
            ProjectKey::new(&laptop, Some(&"first".into())),
            ProjectKey::new(&studio, Some(&"fork".into()))
        );
        let other = location(Some("studio"), "/home/build/source");
        assert_eq!(
            ProjectKey::new(&laptop, Some(&"first".into())),
            ProjectKey::new(&other, Some(&"first".into()))
        );
    }
    #[::core::prelude::v1::test]
    fn prefer_the_last_checkout_then_the_last_host_then_the_current_checkout() {
        let local = location(None, "/work/project");
        let studio = location(Some("studio"), "/work/one");
        let second = location(Some("studio"), "/work/two");
        let choices = [
            choice(local.clone()),
            choice(studio.clone()),
            choice(second.clone()),
        ];
        assert_eq!(
            preferred(&choices, Some(&second), Some(&local), |c| c
                .checkout
                .host
                .is_none()),
            0
        );
        assert_eq!(
            preferred(&choices, Some(&second), Some(&local), |_| true),
            2
        );
        assert_eq!(
            preferred(
                &choices,
                Some(&location(Some("studio"), "/former")),
                Some(&local),
                |_| true
            ),
            1
        );
        assert_eq!(
            preferred(
                &choices,
                Some(&location(Some("removed"), "/former")),
                Some(&second),
                |_| true
            ),
            2
        );
    }
    #[::core::prelude::v1::test]
    fn validate_both_the_origin_and_the_requested_billing_route() {
        let location = location(Some("studio"), "/work/ox");
        let chosen = choice(location.clone());
        let key = ProjectKey::Repository("repo".into());
        let mut dir = ProjectDirectory {
            routes: vec![Route::CODEX_SUBSCRIPTION],
            checkouts: vec![ProjectCheckout {
                path: location.path,
                repository: Some("repo".into()),
            }],
        };
        assert!(matches_directory(&dir, &chosen, &key));
        dir.routes = vec![Route::codex_api("fixture/model".into())];
        assert!(!matches_directory(&dir, &chosen, &key));
        dir.routes = vec![Route::CODEX_SUBSCRIPTION];
        dir.checkouts[0].repository = Some("fork".into());
        assert!(!matches_directory(&dir, &chosen, &key));
    }
}

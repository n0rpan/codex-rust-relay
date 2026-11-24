//! Persist only route, thread identity and geometry. Native Codex owns thread contents.
use super::*;
use gpui::MouseButton;
use ox_desk_core::runtime::Billing;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io, path::Path};

#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Layout {
    pub width: u32,
    pub height: u32,
    sidebar_width: u32,
    pub(super) sidebar_hidden: bool,
    panes: Vec<SavedPane>,
    active: usize,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct SavedPane {
    #[serde(default)]
    runtime: ox_desk_core::runtime::Runtime,
    project: PathBuf,
    api_model: Option<String>,
    selection: Option<SavedThread>,
    #[serde(default)]
    review_open: bool,
    #[serde(default)]
    review_width: u32,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct SavedThread {
    id: String,
    provider: String,
    #[serde(default)]
    updated_at: i64,
    model: Option<String>,
    cwd: PathBuf,
}
impl SavedThread {
    fn capture(row: &ThreadRow) -> Option<Self> {
        (!row.worker).then(|| Self {
            id: row.id.clone(),
            provider: row.model_provider.clone(),
            updated_at: row.updated_at,
            model: row.model.clone(),
            cwd: row.cwd.clone(),
        })
    }
    fn row(&self) -> ThreadRow {
        ThreadRow {
            id: self.id.clone(),
            model_provider: self.provider.clone(),
            cwd: self.cwd.clone(),
            model: self.model.clone(),
            parent_thread_id: None,
            agent_nickname: None,
            name: None,
            generated_title: None,
            preview: String::new(),
            updated_at: self.updated_at,
            status: None,
            worker: false,
            unread: false,
            activity_order: 0,
        }
    }
}
/// Keep the last window at the original JSON keys for existing desktop versions.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct SavedLayouts {
    #[serde(flatten)]
    current: Layout,
    #[serde(default)]
    projects: BTreeMap<PathBuf, Layout>,
}
impl SavedLayouts {
    pub(crate) fn load() -> Self {
        crate::workspace_state::read_preference("layout.json").unwrap_or_default()
    }

    pub(crate) fn for_project(&self, project: &Path) -> Layout {
        if self.current.active_project() == Some(project) {
            return self.current.clone();
        }
        self.projects
            .get(project)
            .cloned()
            .or_else(|| self.current.only_project(project))
            .unwrap_or_default()
    }

    pub(crate) fn save_at(path: &Path, layout: Layout) -> io::Result<()> {
        crate::workspace_state::update_preference_at(path, Self::default, |mut saved: Self| {
            saved.remember(layout);
            saved
        })
    }
    fn remember(&mut self, layout: Layout) {
        let Some(active) = layout.active_project() else {
            return;
        };
        // Older files contain a single window with panes from multiple projects.
        for pane in &self.current.panes {
            if !self.projects.contains_key(&pane.project)
                && let Some(prior) = self.current.only_project(&pane.project)
            {
                self.projects.insert(pane.project.clone(), prior);
            }
        }
        if let Some(current) = layout.only_project(active) {
            self.projects.insert(active.to_owned(), current);
        }
        self.current = layout;
    }
}
impl Layout {
    fn active_project(&self) -> Option<&Path> {
        Some(&self.panes.get(self.active)?.project)
    }

    pub(super) fn active_route(&self, project: &Path) -> Option<Route> {
        let pane = self
            .panes
            .get(self.active)
            .filter(|p| p.project == project)?;
        Some(pane.route())
    }

    fn only_project(&self, project: &Path) -> Option<Self> {
        let mut panes = Vec::new();
        let mut active = 0;
        for (index, pane) in self.panes.iter().enumerate() {
            if pane.project == project {
                if index == self.active {
                    active = panes.len();
                }
                panes.push(pane.clone());
            }
        }
        (!panes.is_empty()).then_some(Self {
            panes,
            active,
            width: self.width,
            height: self.height,
            sidebar_width: self.sidebar_width,
            sidebar_hidden: self.sidebar_hidden,
        })
    }
    pub fn window_size(&self) -> Size<Pixels> {
        if self.width >= 360 && self.height >= 480 {
            size(
                px(self.width.min(10000) as f32),
                px(self.height.min(10000) as f32),
            )
        } else {
            size(px(1130.), px(760.))
        }
    }
    pub(super) fn side_width(&self) -> u32 {
        if self.sidebar_width == 0 {
            254
        } else {
            self.sidebar_width.clamp(190, 540)
        }
    }
}
impl SavedPane {
    fn route(&self) -> Route {
        self.runtime.route(self.api_model.clone())
    }
    fn restore(self, pane: &Entity<Workbench>, cx: &mut Context<Host>) {
        pane.update(cx, |view, _| {
            if let Some(selection) = self.selection {
                view.restore_selection(selection.row());
            }
            view.restore_review(self.review_open, self.review_width);
        });
    }
}
impl Host {
    pub(super) fn restore_layout(
        &mut self,
        layout: Layout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.panes[0].read(cx);
        let active = layout.panes.get(layout.active);
        let project = pane.project.clone();
        if !active.is_some_and(|p| p.project == project) {
            return;
        }
        let route = pane.route.clone();
        let first = self.panes[0].clone();
        let mut panes = Vec::new();
        let mut active = None;
        for saved in layout.panes {
            if !saved.project.is_absolute() || !saved.project.is_dir() {
                continue;
            }
            let r = saved.route();
            if panes.iter().any(|p: &Entity<Workbench>| {
                let p = p.read(cx);
                p.project == saved.project && p.route == r
            }) {
                continue;
            }
            let p = if saved.project == project && r == route {
                active = Some(panes.len());
                first.clone()
            } else {
                let extra = Self::pane(saved.project.clone(), r, None, self.stats, window, cx);
                extra.update(cx, |b, cx| b.set_front(false, cx));
                extra
            };
            saved.restore(&p, cx);
            panes.push(p);
        }
        self.active = active.unwrap_or_else(|| {
            panes.push(first);
            panes.len() - 1
        });
        self.panes = panes;
    }
    pub(super) fn restore_project(
        &mut self,
        project: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .panes
            .iter()
            .any(|pane| pane.read(cx).project == project)
        {
            return;
        }
        for saved in self.saved_layouts.for_project(project).panes {
            if saved.project != project {
                continue;
            }
            let route = saved.route();
            if self.panes.iter().any(|pane| {
                let view = pane.read(cx);
                view.project == project && view.route == route
            }) {
                continue;
            }
            let pane = Self::pane(saved.project.clone(), route, None, self.stats, window, cx);
            pane.update(cx, |view, cx| view.set_front(false, cx));
            saved.restore(&pane, cx);
            self.panes.push(pane);
        }
    }

    pub(super) fn remember_layout(&mut self, window: &Window, cx: &App) {
        let bounds = window.window_bounds().get_bounds().size;
        let saved = Layout {
            width: f32::from(bounds.width).round() as u32,
            height: f32::from(bounds.height).round() as u32,
            sidebar_width: self.sidebar_width,
            sidebar_hidden: !self.sidebar_visible,
            active: self.active,
            panes: self
                .panes
                .iter()
                .map(|p| {
                    let p = p.read(cx);
                    let route = p
                        .store
                        .as_ref()
                        .and_then(|store| {
                            let id = store.selected()?;
                            store.transcript(id).filter(|t| t.is_ready())?;
                            Some(store.thread_route(Some(id)))
                        })
                        .unwrap_or_else(|| p.route.clone());
                    SavedPane {
                        runtime: route.runtime,
                        project: p.project.clone(),
                        api_model: match route.billing {
                            Billing::Api(model) => Some(model),
                            Billing::Subscription => None,
                        },
                        selection: p.layout_selection().and_then(SavedThread::capture),
                        review_open: p.review_visible(),
                        review_width: p.review_width,
                    }
                })
                .collect(),
        };
        if self.saved_layouts.current != saved {
            self.saved_layouts.remember(saved.clone());
            crate::workspace_state::remember_layout(saved);
        }
    }
    pub(super) fn divider(&self, p: Palette, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("sidebar-divider")
            .w(px(DIVIDER_WIDTH))
            .h_full()
            .cursor_col_resize()
            .bg(rgb(p.line))
            .hover(move |d| d.bg(rgb(p.accent)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|s, _, _, cx| {
                    s.resizing_sidebar = true;
                    cx.notify()
                }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{Layout, SavedLayouts, SavedThread, age, session_day};
    use gpui::{px, size};
    use serde_json::json;
    use std::fs::File;
    use std::path::Path;

    #[test]
    fn compact_geometry_survives_relaunch() {
        let layout = Layout {
            width: 717,
            height: 870,
            ..Default::default()
        };
        assert_eq!(layout.window_size(), size(px(717.), px(870.)));
        let minimum = Layout {
            width: 360,
            height: 480,
            ..layout
        };
        assert_eq!(minimum.window_size(), size(px(360.), px(480.)));
    }

    fn fixture(path: &str, thread: &str, width: u32) -> Layout {
        serde_json::from_value(json!({
            "width":1000,"height":720,"sidebar_width":width,"active":0,
            "panes":[{"project":path,"api_model":null,
                "selection":{"id":thread,"provider":"openai","model":null,"cwd":path},
                "review_open":true,"review_width":480}]
        }))
        .unwrap()
    }

    #[test]
    fn restored_selection_recovers_its_stock_date() {
        let mut saved = fixture("/project", "session", 280)
            .panes
            .remove(0)
            .selection
            .unwrap()
            .row();
        assert_eq!(session_day(saved.updated_at), None);
        assert!(age(1_795_543_164, saved.updated_at).is_empty());
        saved.updated_at = 1_795_543_164;
        let persisted = serde_json::to_vec(&SavedThread::capture(&saved).unwrap()).unwrap();
        let restored: SavedThread = serde_json::from_slice(&persisted).unwrap();
        assert_eq!(restored.row().updated_at, saved.updated_at);
    }

    #[test]
    fn older_windows_keep_each_projects_selections_when_a_new_project_starts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.json");
        let mut legacy = fixture("/first", "first-thread", 299);
        legacy
            .panes
            .push(fixture("/second", "second-thread", 299).panes.remove(0));
        legacy.active = 1;
        serde_json::to_writer(File::create(&path).unwrap(), &legacy).unwrap();
        let previous: SavedLayouts = serde_json::from_reader(File::open(&path).unwrap()).unwrap();
        assert_eq!(previous.for_project(Path::new("/second")).panes.len(), 2);
        let first = previous.for_project(Path::new("/first"));
        assert_eq!(first.panes.len(), 1);
        assert_eq!(
            first.panes[0].selection.as_ref().unwrap().id,
            "first-thread"
        );
        assert!(previous.for_project(Path::new("/new")).panes.is_empty());

        SavedLayouts::save_at(&path, fixture("/new", "new-thread", 401)).unwrap();
        let saved: SavedLayouts = serde_json::from_reader(File::open(&path).unwrap()).unwrap();
        for (project, thread, width) in [
            ("/first", "first-thread", 299),
            ("/second", "second-thread", 299),
            ("/new", "new-thread", 401),
        ] {
            let group = saved.for_project(Path::new(project));
            assert_eq!(group.side_width(), width);
            let pane = &group.panes[group.active];
            assert_eq!(pane.selection.as_ref().unwrap().id, thread);
            assert!(pane.review_open);
            assert_eq!(pane.review_width, 480);
        }
    }
    #[test]
    fn simultaneous_windows_preserve_other_projects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.json");
        std::thread::scope(|scope| {
            for project in ["/one", "/two", "/three"] {
                let path = &path;
                scope.spawn(move || {
                    SavedLayouts::save_at(path, fixture(project, project, 300)).unwrap()
                });
            }
        });
        let saved: SavedLayouts = serde_json::from_reader(File::open(&path).unwrap()).unwrap();
        for project in ["/one", "/two", "/three"] {
            let layout = saved.for_project(Path::new(project));
            assert_eq!(layout.panes[0].selection.as_ref().unwrap().id, project);
        }
    }
}

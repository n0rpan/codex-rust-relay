//! One workbench per canonical project and billing route. Each owns its editors,
//! server requests and connection for the lifetime of the window.
mod discovery;
use discovery::{Discovery, Location, Scope};
pub(crate) mod layout;
mod palette;
mod shortcuts;
use super::workbench::{CatalogChanged, Workbench};
use crate::kit::{DropdownMenu, PopupMenuItem};
use crate::{
    activity, appearance::Palette, col, kit::button, platform, row, settings, small,
    workspace_state::LastWorkspace,
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{FocusTrapElement, button::ButtonVariants};
use ox_desk_core::actions::Action as DeskAction;
use ox_desk_core::runtime::Billing;
use ox_desk_core::store::{Connection, Route, ThreadRow};
use std::{ops::Range, path::PathBuf, time::Instant};

#[derive(Clone, Copy)]
enum CatalogEntry {
    Day(Option<chrono::NaiveDate>),
    Thread {
        loc: Location,
        timestamp: i64,
        ordinal: usize,
        depth: usize,
    },
}

fn session_day(timestamp: i64) -> Option<chrono::NaiveDate> {
    if timestamp <= 0 {
        return None;
    }
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|t| t.with_timezone(&chrono::Local).date_naive())
}

fn day_label(day: Option<chrono::NaiveDate>, today: chrono::NaiveDate) -> String {
    let Some(day) = day else {
        return "Date unknown".into();
    };
    match (today - day).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => day.format("%b %-d, %Y").to_string(),
    }
}

fn age(now: i64, timestamp: i64) -> String {
    if timestamp <= 0 {
        return String::new();
    }
    match now.saturating_sub(timestamp).max(0) {
        0..60 => "now".into(),
        s @ 60..3600 => format!("{}m", s / 60),
        s @ 3600..86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

pub struct Host {
    palette: Option<palette::State>,
    _palette_keys: Subscription,
    sidebar_visible: bool,
    drawer_open: bool,
    drawer_focus: FocusHandle,
    before_drawer: Option<FocusHandle>,
    sidebar_width: u32,
    resizing_sidebar: bool,
    saved_layouts: layout::SavedLayouts,
    filter: Entity<gpui_component::input::InputState>,
    filter_task: Option<Task<()>>,
    recent: LastWorkspace,
    preferences: Option<Entity<crate::preferences::Preferences>>,
    before_settings: Option<FocusHandle>,
    panes: Vec<Entity<Workbench>>,
    active: usize,
    models: Vec<String>,
    api_provider: Option<String>,
    native_account: bool,
    favorite_models: Vec<String>,
    catalog_versions: Vec<(usize, u64)>,
    catalog: Vec<CatalogEntry>,
    discovery: Discovery,
    list_focus: FocusHandle,
    list_scroll: UniformListScrollHandle,
    choose_route: Option<(PathBuf, ThreadRow)>,
    stats: bool,
    list_rows: u64,
    max_range: usize,
    furthest_row: usize,
    measure_from: Option<gpui::FrameDurationSnapshot>,
    _probe: Option<platform::probe::Socket>,
    message: Option<String>,
}
const MIN_THREAD_WIDTH: f32 = 600.;
const DIVIDER_WIDTH: f32 = 5.;

fn needs_drawer(viewport: Pixels, sidebar_width: u32) -> bool {
    viewport < px(sidebar_width as f32 + DIVIDER_WIDTH + MIN_THREAD_WIDTH)
}

impl Host {
    pub fn new(
        project: PathBuf,
        saved_layouts: layout::SavedLayouts,
        first_frame: Option<Instant>,
        stats: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let probe = if stats { Self::probe(window, cx) } else { None };
        let mut recent = LastWorkspace::load();
        let layout = saved_layouts.for_project(&project);
        let route = layout
            .active_route(&project)
            .unwrap_or_else(|| recent.route(&project));
        recent.remember(&project, &route);
        let message = LastWorkspace::save(&project, &route)
            .err()
            .map(|e| e.to_string());
        let pane = Self::pane(project, route, first_frame, stats, window, cx);
        Self::refresh_routes(window, cx);
        cx.observe_window_visibility(window, |_, _, w, cx| {
            if w.is_visible() {
                cx.notify()
            }
        })
        .detach();
        let filter = crate::kit::field("", "Search threads…", false, window, cx);
        cx.subscribe(
            &filter,
            |s, field, event: &gpui_component::input::InputEvent, cx| {
                if matches!(event, gpui_component::input::InputEvent::Change) {
                    let query = field.read(cx).value().trim().to_owned();
                    s.filter_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(120))
                            .await;
                        let _ = this.update(cx, |s, cx| s.search(query, cx));
                    }));
                }
            },
        )
        .detach();

        let mut host = Self {
            palette: None,
            sidebar_visible: !layout.sidebar_hidden,
            drawer_open: false,
            drawer_focus: cx.focus_handle(),
            before_drawer: None,
            sidebar_width: layout.side_width(),
            resizing_sidebar: false,
            saved_layouts,
            _palette_keys: Self::palette_keys(window, cx),
            filter,
            filter_task: None,
            list_rows: 0,
            max_range: 0,
            furthest_row: 0,
            measure_from: None,
            _probe: probe,
            recent,
            preferences: None,
            before_settings: None,
            panes: vec![pane],
            active: 0,
            models: vec![],
            api_provider: None,
            native_account: false,
            favorite_models: vec![],
            catalog_versions: vec![],
            catalog: vec![],
            discovery: Discovery::default(),
            list_focus: cx.focus_handle().tab_stop(true),
            list_scroll: Default::default(),
            choose_route: None,
            stats,
            message,
        };
        host.restore_layout(layout, window, cx);
        host.start_discovery(cx);
        host.watch_sessions(window, cx);
        cx.observe_window_bounds(window, |_, _, cx| cx.notify())
            .detach();
        host
    }
    fn probe(window: &mut Window, cx: &mut Context<Self>) -> Option<platform::probe::Socket> {
        let (socket, requests) = match platform::probe::Socket::bind() {
            Ok(Some(probe)) => probe,
            Ok(None) => return None,
            Err(e) => {
                eprintln!("Could not bind desk stats socket: {e}");
                return None;
            }
        };
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(sample) = requests.recv_async().await {
                if this
                    .update_in(cx, |s, window, cx| {
                        let stats = s.snapshot(window, cx);
                        let _ = sample.reply.send(stats);
                        if sample.reset {
                            s.measure_from = Some(window.frame_duration_snapshot());
                        }
                        if sample.refresh {
                            window.refresh();
                            let main = window.window_handle().window_id();
                            for other in cx.windows() {
                                if other.window_id() != main {
                                    let _ = other.update(cx, |_, window, _| window.refresh());
                                }
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
        Some(socket)
    }
    fn snapshot(&self, window: &Window, cx: &App) -> String {
        let mut frames = window.frame_duration_snapshot();
        if let Some(start) = &self.measure_from {
            let _ = frames
                .draw_duration_histogram
                .subtract(&start.draw_duration_histogram);
            let _ = frames
                .dirty_to_present_histogram
                .subtract(&start.dirty_to_present_histogram);
            let _ = frames
                .present_interval_histogram
                .subtract(&start.present_interval_histogram);
        }
        let mut drains = 0;
        let mut notifications = 0;
        let mut rows = self.list_rows;
        let mut spawns = 0;
        let mut unknown = 0;
        let mut stop_ns = 0;
        let mut tail_ns = 0;
        let mut measured_items = 0;
        let mut uniform_row_budget = None;
        for pane in &self.panes {
            let pane = pane.read(cx);
            if let Some(stats) = &pane.stats {
                drains += stats.drains;
                notifications += stats.notifications;
                rows += stats.rows;
                spawns += stats.spawns;
                stop_ns = stop_ns.max(stats.stop_ns);
                if pane.front {
                    tail_ns = stats.painted.get();
                    measured_items = stats.measured_items.len();
                    uniform_row_budget = stats.tail_owner.as_ref().and_then(|(id, _)| {
                        pane.transcript_views
                            .get(id)?
                            .uniform_row_budget(stats.tail_height.get())
                    });
                }
            }
            unknown += pane.store.as_ref().map_or(0, |s| s.unknown_frames());
        }
        let active = self.panes[self.active].read(cx).store.as_ref();
        let (windows, layout) = crate::platform::layout_probe::snapshot();
        let settings = self
            .preferences
            .as_ref()
            .map(|p| p.read(cx).probe_state(cx));
        serde_json::json!({
            "windows": windows, "layout": layout, "settings": settings,
            "saved_sessions":self.discovery.rows.len(),"discovery_pending":self.discovery.pending,
            "connected":active.is_some_and(|s|s.state()==&Connection::Ready),
            "pane": self.active, "selected_thread": active.and_then(|s|s.selected()), "catalog_len": active.map_or(0, |s|s.threads().len()),
            "selected_requests":active.map_or(0, |s|s.asks().iter().filter(|a|a.thread.as_deref()==s.selected()).count()),
            "selected_parent":active.and_then(|s|s.catalog_row(s.selected()?)).and_then(|r|r.parent_thread_id.as_deref()),
            "visible_turn":active.and_then(|s|s.selected().and_then(|id|s.transcript(id))).is_some_and(|t|t.active_turn().is_some()),
            "draft":active.map(|s|s.draft(s.selected())),
            "voluntary_switches":ox_desk_core::perf::voluntary_switches(),
            "frames_drawn":frames.draw_duration_histogram.len(),"draw_p95_ns":frames.draw_duration_histogram.value_at_quantile(0.95),
            "dirty_to_present_p95_ns":frames.dirty_to_present_histogram.value_at_quantile(0.95),"present_interval_p95_ns":frames.present_interval_histogram.value_at_quantile(0.95),
            "drains":drains,"notifications":notifications,"rows":rows,"list_rows":self.list_rows,"max_list_range":self.max_range,"furthest_row":self.furthest_row,
            "spawns":spawns,"unknown":unknown,"stop_ns":stop_ns,"tail_ns":tail_ns,"visible":window.is_visible(),"measured_items":measured_items,"uniform_row_budget":uniform_row_budget,
        }).to_string()
    }
    fn refresh_routes(window: &mut Window, cx: &mut Context<Self>) {
        let task = cx.background_spawn(async {
            settings::load().map(|s| {
                (
                    s.models,
                    s.api_provider,
                    s.chatgpt_account.is_some(),
                    s.favorites,
                )
            })
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |s, window, cx| {
                match result {
                    Ok((models, provider, native_account, favorites)) => {
                        s.favorite_models = favorites;
                        s.models = models;
                        s.api_provider = provider;
                        if native_account && !s.native_account {
                            // A sign-in started in Accounts has no live app-server to notify
                            // when the signed-out OX helper exited before initialization.
                            s.panes[s.active].update(cx, |pane, cx| {
                                if pane.waiting_for_subscription() {
                                    pane.reconnect(window, cx);
                                }
                            });
                        }
                        s.native_account = native_account;
                    }
                    Err(error) => s.message = Some(error),
                }
                if window.is_visible() {
                    cx.notify();
                }
            });
        })
        .detach();
    }
    fn compact(&self, window: &Window) -> bool {
        needs_drawer(window.viewport_size().width, self.sidebar_width)
    }

    fn close_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drawer_open {
            self.drawer_open = false;
            if let Some(previous) = self.before_drawer.take() {
                window.focus(&previous, cx);
            }
            cx.notify();
        }
    }

    fn reveal_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let visible = if self.compact(window) {
            self.drawer_open
        } else {
            self.sidebar_visible
        };
        if !visible {
            self.toggle_sidebar(window, cx);
        }
    }

    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compact(window) {
            if self.drawer_open {
                self.close_drawer(window, cx);
            } else {
                self.before_drawer = window.focused(cx);
                self.drawer_open = true;
                let filter = self.filter.read(cx).focus_handle(cx);
                window.on_next_frame(move |w, cx| w.focus(&filter, cx));
            }
        } else {
            self.sidebar_visible = !self.sidebar_visible;
        }
        cx.notify();
    }

    fn show_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_drawer(window, cx);
        if self.preferences.take().is_some() {
            if let Some(previous) = self.before_settings.take() {
                window.focus(&previous, cx);
            }
            Self::refresh_routes(window, cx);
        } else {
            let project = self.panes[self.active].read(cx).project.clone();
            self.before_settings = window.focused(cx);
            self.preferences =
                Some(cx.new(|cx| crate::preferences::Preferences::new(project, window, cx)));
        }
        self.panes[self.active].update(cx, |p, cx| p.set_front(self.preferences.is_none(), cx));
        cx.notify();
    }
    fn pane(
        project: PathBuf,
        route: Route,
        first_frame: Option<Instant>,
        stats: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Workbench> {
        let pane = cx.new(|cx| Workbench::new(project, route, first_frame, stats, window, cx));
        cx.subscribe_in(
            &pane,
            window,
            |s, _, _: &super::workbench::FocusSearch, window, cx| {
                s.reveal_sidebar(window, cx);
                s.filter.read(cx).focus_handle(cx).focus(window, cx);
            },
        )
        .detach();
        cx.subscribe_in(
            &pane,
            window,
            |s, owner, _: &super::onboarding::ConfigureProvider, window, cx| {
                let project = owner.read(cx).project.clone();
                owner.update(cx, |p, cx| p.set_front(false, cx));
                s.preferences = Some(
                    cx.new(|cx| crate::preferences::Preferences::providers(project, window, cx)),
                );
                cx.notify();
            },
        )
        .detach();
        cx.subscribe_in(
            &pane,
            window,
            |s, owner, event: &super::route_chip::NewModelRoute, window, cx| {
                let project = owner.read(cx).project.clone();
                if let Billing::Api(alias) = &event.0.billing
                    && s.models.contains(alias)
                {
                    s.open(project, event.0.clone(), window, cx);
                    s.panes[s.active].update(cx, |pane, cx| pane.action(cx, DeskAction::NewDraft));
                } else {
                    s.message = Some("Configure this billing route in Settings first.".into());
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe(&pane, |s, _, _: &CatalogChanged, cx| {
            s.refresh_palette_search(cx);
            cx.notify();
        })
        .detach();
        pane
    }
    fn switch(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.close_drawer(window, cx);
        if self.preferences.take().is_some() {
            self.before_settings = None;
            self.panes[self.active].update(cx, |p, cx| p.set_front(true, cx));
            cx.notify();
        }
        if self.active == index {
            return;
        }
        self.panes[self.active].update(cx, |p, cx| p.set_front(false, cx));
        self.active = index;
        self.choose_route = None;
        self.search(self.filter.read(cx).value().trim().to_owned(), cx);
        self.panes[index].update(cx, |p, cx| p.set_front(true, cx));
        let p = self.panes[index].read(cx);
        self.recent.remember(&p.project, &p.route);
        if let Err(err) = LastWorkspace::save(&p.project, &p.route) {
            self.message = Some(err.to_string());
        }
        cx.notify();
    }
    fn open(
        &mut self,
        project: PathBuf,
        route: Route,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.restore_project(&project, window, cx);
        let i = self
            .panes
            .iter()
            .position(|p| {
                let p = p.read(cx);
                p.project == project && p.route == route
            })
            .unwrap_or_else(|| {
                self.panes
                    .push(Self::pane(project, route, None, self.stats, window, cx));
                self.panes.len() - 1
            });
        self.switch(i, window, cx);
    }
    fn pick_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let choice = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |s, cx| {
            let result = choice.await;
            let _ = s.update_in(cx, |s, window, cx| match result {
                Ok(Ok(Some(paths))) if !paths.is_empty() => match paths[0].canonicalize() {
                    Ok(project) => {
                        let route = s.recent.route(&project);
                        s.open(project, route, window, cx);
                    }
                    Err(err) => {
                        s.message = Some(err.to_string());
                        cx.notify();
                    }
                },
                Ok(Err(err)) => {
                    s.message = Some(err.to_string());
                    cx.notify();
                }
                _ => (),
            });
        })
        .detach();
    }
    /// Status filters as chips. Each chip toggles the same `is:` token the search box accepts.
    fn filter_chips(&self, p: Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        const CHIPS: [(&str, &str, &str); 3] = [
            ("filter-running", "is:running", "Running"),
            ("filter-waiting", "is:waiting", "Needs you"),
            ("filter-finished", "is:finished", "Finished"),
        ];
        let current = self.filter.read(cx).value().to_string();
        let mut chips = crate::kit::row().gap_1().px_2().flex_wrap();
        for (id, token, label) in CHIPS {
            let filter = self.filter.clone();
            let active = current.split_whitespace().any(|word| word == token);
            chips = chips.child(crate::kit::button(
                p,
                id,
                label,
                active,
                true,
                move |_, window, cx| {
                    let value = filter.read(cx).value().to_string();
                    let next = toggle_status_filter(&value, token);
                    filter.update(cx, |field, cx| field.set_value(next, window, cx));
                },
            ));
        }
        chips
    }
    fn search(&mut self, query: String, cx: &mut Context<Self>) {
        for pane in &self.panes {
            if pane.read(cx).query != query {
                pane.update(cx, |p, cx| {
                    p.query = query.clone();
                    p.action(cx, DeskAction::SearchThreads(query.clone()));
                });
            }
        }
        cx.notify();
    }
    fn open_catalog(
        &mut self,
        project: PathBuf,
        row: ThreadRow,
        route: Route,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open(project, route, window, cx);
        self.choose_route = None;
        self.panes[self.active].update(cx, |pane, cx| pane.select_catalog(row, false, cx));
        cx.notify();
    }
    fn project_row_state(
        &self,
        project: &std::path::Path,
        t: &ThreadRow,
        scope: Scope,
        cx: &App,
    ) -> (Option<Route>, Option<&'static str>) {
        let hosted = self.discovery.route_status(project, scope, t);
        let mut status = hosted
            .as_ref()
            .and_then(|h| h.1)
            .or_else(|| t.status.as_ref().and_then(|s| s.label()));
        let mut owner = hosted.map(|h| h.0);
        for pane in &self.panes {
            let view = pane.read(cx);
            if view.project != project || !scope.matches(&view.route) {
                continue;
            }
            let Some(store) = &view.store else { continue };
            if store.profile().provider != t.model_provider {
                continue;
            }
            if store
                .asks()
                .iter()
                .any(|a| a.thread.as_ref() == Some(&t.id))
            {
                status = Some("Needs you");
            }
            if let Some(thread) = store.transcript(&t.id) {
                if thread.is_ready() {
                    owner = Some(view.route.clone());
                }
                status = status.or_else(|| {
                    if thread.active_turn().is_some() {
                        Some("Running")
                    } else if thread.error().is_some() {
                        Some("Error")
                    } else if thread.unread() {
                        Some("Unread")
                    } else {
                        None
                    }
                });
            }
        }
        (
            owner.or_else(|| scope.saved_route(t, &self.models, self.api_provider.as_deref())),
            status,
        )
    }

    fn activate_row(
        &mut self,
        project: PathBuf,
        scope: Scope,
        row: ThreadRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !project.is_dir() {
            self.message = Some(format!("Project unavailable: {}", project.display()));
            cx.notify();
            return;
        }
        let (route, _) = self.project_row_state(&project, &row, scope, cx);
        if let Some(route) = route {
            self.open_catalog(project, row, route, window, cx);
        } else if (row.needs_claude_alias()
            || (scope.api && self.api_provider.as_deref() == Some(&row.model_provider)))
            && !self.models.is_empty()
        {
            self.choose_route = Some((project, row));
            self.reveal_sidebar(window, cx);
            cx.notify();
        } else {
            self.reveal_sidebar(window, cx);
            self.message = Some(format!(
                "Configure the saved provider {} in Settings to open this thread.",
                row.model_provider
            ));
            cx.notify();
        }
    }

    fn navigate(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers != Modifiers::default() {
            return;
        }
        let selected = self.panes[self.active]
            .read(cx)
            .store
            .as_ref()
            .and_then(|s| s.selected());
        let current = selected.and_then(|id| {
            self.catalog.iter().position(|entry| {
                self.located_entry(*entry, cx)
                    .is_some_and(|(r, project, scope)| {
                        r.id == id
                            && project == self.panes[self.active].read(cx).project
                            && scope.matches(&self.panes[self.active].read(cx).route)
                    })
            })
        });
        let rows = self
            .catalog
            .iter()
            .enumerate()
            .filter_map(|(i, e)| Some((i, self.located_entry(*e, cx)?)));
        let target = match event.keystroke.key.as_str() {
            "home" => rows.into_iter().next(),
            "end" => rows.into_iter().next_back(),
            "up" => rows.rev().find(|(i, _)| current.is_none_or(|pos| *i < pos)),
            "down" => rows
                .into_iter()
                .find(|(i, _)| current.is_none_or(|pos| *i > pos)),
            _ => return,
        };
        if let Some((index, (row, project, scope))) = target {
            self.activate_row(project.to_owned(), scope, row.clone(), window, cx);
            self.list_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
        cx.stop_propagation();
    }

    fn rows(
        &mut self,
        range: Range<usize>,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let p = Palette::get(cx);
        let active = self.panes[self.active].read(cx);
        let mut measured = 0;
        let mut rows = Vec::with_capacity(range.len());
        let now = chrono::Local::now();
        for &entry in self.catalog.get(range.clone()).unwrap_or_default() {
            let (depth, timestamp) = match entry {
                CatalogEntry::Thread {
                    ordinal,
                    depth,
                    timestamp,
                    ..
                } => {
                    if self.stats {
                        measured += 1;
                        self.furthest_row = self.furthest_row.max(ordinal + 1);
                    }
                    (depth, timestamp)
                }
                CatalogEntry::Day(day) => {
                    rows.push(
                        div()
                            .h(px(39.))
                            .flex()
                            .items_end()
                            .px_3()
                            .pb_1()
                            .gap_2()
                            .child(div().w(px(13.)).h(px(2.)).mb(px(7.)).bg(rgb(p.accent)))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(13.))
                                    .child(day_label(day, now.date_naive())),
                            )
                            .child(div().flex_1().h(px(1.)).mb(px(7.)).bg(rgb(p.line)))
                            .into_any_element(),
                    );
                    continue;
                }
            };
            let Some((t, project, scope)) = self.located_entry(entry, cx) else {
                continue;
            };
            let (route, status) = self.project_row_state(project, t, scope, cx);
            let api = scope.api;
            let badge = {
                route.as_ref().map_or_else(
                    || match (&t.model, api) {
                        (None, true) => "OX API · saved model unknown".into(),
                        (Some(model), _) => format!("{} · {model} · unavailable", t.model_provider),
                        _ => format!("{} · unavailable", t.model_provider),
                    },
                    |route| {
                        self.panes
                            .iter()
                            .map(|pane| pane.read(cx))
                            .filter(|view| &view.route == route && view.project == project)
                            .filter_map(|view| view.store.as_ref())
                            .find(|store| store.transcript(&t.id).is_some_and(|t| t.is_ready()))
                            .map_or_else(
                                || route.label(),
                                |store| store.thread_route(Some(&t.id)).label(),
                            )
                    },
                )
            };
            let row = t.clone();
            let project = project.to_owned();
            let cartridge = if depth > 0 || t.worker {
                activity::Cartridge::Worker
            } else if api {
                activity::Cartridge::Api
            } else if scope.runtime == ox_desk_core::runtime::Runtime::Claude {
                activity::Cartridge::Claude
            } else {
                activity::Cartridge::Codex
            };
            let model = crate::kit::ListRow {
                cartridge,
                title: if depth > 0 {
                    format!("↳ {}", t.title())
                } else {
                    t.title().into()
                },
                route: if !t.worker && t.parent_thread_id.is_some() {
                    format!("Subagent · {badge}")
                } else {
                    format!(
                        "{} · {badge}",
                        project.file_name().unwrap_or_default().to_string_lossy()
                    )
                },
                age: age(now.timestamp(), timestamp),
                status: status.or(t.unread.then_some("Unread")),
                selected: active.project == project
                    && scope.matches(&active.route)
                    && active.store.as_ref().and_then(|s| s.selected()) == Some(&t.id),
            };
            rows.push(
                model
                    .render(
                        p,
                        SharedString::from(format!(
                            "thread-{}-{}-{}",
                            project.display(),
                            scope.api,
                            t.id
                        )),
                        cx.listener(move |s, _, window, cx| {
                            window.focus(&s.list_focus, cx);
                            s.activate_row(project.clone(), scope, row.clone(), window, cx);
                        }),
                    )
                    .w(width)
                    .pl(px(6. + depth.min(4) as f32 * 12.))
                    .into_any_element(),
            );
        }
        if self.stats {
            self.list_rows += measured as u64;
            self.max_range = self.max_range.max(measured);
        }
        if range.end > 0 && range.end + 3 >= self.catalog.len() {
            for &(i, _) in &self.catalog_versions {
                let pane = self.panes[i].clone();
                if pane.read(cx).store.as_ref().is_some_and(|s| s.has_more()) {
                    cx.defer(move |cx| {
                        pane.update(cx, |p, cx| {
                            if p.store.as_ref().is_some_and(|s| s.has_more()) {
                                p.action(cx, DeskAction::MoreThreads);
                            }
                        })
                    });
                }
            }
        }
        rows
    }
}
impl Render for Host {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::app::guard(cx, |cx| self.draw(window, cx).into_any_element())
    }
}

impl Host {
    fn draw(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.remember_layout(window, cx);
        let p = Palette::get(cx);
        if self.stats && self._probe.is_none() {
            eprintln!("desk-stats {}", self.snapshot(window, cx));
        }
        self.index_catalog(cx);
        let compact = self.compact(window);
        if !compact {
            self.close_drawer(window, cx);
        }
        let inline = !compact && self.sidebar_visible;
        let pane_width = window.viewport_size().width
            - if inline {
                px(self.sidebar_width as f32 + DIVIDER_WIDTH)
            } else {
                px(0.)
            };
        if self.panes[self.active].read(cx).pane_width != pane_width {
            self.panes[self.active].update(cx, |pane, cx| {
                pane.pane_width = pane_width;
                cx.notify();
            });
        }
        let pane = self.panes[self.active].read(cx);
        let project = pane.project.clone();
        let saved_route = pane.route.clone();
        let current = saved_route.label();
        let hot = pane
            .store
            .as_ref()
            .and_then(|s| s.transcript(s.selected()?))
            .and_then(|t| t.access())
            .is_some_and(ox_desk_core::runtime::Access::bypasses_permissions);
        let pane_ready = pane
            .store
            .as_ref()
            .is_some_and(|s| s.state() == &Connection::Ready);
        let count = self.catalog.len();
        let models = self.models.clone();
        let favorites = self.favorite_models.clone();
        let host = cx.weak_entity();
        let directory = project.clone();
        let route = button(
            p,
            "billing-route",
            format!("{current}  ▾"),
            false,
            true,
            |_, _, _| {},
        )
        .ghost()
        .border_0()
        .justify_start()
        .h(px(26.))
        .w_full()
        .dropdown_menu(move |mut menu, _, _| {
            for route in ox_desk_core::runtime::Runtime::routes(&models) {
                let host = host.clone();
                let directory = directory.clone();
                let starred =
                    matches!(&route.billing, Billing::Api(alias) if favorites.contains(alias));
                let label = route.label();
                menu = menu.item(
                    PopupMenuItem::element(move |_, _| {
                        crate::kit::row()
                            .w_full()
                            .gap_1()
                            .child(crate::kit::star_column(p, starred))
                            .child(label.clone())
                    })
                    .checked(route == saved_route)
                    .on_click(move |_, window, cx| {
                        let _ = host.update(cx, |s, cx| {
                            s.open(directory.clone(), route.clone(), window, cx)
                        });
                    }),
                );
            }
            menu
        });
        let label = project
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let host = cx.weak_entity();
        let projects: Vec<_> = self
            .panes
            .iter()
            .enumerate()
            .map(|(i, pane)| {
                let pane = pane.read(cx);
                (
                    i,
                    pane.project.display().to_string(),
                    pane.route.label(),
                    pane.store.as_ref().map_or(0, |s| s.asks().len()),
                )
            })
            .collect();
        let recent: Vec<_> = self
            .recent
            .recent
            .iter()
            .filter(|r| {
                !self
                    .panes
                    .iter()
                    .any(|pane| pane.read(cx).project == r.project)
            })
            .map(|r| (r.project.clone(), self.recent.route(&r.project)))
            .collect();
        let picker = button(
            p,
            "project-picker",
            format!("▱  {label}  ▾"),
            false,
            true,
            |_, _, _| {},
        )
        .ghost()
        .border_0()
        .justify_start()
        .h(px(26.))
        .w_full()
        .dropdown_menu(move |mut menu, _, _| {
            for (index, path, route, asks) in &projects {
                let index = *index;
                let host = host.clone();
                let label = if *asks > 0 {
                    format!("{path} · {route} · {asks} waiting")
                } else {
                    format!("{path} · {route}")
                };
                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                    let _ = host.update(cx, |s, cx| s.switch(index, window, cx));
                }));
            }
            for (directory, route) in &recent {
                let host = host.clone();
                let directory = directory.clone();
                let route = route.clone();
                menu = menu.item(
                    PopupMenuItem::new(directory.display().to_string()).on_click(
                        move |_, window, cx| {
                            let _ = host.update(cx, |s, cx| match directory.canonicalize() {
                                Ok(project) if project.is_dir() => {
                                    s.open(project, route.clone(), window, cx)
                                }
                                _ => {
                                    s.message = Some(format!(
                                        "Project unavailable: {}",
                                        directory.display()
                                    ));
                                    cx.notify();
                                }
                            });
                        },
                    ),
                );
            }
            let host = host.clone();
            menu.item(
                PopupMenuItem::new("Open project…").on_click(move |_, window, cx| {
                    let _ = host.update(cx, |s, cx| s.pick_project(window, cx));
                }),
            )
        });
        let mut background = Vec::new();
        let mut background_waiting = 0;
        for (index, other) in self
            .panes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.active)
        {
            let other = other.read(cx);
            let waiting = other.store.as_ref().map_or(0, |s| s.asks().len());
            if waiting > 0 {
                background_waiting += waiting;
                background.push(button(
                    p,
                    ("waiting", index),
                    format!(
                        "{waiting} waiting · {}",
                        other
                            .project
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                    ),
                    false,
                    true,
                    cx.listener(move |s, _, window, cx| s.switch(index, window, cx)),
                ));
            }
        }
        let threads_label = if background_waiting > 0 {
            format!("Threads · {background_waiting} waiting")
        } else {
            "Threads".into()
        };
        let sidebar_width =
            px(self.sidebar_width as f32).min(window.viewport_size().width - px(36.));
        let sidebar = col()
            .w(sidebar_width)
            .flex_shrink_0()
            .h_full()
            .bg(rgb(p.nav))
            .gap_2()
            .child(
                row()
                    .relative()
                    .overflow_hidden()
                    .h(px(62.))
                    .flex_shrink_0()
                    .p_3()
                    .gap_2()
                    .justify_between()
                    .child(activity::horizon(p))
                    .child(row().gap_2().child(activity::marquee(p, hot, false)))
                    .child(self.palette_button(cx))
                    .when(compact, |bar| {
                        bar.child(crate::kit::icon_button(
                            p,
                            "close-threads",
                            "×",
                            "Close threads",
                            cx.listener(|s, _, w, cx| s.close_drawer(w, cx)),
                        ))
                    }),
            )
            .child(
                col()
                    .mx_2()
                    .px_1()
                    .border_l_2()
                    .border_color(rgb(p.line))
                    .child(picker)
                    .child(route),
            )
            .child(
                div().px_2().child(
                    button(
                        p,
                        "new-session",
                        "▶   New session",
                        true,
                        pane_ready,
                        cx.listener(|s, _, window, cx| {
                            s.close_drawer(window, cx);
                            s.panes[s.active]
                                .update(cx, |p, cx| p.action(cx, DeskAction::NewDraft));
                            cx.notify();
                        }),
                    )
                    .w_full(),
                ),
            )
            .child(
                div()
                    .px_2()
                    .child(gpui_component::input::Input::new(&self.filter)),
            )
            .child(self.filter_chips(p, cx))
            .when_some(self.choose_route.clone(), |sidebar, (project, row)| {
                let host = cx.weak_entity();
                let aliases = self.models.clone();
                sidebar.child(
                    button(
                        p,
                        "select-saved-model",
                        if row.needs_claude_alias() {
                            "Choose the billing alias to resume"
                        } else {
                            "Start new API session"
                        },
                        true,
                        true,
                        |_, _, _| {},
                    )
                    .dropdown_menu(move |mut menu, _, _| {
                        for alias in &aliases {
                            let project = project.clone();
                            let resume = row.needs_claude_alias();
                            let route = if resume {
                                ox_desk_core::runtime::Runtime::Claude.route(Some(alias.clone()))
                            } else {
                                Route::codex_api(alias.clone())
                            };
                            let row = row.clone();
                            let host = host.clone();
                            let label = if resume {
                                format!("Resume using {alias}")
                            } else {
                                format!("New session · {alias}")
                            };
                            menu = menu.item(PopupMenuItem::new(label).on_click(
                                move |_, window, cx| {
                                    let _ = host.update(cx, |s, cx| {
                                        let project = project.clone();
                                        if resume {
                                            s.open(project, route.clone(), window, cx);
                                            s.choose_route = None;
                                            s.panes[s.active].update(cx, |pane, cx| {
                                                pane.select_catalog(row.clone(), true, cx)
                                            });
                                            return;
                                        }
                                        s.open(project, route.clone(), window, cx);
                                        s.panes[s.active].update(cx, |pane, cx| {
                                            pane.action(cx, DeskAction::NewDraft)
                                        });
                                    });
                                },
                            ));
                        }
                        menu
                    }),
                )
            })
            .children(background)
            .child(
                div()
                    .id("thread-navigation")
                    .track_focus(&self.list_focus)
                    .on_key_down(cx.listener(Self::navigate))
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "project-threads",
                            count,
                            cx.processor(move |s, range, _, cx| {
                                crate::app::try_draw(cx, |cx| s.rows(range, sidebar_width, cx))
                                    .unwrap_or_default()
                            }),
                        )
                        .track_scroll(&self.list_scroll)
                        .flex_1()
                        .min_h_0(),
                    ),
            )
            .child(
                div().px_2().border_t_1().border_color(rgb(p.line)).child(
                    button(
                        p,
                        "settings",
                        if self.preferences.is_some() {
                            "Back to thread"
                        } else {
                            "Settings"
                        },
                        false,
                        true,
                        cx.listener(|s, _, w, cx| s.show_settings(w, cx)),
                    )
                    .w_full(),
                ),
            )
            .when_some(self.message.clone(), |d, m| {
                d.child(crate::kit::banner(
                    p,
                    "host-error",
                    "host-error-dismiss",
                    m,
                    cx.listener(|s, _, _, cx| {
                        s.message = None;
                        cx.notify();
                    }),
                ))
            });
        let (side, drawer) = if inline {
            (Some(sidebar), None)
        } else if self.drawer_open {
            let drawer = div()
                .absolute()
                .inset_0()
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .bg(black().opacity(0.48))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|s, _, w, cx| s.close_drawer(w, cx)),
                        ),
                )
                .child(
                    sidebar
                        .occlude()
                        .focus_trap("thread-drawer", &self.drawer_focus),
                );
            (None, Some(drawer))
        } else {
            (None, None)
        };
        row()
            .relative()
            .size_full()
            .bg(rgb(p.bg))
            .text_color(rgb(p.fg))
            .on_mouse_move(cx.listener(|s, e: &MouseMoveEvent, w, cx| {
                if s.resizing_sidebar {
                    if e.pressed_button == Some(MouseButton::Left) {
                        let limit =
                            (f32::from(w.viewport_size().width) - MIN_THREAD_WIDTH - DIVIDER_WIDTH)
                                .clamp(190., 540.);
                        s.sidebar_width = f32::from(e.position.x).clamp(190., limit) as u32;
                    } else {
                        s.resizing_sidebar = false
                    }
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|s, _, _, cx| {
                    if s.resizing_sidebar {
                        s.resizing_sidebar = false;
                        cx.notify()
                    }
                }),
            )
            .when_some(side, |layout, side| {
                layout.child(side).child(self.divider(p, cx))
            })
            .child(
                col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .when(!inline, |pane| {
                        pane.child(
                            row()
                                .relative()
                                .overflow_hidden()
                                .px_2()
                                .py_1()
                                .gap_2()
                                .bg(rgb(p.nav))
                                .child(activity::horizon(p))
                                .child(activity::marquee(p, hot, true))
                                .child(button(
                                    p,
                                    "show-threads",
                                    threads_label,
                                    false,
                                    true,
                                    cx.listener(|s, _, w, cx| s.toggle_sidebar(w, cx)),
                                ))
                                .child(div().child(self.palette_button(cx))),
                        )
                    })
                    .child(
                        col()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .child(match &self.preferences {
                                Some(view) => view.clone().into_any_element(),
                                None => self.panes[self.active].clone().into_any_element(),
                            }),
                    ),
            )
            .children(drawer)
            .children(self.palette_overlay(cx))
    }
}

#[cfg(test)]
mod responsive_tests {
    use super::{needs_drawer, px};

    #[test]
    fn drawer_reserves_conversation_space_at_saved_sidebar_width() {
        assert!(needs_drawer(px(717.), 254));
        assert!(!needs_drawer(px(859.), 254));
        assert!(needs_drawer(px(1000.), 540));
        assert!(!needs_drawer(px(1280.), 540));
    }
}

/// Turns one `is:` status token on or off. Only one status filter applies at a time.
fn toggle_status_filter(value: &str, token: &str) -> String {
    let words: Vec<&str> = value.split_whitespace().collect();
    if words.contains(&token) {
        return words
            .into_iter()
            .filter(|word| *word != token)
            .collect::<Vec<_>>()
            .join(" ");
    }
    let mut next: Vec<&str> = words
        .into_iter()
        .filter(|word| !word.starts_with("is:"))
        .collect();
    next.push(token);
    next.join(" ")
}

#[cfg(test)]
mod tests {
    use super::toggle_status_filter;

    #[test]
    fn status_chips_toggle_one_filter_and_keep_text() {
        assert_eq!(toggle_status_filter("", "is:running"), "is:running");
        assert_eq!(
            toggle_status_filter("fix bug", "is:waiting"),
            "fix bug is:waiting"
        );
        assert_eq!(
            toggle_status_filter("is:running fix", "is:waiting"),
            "fix is:waiting"
        );
        assert_eq!(toggle_status_filter("is:running fix", "is:running"), "fix");
    }
}

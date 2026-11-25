//! One workbench per canonical project and billing route. Each owns its editors,
//! server requests and connection for the lifetime of the window.
use crate::appearance::Step;
mod discovery;
pub(super) use discovery::TreeIndex;
use discovery::{Discovery, Location, Scope};
pub(crate) mod layout;
mod palette;
mod shortcuts;
use super::model_picker::{self, ModelPicker, ModelPickerEvent};
use super::running::Running;
use super::workbench::{CatalogChanged, Workbench};
use crate::kit::{DropdownMenu, PopupMenuItem, SPACE_M, SPACE_S, SPACE_XS};
use crate::{
    activity, appearance::Palette, col, kit::button, platform, row, settings, small,
    workspace_state::LastWorkspace,
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::FocusTrapElement;
use ox_desk_core::actions::Action as DeskAction;
use ox_desk_core::runtime::{Billing, Runtime};
use ox_desk_core::store::{Connection, Route, ThreadRow};
use std::{ops::Range, path::PathBuf, time::Instant};

#[derive(Clone, Copy)]
enum CatalogEntry {
    /// A day group header: its root-thread count and whether it is folded.
    Day {
        day: Option<chrono::NaiveDate>,
        count: usize,
        folded: bool,
    },
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

/// Whether a new session can start on `route` now, or the one next step to show instead.
/// An API route needs its model alias to be configured. The ChatGPT plan needs a signed-in
/// account. The Claude plan is checked by the session itself, which asks to sign in.
fn new_route_gate(
    route: &Route,
    models: &[String],
    chatgpt_account: bool,
) -> Result<(), &'static str> {
    use ox_desk_core::runtime::Runtime;
    match &route.billing {
        Billing::Api(alias) if models.contains(alias) => Ok(()),
        Billing::Api(_) => Err("Add this model in Settings → Models first."),
        Billing::Subscription if route.runtime == Runtime::Codex && !chatgpt_account => {
            Err("Sign in to ChatGPT in Settings → Accounts first.")
        }
        Billing::Subscription => Ok(()),
    }
}

pub struct Host {
    palette: Option<palette::State>,
    _palette_keys: Subscription,
    /// The searchable all-models picker, opened from the route menu.
    model_picker: Option<(Entity<ModelPicker>, Subscription)>,
    /// The open picker is the AGENT picker (the composer's choices), not the all-models list.
    agent_picker: bool,
    /// Runtime the all-models picker routes to. Claude Code is offered only when aliases exist,
    /// as the route menu's Claude alias routes were.
    model_runtime: Runtime,
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
    running: Entity<Running>,
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
    /// Set when first-run onboarding opened Providers: models known then; a newly saved one becomes the route.
    provider_onboarding: Option<Vec<String>>,
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
            .or_else(|| recent.saved_route(&project))
            .unwrap_or_else(super::onboarding::first_launch_route);
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
            model_picker: None,
            agent_picker: false,
            model_runtime: Runtime::Codex,
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
            running: cx.new(Running::new),
            list_focus: cx.focus_handle().tab_stop(true),
            list_scroll: Default::default(),
            choose_route: None,
            stats,
            message,
            provider_onboarding: None,
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
                // Input runs outside the Host update: a click handler updates Host itself.
                if let Some(input) = &sample.input {
                    let reply = cx
                        .update(|window, cx| Self::probe_input(input, window, cx))
                        .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
                    let _ = sample.reply.send(reply);
                    // A hidden window gets no frame callbacks, so lay out every window now:
                    // the control registry and hit testing then reflect what the input did.
                    let _ = cx.update(|window, cx| {
                        {
                            let pending = window.draw(cx);
                            pending.clear(cx);
                        }
                        let main = window.window_handle().window_id();
                        for other in cx.windows() {
                            if other.window_id() != main {
                                let _ = other.update(cx, |_, window, cx| {
                                    let pending = window.draw(cx);
                                    pending.clear(cx);
                                });
                            }
                        }
                    });
                    continue;
                }
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
    /// Runs a probe `press`/`key` in-process and returns `{"ok":true}` or `{"error":...}`.
    fn probe_input(input: &platform::probe::Input, window: &mut Window, cx: &mut App) -> String {
        use gpui::{
            Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput,
        };
        let result = match input {
            platform::probe::Input::Press(query) => {
                platform::layout_probe::find(query).map(|(target, position)| {
                    let click = move |window: &mut Window, cx: &mut App| {
                        let modifiers = Modifiers::default();
                        window.dispatch_event(
                            PlatformInput::MouseMove(MouseMoveEvent {
                                position,
                                pressed_button: None,
                                modifiers,
                            }),
                            cx,
                        );
                        window.dispatch_event(
                            PlatformInput::MouseDown(MouseDownEvent {
                                button: MouseButton::Left,
                                position,
                                modifiers,
                                click_count: 1,
                                first_mouse: false,
                            }),
                            cx,
                        );
                        window.dispatch_event(
                            PlatformInput::MouseUp(MouseUpEvent {
                                button: MouseButton::Left,
                                position,
                                modifiers,
                                click_count: 1,
                            }),
                            cx,
                        );
                    };
                    if window.window_handle().window_id() == target {
                        click(window, cx);
                    } else if let Some(other) =
                        cx.windows().into_iter().find(|w| w.window_id() == target)
                    {
                        let _ = other.update(cx, |_, window, cx| click(window, cx));
                    }
                })
            }
            platform::probe::Input::Key(key) => {
                // Test-only path: the leak is one short string per probe command.
                match gpui::Keystroke::parse(Box::leak(key.clone().into_boxed_str())) {
                    Ok(keystroke) => {
                        let active = cx.active_window();
                        match active {
                            Some(other)
                                if other.window_id() != window.window_handle().window_id() =>
                            {
                                let _ = other.update(cx, |_, window, cx| {
                                    window.dispatch_keystroke(keystroke, cx)
                                });
                            }
                            _ => {
                                window.dispatch_keystroke(keystroke, cx);
                            }
                        }
                        Ok(())
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
        };
        match result {
            Ok(()) => serde_json::json!({"ok": true}),
            Err(error) => serde_json::json!({"error": error}),
        }
        .to_string()
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
    /// Open the searchable picker over every model. Settings lists only starred discovered
    /// models, so the full catalog comes from the same request the Favorites page refreshes with.
    fn open_all_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.panes[self.active].read(cx).route.runtime;
        self.model_runtime = if active == Runtime::Claude && self.models.is_empty() {
            Runtime::Codex
        } else {
            active
        };
        let entries: Vec<_> = self
            .models
            .iter()
            .map(|id| model_picker::api_entry(id))
            .collect();
        let picker = cx.new(|cx| ModelPicker::new(entries, window, cx));
        let subscription = cx.subscribe_in(
            &picker,
            window,
            |s: &mut Host, _, event: &ModelPickerEvent, window, cx| match event {
                ModelPickerEvent::Choose(alias) => {
                    s.model_picker = None;
                    let project = s.panes[s.active].read(cx).project.clone();
                    let route = s.model_runtime.route(Some(alias.clone()));
                    s.open(project, route, window, cx);
                }
                ModelPickerEvent::Starred(favorites) => s.favorite_models = favorites.clone(),
                ModelPickerEvent::HandOff(_) => {}
                ModelPickerEvent::AddProvider => {
                    s.model_picker = None;
                    let pane = s.panes[s.active].clone();
                    s.open_providers(&pane, window, cx);
                }
                ModelPickerEvent::Close => s.model_picker = None,
            },
        );
        self.agent_picker = false;
        self.model_picker = Some((picker.clone(), subscription));
        picker.update(cx, |p, cx| p.set_loading(true, cx));
        let task = cx.background_spawn(async { settings::discover_models() });
        cx.spawn_in(window, async move |_, cx| {
            let result = task.await;
            picker.update(cx, |p, cx| {
                match result {
                    Ok(ids) => p.set_entries(
                        ids.iter().map(|id| model_picker::api_entry(id)).collect(),
                        cx,
                    ),
                    Err(error) => p.set_notice(Some(error), cx),
                }
                p.set_loading(false, cx);
            });
        })
        .detach();
        cx.notify();
    }
    /// The AGENT picker: the composer's model choices, shown over the window.
    fn open_agent_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model_picker.take().is_some() && self.agent_picker {
            self.agent_picker = false;
            cx.notify();
            return;
        }
        let pane = self.panes[self.active].clone();
        let (entries, runtime, owner) = {
            let workbench = pane.read(cx);
            let Some(store) = &workbench.store else {
                return self.open_all_models(window, cx);
            };
            let owner = store.selected().map(str::to_owned);
            (
                super::route_chip::composer_entries(store, owner.as_deref()),
                store.profile().route.runtime,
                owner,
            )
        };
        let picker = cx.new(|cx| {
            ModelPicker::new(entries, window, cx)
                .with_runtime(runtime)
                .with_discovered_new_session()
        });
        let subscription = cx.subscribe_in(
            &picker,
            window,
            move |s: &mut Host, picker, event: &ModelPickerEvent, window, cx| match event {
                ModelPickerEvent::Choose(id) => {
                    let runtime = picker.read(cx).runtime();
                    s.model_picker = None;
                    s.agent_picker = false;
                    let id = id.clone();
                    let owner = owner.clone();
                    s.panes[s.active]
                        .update(cx, |pane, cx| pane.choose_model(owner, id, runtime, cx));
                    cx.notify();
                }
                ModelPickerEvent::Starred(favorites) => s.favorite_models = favorites.clone(),
                ModelPickerEvent::HandOff(_) => {}
                ModelPickerEvent::AddProvider => {
                    s.model_picker = None;
                    s.agent_picker = false;
                    let pane = s.panes[s.active].clone();
                    s.open_providers(&pane, window, cx);
                }
                ModelPickerEvent::Close => {
                    s.model_picker = None;
                    s.agent_picker = false;
                    cx.notify();
                }
            },
        );
        self.agent_picker = true;
        self.model_picker = Some((picker, subscription));
        cx.notify();
    }
    fn model_overlay(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (picker, _) = self.model_picker.as_ref()?;
        let p = Palette::get(cx);
        let runtime = self.model_runtime;
        let mut runtimes = div().flex().gap_1().child(button(
            p,
            "model-runtime-codex",
            Runtime::Codex.name(),
            runtime == Runtime::Codex,
            true,
            cx.listener(|s, _, _, cx| {
                s.model_runtime = Runtime::Codex;
                cx.notify();
            }),
        ));
        if !self.models.is_empty() {
            runtimes = runtimes.child(button(
                p,
                "model-runtime-claude",
                Runtime::Claude.name(),
                runtime == Runtime::Claude,
                true,
                cx.listener(|s, _, _, cx| {
                    s.model_runtime = Runtime::Claude;
                    cx.notify();
                }),
            ));
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .justify_center()
                .pt(px(72.))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|s, _, _, cx| {
                        s.model_picker = None;
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .w(px(440.))
                        .p_2()
                        .bg(rgb(p.panel))
                        .border_1()
                        .border_color(rgb(p.line_strong))
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .children((!self.agent_picker).then_some(runtimes))
                        .child(picker.clone()),
                ),
        )
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
                        if s.provider_onboarding.is_some() && s.preferences.is_none() {
                            let before = s.provider_onboarding.take().unwrap_or_default();
                            if let Some(model) =
                                s.models.iter().find(|m| !before.contains(m)).cloned()
                            {
                                let owner = s.panes[s.active].clone();
                                let (project, route) = {
                                    let o = owner.read(cx);
                                    (o.project.clone(), o.api_route(model))
                                };
                                s.open(project, route, window, cx);
                            }
                        }
                    }
                    Err(error) => s.message = Some(error),
                }
                if window.is_visible() {
                    cx.notify();
                }
            });
        })
        .detach();
        // Any catalog model can be used, starred or not: the discovered aliases follow the
        // settings' own so saved history on an unstarred alias reopens and routes list it.
        let task = cx.background_spawn(async { settings::discover_models() });
        cx.spawn(async move |this, cx| {
            if let Ok(found) = task.await {
                let _ = this.update(cx, |s, cx| {
                    let merged = ox_desk_core::ports::ox::merge_catalog(&s.models, found);
                    if merged != s.models {
                        s.models = merged;
                        cx.notify();
                    }
                });
            }
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
    /// Open Settings on its Remote access page, or move an open Settings there.
    fn show_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preferences.is_none() {
            self.show_settings(window, cx);
        }
        if let Some(view) = &self.preferences {
            view.update(cx, |view, cx| view.open_remote(cx));
        }
    }

    /// The sidebar footer: an icon rail on the keyline, and the one running indicator.
    fn sidebar_footer(
        &self,
        p: Palette,
        running_count: usize,
        running_open: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let settings_label = if self.preferences.is_some() {
            "Back to thread"
        } else {
            "Settings"
        };
        let pane = self.panes[self.active].downgrade();
        let usage = gpui_component::popover::Popover::new("sidebar-usage")
            .anchor(Anchor::BottomLeft)
            .trigger(crate::kit::icon(
                p,
                "sidebar-usage-button",
                gpui_component::IconName::ChartPie,
                "Usage",
                true,
                |_, _, _| {},
            ))
            .content(move |_, window, cx| {
                pane.read_with(cx, |view, _| view.usage_details(p, window, &pane))
                    .unwrap_or_else(|_| div().into_any_element())
            });
        let running = self.running.clone();
        row()
            .flex_shrink_0()
            .h(px(44.))
            .px(px(crate::kit::GUTTER))
            .gap(px(SPACE_XS))
            .child(
                crate::kit::icon(
                    p,
                    "settings",
                    gpui_component::IconName::Settings,
                    settings_label,
                    true,
                    cx.listener(|s, _, w, cx| s.show_settings(w, cx)),
                )
                .when(self.preferences.is_some(), |b| b.text_color(rgb(p.accent))),
            )
            .child(crate::kit::icon(
                p,
                "remote-access",
                gpui_component::IconName::Globe,
                "Remote access",
                true,
                cx.listener(|s, _, w, cx| s.show_remote(w, cx)),
            ))
            .child(usage)
            .child(div().flex_1())
            .when(running_count > 0, |footer| {
                footer.child(
                    crate::kit::nav_row(
                        "running-toggle",
                        format!(
                            "{running_count} running, {}",
                            if running_open { "hide" } else { "show" }
                        ),
                        crate::kit::ICON_SIZE,
                        true,
                        move |_, _, cx| {
                            running.update(cx, |r, cx| {
                                r.open = !r.open;
                                cx.notify();
                            })
                        },
                    )
                    .w_auto()
                    .child(
                        row()
                            .gap(px(SPACE_S))
                            .child(div().size(px(6.)).rounded_full().bg(rgb(p.success)))
                            .child(
                                div()
                                    .text_size(p.text(Step::Meta))
                                    .text_color(rgb(p.muted))
                                    .child(format!("{running_count} running")),
                            )
                            .child(
                                div()
                                    .text_size(p.text(Step::Label))
                                    .text_color(rgb(p.quiet()))
                                    .child(if running_open { "▾" } else { "▴" }),
                            ),
                    ),
                )
            })
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
            |s, _, _: &super::workbench::empty::OpenProject, window, cx| {
                s.pick_project(window, cx);
            },
        )
        .detach();
        cx.subscribe_in(
            &pane,
            window,
            |s, owner, event: &super::onboarding::SwitchRoute, window, cx| {
                let project = owner.read(cx).project.clone();
                let route = match &event.0 {
                    Some(route) => route.clone(),
                    None => match s.models.first() {
                        Some(model) => owner.read(cx).api_route(model.clone()),
                        None => {
                            s.provider_onboarding = Some(s.models.clone());
                            return s.open_providers(owner, window, cx);
                        }
                    },
                };
                s.open(project, route, window, cx);
            },
        )
        .detach();
        cx.subscribe_in(
            &pane,
            window,
            |s, owner, _: &super::onboarding::ConfigureProvider, window, cx| {
                s.provider_onboarding = Some(s.models.clone());
                s.open_providers(owner, window, cx);
            },
        )
        .detach();
        cx.subscribe_in(
            &pane,
            window,
            |s, owner, event: &super::route_chip::NewModelRoute, window, cx| {
                let project = owner.read(cx).project.clone();
                match new_route_gate(&event.0, &s.models, s.native_account) {
                    Ok(()) => {
                        s.open(project, event.0.clone(), window, cx);
                        s.panes[s.active]
                            .update(cx, |pane, cx| pane.action(cx, DeskAction::NewDraft));
                    }
                    Err(message) => {
                        s.message = Some(message.into());
                        cx.notify();
                    }
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &pane,
            window,
            |s, owner, _: &super::route_chip::OpenProviders, window, cx| {
                s.open_providers(owner, window, cx);
            },
        )
        .detach();
        cx.subscribe(&pane, |s, _, _: &CatalogChanged, cx| {
            s.refresh_palette_search(cx);
            cx.notify();
        })
        .detach();
        cx.subscribe_in(
            &pane,
            window,
            |s, owner, event: &super::workbench::OpenThread, window, cx| {
                s.open_thread(owner, event.0.clone(), window, cx);
            },
        )
        .detach();
        pane
    }
    fn open_providers(
        &mut self,
        owner: &Entity<Workbench>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = owner.read(cx).project.clone();
        owner.update(cx, |p, cx| p.set_front(false, cx));
        self.preferences =
            Some(cx.new(|cx| crate::preferences::Preferences::providers(project, window, cx)));
        cx.notify();
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
                Ok(Ok(Some(paths))) if !paths.is_empty() => {
                    let path = paths[0].clone();
                    s.with_project_dir(path, true, window, cx, |s, project, window, cx| {
                        let route = s.recent.route(&project);
                        s.open(project, route, window, cx);
                    });
                }
                Ok(Err(err)) => {
                    s.message = Some(err.to_string());
                    cx.notify();
                }
                _ => (),
            });
        })
        .detach();
    }
    /// Check, off the UI thread, that `path` (canonicalised when asked) is a directory,
    /// then continue with it. A stalled network mount cannot freeze the window this way.
    /// An unavailable project sets the sidebar message instead.
    fn with_project_dir(
        &mut self,
        path: PathBuf,
        canonical: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, PathBuf, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let probe = path.clone();
        let task = cx.background_spawn(async move {
            let resolved = if canonical {
                probe.canonicalize().ok()
            } else {
                Some(probe)
            };
            resolved.filter(|p| p.is_dir())
        });
        cx.spawn_in(window, async move |s, cx| {
            let resolved = task.await;
            let _ = s.update_in(cx, |s, window, cx| match resolved {
                Some(project) => then(s, project, window, cx),
                None => {
                    s.message = Some(format!("Project unavailable: {}", path.display()));
                    cx.notify();
                }
            });
        })
        .detach();
    }
    /// The status filter as one quiet icon menu in the search row. Each choice sets the
    /// same `is:` token the search box accepts; "All" clears it.
    fn filter_menu(&self, p: Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        const CHOICES: [(&str, &str); 4] = [
            ("", "All"),
            ("is:running", "Running"),
            ("is:waiting", "Needs you"),
            ("is:finished", "Finished"),
        ];
        let current = self.filter.read(cx).value().to_string();
        let active = CHOICES
            .iter()
            .skip(1)
            .find(|(token, _)| current.split_whitespace().any(|word| word == *token))
            .copied();
        let label = match active {
            Some((_, name)) => format!("Filter threads: {name}"),
            None => "Filter threads".to_owned(),
        };
        let filter = self.filter.clone();
        crate::kit::icon(
            p,
            "thread-filter",
            gpui_component::IconName::Settings2,
            label,
            true,
            |_, _, _| {},
        )
        .size(px(24.))
        .when(active.is_some(), |b| b.text_color(rgb(p.accent)))
        .dropdown_menu(move |mut menu, _, _| {
            for (token, name) in CHOICES {
                let filter = filter.clone();
                let checked = match active {
                    Some((current, _)) => current == token,
                    None => token.is_empty(),
                };
                menu = menu.item(PopupMenuItem::new(name).checked(checked).on_click(
                    move |_, window, cx| {
                        let value = filter.read(cx).value().to_string();
                        let next = select_status_filter(&value, token);
                        filter.update(cx, |field, cx| field.set_value(next, window, cx));
                    },
                ));
            }
            menu
        })
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
            if store.is_native_live(&t.id) {
                // Another client is driving it: not an error, not ended, not ours.
                status = Some(if store.native_app() == "Codex" {
                    "Live in Codex"
                } else {
                    "Live in Claude Code"
                });
            } else if store.is_ended(&t.id) {
                status = status.or(Some("Ended"));
            }
            if let Some(thread) = store.transcript(&t.id) {
                if thread.is_ready() {
                    owner = Some(view.route.clone());
                }
                status = status.or_else(|| {
                    if store.is_native_live(&t.id) {
                        None
                    } else if thread.active_turn().is_some() {
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
        self.with_project_dir(project, false, window, cx, move |s, project, window, cx| {
            let (route, _) = s.project_row_state(&project, &row, scope, cx);
            if let Some(route) = route {
                s.open_catalog(project, row, route, window, cx);
            } else if (row.needs_claude_alias()
                || (scope.api && s.api_provider.as_deref() == Some(&row.model_provider)))
                && !s.models.is_empty()
            {
                s.choose_route = Some((project, row));
                s.reveal_sidebar(window, cx);
                cx.notify();
            } else {
                s.reveal_sidebar(window, cx);
                s.message = Some(format!(
                    "Set up {} in Settings to open this thread.",
                    row.model_provider
                ));
                cx.notify();
            }
        });
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
                CatalogEntry::Day { day, count, folded } => {
                    let title = day_label(day, now.date_naive());
                    let label = format!(
                        "{title}, {count} {}",
                        if count == 1 { "thread" } else { "threads" }
                    );
                    rows.push(
                        div()
                            .w(width)
                            .h(px(crate::kit::ROW_HEIGHT))
                            .px(px(crate::kit::GUTTER))
                            .flex()
                            .flex_col()
                            .justify_end()
                            .child(
                                crate::kit::nav_row(
                                    SharedString::from(format!("day-{title}")),
                                    label,
                                    24.,
                                    true,
                                    cx.listener(move |s, _, _, cx| {
                                        s.discovery.toggle_day(day);
                                        cx.notify();
                                    }),
                                )
                                .child(
                                    row()
                                        .w_full()
                                        .gap(px(SPACE_S))
                                        .child(
                                            div()
                                                .text_size(p.text(Step::Meta))
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(rgb(p.quiet()))
                                                .child(title),
                                        )
                                        .child(
                                            div()
                                                .font_family(crate::appearance::LABEL_FONT)
                                                .text_size(p.text(Step::Label))
                                                .text_color(rgb(p.quiet()))
                                                .child(count.to_string()),
                                        )
                                        .child(div().flex_1())
                                        .child(
                                            div()
                                                .text_size(p.text(Step::Label))
                                                .text_color(rgb(p.quiet()))
                                                .child(if folded { "▸" } else { "▾" }),
                                        ),
                                ),
                            )
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
            let tree = cx
                .try_global::<TreeIndex>()
                .and_then(|index| index.0.get(&(project.to_path_buf(), t.id.clone())))
                .cloned();
            let row = t.clone();
            let project = project.to_owned();
            let fold_key = (project.clone(), t.id.clone());
            let folded = tree.as_ref().is_some_and(|tree| tree.children > 0)
                && self.discovery.collapsed.contains(&fold_key);
            // A folded parent speaks for the children that need attention.
            let status = match tree.as_ref() {
                Some(tree) if folded && tree.waiting > 0 => Some("Needs you"),
                Some(tree) if folded && tree.running > 0 => status.or(Some("Running")),
                _ => status,
            };
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
                title: t.title().into(),
                route: {
                    let mut route = if !t.worker && t.parent_thread_id.is_some() {
                        format!("Subagent · {badge}")
                    } else {
                        format!(
                            "{} · {badge}",
                            project.file_name().unwrap_or_default().to_string_lossy()
                        )
                    };
                    if let Some(tree) = tree.as_ref().filter(|tree| tree.children > 0) {
                        let noun = if tree.children == 1 {
                            "worker"
                        } else {
                            "workers"
                        };
                        route.push_str(&format!(" · {} {noun}", tree.children));
                        if tree.running > 0 {
                            route.push_str(&format!(", {} running", tree.running));
                        }
                    }
                    route
                },
                age: age(now.timestamp(), timestamp),
                status: status.or(t.unread.then_some("Unread")),
                selected: active.project == project
                    && scope.matches(&active.route)
                    && active.store.as_ref().and_then(|s| s.selected()) == Some(&t.id),
                depth,
            };
            // The disclosure is a sibling of the row in the gutter, so it folds without
            // selecting and the row keeps the sidebar keyline.
            let disclosure = tree.as_ref().filter(|tree| tree.children > 0).map(|tree| {
                let glyph = if folded { "▸" } else { "▾" };
                let label = format!(
                    "{} {} worker{}",
                    if folded { "Show" } else { "Hide" },
                    tree.children,
                    if tree.children == 1 { "" } else { "s" }
                );
                gpui::div()
                    .id(SharedString::from(format!(
                        "fold-{}-{}-{}",
                        fold_key.0.display(),
                        scope.api,
                        fold_key.1
                    )))
                    .absolute()
                    .left_0()
                    .top_0()
                    .w(px(crate::kit::GUTTER))
                    .h(px(crate::kit::ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_size(p.text(Step::Label))
                    .text_color(rgb(p.quiet()))
                    .hover(move |d| d.text_color(rgb(p.fg)))
                    .child(glyph)
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(label.clone()).build(window, cx)
                    })
                    .on_click(cx.listener(move |s, _, _, cx| {
                        s.discovery.toggle(fold_key.0.clone(), fold_key.1.clone());
                        cx.notify();
                    }))
            });
            rows.push(
                gpui::div()
                    .w(width)
                    .relative()
                    .px(px(crate::kit::GUTTER))
                    .child(model.render(
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
                    ))
                    .children(disclosure)
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
        let shown = inline || self.drawer_open;
        self.running.update(cx, |r, _| r.shown = shown);
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
        let (running_count, running_open) = {
            let running = self.running.read(cx);
            (running.process_count(), running.open)
        };
        // The AGENT picker shows and offers what the composer's model pill does.
        let value = pane.store.as_ref().map_or_else(
            || current.clone(),
            |s| super::route_chip::route_value(s, s.selected()),
        );
        let host = cx.weak_entity();
        let route = picker_row(p, "billing-route", "AGENT", value.clone())
            .tooltip(format!("Agent and model · {value}"))
            .on_click(move |_, window, cx| {
                let _ = host.update(cx, |s, cx| s.open_agent_picker(window, cx));
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
        let picker = picker_row(p, "project-picker", "PROJECT", label.clone())
            .tooltip(project.display().to_string())
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
                                let _ = host.update(cx, |s, cx| {
                                    let route = route.clone();
                                    s.with_project_dir(
                                        directory.clone(),
                                        true,
                                        window,
                                        cx,
                                        move |s, project, window, cx| {
                                            s.open(project, route, window, cx)
                                        },
                                    );
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
                background.push(
                    crate::kit::toolbar_button(
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
                        true,
                        cx.listener(move |s, _, window, cx| s.switch(index, window, cx)),
                    )
                    .w_full()
                    .justify_start()
                    .text_color(rgb(p.warning)),
                );
            }
        }
        let threads_label = if background_waiting > 0 {
            format!("Threads · {background_waiting} waiting")
        } else {
            "Threads".into()
        };
        let sidebar_width =
            px(self.sidebar_width as f32).min(window.viewport_size().width - px(36.));
        let gutter = px(crate::kit::GUTTER);
        let new_session_key = if cfg!(target_os = "macos") {
            "⌘N"
        } else {
            "Ctrl N"
        };
        let sidebar = col()
            .w(sidebar_width)
            .flex_shrink_0()
            .h_full()
            .bg(rgb(p.nav))
            // Signature header: the sky runs edge to edge, the OX mark and the borderless
            // command button sit on the keyline.
            .child(
                row()
                    .relative()
                    .overflow_hidden()
                    .h(px(56.))
                    .flex_shrink_0()
                    .px(gutter)
                    .gap(px(SPACE_S))
                    .justify_between()
                    .child(activity::horizon(p, &crate::sky::now(cx)))
                    .child(activity::marquee(p, hot, false))
                    .child(row().gap(px(SPACE_XS)).child(self.palette_button(cx)).when(
                        compact,
                        |bar| {
                            bar.child(crate::kit::icon_button(
                                p,
                                "close-threads",
                                "×",
                                "Close threads",
                                cx.listener(|s, _, w, cx| s.close_drawer(w, cx)),
                            ))
                        },
                    )),
            )
            .child(col().pt(px(SPACE_M)).px(gutter).child(picker).child(route))
            .child(
                div().pt(px(SPACE_M)).px(gutter).child(
                    crate::kit::nav_row(
                        "new-session",
                        "New session",
                        crate::kit::CONTROL_HEIGHT,
                        pane_ready,
                        cx.listener(|s, _, window, cx| {
                            s.close_drawer(window, cx);
                            s.panes[s.active]
                                .update(cx, |p, cx| p.action(cx, DeskAction::NewDraft));
                            cx.notify();
                        }),
                    )
                    .bg(rgb(p.surface()))
                    .tooltip(format!("New session · {new_session_key}"))
                    .child(
                        row()
                            .w_full()
                            .gap(px(SPACE_S))
                            .child(
                                gpui_component::Icon::new(gpui_component::IconName::Plus)
                                    .size(px(14.))
                                    .text_color(rgb(p.muted)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(p.text(Step::Ui))
                                    .text_color(rgb(p.fg))
                                    .child("New session"),
                            )
                            .child(
                                div()
                                    .font_family(crate::appearance::LABEL_FONT)
                                    .text_size(p.text(Step::Label))
                                    .text_color(rgb(p.quiet()))
                                    .child(new_session_key),
                            ),
                    ),
                ),
            )
            .child(
                div().pt(px(SPACE_S)).px(gutter).child(
                    gpui_component::input::Input::new(&self.filter)
                        .prefix(
                            gpui_component::Icon::new(gpui_component::IconName::Search)
                                .size(px(14.))
                                .text_color(rgb(p.quiet())),
                        )
                        .suffix(self.filter_menu(p, cx)),
                ),
            )
            .when_some(self.choose_route.clone(), |sidebar, (project, row)| {
                let host = cx.weak_entity();
                let aliases = self.models.clone();
                sidebar.child(
                    div().pt(px(SPACE_S)).px(gutter).child(
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
                        .w_full()
                        .dropdown_menu(move |mut menu, _, _| {
                            for alias in &aliases {
                                let project = project.clone();
                                let resume = row.needs_claude_alias();
                                let route = if resume {
                                    ox_desk_core::runtime::Runtime::Claude
                                        .route(Some(alias.clone()))
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
                    ),
                )
            })
            .when(!background.is_empty(), |sidebar| {
                sidebar.child(div().pt(px(SPACE_S)).px(gutter).children(background))
            })
            .child(
                div()
                    .id("thread-navigation")
                    .track_focus(&self.list_focus)
                    .on_key_down(cx.listener(Self::navigate))
                    .mt(px(SPACE_XS))
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
            .child(self.running.clone())
            .child(self.sidebar_footer(p, running_count, running_open, window, cx))
            .when_some(
                // The pane header already shows its own connection error: say it once.
                self.message.clone().filter(|m| {
                    self.panes[self.active].read(cx).visible_error() != Some(m.as_str())
                }),
                |d, m| {
                    d.child(div().px(gutter).pb(px(SPACE_S)).child(crate::kit::banner(
                        p,
                        "host-error",
                        "host-error-dismiss",
                        m,
                        cx.listener(|s, _, _, cx| {
                            s.message = None;
                            cx.notify();
                        }),
                    )))
                },
            );
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
                                .child(activity::horizon(p, &crate::sky::now(cx)))
                                .child(activity::marquee(p, hot, true))
                                .child(crate::kit::toolbar_button(
                                    p,
                                    "show-threads",
                                    threads_label,
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
            .children(self.model_overlay(cx))
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

/// A sidebar picker on the keyline: a quiet micro label, the value and a caret.
fn picker_row(
    p: Palette,
    id: &'static str,
    label: &'static str,
    value: String,
) -> gpui_component::button::Button {
    crate::kit::nav_row(
        id,
        format!("{} {value}", label.to_lowercase()),
        crate::kit::ICON_SIZE,
        true,
        |_, _, _| {},
    )
    .child(
        row()
            .w_full()
            .min_w_0()
            .gap(px(SPACE_S))
            .child(crate::kit::micro_label(p, label).w(px(56.)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(p.text(Step::Ui))
                    .text_color(rgb(p.fg))
                    .child(value),
            )
            .child(crate::kit::caret(p)),
    )
}

/// Sets the one `is:` status filter to `token`, keeping the search text. An empty token
/// clears the status filter ("All").
fn select_status_filter(value: &str, token: &str) -> String {
    let mut next: Vec<&str> = value
        .split_whitespace()
        .filter(|word| !word.starts_with("is:"))
        .collect();
    if !token.is_empty() {
        next.push(token);
    }
    next.join(" ")
}

#[cfg(test)]
mod tests {
    use super::select_status_filter;

    #[test]
    fn status_filter_menu_sets_one_filter_and_keeps_text() {
        assert_eq!(select_status_filter("", "is:running"), "is:running");
        assert_eq!(
            select_status_filter("fix bug", "is:waiting"),
            "fix bug is:waiting"
        );
        assert_eq!(
            select_status_filter("is:running fix", "is:waiting"),
            "fix is:waiting"
        );
        assert_eq!(select_status_filter("is:running fix", ""), "fix");
    }
}

#[cfg(test)]
mod route_gate_tests {
    use super::new_route_gate;
    use ox_desk_core::runtime::Runtime;

    #[test]
    fn subscription_routes_need_only_their_account() {
        let claude = Runtime::Claude.route(None);
        let codex = Runtime::Codex.route(None);
        assert!(new_route_gate(&claude, &[], false).is_ok());
        assert!(new_route_gate(&codex, &[], true).is_ok());
        assert!(
            new_route_gate(&codex, &[], false)
                .unwrap_err()
                .contains("ChatGPT")
        );
    }

    #[test]
    fn api_routes_need_a_configured_alias() {
        let api = Runtime::Codex.route(Some("fixture/demo".into()));
        assert!(new_route_gate(&api, &["fixture/demo".into()], false).is_ok());
        assert!(new_route_gate(&api, &[], false).is_err());
    }
}

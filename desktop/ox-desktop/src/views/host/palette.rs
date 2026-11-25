use super::*;
use crate::kit::field;
use gpui_component::input::{Input, InputEvent, InputState};
use std::collections::{HashMap, HashSet};

mod search;

#[derive(Clone, PartialEq, Eq, Hash)]
enum Target {
    Thread(usize, String),
    Saved(PathBuf, Scope, String),
    Refresh,
    LocalTerminal,
    Project(PathBuf, Route),
    Route(PathBuf, Route),
    Command {
        pane: usize,
        owner: Option<String>,
        text: String,
    },
    More,
}
struct Item {
    title: String,
    context: String,
    target: Target,
}
#[derive(Default, PartialEq)]
struct Sources {
    query: String,
    catalogs: Vec<Option<(u64, usize, bool)>>,
    commands: Vec<String>,
    owner: Option<String>,
    routes: Vec<String>,
    provider: Option<String>,
    discovery_revision: usize,
    discovering: bool,
}
pub(super) struct State {
    sources: Option<Sources>,
    requested: Option<String>,
    input: Entity<InputState>,
    previous_focus: Option<FocusHandle>,
    rows: Vec<Item>,
    cursor: usize,
    scroll: UniformListScrollHandle,
    debounce: Option<Task<()>>,
    _input_events: Subscription,
    _command_updates: Subscription,
}

impl Host {
    pub(super) fn palette_keys(window: &Window, cx: &mut Context<Self>) -> Subscription {
        let id = window.window_handle().window_id();
        let view = cx.weak_entity();
        let bindings = crate::keymap::bindings(cx);
        cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != id {
                return;
            }
            let key = &event.keystroke;
            let binding = bindings
                .iter()
                .find(|(k, _)| k.modifiers == key.modifiers && k.key == key.key)
                .map(|(_, action)| *action);
            let toggle = binding == Some(ox_desk_core::keys::Command::Palette);
            let reverse_tab = key.modifiers == Modifiers::shift() && key.key == "tab";
            if binding.is_none()
                && !reverse_tab
                && (key.modifiers != Modifiers::default()
                    || !matches!(key.key.as_str(), "enter" | "escape" | "up" | "down" | "tab"))
            {
                return;
            }
            let _ = view.update(cx, |host, cx| {
                if toggle {
                    if host.palette.is_some() {
                        host.close_palette(window, cx);
                    } else {
                        host.show_palette(window, cx);
                    }
                } else if let Some(state) = &mut host.palette {
                    if state.input.update(cx, |field, cx| {
                        field.marked_text_range(window, cx).is_some()
                    }) {
                        return;
                    }
                    if binding.is_some() {
                        cx.stop_propagation();
                        return;
                    }
                    match key.key.as_str() {
                        "escape" => host.close_palette(window, cx),
                        "enter" => host.choose_palette(window, cx),
                        _ => {
                            let end = state.rows.len().saturating_sub(1);
                            state.cursor = if key.key == "up" || reverse_tab {
                                state.cursor.saturating_sub(1)
                            } else {
                                (state.cursor + 1).min(end)
                            };
                            state
                                .scroll
                                .scroll_to_item(state.cursor, ScrollStrategy::Center);
                            cx.notify();
                        }
                    }
                } else if host.drawer_open
                    && key.key == "escape"
                    && key.modifiers == Modifiers::default()
                {
                    host.close_drawer(window, cx);
                } else if !host.drawer_open
                    && binding.is_none()
                    && key.modifiers == Modifiers::default()
                    && matches!(key.key.as_str(), "up" | "down" | "tab")
                    && host.preferences.is_none()
                    && host.panes[host.active]
                        .update(cx, |pane, cx| pane.mention_key(&key.key, window, cx))
                {
                    // The composer consumed the key while its file list was open.
                } else if let Some(action) = binding {
                    if !host.shortcut(action, window, cx) {
                        return;
                    }
                } else {
                    return;
                }
                window.prevent_default();
                cx.stop_propagation();
            });
        })
    }
    pub(super) fn palette_button(&self, cx: &mut Context<Self>) -> gpui_component::button::Button {
        let shortcut = if cfg!(target_os = "macos") {
            "⌘K"
        } else {
            "Ctrl K"
        };
        button(
            Palette::get(cx),
            "command-palette",
            ">_",
            false,
            true,
            cx.listener(|s, _, window, cx| s.show_palette(window, cx)),
        )
        .ghost()
        .border_0()
        .p_0()
        .bg(rgb(Palette::get(cx).nav))
        .w(px(30.))
        .tooltip(format!("Commands · {shortcut}"))
    }
    fn show_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_drawer(window, cx);
        if self.palette.is_some() {
            return;
        }
        let previous_focus = window.focused(cx);
        let input = field(
            "",
            "Find threads, projects, routes or commands…",
            false,
            window,
            cx,
        );
        let events = cx.subscribe(&input, |s, input, event: &InputEvent, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let query = input.read(cx).value().trim().to_owned();
            s.reset_palette_search(cx);
            let Some(state) = s.palette.as_mut() else {
                return;
            };
            state.cursor = 0;
            state.scroll.scroll_to_item(0, ScrollStrategy::Top);
            if !query.is_empty() && !query.starts_with('/') {
                state.debounce = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(140))
                        .await;
                    let _ = this.update(cx, |s, cx| s.start_palette_search(&query, cx));
                }));
            }
            cx.notify();
        });
        let pane = self.panes[self.active].clone();
        let updates = cx.observe(&pane, |s, _, cx| {
            if s.palette.is_some() {
                cx.notify();
            }
        });
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.palette = Some(State {
            input,
            sources: None,
            requested: None,
            previous_focus,
            rows: vec![],
            cursor: 0,
            scroll: Default::default(),
            debounce: None,
            _input_events: events,
            _command_updates: updates,
        });
        cx.notify();
    }
    fn reset_palette_search(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.palette {
            state.debounce = None;
            state.requested = None;
        }
        for pane in &self.panes {
            pane.update(cx, |p, _| {
                if let Some(store) = &mut p.store {
                    let _ = store.palette_search("");
                }
            });
        }
    }
    pub(super) fn refresh_palette_search(&mut self, cx: &mut Context<Self>) {
        if let Some(query) = self
            .palette
            .as_ref()
            .and_then(|state| state.requested.clone())
        {
            self.start_palette_search(&query, cx);
        }
    }
    fn start_palette_search(&mut self, query: &str, cx: &mut Context<Self>) {
        if self
            .palette
            .as_ref()
            .is_none_or(|s| s.input.read(cx).value().trim() != query)
        {
            return;
        }
        self.palette.as_mut().unwrap().requested = Some(query.into());
        let mut profiles = HashSet::new();
        for pane in &self.panes {
            let p = pane.read(cx);
            let Some(store) = &p.store else {
                continue;
            };
            if !matches!(
                store.state(),
                Connection::Ready | Connection::SignInRequired
            ) || !profiles.insert((p.project.clone(), p.route.runtime, p.route.is_api()))
            {
                continue;
            }
            if store.palette_query() == query {
                continue;
            }
            pane.update(cx, |p, _| {
                if let Err(e) = p.store.as_mut().unwrap().palette_search(query) {
                    self.message = Some(e.to_string());
                }
                let _ = p.wake.try_send(());
            });
        }
        cx.notify();
    }
    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.palette.take() else {
            return;
        };
        if let Some(focus) = state.previous_focus {
            window.focus(&focus, cx);
        }
        self.reset_palette_search(cx);
        cx.notify();
    }
    fn palette_items(&self, query: &str, cx: &App) -> Vec<Item> {
        let active = self.panes[self.active].read(cx);
        let project = &active.project;
        let mut items = Vec::new();
        let mut add = |title: String, context: String, target: Target| {
            items.push(Item {
                title,
                context,
                target,
            })
        };
        add(
            "Open project terminal".into(),
            format!("Local shell · {}", project.display()),
            Target::LocalTerminal,
        );
        if self.discovery.pending == 0 {
            add(
                "Refresh sessions".into(),
                "Find saved sessions from the native CLIs".into(),
                Target::Refresh,
            );
        }
        if let Some(store) = &active.store
            && store.state() == &Connection::Ready
            && !store.selected().is_some_and(|id| store.in_terminal(id))
        {
            for (cmd, hint) in store.command_caps(store.selected()).choices() {
                let text = if query.split_whitespace().next() == Some(cmd.as_str()) {
                    query
                } else {
                    cmd.as_str()
                };
                add(
                    text.into(),
                    format!(
                        "{} command · {} · {hint}",
                        store.runtime_name(),
                        active.route.label()
                    ),
                    Target::Command {
                        pane: self.active,
                        owner: store.selected().map(str::to_owned),
                        text: text.into(),
                    },
                );
            }
        }
        let query = query.to_lowercase();
        let mut preview_matches = HashMap::new();
        let mut threads = HashSet::new();
        let mut projects = HashSet::new();
        for (i, pane) in self.panes.iter().enumerate() {
            let p = pane.read(cx);
            if projects.insert(p.project.clone()) {
                let route = self.recent.route(&p.project);
                let label = p
                    .project
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                add(
                    label,
                    format!("Project · {} · {}", p.project.display(), route.label()),
                    Target::Project(p.project.clone(), route),
                );
            }
            if let Some(store) = &p.store {
                for r in store.threads().iter().chain(store.palette_rows()) {
                    let scope = Scope {
                        runtime: p.route.runtime,
                        api: r.saved_claude_api().unwrap_or(p.route.is_api()),
                    };
                    if self.archived(&p.project, scope, &r.id, cx)
                        || !threads.insert((&p.project, scope, &r.id))
                    {
                        continue;
                    }
                    if let Some(score) = search::preview_rank(&query, &r.preview) {
                        preview_matches.insert(Target::Thread(i, r.id.clone()), score);
                    }
                    let route = self
                        .project_row_state(&p.project, r, scope, cx)
                        .0
                        .map(|r| r.label())
                        .unwrap_or_else(|| {
                            format!(
                                "{} · {}",
                                r.model_provider,
                                r.model.as_deref().unwrap_or("Choose model")
                            )
                        });
                    add(
                        r.title().into(),
                        format!("Thread · {} · {route}", p.project.display()),
                        Target::Thread(i, r.id.clone()),
                    );
                }
            }
        }
        for saved in self.discovery.all() {
            let r = &saved.row;
            if self.archived(&r.cwd, saved.scope, &r.id, cx)
                || !threads.insert((&r.cwd, saved.scope, &r.id))
            {
                continue;
            }
            if let Some(score) = search::preview_rank(&query, &r.preview) {
                preview_matches.insert(
                    Target::Saved(r.cwd.clone(), saved.scope, r.id.clone()),
                    score,
                );
            }
            let route = self.project_row_state(&r.cwd, r, saved.scope, cx).0;
            let label = route
                .as_ref()
                .map(Route::label)
                .unwrap_or_else(|| r.model_provider.clone());
            add(
                r.title().into(),
                format!("Thread · {} · {label}", r.cwd.display()),
                Target::Saved(r.cwd.clone(), saved.scope, r.id.clone()),
            );
            if let Some(route) = route
                && projects.insert(r.cwd.clone())
            {
                add(
                    r.cwd
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    format!("Project · {} · {label}", r.cwd.display()),
                    Target::Project(r.cwd.clone(), route),
                );
            }
        }
        for recent in &self.recent.recent {
            if projects.insert(recent.project.clone()) {
                let route = self.recent.route(&recent.project);
                let label = recent
                    .project
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                add(
                    label,
                    format!("Project · {} · {}", recent.project.display(), route.label()),
                    Target::Project(recent.project.clone(), route),
                );
            }
        }
        for route in ox_desk_core::runtime::Runtime::routes(&self.models) {
            add(
                route.label(),
                format!("Billing route · {}", project.display()),
                Target::Route(project.clone(), route),
            );
        }
        let mut matches: Vec<_> = items
            .into_iter()
            .filter_map(|item| {
                let score = search::rank(&query, &item.title, &item.context)
                    .max(preview_matches.get(&item.target).copied())
                    .or_else(|| {
                        let Target::Thread(pane, ref id) = item.target else {
                            return None;
                        };
                        let project = &self.panes[pane].read(cx).project;
                        self.panes
                            .iter()
                            .any(|pane| {
                                let p = pane.read(cx);
                                p.project == *project
                                    && p.store.as_ref().is_some_and(|s| s.palette_contains(id))
                            })
                            .then_some(-1000)
                    })?;
                Some((score, item))
            })
            .collect();
        matches.sort_by_key(|item| std::cmp::Reverse(item.0));
        let mut rows: Vec<_> = matches.into_iter().map(|(_, item)| item).collect();
        if self.panes.iter().any(|p| {
            p.read(cx)
                .store
                .as_ref()
                .is_some_and(|s| s.palette_has_more())
        }) {
            rows.push(Item {
                title: "More matching threads".into(),
                context: "Search older sessions".into(),
                target: Target::More,
            });
        }
        rows
    }
    fn choose_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = &self.palette else {
            return;
        };
        let input = state.input.read(cx).value();
        let query = input.trim();
        let target = if state.sources.as_ref().is_some_and(|s| s.query == query) {
            state.rows.get(state.cursor).map(|row| row.target.clone())
        } else {
            self.palette_items(query, cx)
                .into_iter()
                .next()
                .map(|row| row.target)
        };
        let Some(target) = target else { return };
        match target {
            Target::More => {
                for pane in &self.panes {
                    pane.update(cx, |p, _| {
                        if let Some(store) = &mut p.store {
                            if let Err(e) = store.palette_more() {
                                self.message = Some(e.to_string());
                            }
                            let _ = p.wake.try_send(());
                        }
                    });
                }
                cx.notify();
            }
            Target::Thread(i, id) => {
                let row = self.panes[i]
                    .read(cx)
                    .store
                    .as_ref()
                    .and_then(|s| {
                        s.catalog_row(&id)
                            .or_else(|| s.palette_rows().iter().find(|r| r.id == id))
                    })
                    .cloned();
                self.close_palette(window, cx);
                if let Some(row) = row {
                    let p = self.panes[i].read(cx);
                    let project = p.project.clone();
                    let scope = Scope {
                        runtime: p.route.runtime,
                        api: row.saved_claude_api().unwrap_or(p.route.is_api()),
                    };
                    self.activate_row(project, scope, row, window, cx);
                }
            }
            Target::LocalTerminal => {
                let project = self.panes[self.active].read(cx).project.clone();
                self.close_palette(window, cx);
                if let Err(error) = crate::terminal::open(project, None, |_, _| {}, cx) {
                    self.message = Some(error);
                    cx.notify();
                }
            }
            Target::Refresh => {
                self.close_palette(window, cx);
                self.start_discovery(cx);
            }
            Target::Saved(project, scope, id) => {
                self.close_palette(window, cx);
                let saved = self
                    .discovery
                    .all()
                    .find(|s| s.scope == scope && s.row.cwd == project && s.row.id == id)
                    .map(|s| s.row.clone());
                if let Some(row) = saved {
                    self.activate_row(project, scope, row, window, cx);
                }
            }
            Target::Project(project, route) | Target::Route(project, route) => {
                self.close_palette(window, cx);
                match project.canonicalize() {
                    Ok(path) if path.is_dir() => self.open(path, route, window, cx),
                    _ => {
                        self.message = Some(format!("Project unavailable: {}", project.display()));
                        cx.notify();
                    }
                }
            }
            Target::Command { pane, owner, text } => {
                if text == "/rename" {
                    state
                        .input
                        .update(cx, |s, cx| s.set_value("/rename ", window, cx));
                    return;
                }
                self.close_palette(window, cx);
                if self.active == pane {
                    self.switch(pane, window, cx);
                    self.panes[pane].update(cx, |p, cx| {
                        p.action(cx, DeskAction::PaletteCommand(owner, text))
                    });
                }
            }
        }
    }
    pub(super) fn palette_overlay(&mut self, cx: &mut Context<Self>) -> Option<Div> {
        let state = self.palette.as_ref()?;
        let query = state.input.read(cx).value().trim().to_owned();
        let pane = self.panes[self.active].read(cx);
        let store = pane.store.as_ref();
        let sources = Sources {
            query: query.clone(),
            commands: store
                .map(|s| {
                    s.command_caps(s.selected())
                        .choices()
                        .into_iter()
                        .map(|(name, _)| name)
                        .collect()
                })
                .unwrap_or_default(),
            owner: store.and_then(|s| s.selected().map(str::to_owned)),
            catalogs: self
                .panes
                .iter()
                .map(|p| {
                    p.read(cx).store.as_ref().map(|s| {
                        (
                            s.catalog_revision(),
                            s.palette_rows().len(),
                            s.palette_has_more(),
                        )
                    })
                })
                .collect(),
            routes: self.models.clone(),
            provider: self.api_provider.clone(),
            discovery_revision: self.discovery.revision,
            discovering: self.discovery.pending > 0,
        };
        if state.sources.as_ref() != Some(&sources) {
            let rows = self.palette_items(&query, cx);
            let state = self.palette.as_mut().unwrap();
            let keep = state.sources.as_ref().is_some_and(|old| old.query == query);
            state.cursor = if keep {
                state
                    .rows
                    .get(state.cursor)
                    .and_then(|old| rows.iter().position(|new| new.target == old.target))
                    .unwrap_or(0)
            } else {
                0
            };
            state.rows = rows;
            state.sources = Some(sources);
        }
        let p = Palette::get(cx);
        let state = self.palette.as_ref().unwrap();
        let count = state.rows.len();
        let errors = self
            .panes
            .iter()
            .filter_map(|p| p.read(cx).store.as_ref()?.palette_search_error())
            .collect::<Vec<_>>()
            .join(" · ");
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(gpui::black().opacity(0.58))
                .flex()
                .justify_center()
                .items_start()
                .pt(px(32.))
                .px_2()
                .child(
                    col()
                        .w_full()
                        .max_w(px(600.))
                        .p_3()
                        .gap_2()
                        .bg(rgb(p.panel))
                        .border_1()
                        .border_color(rgb(p.line))
                        .rounded_lg()
                        .shadow_lg()
                        .child(
                            row()
                                .items_center()
                                .justify_between()
                                .flex_wrap()
                                .child("Command palette")
                                .child(small(p, "↑ ↓   Select     Enter   Open     Esc   Close")),
                        )
                        .child(Input::new(&state.input))
                        .when(!errors.is_empty(), |d| d.child(small(p, errors)))
                        .when(count == 0, |d| d.child(small(p, "No matches")))
                        .when(count > 0, |d| {
                            d.child(
                                uniform_list(
                                    "palette-results",
                                    count,
                                    cx.processor(move |s, range: Range<usize>, _, cx| {
                                        let Some(state) = &s.palette else {
                                            return vec![];
                                        };
                                        range
                                            .filter_map(|i| {
                                                let item = state.rows.get(i)?;
                                                Some(
                                                    col()
                                                        .id(("palette-result", i))
                                                        .h(px(44.))
                                                        .px_2()
                                                        .justify_center()
                                                        .rounded_sm()
                                                        .cursor_pointer()
                                                        .when(state.cursor == i, |d| {
                                                            d.bg(rgb(p.hover))
                                                        })
                                                        .hover(|d| d.bg(rgb(p.hover)))
                                                        .child(
                                                            div()
                                                                .text_size(px(14.))
                                                                .truncate()
                                                                .child(item.title.clone()),
                                                        )
                                                        .child(
                                                            small(p, item.context.clone())
                                                                .truncate(),
                                                        )
                                                        .on_click(cx.listener(
                                                            move |s, _, w, cx| {
                                                                if let Some(state) = &mut s.palette
                                                                {
                                                                    state.cursor = i;
                                                                }
                                                                s.choose_palette(w, cx);
                                                            },
                                                        )),
                                                )
                                            })
                                            .collect()
                                    }),
                                )
                                .track_scroll(&state.scroll)
                                .h(px((count.min(7) * 44) as f32)),
                            )
                        }),
                ),
        )
    }
}

//! Release the session tree if a desktop view fails while building a frame.
mod fleet;
use crate::{appearance::Palette, kit, views::host::Host};
use gpui::*;
use std::{
    cell::Cell,
    panic::{self, AssertUnwindSafe},
};

struct FailedView;
impl Global for FailedView {}

thread_local! {
    // View payloads can include model output, local paths, or text from an editor.
    static IN_VIEW: Cell<bool> = const { Cell::new(false) };
}

pub fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if IN_VIEW.get() {
            if let Some(loc) = info.location() {
                eprintln!("OX Desktop view failed at {}:{}.", loc.file(), loc.line());
            } else {
                eprintln!("OX Desktop view failed.");
            }
        } else {
            previous(info);
        }
    }));
}

pub fn guard<T: 'static>(
    cx: &mut Context<T>,
    draw: impl FnOnce(&mut Context<T>) -> AnyElement,
) -> AnyElement {
    try_draw(cx, draw).unwrap_or_else(|| div().into_any_element())
}

pub fn try_draw<T: 'static, R>(
    cx: &mut Context<T>,
    draw: impl FnOnce(&mut Context<T>) -> R,
) -> Option<R> {
    if cx.has_global::<FailedView>() {
        return None;
    }
    let nested = IN_VIEW.replace(true);
    let result = panic::catch_unwind(AssertUnwindSafe(|| draw(cx)));
    IN_VIEW.set(nested);
    match result {
        Ok(output) => Some(output),
        Err(_) => {
            cx.set_global(FailedView);
            None
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ClientOnly(pub bool);
impl Global for ClientOnly {}

pub struct Workspace {
    local: Option<Entity<Host>>,
    hosts: std::collections::HashMap<String, Entity<Host>>,
    active: Option<String>,
    settings: Option<Entity<crate::preferences::Hosts>>,
    error: Option<String>,
    inventory: std::collections::HashMap<Option<String>, Entity<fleet::Inventory>>,
    page: Option<fleet::Page>,
    filter: Entity<gpui_component::input::InputState>,
    _failure: Subscription,
    _updates: Subscription,
    _directory: Subscription,
}
impl Workspace {
    pub fn new(
        local: Option<Entity<Host>>,
        restore_host: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let failure = cx.observe_global::<FailedView>(|s, cx| {
            s.local.take();
            s.hosts.clear();
            s.inventory.clear();
            cx.notify();
        });
        let settings = (local.is_none()
            && cx
                .global::<crate::preferences::HostDirectory>()
                .0
                .is_empty())
        .then(|| cx.new(|cx| crate::preferences::Hosts::new(w, cx)));
        let filter = kit::field(
            "",
            "Filter by title, project, @host or is:waiting",
            false,
            w,
            cx,
        );
        cx.subscribe(
            &filter,
            |_, _, _: &gpui_component::input::InputEvent, cx| cx.notify(),
        )
        .detach();
        let mut workspace = Self {
            inventory: Default::default(),
            page: None,
            filter,
            local,
            hosts: Default::default(),
            active: None,
            settings,
            error: None,
            _failure: failure,
            _updates: cx.observe_global::<crate::updates::Updates>(|_, cx| cx.notify()),
            _directory: cx.observe_global::<crate::preferences::HostDirectory>(|s, cx| {
                let saved = &cx.global::<crate::preferences::HostDirectory>().0;
                s.hosts.retain(|id, view| {
                    saved
                        .iter()
                        .any(|h| &h.id == id && view.read(cx).paired_with(h))
                });
                if s.active
                    .as_ref()
                    .is_some_and(|id| !s.hosts.contains_key(id))
                {
                    s.active = None;
                }
                s.sync_inventory(cx);
                cx.notify();
            }),
        };
        workspace.sync_inventory(cx);
        if restore_host
            && let Ok(Some(id)) =
                crate::workspace_state::read_preference::<Option<String>>("selected-host.json")
            && let Some(host) = cx
                .global::<crate::preferences::HostDirectory>()
                .0
                .iter()
                .find(|host| host.id == id)
                .cloned()
        {
            workspace.select(host, w, cx);
        }
        workspace
    }
    fn set_active(&mut self, id: Option<String>) {
        if self.active != id {
            crate::workspace_state::remember_host(id.clone());
            self.active = id;
        }
    }

    fn select(
        &mut self,
        host: crate::platform::hosts::SavedHost,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings = None;
        self.page = None;
        self.error = None;
        if self.hosts.contains_key(&host.id) {
            self.set_active(Some(host.id));
            cx.notify();
            return;
        }
        let link = crate::platform::HostLink::Remote(host.clone());
        let recent =
            crate::workspace_state::read_preference::<crate::workspace_state::LastWorkspace>(
                &link.state_file("workspace.json"),
            )
            .unwrap_or_default();
        if let Some(route) = recent.saved_route(&recent.project)
            && recent.project.is_absolute()
        {
            self.show_project(host, recent.project, route, w, cx);
            return;
        }
        let owner = cx.weak_entity();
        let handle = w.window_handle();
        self.error = crate::views::remote_project::open(
            host.clone(),
            None,
            move |path, route, cx| {
                let _ = handle.update(cx, |_, w, cx| {
                    owner.update(cx, |s, cx| s.show_project(host, path, route, w, cx))
                });
            },
            cx,
        )
        .err();
        cx.notify();
    }

    fn show_project(
        &mut self,
        host: crate::platform::hosts::SavedHost,
        path: std::path::PathBuf,
        route: ox_desk_core::store::Route,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !cx
            .global::<crate::preferences::HostDirectory>()
            .0
            .iter()
            .any(|h| h.id == host.id)
        {
            return;
        }
        let id = host.id.clone();
        let link = crate::platform::HostLink::Remote(host);
        let layouts = crate::workspace_state::read_preference(&link.state_file("layout.json"))
            .unwrap_or_default();
        let view = cx.new(|cx| Host::new(link, Some(route), path, layouts, None, false, w, cx));
        self.hosts.insert(id.clone(), view);
        self.set_active(Some(id));
        self.settings = None;
        cx.notify();
    }
}
impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::platform::layout_probe::begin_frame(window, "OX Desktop", || ());
        if !cx.has_global::<FailedView>() {
            let p = Palette::get(cx);
            let saved = &cx.global::<crate::preferences::HostDirectory>().0;
            let active = self
                .active
                .as_ref()
                .and_then(|id| self.hosts.get(id))
                .or(self.local.as_ref());
            if saved.is_empty()
                && self.settings.is_none()
                && let Some(host) = active
                && let Some(banner) = crate::updates::banner(p, cx)
            {
                return kit::col()
                    .size_full()
                    .child(banner)
                    .child(div().relative().flex_1().min_h_0().child(host.clone()))
                    .into_any_element();
            }
            if saved.is_empty()
                && self.settings.is_none()
                && let Some(host) = active
            {
                return host.clone().into_any_element();
            }
            let mut bar = kit::row().flex_wrap().gap_1().p_1();
            if self.local.is_some() {
                bar = bar.child(kit::button(
                    p,
                    "choose-local",
                    "This computer",
                    self.active.is_none() && self.settings.is_none() && self.page.is_none(),
                    true,
                    cx.listener(|s, _, _, cx| {
                        s.set_active(None);
                        s.page = None;
                        s.settings = None;
                        cx.notify();
                    }),
                ));
            }
            for host in saved {
                let host = host.clone();
                bar = bar.child(kit::button(
                    p,
                    format!("select-host-{}", host.id),
                    self.inventory
                        .get(&Some(host.id.clone()))
                        .map(|v| v.read(cx).display_name().to_owned())
                        .unwrap_or_else(|| host.name.clone()),
                    self.active.as_ref() == Some(&host.id)
                        && self.settings.is_none()
                        && self.page.is_none(),
                    true,
                    cx.listener(move |s, _, w, cx| s.select(host.clone(), w, cx)),
                ));
            }
            if !saved.is_empty() {
                let waiting: usize = self.inventory.values().map(|e| e.read(cx).waiting()).sum();
                for (page, label) in [
                    (fleet::Page::Sessions, "All sessions".to_owned()),
                    (fleet::Page::Inbox, format!("Inbox ({waiting})")),
                ] {
                    bar = bar.child(kit::button(
                        p,
                        label.clone(),
                        label,
                        self.page == Some(page),
                        true,
                        cx.listener(move |s, _, _, cx| {
                            s.page = if s.page == Some(page) {
                                None
                            } else {
                                Some(page)
                            };
                            s.settings = None;
                            cx.notify();
                        }),
                    ));
                }
            }
            bar = bar.child(kit::button(
                p,
                "manage-hosts",
                "Manage hosts",
                self.settings.is_some(),
                true,
                cx.listener(|s, _, w, cx| {
                    s.page = None;
                    s.settings = if s.settings.is_some() {
                        None
                    } else {
                        Some(cx.new(|cx| crate::preferences::Hosts::new(w, cx)))
                    };
                    cx.notify()
                }),
            ));
            let mut view = kit::col()
                .size_full()
                .bg(rgb(p.bg))
                .text_color(rgb(p.fg))
                .child(bar);
            if let Some(error) = &self.error {
                view = view.child(kit::small(p, error.clone()));
            }
            if let Some(settings) = &self.settings {
                view = view.child(
                    div()
                        .id("manage-hosts-pane")
                        .overflow_y_scroll()
                        .flex_1()
                        .p_4()
                        .child(settings.clone()),
                );
            } else if let Some(page) = self.page {
                view = view.child(self.fleet_view(page, cx));
            } else if let Some(host) = active {
                view = view.child(div().relative().flex_1().min_h_0().child(host.clone()));
            } else {
                view = view.child(kit::small(p, "Choose a host to open a project."));
            }
            return view.into_any_element();
        }
        let p = Palette::get(cx);
        kit::col()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(rgb(p.bg))
            .text_color(rgb(p.fg))
            .child("OX Desktop closed the workspace after a display error.")
            .child(kit::small(
                p,
                "Reopen the app to resume a saved conversation.",
            ))
            .child(kit::button(
                p,
                "close-failed-workspace",
                "Close window",
                true,
                true,
                |_, window, _| window.remove_window(),
            ))
            .into_any_element()
    }
}

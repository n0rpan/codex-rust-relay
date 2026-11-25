//! Release the session tree if a desktop view fails while building a frame.
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

pub struct Workspace {
    host: Option<Entity<Host>>,
    _failure: Subscription,
}
impl Workspace {
    pub fn new(host: Entity<Host>, cx: &mut Context<Self>) -> Self {
        let failure = cx.observe_global::<FailedView>(|s, cx| {
            // Disconnect the session owners outside the failing GPUI update.
            s.host.take();
            cx.notify();
        });
        Self {
            host: Some(host),
            _failure: failure,
        }
    }
}
impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        kit::layout_probe::begin_frame(window, "OX Desktop", || serde_json::Value::Null);
        if let Some(host) = &self.host {
            return host.clone().into_any_element();
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

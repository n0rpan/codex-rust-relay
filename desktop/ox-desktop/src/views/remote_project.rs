//! Browse directories on a paired session host and register the selected scope.
use crate::{
    appearance::Palette,
    kit::{button, col, small},
    platform::hosts::{Folders, SavedHost},
};
use gpui::{prelude::*, *};
use gpui_component::Root;
use ox_desk_core::store::Route;
use std::path::PathBuf;

type OnChoose = Box<dyn FnOnce(PathBuf, Route, &mut App)>;
pub(crate) fn open(
    host: SavedHost,
    route: Option<Route>,
    on_choose: impl FnOnce(PathBuf, Route, &mut App) + 'static,
    cx: &mut App,
) -> Result<(), String> {
    let title = format!("Open project on {}", host.name);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(560.), px(480.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        move |w, cx| {
            let view = cx.new(|cx| {
                let mut picker = Picker {
                    host,
                    route,
                    listing: None,
                    busy: false,
                    error: None,
                    on_choose: Some(Box::new(on_choose)),
                };
                picker.browse(None, false, cx);
                picker
            });
            cx.new(|cx| Root::new(view, w, cx))
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
struct Picker {
    host: SavedHost,
    route: Option<Route>,
    listing: Option<Folders>,
    busy: bool,
    error: Option<String>,
    on_choose: Option<OnChoose>,
}
impl Picker {
    fn browse(&mut self, path: Option<PathBuf>, more: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        let host = self.host.clone();
        let route = self.route.clone();
        let cursor = if more {
            self.listing
                .as_ref()
                .and_then(|l| l.folders.last().cloned())
        } else {
            None
        };
        let task = cx.background_spawn(async move {
            let listing = host.folders(path, cursor)?;
            let route = match route {
                Some(route) => route,
                None => {
                    let snapshot = host.snapshot().map_err(std::io::Error::other)?;
                    if snapshot.chatgpt_account.is_some() {
                        Route::CODEX_SUBSCRIPTION
                    } else if snapshot.claude_account {
                        ox_desk_core::runtime::Runtime::Claude.route(None)
                    } else if let Some(m) = snapshot.models.first() {
                        ox_desk_core::runtime::Runtime::Codex.route(Some(m.clone()))
                    } else {
                        ox_desk_core::runtime::Runtime::Claude.route(None)
                    }
                }
            };
            Ok::<_, std::io::Error>((listing, route))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                s.busy = false;
                match result {
                    Ok((mut page, route)) => {
                        if more
                            && let Some(prev) = &mut s.listing
                            && page.path == prev.path
                        {
                            prev.folders.append(&mut page.folders);
                            prev.more = page.more;
                        } else {
                            s.listing = Some(page)
                        }
                        s.route = Some(route);
                    }
                    Err(e) => s.error = Some(e.to_string()),
                }
                cx.notify()
            });
        })
        .detach();
        cx.notify();
    }
    fn choose(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(page) = &self.listing else { return };
        let Some(route) = self.route.clone() else {
            return;
        };
        let host = self.host.clone();
        let path = page.path.clone();
        let task = cx
            .background_spawn(async move { host.choose(&path, &route).map(|path| (path, route)) });
        self.busy = true;
        cx.spawn_in(w, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |s, w, cx| {
                s.busy = false;
                match result {
                    Ok((path, route)) => {
                        if let Some(done) = s.on_choose.take() {
                            done(path, route, cx)
                        }
                        w.remove_window();
                    }
                    Err(e) => {
                        s.error = Some(e.to_string());
                        cx.notify()
                    }
                }
            });
        })
        .detach();
        cx.notify()
    }
}
impl Render for Picker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let mut panel = col()
            .id("host-folder-list")
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(p.bg))
            .text_color(rgb(p.fg))
            .p_4()
            .gap_2();
        if let Some(page) = &self.listing {
            panel = panel.child(small(
                p,
                format!("{} · {}", self.host.name, page.path.display()),
            ));
            if let Some(parent) = page.parent.clone() {
                panel = panel.child(button(
                    p,
                    "folder-parent",
                    "Up one folder",
                    false,
                    !self.busy,
                    cx.listener(move |s, _, _, cx| s.browse(Some(parent.clone()), false, cx)),
                ));
            }
            panel = panel.child(button(
                p,
                "open-folder",
                "Open this folder",
                true,
                !self.busy,
                cx.listener(|s, _, w, cx| s.choose(w, cx)),
            ));
            for (i, name) in page.folders.iter().enumerate() {
                let path = page.path.join(name);
                panel = panel.child(button(
                    p,
                    ("folder", i),
                    name.clone(),
                    false,
                    !self.busy,
                    cx.listener(move |s, _, _, cx| s.browse(Some(path.clone()), false, cx)),
                ));
            }
            if page.more {
                let path = page.path.clone();
                panel = panel.child(button(
                    p,
                    "more-folders",
                    "More folders",
                    false,
                    !self.busy,
                    cx.listener(move |s, _, _, cx| s.browse(Some(path.clone()), true, cx)),
                ));
            }
        }
        if self.busy {
            panel = panel.child(small(p, "Loading…"));
        }
        if let Some(e) = &self.error {
            panel = panel.child(small(p, e.clone())).child(button(
                p,
                "retry-folders",
                "Retry",
                false,
                !self.busy,
                cx.listener(|s, _, _, cx| {
                    s.browse(s.listing.as_ref().map(|p| p.path.clone()), false, cx)
                }),
            ));
        }
        panel
    }
}

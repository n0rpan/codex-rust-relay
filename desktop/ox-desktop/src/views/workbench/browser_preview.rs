//! A local, read-only view of a browser the operator or agent already started.
use super::Workbench;
use crate::{
    appearance::{LABEL_FONT, Palette},
    kit::{button, col, row, small},
};
use gpui::{prelude::FluentBuilder, *};
use ox_desk_core::store::{Connection, Store};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

mod stream;

pub(super) const WIDTH: f32 = 435.;

pub(super) struct BrowserPreview {
    owner: String,
    viewport: Entity<Viewport>,
    focus: FocusHandle,
    pub(super) expanded: bool,
}

struct Viewport {
    project: PathBuf,
    label: String,
    image: Option<Arc<RenderImage>>,
    error: Option<String>,
    reader: Option<Task<()>>,
}

fn session_label(route: &ox_desk_core::store::Route, thread: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&(route, thread)).expect("route and thread serialize"));
    let key = format!("{:x}", digest.finalize());
    format!("thread-{}", &key[..32])
}

impl Viewport {
    fn start(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.reader.is_some() {
            return;
        }
        self.error = None;
        let (wake, incoming) = flume::bounded(1);
        let latest = Arc::new(Mutex::new(None));
        let worker_slot = latest.clone();
        let project = self.project.clone();
        let label = self.label.clone();
        let worker = std::thread::Builder::new()
            .name("ox-browser-viewport".into())
            .spawn(move || stream::run(&project, &label, &wake, &worker_slot));
        if let Err(error) = worker {
            self.error = Some(format!("Could not start the viewport reader: {error}"));
            cx.notify();
            return;
        }
        self.reader = Some(cx.spawn_in(window, async move |view, cx| {
            while incoming.recv_async().await.is_ok() {
                let latest = latest
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(latest) = latest
                    && view
                        .update_in(cx, |viewport, window, cx| {
                            match latest {
                                stream::Update::Frame(next) => {
                                    if let Some(old) = viewport.image.replace(next) {
                                        let _ = window.drop_image(old);
                                    }
                                }
                                stream::Update::Ended(error) => {
                                    if let Some(old) = viewport.image.take() {
                                        let _ = window.drop_image(old);
                                    }
                                    viewport.error = Some(error);
                                    viewport.reader = None;
                                }
                            }
                            cx.notify();
                        })
                        .is_err()
                {
                    break;
                }
            }
        }));
        cx.notify();
    }
}

impl Render for Viewport {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let open_command = format!(
            "ox browser --session {} open http://127.0.0.1:3000",
            self.label
        );
        if let Some(image) = &self.image {
            let panel = col()
                .id("browser-preview-live")
                .min_h_0()
                .flex_1()
                .gap_2()
                .child(small(p, "Live · View only"))
                .child(
                    col()
                        .bg(rgb(p.nav))
                        .border_1()
                        .border_color(rgb(p.line))
                        .overflow_hidden()
                        .rounded_md()
                        .min_h_0()
                        .flex_1()
                        .child(
                            img(image.clone())
                                .size_full()
                                .object_fit(ObjectFit::Contain),
                        ),
                );
            return crate::platform::layout_probe::mark_element(
                panel,
                &"browser-preview-live".into(),
                &"Live browser viewport".into(),
                false,
            )
            .into_any_element();
        }
        let tip = if let Some(error) = &self.error {
            small(p, error.clone())
        } else {
            small(p, "Connecting to this session’s browser…")
        };
        col()
            .min_h_0()
            .flex_1()
            .justify_center()
            .gap_2()
            .child("Watch a local app")
            .child(tip)
            .child(small(p, "In the project terminal, open your app with this command. Use your dev server’s port."))
            .child(
                div()
                    .text_color(rgb(p.fg))
                    .font_family(LABEL_FONT)
                    .truncate()
                    .child(open_command.clone()),
            )
            .child(
                row()
                    .flex_wrap()
                    .gap_2()
                    .child(button(
                        p, "browser-preview-copy", "Copy open command", false, true,
                        move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(open_command.clone())),
                    ))
                    .child(button(
                        p, "browser-preview-retry", "Retry", false, self.reader.is_none(),
                        cx.listener(|s, _, window, cx| s.start(window, cx)),
                    )),
            )
            .into_any_element()
    }
}

impl Workbench {
    pub(super) fn clear_browser_preview(&mut self, window: Option<&mut Window>, cx: &mut App) {
        if let Some(preview) = self.browser_preview.take()
            && let Some(frame) = preview.viewport.read(cx).image.clone()
        {
            cx.drop_image(frame, window);
        }
    }

    fn browser_owner(&self) -> Option<&str> {
        if !self.front || self.link.remote().is_some() {
            return None;
        }
        let store = self.store.as_ref()?;
        (store.state() == &Connection::Ready)
            .then(|| store.selected())
            .flatten()
    }

    pub(super) fn browser_preview_is_current(&self) -> bool {
        self.browser_preview
            .as_ref()
            .is_some_and(|preview| self.browser_owner() == Some(preview.owner.as_str()))
    }

    fn toggle_browser_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.browser_preview.is_some() {
            self.clear_browser_preview(Some(window), cx);
            cx.notify();
            return;
        }
        let Some(owner) = self.browser_owner().map(str::to_owned) else {
            return;
        };
        self.review.open = false;
        let project = self.project.clone();
        let label = session_label(&self.route, &owner);
        let viewport = cx.new(|cx| {
            let mut view = Viewport {
                project,
                label,
                image: None,
                error: None,
                reader: None,
            };
            view.start(window, cx);
            view
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.browser_preview = Some(BrowserPreview {
            owner,
            viewport,
            focus,
            expanded: false,
        });
        cx.notify();
    }

    pub(super) fn browser_preview_toggle(&self, cx: &Context<Self>) -> Option<AnyElement> {
        self.browser_owner()?;
        let p = Palette::get(cx);
        Some(
            button(
                p,
                "browser-preview-toggle",
                "Preview",
                false,
                true,
                cx.listener(|s, _, window, cx| s.toggle_browser_preview(window, cx)),
            )
            .into_any_element(),
        )
    }

    pub(super) fn browser_preview_panel(
        &self,
        narrow: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let preview = self.browser_preview.as_ref()?;
        let p = Palette::get(cx);
        let full_width = narrow || preview.expanded;
        let close = button(
            p,
            "browser-preview-close",
            if narrow {
                "Back to thread"
            } else {
                "Close Preview"
            },
            false,
            true,
            cx.listener(|s, _, window, cx| {
                s.clear_browser_preview(Some(window), cx);
                s.focus_editor_for = Some(
                    s.store
                        .as_ref()
                        .and_then(Store::selected)
                        .map(str::to_owned),
                );
                cx.notify();
            }),
        );
        let expand = (!narrow).then(|| {
            button(
                p,
                "browser-preview-expand",
                if preview.expanded {
                    "Dock Preview"
                } else {
                    "Expand Preview"
                },
                false,
                true,
                cx.listener(|s, _, window, cx| {
                    if let Some(preview) = s.browser_preview.as_mut() {
                        preview.expanded = !preview.expanded;
                        window.focus(&preview.focus, cx);
                    }
                    cx.notify();
                }),
            )
        });
        Some(
            col()
                .id("browser-preview")
                .track_focus(&preview.focus)
                .on_key_down(cx.listener(move |s, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        if let Some(preview) = s.browser_preview.as_mut()
                            && preview.expanded
                            && !narrow
                        {
                            preview.expanded = false;
                            window.focus(&preview.focus, cx);
                        } else {
                            s.clear_browser_preview(Some(window), cx);
                            s.focus_editor_for = Some(
                                s.store
                                    .as_ref()
                                    .and_then(Store::selected)
                                    .map(str::to_owned),
                            );
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .flex_shrink_0()
                .min_w_0()
                .h_full()
                .when(full_width, |panel| panel.w_full())
                .when(!full_width, |panel| panel.w(px(WIDTH)))
                .bg(rgb(p.panel))
                .border_l_1()
                .border_color(rgb(p.line))
                .p_3()
                .gap_3()
                .child(
                    row()
                        .flex_wrap()
                        .gap_2()
                        .justify_between()
                        .child("Preview")
                        .child(row().gap_2().children(expand).child(close)),
                )
                .child(preview.viewport.clone())
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::store::Route;

    #[test]
    fn session_labels_are_stable_and_scoped_to_conversation_and_route() {
        let route = &Route::CODEX_SUBSCRIPTION;
        assert_eq!(
            session_label(route, "desk-0000"),
            session_label(route, "desk-0000")
        );
        assert_ne!(
            session_label(route, "desk-0000"),
            session_label(route, "desk-0001")
        );
        assert_ne!(
            session_label(route, "desk-0000"),
            session_label(&Route::codex_api("example".into()), "desk-0000")
        );
    }
}

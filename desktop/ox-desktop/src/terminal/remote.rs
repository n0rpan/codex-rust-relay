//! The same signed PTY review and screen stream used by paired phones.
use super::{FONT, dimensions, key_bytes};
use crate::{
    appearance::Palette,
    kit::{button, col, small},
    platform::hosts::SavedHost,
};
use gpui::{prelude::*, *};
use gpui_component::Root;
use ox_host_web::terminal_protocol::{Event, Input, OpenTerminal};

pub(crate) fn open(host: SavedHost, request: OpenTerminal, cx: &mut App) -> Result<(), String> {
    let title = format!("OX Terminal · {}", host.name);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(800.), px(490.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        move |w, cx| {
            let panel = cx.new(|cx| {
                let focus = cx.focus_handle();
                focus.focus(w, cx);
                let mut panel = Panel {
                    focus,
                    input: None,
                    preview: None,
                    running: false,
                    screen: String::new(),
                    status: format!("Connecting to {}", host.name),
                    size: None,
                };
                panel.connect(host, request, cx);
                panel
            });
            cx.new(|cx| Root::new(panel, w, cx))
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
struct Panel {
    focus: FocusHandle,
    input: Option<flume::Sender<Input>>,
    preview: Option<String>,
    running: bool,
    screen: String,
    status: String,
    size: Option<(u16, u16)>,
}
impl Panel {
    fn connect(&mut self, host: SavedHost, request: OpenTerminal, cx: &mut Context<Self>) {
        let task = cx.background_spawn(async move {
            ox_host_web::client::runtime()?.block_on(host.client()?.terminal(request))
        });
        cx.spawn(async move |this, cx| {
            let (input, events) = match task.await {
                Ok(pair) => pair,
                Err(e) => {
                    let _ = this.update(cx, |s, cx| {
                        s.status = e.to_string();
                        cx.notify()
                    });
                    return;
                }
            };
            if this.update(cx, |s, _| s.input = Some(input)).is_err() {
                return;
            }
            let mut finished = false;
            while let Ok(v) = events.recv_async().await {
                let end = matches!(v, Event::Exit { .. } | Event::Error { .. });
                if this
                    .update(cx, |s, cx| {
                        match v {
                            Event::Unknown(_) => return,
                            Event::Review { review, cwd } => {
                                s.preview = Some(review);
                                s.status = format!("Run on host · {}", cwd.display());
                            }
                            Event::Running { running } => {
                                s.running = running;
                                s.status.clear();
                            }
                            Event::Screen { screen, .. } => s.screen = screen,
                            Event::Error { error } => s.status = error,
                            Event::Exit { exit } => {
                                s.status = exit.map_or_else(
                                    || "Terminal closed".into(),
                                    |n| format!("Exited with status {n}"),
                                )
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                if end {
                    finished = true;
                    break;
                }
            }
            let _ = this.update(cx, |s, cx| {
                if !finished {
                    s.status = "Host terminal disconnected".into();
                }
                s.running = false;
                s.preview = None;
                s.input = None;
                cx.notify()
            });
        })
        .detach()
    }
    fn send(&mut self, frame: Input) -> bool {
        self.input
            .as_ref()
            .is_some_and(|tx| tx.try_send(frame).is_ok())
    }
    fn key(&mut self, e: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.running {
            return;
        }
        let k = &e.keystroke;
        let text =
            if k.key == "v" && (k.modifiers.platform || k.modifiers.control && k.modifiers.shift) {
                cx.read_from_clipboard().and_then(|c| c.text()).map(|s| {
                    s.replace('\x1b', "")
                        .replace("\r\n", "\r")
                        .replace('\n', "\r")
                })
            } else {
                key_bytes(k, false).and_then(|v| String::from_utf8(v).ok())
            };
        if let Some(text) = text {
            let mut tail = text.as_str();
            while !tail.is_empty() {
                let end = tail.floor_char_boundary(8192.min(tail.len()));
                if !self.send(Input::Keys {
                    text: tail[..end].into(),
                }) {
                    self.status = "Terminal input is full.".into();
                    cx.notify();
                    break;
                }
                tail = &tail[end..];
            }
            cx.stop_propagation()
        }
    }
}
impl Render for Panel {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let mut view = col()
            .size_full()
            .bg(rgb(p.bg))
            .text_color(rgb(p.fg))
            .p_3()
            .gap_2()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key));
        if !self.status.is_empty() {
            view = view.child(small(p, self.status.clone()));
        }
        if let Some(preview) = self.preview.clone() {
            view = view.child(preview).child(button(
                p,
                "start-remote-terminal",
                "Run",
                true,
                true,
                cx.listener(|s, _, w, cx| {
                    if s.send(Input::Run) {
                        s.preview = None;
                        s.status = "Starting…".into();
                    }
                    s.focus.focus(w, cx);
                    cx.notify()
                }),
            ));
        }
        if self.running {
            let (rows, cols) = dimensions(w, cx);
            let sz = (rows.clamp(8, 60), cols.clamp(20, 180));
            if self.size != Some(sz)
                && self.send(Input::Resize {
                    rows: sz.0,
                    cols: sz.1,
                })
            {
                self.size = Some(sz);
            }
        }
        view.child(
            div()
                .flex_1()
                .font_family(FONT)
                .text_size(px(13.))
                .line_height(px(18.))
                .child(self.screen.clone()),
        )
    }
}

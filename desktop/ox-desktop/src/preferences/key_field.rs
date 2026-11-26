//! A paste/type-only field for ASCII API credentials, with no text editor or undo history.
use crate::appearance::Palette;
use crate::appearance::Step;
use gpui::{prelude::*, *};
use ox_desk_core::settings_protocol::model::Secret;

pub struct KeyField {
    focus: FocusHandle,
    value: Secret,
    selected: bool,
}
impl KeyField {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            value: Secret::default(),
            selected: false,
        }
    }
    pub fn take(&mut self, cx: &mut Context<Self>) -> Secret {
        self.selected = false;
        cx.notify();
        std::mem::take(&mut self.value)
    }
    pub fn restore(&mut self, value: Secret, cx: &mut Context<Self>) {
        self.value = value;
        cx.notify()
    }
    fn insert(&mut self, text: &str) {
        if self.selected {
            self.value.clear();
            self.selected = false
        }
        self.value.append(text)
    }
}
impl Focusable for KeyField {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for KeyField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::app::guard(cx, |cx| self.draw(window, cx).into_any_element())
    }
}

impl KeyField {
    fn draw(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let focused = self.focus.is_focused(window);
        let empty = self.value.expose().is_empty();
        div()
            .id("secure-entry")
            .track_focus(&self.focus)
            .tab_stop(true)
            .cursor_text()
            .border_1()
            .border_color(rgb(if focused { p.accent } else { p.line }))
            .rounded_md()
            .p_2()
            .text_size(p.text(Step::Ui))
            .text_color(rgb(if empty { p.muted } else { p.fg }))
            .bg(rgb(if self.selected { p.hover } else { p.bg }))
            .child(if empty {
                "Enter API key".to_owned()
            } else {
                "•".repeat(self.value.expose().len().min(48))
            })
            .on_click(cx.listener(|s, _, w, cx| {
                s.focus.focus(w, cx);
                cx.notify()
            }))
            .on_key_down(cx.listener(|s, e: &KeyDownEvent, _, cx| {
                let k = &e.keystroke;
                if k.modifiers.platform || k.modifiers.control {
                    match k.key.as_ref() {
                        "a" => s.selected = true,
                        "v" => {
                            if let Some(mut pasted) =
                                cx.read_from_clipboard().and_then(|i| i.text())
                            {
                                s.insert(&pasted);
                                // SAFETY: erase this temporary owned clipboard copy before dropping it.
                                unsafe {
                                    for b in pasted.as_mut_vec() {
                                        std::ptr::write_volatile(b, 0)
                                    }
                                }
                            }
                        }
                        "c" | "x" | "z" | "y" => {}
                        _ => return,
                    }
                } else if k.key == "backspace" || k.key == "delete" {
                    if s.selected {
                        s.value.clear();
                        s.selected = false
                    } else {
                        s.value.backspace()
                    }
                } else if let Some(text) = &k.key_char {
                    s.insert(text)
                } else {
                    return;
                }
                cx.stop_propagation();
                cx.notify()
            }))
    }
}

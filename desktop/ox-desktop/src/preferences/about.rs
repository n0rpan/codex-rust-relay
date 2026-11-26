use crate::appearance::Step;
use crate::{appearance::Palette, kit, platform::versions, version::VERSION};
use gpui::{prelude::*, *};

pub(super) struct About {
    companion: Result<String, String>,
    checking: bool,
    notice: Option<String>,
}
impl About {
    pub(super) fn new(cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { versions::companion() })
                .await;
            let _ = this.update(cx, |s, cx| {
                s.companion = result;
                cx.notify();
            });
        })
        .detach();
        Self {
            companion: Ok(String::new()),
            checking: false,
            notice: None,
        }
    }

    fn check(&mut self, cx: &mut Context<Self>) {
        if self.checking {
            return;
        }
        self.checking = true;
        self.notice = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async { versions::check() }).await;
            let _ = this.update(cx, |s, cx| {
                s.checking = false;
                s.notice = Some(match result {
                    Ok(update) => match update.available {
                        Some(version) => format!("OX {version} is available. Run the installer with --desktop to update the pair."),
                        None => format!("OX {} is up to date.", update.installed),
                    },
                    Err(message) => message,
                });
                cx.notify();
            });
        }).detach();
    }
}
impl Render for About {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::app::guard(cx, |cx| self.draw(cx).into_any_element())
    }
}

impl About {
    fn draw(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let (label, mismatch) = match &self.companion {
            Ok(v) if v.is_empty() => (format!("OX Desktop {VERSION} · Checking companion…"), false),
            Ok(v) if v == VERSION => (format!("OX Desktop and ox CLI {VERSION}"), false),
            Ok(v) => (
                format!(
                    "Version mismatch. OX Desktop {VERSION} · ox CLI {v}. Reinstall with --desktop to match them."
                ),
                true,
            ),
            Err(e) => (format!("OX Desktop {VERSION} · {e}"), true),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(rgb(p.line))
            .bg(rgb(p.panel))
            .child(
                div()
                    .text_size(p.text(Step::Ui))
                    .text_color(rgb(if mismatch { p.warning } else { p.fg }))
                    .child(label),
            )
            .child(div().child(kit::button(
                p,
                "ox-update-check",
                if self.checking {
                    "Checking…"
                } else {
                    "Check for updates"
                },
                false,
                !self.checking && matches!(&self.companion, Ok(v) if v == VERSION),
                cx.listener(|s, _, _, cx| s.check(cx)),
            )))
            .children(
                self.notice
                    .clone()
                    .map(|text| div().text_size(p.text(Step::Ui)).child(text)),
            )
            .child(
                div()
                    .pt_2()
                    .text_size(p.text(Step::Meta))
                    .text_color(rgb(p.muted))
                    .child("Made by Oscar"),
            )
            .child(
                div()
                    .text_size(p.text(Step::Meta))
                    .text_color(rgb(p.muted))
                    .child("Weather data from MET Norway (CC BY 4.0)"),
            )
    }
}

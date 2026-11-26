//! The friendly empty state: what the main area shows before any thread is selected.
//! Composer first: the ox, the greeting and the project, then the composer itself.
use super::Workbench;
use crate::appearance::{Palette, Step};
use crate::kit::{SPACE_L, col};
use gpui::{prelude::FluentBuilder, *};

/// Ask the host to open its folder picker, so a user with no thread can open a project.
pub(in crate::views) struct OpenProject;
impl EventEmitter<OpenProject> for Workbench {}

impl Workbench {
    /// The empty main area: the ox above "What shall we work on?", the project name and
    /// `dock`, the centred composer column. Typing in it starts the new session.
    pub(super) fn empty_state(&mut self, p: Palette, dock: Div, cx: &mut Context<Self>) -> Div {
        let project = self
            .project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty());
        let frame = self.mascot_frame;
        col()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .px(px(SPACE_L))
            .child(
                div()
                    .id("empty-mascot")
                    .cursor_pointer()
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new("READY PLAYER ONE · Click to trot")
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|s, _, _, cx| {
                        s.mascot_frame = (s.mascot_frame + 1) % 8;
                        cx.notify();
                    }))
                    .child(crate::activity::ox(p, frame, 4.)),
            )
            .child(
                div()
                    .mt(px(SPACE_L))
                    .text_size(p.text(Step::Title) * 1.1)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(p.fg))
                    .child("What shall we work on?"),
            )
            .when_some(project, |view, name| {
                view.child(
                    div()
                        .mt(px(4.))
                        .text_size(p.text(Step::Meta))
                        .text_color(rgb(p.muted))
                        .child(name),
                )
            })
            .child(div().mt(px(24.)).w_full().child(dock))
            .child(
                div()
                    .mt(px(12.))
                    .font_family(crate::appearance::LABEL_FONT)
                    .text_size(p.text(Step::Label) * 0.9)
                    .text_color(rgb(p.quiet()))
                    .child("PASTE OR DROP SCREENSHOTS & FILES · @ TO MENTION · / FOR COMMANDS"),
            )
    }
}

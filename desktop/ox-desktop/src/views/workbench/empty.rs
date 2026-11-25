//! The friendly empty state: what the main area shows before any thread is selected.
use super::Workbench;
use crate::appearance::{Palette, Step};
use crate::kit::{button, col, row, small};
use gpui::{prelude::FluentBuilder, *};
use ox_desk_core::actions::Action as DeskAction;

/// Ask the host to open its folder picker, so a user with no thread can open a project.
pub(in crate::views) struct OpenProject;
impl EventEmitter<OpenProject> for Workbench {}

impl Workbench {
    /// Put `text` in the new-session composer and focus it.
    fn prefill_composer(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.editors.get(&None) {
            editor.update(cx, |e, cx| {
                if !text.is_empty() {
                    e.set_value(text, window, cx);
                }
                e.focus(window, cx);
            });
        }
    }

    /// The empty main area: the ox and three ways to begin.
    pub(super) fn empty_state(&mut self, p: Palette, cx: &mut Context<Self>) -> Div {
        let project = self
            .project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty());
        col()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_3()
            .child(crate::activity::ox(p, 0, 4.))
            .child(
                div()
                    .text_size(p.text(Step::Title))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("What shall we work on?"),
            )
            .when_some(project, |view, name| {
                view.child(small(p, format!("Project: {name}")))
            })
            .child(
                row()
                    .flex_wrap()
                    .justify_center()
                    .gap_2()
                    .child(button(
                        p,
                        "empty-new-session",
                        "New session",
                        true,
                        true,
                        cx.listener(|s, _, window, cx| {
                            s.action(cx, DeskAction::NewDraft);
                            s.prefill_composer("", window, cx);
                        }),
                    ))
                    .child(button(
                        p,
                        "empty-start-worker",
                        "Start worker",
                        false,
                        true,
                        cx.listener(|s, _, window, cx| s.prefill_composer("/worker ", window, cx)),
                    ))
                    .child(button(
                        p,
                        "empty-open-project",
                        "Open project…",
                        false,
                        true,
                        cx.listener(|_, _, _, cx| cx.emit(OpenProject)),
                    )),
            )
    }
}

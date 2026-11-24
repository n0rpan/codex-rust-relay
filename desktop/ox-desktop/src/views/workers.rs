//! Render the controls and conversation supplied by the owning worker runtime.
use super::workbench::Workbench;
use crate::{appearance::Palette, col, kit::button, row, small};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::input::{Textarea, TextareaState};
use ox_desk_core::{
    actions::Action,
    worker::{WorkerAction, WorkerView},
};

impl Workbench {
    pub(super) fn worker_transcript(&self, cx: &mut Context<Self>) -> Option<Div> {
        let store = self.store.as_ref()?;
        let id = store.selected()?;
        let worker = store.job(id)?.view();
        let p = Palette::get(cx);
        let parent = worker.parent.to_string();
        let body = col()
            .id(SharedString::from(format!("worker-history-{id}")))
            .overflow_y_scroll()
            .flex_1()
            .gap_2()
            .child(
                row()
                    .items_center()
                    .justify_between()
                    .child(worker.title.to_owned())
                    .child(button(
                        p,
                        "worker-parent",
                        "Parent thread",
                        false,
                        true,
                        cx.listener(move |s, _, _, cx| {
                            s.action(cx, Action::SelectThread(parent.clone()))
                        }),
                    )),
            )
            .child(small(
                p,
                format!("{} · {} · {}", worker.runtime, worker.model, worker.status),
            ))
            .children(worker.messages.iter().map(|m| {
                col()
                    .gap_1()
                    .child(small(p, m.role.clone()))
                    .child(m.text.clone())
            }))
            .when(!worker.progress.is_empty(), |d| {
                d.child(small(p, worker.progress.to_owned()))
            })
            .when_some(worker.result, |d, r| d.child(r.to_owned()))
            .when_some(worker.error, |d, e| d.child(small(p, e.to_owned())));
        Some(col().flex_1().min_h_0().child(body))
    }
}
pub(super) fn controls(
    worker: WorkerView<'_>,
    id: &str,
    editor: Entity<TextareaState>,
    draft: &str,
    p: Palette,
    cx: &mut Context<Workbench>,
) -> Div {
    let refresh = id.to_owned();
    let stop = id.to_owned();
    let steer = id.to_owned();
    let text = draft.to_owned();
    col()
        .gap_2()
        .p_3()
        .border_1()
        .border_color(rgb(p.line))
        .bg(rgb(p.panel))
        .child(Textarea::new(&editor).appearance(false))
        .child(
            row()
                .gap_2()
                .child(button(
                    p,
                    "worker-refresh",
                    "Refresh",
                    false,
                    worker.can_refresh,
                    cx.listener(move |s, _, _, cx| {
                        s.action(
                            cx,
                            Action::WorkerControl(refresh.clone(), WorkerAction::Refresh),
                        )
                    }),
                ))
                .child(button(
                    p,
                    "worker-stop",
                    "Stop",
                    false,
                    worker.can_stop,
                    cx.listener(move |s, _, _, cx| {
                        s.action(cx, Action::WorkerControl(stop.clone(), WorkerAction::Stop))
                    }),
                ))
                .child(button(
                    p,
                    "worker-steer",
                    "Steer",
                    true,
                    worker.can_steer && !draft.trim().is_empty(),
                    cx.listener(move |s, _, _, cx| {
                        s.action(
                            cx,
                            Action::WorkerControl(steer.clone(), WorkerAction::Steer(text.clone())),
                        )
                    }),
                )),
        )
}

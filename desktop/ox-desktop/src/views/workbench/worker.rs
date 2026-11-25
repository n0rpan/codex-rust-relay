//! A worker session says what it is: its job number, the access ceiling it was
//! launched under, and the session that started it. A parent says how many of its
//! workers are still running.
use super::{OpenThread, Workbench};
use crate::appearance::Palette;
use crate::appearance::Step;
use crate::views::host::TreeIndex;
use crate::{activity, row};
use gpui::{prelude::FluentBuilder, *};

impl Workbench {
    /// The crumb under the title of an open worker: "Parent title ▸ Worker 41 · Ceiling write".
    /// Clicking the parent asks the host to open it.
    pub(super) fn worker_crumb(&self, p: Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let store = self.store.as_ref()?;
        let id = store.selected()?;
        let origin = store.origin(id)?;
        let parent = origin.parent.session.clone();
        let parent_title = parent
            .as_deref()
            .and_then(|pid| store.threads().iter().find(|t| t.id == pid))
            .map(|t| t.title().to_owned());
        let job = format!("Worker {}", origin.job_id);
        let ceiling = format!("Ceiling {}", origin.ceiling.label(origin.runtime));
        Some(
            row()
                .flex_wrap()
                .gap_2()
                .items_center()
                .text_size(p.text(Step::Ui))
                .text_color(rgb(p.fg))
                .when_some(parent, |line, parent| {
                    line.child(
                        div()
                            .id("worker-parent")
                            .cursor_pointer()
                            .text_color(rgb(p.accent))
                            .child(parent_title.unwrap_or_else(|| "Parent session".into()))
                            .on_click(
                                cx.listener(move |_, _, _, cx| cx.emit(OpenThread(parent.clone()))),
                            ),
                    )
                    .child(crate::small(p, "▸"))
                })
                .child(div().px_1().border_1().border_color(rgb(p.line)).child(job))
                .child(crate::small(p, ceiling))
                .into_any_element(),
        )
    }

    /// "2 workers running" under the parent's activity line, linking to the first one.
    pub(super) fn workers_line(&self, p: Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let store = self.store.as_ref()?;
        let id = store.selected()?;
        let tree = cx
            .try_global::<TreeIndex>()?
            .0
            .get(&(self.project.clone(), id.to_owned()))?;
        if tree.running == 0 {
            return None;
        }
        let label = match tree.running {
            1 => "1 worker running".to_owned(),
            n => format!("{n} workers running"),
        };
        let first = tree.first_running.clone();
        Some(
            row()
                .id("workers-running")
                .gap_2()
                .py_1()
                .cursor_pointer()
                .child(activity::Scene::Worker.sprite(p, 0))
                .child(crate::small(p, label))
                .when_some(first, |line, first| {
                    line.on_click(
                        cx.listener(move |_, _, _, cx| cx.emit(OpenThread(first.clone()))),
                    )
                })
                .into_any_element(),
        )
    }
}

//! The review is always addressed by the selected conversation and turn.
use super::Workbench;
use crate::{
    appearance::Palette,
    kit::{button, col, row, small},
};
use gpui::*;
use ox_desk_core::actions::Action;
use std::collections::HashMap;
mod patch;
use patch::Cursor;

#[derive(Default)]
pub(super) struct ReviewPane {
    pub(super) open: bool,
    pinned: HashMap<String, String>,
    expanded: HashMap<(String, String, String), Vec<Cursor>>,
}

impl Workbench {
    pub(super) fn review_layout_changed(&self, cx: &mut Context<Self>) {
        cx.emit(super::CatalogChanged);
    }
    pub(in crate::views) fn toggle_review(&mut self, cx: &mut Context<Self>) {
        if self
            .store
            .as_ref()
            .is_some_and(|s| s.selected().and_then(|id| s.transcript(id)).is_some())
        {
            self.review.open = !self.review.open;
            self.review_layout_changed(cx);
            cx.notify();
        }
    }
    pub(super) fn review_toggle(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let store = self.store.as_ref()?;
        let owner = store.selected()?;
        store.transcript(owner)?;
        Some(
            button(
                Palette::get(cx),
                "toggle-review",
                "Changes & plan",
                self.review.open,
                true,
                cx.listener(|s, _, _, cx| {
                    s.toggle_review(cx);
                }),
            )
            .into_any_element(),
        )
    }

    pub(super) fn review_panel(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.review.open {
            return None;
        }
        let store = self.store.as_ref()?;
        let owner = store.selected()?;
        let transcript = store.transcript(owner)?;
        let p = Palette::get(cx);
        let mut pane =
            col()
                .id(SharedString::from(format!("turn-review-{owner}")))
                .overflow_y_scroll()
                .h_full()
                .min_w_0()
                .gap_3()
                .p_3()
                .bg(rgb(p.panel))
                .text_color(rgb(p.fg))
                .border_l_1()
                .border_color(rgb(p.line))
                .child(row().justify_between().child("Changes & plan").child(
                    crate::kit::icon_button(
                        p,
                        "close-review",
                        "×",
                        "Close changes and plan",
                        cx.listener(|s, _, _, cx| {
                            s.review.open = false;
                            s.review_layout_changed(cx);
                            cx.notify();
                        }),
                    ),
                ));
        let children = store
            .threads()
            .iter()
            .filter(|r| r.parent_thread_id.as_deref() == Some(owner));
        for (i, child) in children.enumerate() {
            if i == 0 {
                pane = pane.child(div().text_lg().child("Workers"));
            }
            let id = child.id.clone();
            let title = child.title().to_owned();
            pane = pane.child(button(
                p,
                ("review-worker", i),
                title,
                false,
                true,
                cx.listener(move |s, _, _, cx| s.action(cx, Action::SelectThread(id.clone()))),
            ));
        }
        if transcript.has_older() {
            let id = owner.to_owned();
            pane = pane.child(button(
                p,
                "older-review-turns",
                "Load earlier turns",
                false,
                !transcript.is_loading(),
                cx.listener(move |s, _, _, cx| s.action(cx, Action::LoadOlder(id.clone()))),
            ));
        }
        let turns = transcript.reviews.turns().collect::<Vec<_>>();
        let selected = self
            .review
            .pinned
            .get(owner)
            .and_then(|pin| turns.iter().position(|(id, _)| id == pin))
            .unwrap_or(0);
        let Some(&(turn, review)) = turns.get(selected) else {
            return Some(
                pane.child(small(p, "No changes or plan in the loaded turns."))
                    .into_any_element(),
            );
        };
        if turns.len() > 1 {
            let navigation = |id: &'static str, label: &'static str, target: Option<usize>| {
                let thread = owner.to_owned();
                let pin = target.filter(|&i| i != 0).map(|i| turns[i].0.to_owned());
                button(
                    p,
                    id,
                    label,
                    false,
                    target.is_some(),
                    cx.listener(move |s, _, _, cx| {
                        match &pin {
                            Some(turn) => {
                                s.review.pinned.insert(thread.clone(), turn.clone());
                            }
                            None => {
                                s.review.pinned.remove(&thread);
                            }
                        }
                        cx.notify();
                    }),
                )
            };
            pane = pane.child(
                row()
                    .gap_2()
                    .child(navigation(
                        "earlier-review",
                        "Earlier",
                        (selected + 1 < turns.len()).then_some(selected + 1),
                    ))
                    .child(small(
                        p,
                        format!("{} of {}", turns.len() - selected, turns.len()),
                    ))
                    .child(navigation("later-review", "Later", selected.checked_sub(1))),
            );
        }
        if !review.has_content() {
            return Some(
                pane.child(small(p, "No changes or plan in this turn."))
                    .into_any_element(),
            );
        }
        if !review.steps.is_empty() || review.explanation.is_some() || !review.plan_text.is_empty()
        {
            pane = pane.child(div().text_lg().child("Plan"));
            if let Some(text) = &review.explanation {
                pane = pane.child(text.clone());
            }
            for step in &review.steps {
                let (label, color) = match step.status.as_str() {
                    "completed" => ("Done", p.accent),
                    "inProgress" | "in_progress" => ("In progress", p.fg),
                    "pending" => ("Pending", p.muted),
                    other => (other, p.muted),
                };
                pane = pane.child(
                    col()
                        .child(small(p, label.to_owned()).text_color(rgb(color)))
                        .child(step.text.clone()),
                );
            }
            if !review.plan_text.is_empty() {
                pane = pane.child(review.plan_text.clone());
            }
        }
        let files = review.file_groups();
        if !files.is_empty() {
            let thread = owner.to_owned();
            let turn = turn.to_owned();
            pane = pane.child(
                row()
                    .justify_between()
                    .child(div().text_lg().child(format!("Files ({})", files.len())))
                    .child(button(
                        p,
                        "copy-turn-patch",
                        "Copy turn patch",
                        false,
                        true,
                        cx.listener(move |s, _, _, cx| {
                            if let Some(review) = s
                                .store
                                .as_ref()
                                .and_then(|store| store.transcript(&thread))
                                .and_then(|t| t.reviews.get(&turn))
                            {
                                cx.write_to_clipboard(ClipboardItem::new_string(review.patch()));
                            }
                        }),
                    )),
            );
        }
        for (index, (path, patches)) in files.into_iter().enumerate() {
            let key = (owner.to_owned(), turn.to_owned(), path.to_owned());
            let pages = self.review.expanded.get(&key);
            let expanded = pages.is_some();
            let toggle_key = key.clone();
            let path_label = std::path::Path::new(path)
                .strip_prefix(&self.project)
                .ok()
                .and_then(|p| p.to_str())
                .unwrap_or(path);
            let bodies = patches.iter().map(|p| p.diff.clone()).collect::<Vec<_>>();
            let mut file = col()
                .gap_1()
                .border_t_1()
                .border_color(rgb(p.line))
                .pt_2()
                .child(
                    row()
                        .gap_2()
                        .justify_between()
                        .child(
                            button(
                                p,
                                ("review-file", index),
                                format!("{} {path_label}", if expanded { "▾" } else { "▸" }),
                                false,
                                true,
                                cx.listener(move |s, _, _, cx| {
                                    if s.review.expanded.remove(&toggle_key).is_none() {
                                        s.review.expanded.insert(toggle_key.clone(), Vec::new());
                                    }
                                    cx.notify();
                                }),
                            )
                            .min_w_0()
                            .tooltip(path.to_owned()),
                        )
                        .child(button(
                            p,
                            ("copy-file-patch", index),
                            "Copy patch",
                            false,
                            bodies.iter().any(|s| !s.is_empty()),
                            move |_, _, cx| {
                                let mut text = String::new();
                                for body in &bodies {
                                    if !text.is_empty() && !text.ends_with('\n') {
                                        text.push('\n');
                                    }
                                    text.push_str(body);
                                }
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            },
                        )),
                );
            if let Some(pages) = pages {
                let page = patch::page(&patches, pages.last().copied().unwrap_or_default());
                for line in page.lines {
                    let color = match line.as_bytes().first() {
                        Some(b'+') => p.accent,
                        Some(b'-') => p.error,
                        Some(b'@') => p.warning,
                        _ => p.fg,
                    };
                    file = file.child(
                        div()
                            .w_full()
                            .font_family("monospace")
                            .text_size(px(12.))
                            .text_color(rgb(color))
                            .child(line),
                    );
                }
                if page.next.is_some() || !pages.is_empty() {
                    let back = key.clone();
                    file = file.child(
                        row()
                            .gap_1()
                            .child(button(
                                p,
                                ("prev-patch", index),
                                "Previous lines",
                                false,
                                !pages.is_empty(),
                                cx.listener(move |s, _, _, cx| {
                                    if let Some(stack) = s.review.expanded.get_mut(&back) {
                                        stack.pop();
                                    }
                                    cx.notify()
                                }),
                            ))
                            .child(button(
                                p,
                                ("next-patch", index),
                                "Next lines",
                                false,
                                page.next.is_some(),
                                cx.listener(move |s, _, _, cx| {
                                    if let (Some(stack), Some(next)) =
                                        (s.review.expanded.get_mut(&key), page.next)
                                    {
                                        stack.push(next);
                                    }
                                    cx.notify()
                                }),
                            )),
                    );
                }
            }
            pane = pane.child(file);
        }
        Some(pane.into_any_element())
    }
}

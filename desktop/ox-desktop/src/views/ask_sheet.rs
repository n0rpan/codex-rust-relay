//! Requests keep their own input entities until Codex resolves them.
use super::workbench::Workbench;
use crate::{appearance::Palette, col, kit::button, row, small};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::input::Input;
use ox_desk_core::actions::Action as DeskAction;
use ox_desk_core::{transcript::ItemKey, transcript::item::Item};

impl Workbench {
    pub(super) fn visible_ask_index(&self, store: &ox_desk_core::store::Store) -> usize {
        let asks = store.asks();
        self.focused_ask
            .as_ref()
            .and_then(|id| asks.iter().position(|a| &a.id == id))
            .or_else(|| {
                asks.iter()
                    .position(|a| a.thread.as_deref() == store.selected())
            })
            .unwrap_or(0)
    }
    pub(super) fn ask_sheet(
        &mut self,
        inline: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let store = self.store.as_ref()?;
        let asks = store.asks();
        self.answer_fields
            .retain(|id, _| asks.iter().any(|ask| ask.id == *id));
        let index = self.visible_ask_index(store);
        let ask = asks.get(index)?;
        let p = Palette::get(cx);
        let id = ask.id.clone();
        let owner = ask.thread.as_deref().unwrap_or("Project");
        let title = store
            .threads()
            .iter()
            .find(|t| t.id == owner)
            .map(|t| t.title())
            .unwrap_or(owner);
        let detail = match (ask.thread.as_ref(), ask.turn.as_ref(), ask.item.as_ref()) {
            (Some(thread), Some(turn), Some(item)) => store
                .transcript(thread)
                .and_then(|t| {
                    t.entry(&ItemKey {
                        turn: turn.clone(),
                        item: item.clone(),
                    })
                })
                .and_then(|e| match &e.item {
                    Item::FileChange { changes, .. } => Some(
                        changes
                            .iter()
                            .map(|c| format!("{}\n{}", c.path, c.diff))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    Item::CommandExecution { command, .. } if !ask.detail.contains(command) => {
                        Some(command.clone())
                    }
                    _ => None,
                }),
            _ => None,
        };
        let mut sheet = crate::kit::sheet(p, "ask-sheet")
            .when(inline, |s| {
                s.w_full()
                    .min_h_0()
                    .flex_shrink_1()
                    .h_auto()
                    .max_h(window.viewport_size().height * 0.35)
                    .border_t_1()
            })
            .child(small(
                p,
                if asks.len() == 1 {
                    "1 request".into()
                } else {
                    format!("{} of {} requests", index + 1, asks.len())
                },
            ))
            .child(div().child(ask.title().to_owned()))
            .child(small(p, title.to_owned()))
            .child(small(
                p,
                format!(
                    "{} · {}",
                    store.profile().project.display(),
                    store.profile().route.label()
                ),
            ))
            .when_some(detail, |d, body| {
                d.child(div().font_family("monospace").text_sm().child(body))
            })
            .when(!ask.detail.is_empty(), |d| {
                d.child(
                    div()
                        .font_family("monospace")
                        .text_sm()
                        .child(ask.detail.clone()),
                )
            });
        if asks.len() > 1 {
            let next = asks[(index + 1) % asks.len()].id.clone();
            sheet = sheet.child(button(
                p,
                "next-request",
                "Next request",
                false,
                true,
                cx.listener(move |s, _, _, cx| {
                    s.focused_ask = Some(next.clone());
                    cx.notify();
                }),
            ));
        }
        for (index, decision) in ask.decisions().iter().enumerate() {
            let id = id.clone();
            sheet = sheet.child(button(
                p,
                ("ask-decision", index),
                if index < 4 {
                    format!("{}   {}", index + 1, decision.label)
                } else {
                    decision.label.clone()
                },
                false,
                true,
                cx.listener(move |s, _, _, cx| {
                    s.action(cx, DeskAction::AnswerAsk(id.clone(), index))
                }),
            ));
            if let Some(detail) = &decision.detail {
                sheet = sheet.child(
                    div()
                        .text_sm()
                        .font_family("monospace")
                        .child(detail.clone()),
                );
            }
        }
        if let Some(command) = &ask.command
            && let Some(review) =
                super::terminal_review::Review::approval(command, &store.profile().project)
            && let Some(token) = store.command_review_token(&id)
        {
            let source = super::terminal_review::Source::Approval {
                id: id.clone(),
                token: token.to_owned(),
            };
            sheet = sheet
                .child(small(p, review.directory_label()))
                .child(button(
                    p,
                    "terminal-approval",
                    "Decline and review in OX terminal…",
                    false,
                    self.pending_local_command.is_none(),
                    cx.listener(move |s, _, _, cx| {
                        s.review_tool_command(source.clone(), review.clone(), cx);
                    }),
                ));
        }
        if !ask.questions.is_empty() {
            let fields = self.answer_fields.entry(id.clone()).or_insert_with(|| {
                ask.questions
                    .iter()
                    .map(|q| crate::kit::field("", "", q.is_secret, window, cx))
                    .collect()
            });
            for (i, (q, field)) in ask.questions.iter().zip(fields.iter()).enumerate() {
                sheet = sheet.child(div().child(q.question.clone()));
                let choices = q.options.as_deref().unwrap_or_default();
                for (j, choice) in choices.iter().enumerate() {
                    let multi = q.multi_select;
                    let label = choice.label.clone();
                    let field = field.clone();
                    sheet = sheet.child(
                        col()
                            .child(button(
                                p,
                                SharedString::from(format!("question-{i}-{j}")),
                                label.clone(),
                                if multi {
                                    field
                                        .read(cx)
                                        .value()
                                        .split(", ")
                                        .any(|value| value == label)
                                } else {
                                    field.read(cx).value().as_ref() == label
                                },
                                true,
                                cx.listener(move |_, _, window, cx| {
                                    field.update(cx, |f, cx| {
                                        let value = if multi {
                                            let current = f.value().to_string();
                                            let mut values: Vec<_> = current
                                                .split(", ")
                                                .filter(|s| !s.is_empty())
                                                .collect();
                                            if let Some(pos) =
                                                values.iter().position(|s| *s == label)
                                            {
                                                values.remove(pos);
                                            } else {
                                                values.push(&label);
                                            }
                                            values.join(", ")
                                        } else {
                                            label.clone()
                                        };
                                        f.set_value(value, window, cx)
                                    });
                                    cx.notify();
                                }),
                            ))
                            .child(small(p, choice.description.clone())),
                    );
                }
                if choices.is_empty() || q.is_other {
                    sheet = sheet.child(Input::new(field));
                }
            }
            sheet = sheet.child(row().child(button(
                p,
                "submit-answers",
                "Submit answers",
                true,
                true,
                cx.listener(move |s, _, _, cx| {
                    if let Some(fields) = s.answer_fields.get(&id) {
                        let values: Vec<_> = fields
                            .iter()
                            .map(|f| f.read(cx).value().to_string())
                            .collect();
                        s.action(cx, DeskAction::AnswerQuestions(id.clone(), values));
                    }
                }),
            )));
        }
        Some(sheet.into_any_element())
    }
}

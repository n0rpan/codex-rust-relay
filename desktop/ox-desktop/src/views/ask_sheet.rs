//! Requests keep their own input entities until Codex resolves them.
use super::workbench::Workbench;
use crate::appearance::Step;
use crate::{
    appearance::Palette,
    col,
    kit::{button, copy_message},
    row, small,
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::{Button, ButtonVariant, ButtonVariants},
    input::Input,
};
use ox_desk_core::actions::Action as DeskAction;
use ox_desk_core::{transcript::ItemKey, transcript::item::Item};

/// The "Needs you" marker: blinks once a second for ten seconds, then holds steady. Motion is off
/// with reduced motion or a hidden window; the dot is then simply lit. Only this entity repaints.
struct NeedsBlink {
    started: std::time::Instant,
    lit: bool,
    tick: Option<Task<()>>,
}
const BLINK_SECONDS: u64 = 10;
impl NeedsBlink {
    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            lit: true,
            tick: None,
        }
    }
}
impl Render for NeedsBlink {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let blinking =
            p.motion && window.is_visible() && self.started.elapsed().as_secs() < BLINK_SECONDS;
        if blinking && self.tick.is_none() {
            self.tick = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let _ = this.update(cx, |s, cx| {
                    s.lit = !s.lit;
                    s.tick = None;
                    cx.notify();
                });
            }));
        }
        let lit = !blinking || self.lit;
        div()
            .size(px(8.))
            .flex_shrink_0()
            .rounded_full()
            .bg(rgb(if lit {
                p.warning
            } else {
                crate::appearance::mix(p.bg, p.warning, 30)
            }))
    }
}

/// Header of the approval card, e.g. "NEEDS YOU · 2 OF 3".
fn needs_you_label(index: usize, count: usize) -> String {
    if count <= 1 {
        "NEEDS YOU · 1".into()
    } else {
        format!("NEEDS YOU · {} OF {count}", index + 1)
    }
}

/// Header of the card. A worker waiting for YOLO names its wait instead of the count.
fn card_kicker(yolo: bool, index: usize, count: usize) -> String {
    if yolo {
        "Worker waits for YOLO".into()
    } else {
        needs_you_label(index, count)
    }
}

/// Session-wide or always-on grants. They are styled apart from one-off decisions.
fn is_broad_grant(label: &str) -> bool {
    let label = label.to_ascii_lowercase();
    label.starts_with("allow")
        && ["always", "session", "turn", "yolo", "forever"]
            .iter()
            .any(|word| label.contains(word))
}

/// A pre-formatted block of command or diff text with a fixed inset look.
fn code_block(p: Palette, id: impl Into<ElementId>, text: String) -> Stateful<Div> {
    div()
        .id(id)
        .overflow_y_scroll()
        .max_h(px(240.))
        .p_2()
        .rounded(px(2.))
        .border_1()
        .border_color(rgb(p.line))
        .bg(rgb(p.bg))
        .font_family("monospace")
        .text_size(p.text(Step::Ui))
        .text_color(rgb(p.fg))
        .child(text)
}

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

        // Code the person is asked to approve: the resolved command or diff, then the request detail.
        let mut code: Vec<String> = detail.into_iter().collect();
        if !ask.detail.is_empty() {
            code.push(ask.detail.clone());
        }

        let mut body = col().gap_2().flex_1().min_w_0();
        let blink = window.use_keyed_state(
            SharedString::from(format!("needs-blink-{:?}", ask.id)),
            cx,
            |_, _| NeedsBlink::new(),
        );
        body = body
            .child(
                row().gap_2().items_center().child(blink).child(
                    div()
                        .text_size(p.text(Step::Label))
                        .text_color(rgb(p.warning))
                        .font_family("monospace")
                        .child(card_kicker(ask.waits_for_yolo(), index, asks.len())),
                ),
            )
            .child(
                div()
                    .text_size(p.text(Step::Title))
                    .line_height(px(28.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(p.fg))
                    .child(ask.title().to_owned()),
            )
            .child(small(
                p,
                format!(
                    "{} · {} · {}",
                    title,
                    store.profile().project.display(),
                    store.profile().route.label()
                ),
            ));
        if !code.is_empty() {
            body = body.child(
                row()
                    .w_full()
                    .justify_between()
                    .child(small(p, "Request"))
                    .child(copy_message(p, "ask-copy", &code.join("\n"))),
            );
            for (i, text) in code.into_iter().enumerate() {
                body = body.child(code_block(p, ("ask-code", i), text));
            }
        }
        if asks.len() > 1 {
            let next = asks[(index + 1) % asks.len()].id.clone();
            body = body.child(button(
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

        // One-off decisions first, then broad grants below a hairline. Key hints keep the original indices.
        let decisions = ask.decisions();
        let mut order: Vec<usize> = (0..decisions.len()).collect();
        order.sort_by_key(|&i| is_broad_grant(&decisions[i].label));
        let first_broad = order
            .iter()
            .position(|&i| is_broad_grant(&decisions[i].label));
        for (position, &index) in order.iter().enumerate() {
            let decision = &decisions[index];
            let broad = is_broad_grant(&decision.label);
            if position > 0 && first_broad == Some(position) {
                body = body.child(div().w_full().h(px(1.)).bg(rgb(p.line)));
            }
            let tone = if broad { p.warning } else { p.fg };
            let mut label_row = row()
                .w_full()
                .justify_between()
                .gap_2()
                .child(div().text_color(rgb(tone)).child(decision.label.clone()));
            if index < 4 {
                label_row = label_row.child(
                    div()
                        .font_family("monospace")
                        .text_size(p.text(Step::Label))
                        .px_1()
                        .border_1()
                        .border_color(rgb(if broad { p.warning } else { p.muted }))
                        .text_color(rgb(if broad { p.warning } else { p.muted }))
                        .child(format!("[{}]", index + 1)),
                );
            }
            let id = id.clone();
            let action = Button::new(("ask-decision", index))
                .with_variant(ButtonVariant::Secondary)
                .h(px(32.))
                .w_full()
                .border_1()
                .border_color(rgb(if broad { p.warning } else { p.line }))
                .tab_stop(true)
                .on_click(cx.listener(move |s, _, _, cx| {
                    s.action(cx, DeskAction::AnswerAsk(id.clone(), index))
                }))
                .child(label_row);
            body = body.child(if broad { action.outline() } else { action });
            if let Some(detail) = &decision.detail {
                body = body.child(small(p, detail.clone()));
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
            body = body.child(small(p, review.directory_label())).child(button(
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
                body = body.child(div().child(q.question.clone()));
                let choices = q.options.as_deref().unwrap_or_default();
                for (j, choice) in choices.iter().enumerate() {
                    let multi = q.multi_select;
                    let label = choice.label.clone();
                    let field = field.clone();
                    body = body.child(
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
                    body = body.child(Input::new(field));
                }
            }
            body = body.child(row().child(button(
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

        // The amber rule is the card's signature: the inline sheet stays flat, with no gradient or shadow.
        let rule = div().w(px(3.)).flex_shrink_0().bg(rgb(p.warning));
        let sheet = crate::kit::sheet(p, "ask-sheet")
            .when(inline, |s| {
                s.w_full()
                    .min_h_0()
                    .flex_shrink_1()
                    .h_auto()
                    .max_h(window.viewport_size().height * 0.35)
                    .border_t_1()
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_stretch()
                    .child(rule)
                    .child(body.pl_3()),
            );
        Some(sheet.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::{card_kicker, is_broad_grant, needs_you_label};

    #[test]
    fn header_counts_pending_requests() {
        assert_eq!(needs_you_label(0, 1), "NEEDS YOU · 1");
        assert_eq!(needs_you_label(0, 3), "NEEDS YOU · 1 OF 3");
        assert_eq!(needs_you_label(2, 3), "NEEDS YOU · 3 OF 3");
    }

    #[test]
    fn yolo_wait_gets_its_own_kicker() {
        assert_eq!(card_kicker(true, 1, 3), "Worker waits for YOLO");
        assert_eq!(card_kicker(false, 1, 3), "NEEDS YOU · 2 OF 3");
    }

    #[test]
    fn broad_grants_are_separated_from_one_off_decisions() {
        for label in [
            "Allow for this session",
            "Allow for this turn",
            "Allow always",
            "Allow YOLO for this worker",
        ] {
            assert!(is_broad_grant(label), "{label} should be a broad grant");
        }
        for label in [
            "Allow once",
            "Allow tool call",
            "Deny",
            "Decline",
            "Cancel turn",
        ] {
            assert!(!is_broad_grant(label), "{label} should be one-off");
        }
    }
}

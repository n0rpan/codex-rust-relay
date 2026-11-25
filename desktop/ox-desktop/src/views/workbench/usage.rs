use super::Workbench;
use crate::appearance::Step;
use crate::{
    appearance::Palette,
    kit::{button, col, row, small},
};
use chrono::{DateTime, Local, Utc};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{popover::Popover, progress::Progress};
use ox_desk_core::store::Route;

fn compact(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.)
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.)
    } else {
        n.to_string()
    }
}

fn duration(minutes: Option<i64>) -> String {
    match minutes {
        Some(m) if m > 0 && m % 1440 == 0 => format!("{}d", m / 1440),
        Some(m) if m > 0 && m % 60 == 0 => format!("{}h", m / 60),
        Some(m) if m > 0 => format!("{m}m"),
        _ => "Limit".into(),
    }
}

fn metric(p: Palette, label: impl Into<SharedString>, value: impl Into<SharedString>) -> Div {
    row()
        .gap_3()
        .justify_between()
        .child(small(p, label))
        .child(
            div()
                .font_family("monospace")
                .text_size(p.text(Step::Ui))
                .child(value.into()),
        )
}

impl Workbench {
    pub(super) fn usage_bar(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let store = self.store.as_ref()?;
        let p = Palette::get(cx);
        let tokens = store
            .selected()
            .and_then(|id| store.transcript(id))
            .and_then(|t| t.usage.as_ref());
        // With no thread selected, or no usage data yet, only a limit warning shows.
        if store.selected().is_none() || tokens.is_none() {
            let limit = store.usage.closest_limit(Utc::now().timestamp())?;
            return Some(
                small(
                    p,
                    format!(
                        "{} · {} limit {}% used",
                        limit.0,
                        duration(limit.1.window_duration_mins),
                        limit.1.used_percent
                    ),
                )
                .text_color(rgb(p.warning))
                .into_any_element(),
            );
        }
        let mut label = tokens.map_or_else(String::new, |u| {
            format!("{} tokens", compact(u.total.total_tokens))
        });
        if let Some(percent) = tokens.and_then(|u| u.context_percent()) {
            label.push_str(&format!(" · {percent:.0}% context"));
        }
        let view = cx.weak_entity();
        let popover = Popover::new("usage-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(
                crate::kit::toolbar_button(p, "show-usage", label, true, |_, _, _| {})
                    .compact()
                    .text_size(p.text(Step::Meta)),
            )
            .content(move |_, window, cx| {
                view.read_with(cx, |s, _| s.usage_details(p, window, &view))
                    .unwrap_or_else(|_| div().into_any_element())
            });
        Some(
            row()
                .flex_wrap()
                .gap_2()
                .child(popover)
                .when_some(
                    store.usage.closest_limit(Utc::now().timestamp()),
                    |bar, (name, limit)| {
                        bar.child(
                            small(
                                p,
                                format!(
                                    "{name} · {} limit {}% used",
                                    duration(limit.window_duration_mins),
                                    limit.used_percent
                                ),
                            )
                            .text_color(rgb(p.warning)),
                        )
                    },
                )
                .into_any_element(),
        )
    }

    pub(in crate::views) fn usage_details(
        &self,
        p: Palette,
        window: &Window,
        view: &WeakEntity<Self>,
    ) -> AnyElement {
        let Some(store) = self.store.as_ref() else {
            return div().into_any_element();
        };
        let mut panel = col()
            .id("session-usage-details")
            .w(px(290.).min((window.viewport_size().width - px(40.)).max(px(100.))))
            .max_h(window.viewport_size().height * 0.65)
            .overflow_y_scroll()
            .p_2()
            .gap_2()
            .text_color(rgb(p.fg))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Session usage"),
            )
            .when_some(
                store.selected().and_then(|id| store.catalog_row(id)),
                |panel, thread| {
                    panel.child(
                        div()
                            .text_size(p.text(Step::Ui))
                            .truncate()
                            .child(thread.title().to_owned()),
                    )
                },
            )
            .child(small(p, store.profile().route.label()).truncate());
        if let Some(u) = store
            .selected()
            .and_then(|id| store.transcript(id))
            .and_then(|t| t.usage.as_ref())
        {
            if let (Some(capacity), Some(percent)) = (u.model_context_window, u.context_percent()) {
                panel = panel
                    .child(metric(p, "Context window", format!("{percent:.0}%")))
                    .child(
                        Progress::new("usage-context")
                            .color(rgb(p.accent))
                            .value(percent.clamp(0., 100.) as f32),
                    )
                    .child(small(
                        p,
                        format!(
                            "{} of {} tokens",
                            compact(u.last.total_tokens),
                            compact(capacity)
                        ),
                    ));
            }
            panel = panel.child(
                col()
                    .border_t_1()
                    .border_color(rgb(p.line))
                    .pt_2()
                    .gap_1()
                    .child(metric(p, "Total tokens", u.total.total_tokens.to_string()))
                    .child(metric(p, "Input", u.total.input_tokens.to_string()))
                    .child(metric(
                        p,
                        "Cached input",
                        u.total.cached_input_tokens.to_string(),
                    ))
                    .child(metric(p, "Output", u.total.output_tokens.to_string()))
                    .child(metric(
                        p,
                        "Reasoning",
                        u.total.reasoning_output_tokens.to_string(),
                    )),
            );
        } else {
            panel = panel.child(small(p, "Waiting for token usage."));
        }
        if store.profile().route == Route::CODEX_SUBSCRIPTION {
            let view = view.clone();
            panel = panel.child(
                row()
                    .justify_between()
                    .gap_2()
                    .border_t_1()
                    .border_color(rgb(p.line))
                    .pt_2()
                    .child(
                        div()
                            .text_size(p.text(Step::Ui))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Account limits"),
                    )
                    .child(button(
                        p,
                        "refresh-usage",
                        "Refresh",
                        false,
                        !store.usage.loading,
                        move |_, _, cx| {
                            let _ = view.update(cx, |s, cx| {
                                s.action(cx, ox_desk_core::actions::Action::ReadAccountLimits)
                            });
                        },
                    )),
            );
            for (id, bucket) in &store.usage.limits {
                for (index, w) in bucket.windows().enumerate() {
                    panel = panel
                        .child(metric(
                            p,
                            format!(
                                "{} · {}",
                                bucket.limit_name.as_deref().unwrap_or(id),
                                duration(w.window_duration_mins)
                            ),
                            format!("{}%", w.used_percent),
                        ))
                        .child(
                            Progress::new(SharedString::from(format!("limit-{id}-{index}")))
                                .color(rgb(p.accent))
                                .value(w.used_percent.clamp(0, 100) as f32),
                        );
                    if let Some(at) = w.resets_at.and_then(DateTime::<Utc>::from_timestamp_secs) {
                        panel = panel.child(small(
                            p,
                            format!("Resets {}", at.with_timezone(&Local).format("%a %H:%M")),
                        ));
                    }
                }
            }
            if store.usage.loading {
                panel = panel.child(small(p, "Reading limits…"));
            } else if let Some(err) = &store.usage.error {
                panel = panel.child(small(p, err.clone()));
            } else if store
                .usage
                .limits
                .values()
                .all(|b| b.windows().next().is_none())
            {
                panel = panel.child(small(p, "No account limit windows reported."));
            }
        }
        panel.into_any_element()
    }
}

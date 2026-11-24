use super::workbench::Workbench;
use crate::{
    appearance::Palette,
    kit::{button, col, field, small},
};
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState};
use ox_desk_core::runtime::Billing;
use ox_desk_core::{
    slash::Capabilities,
    store::{
        Route,
        routes_thread::{Choice, Options},
    },
};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Model,
    Effort,
    Speed,
}
pub(super) struct Picker {
    owner: Option<String>,
    kind: Kind,
    search: Entity<InputState>,
    favorites: Vec<String>,
}
fn choices(
    caps: &Capabilities,
    settings: &Options,
    kind: Kind,
    favorites: &[String],
    query: &str,
) -> Vec<(String, Choice)> {
    let needle = query.trim().to_lowercase();
    match kind {
        Kind::Model => {
            let rank = favorites
                .iter()
                .enumerate()
                .map(|(i, s)| (s.as_str(), i))
                .collect::<HashMap<_, _>>();
            let mut rows = caps
                .models
                .iter()
                .filter(|(id, model)| {
                    id.to_lowercase().contains(&needle)
                        || model.label.to_lowercase().contains(&needle)
                })
                .collect::<Vec<_>>();
            rows.sort_unstable_by(|(a, _), (b, _)| {
                rank.get(a.as_str())
                    .unwrap_or(&usize::MAX)
                    .cmp(rank.get(b.as_str()).unwrap_or(&usize::MAX))
                    .then_with(|| a.cmp(b))
            });
            rows.into_iter()
                .map(|(id, m)| {
                    let label = if m.label.is_empty() || m.label == *id {
                        id.clone()
                    } else {
                        format!("{} · {id}", m.label)
                    };
                    let label = if rank.contains_key(id.as_str()) {
                        format!("★ {label}")
                    } else {
                        label
                    };
                    (label, Choice::Model(id.clone()))
                })
                .collect()
        }
        Kind::Effort => caps
            .models
            .get(&settings.model)
            .into_iter()
            .flat_map(|m| &m.efforts)
            .map(|e| {
                (
                    e.reasoning_effort.clone(),
                    Choice::Effort(e.reasoning_effort.clone()),
                )
            })
            .collect(),
        Kind::Speed if !caps.speed => vec![],
        Kind::Speed => std::iter::once(("Default".into(), Choice::Speed(None)))
            .chain(
                caps.catalog
                    .get(&settings.model)
                    .into_iter()
                    .flatten()
                    .map(|t| (t.name.clone(), Choice::Speed(Some(t.id.clone())))),
            )
            .collect(),
    }
}
pub(super) struct NewModelRoute(pub Route);
impl EventEmitter<NewModelRoute> for Workbench {}

impl Workbench {
    fn choice_row(
        &self,
        index: usize,
        label: &str,
        choice: &Choice,
        settings: &Options,
        owner: &Option<String>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = Palette::get(cx);
        let new_route = match choice {
            Choice::Model(id)
                if self
                    .store
                    .as_ref()
                    .is_some_and(|store| !store.same_model_route(id)) =>
            {
                Some(self.route.runtime.route(Some(id.clone())))
            }
            _ => None,
        };
        let label = if new_route.is_some() {
            format!("New session · {label}")
        } else {
            label.to_owned()
        };
        let active = match choice {
            Choice::Effort(id) => settings.effort.as_ref() == Some(id),
            Choice::Model(id) => &settings.model == id,
            Choice::Speed(tier) => settings.tier == *tier,
        };
        let choice = choice.clone();
        let owner = owner.clone();
        button(
            p,
            ("model-choice", index),
            label,
            active,
            true,
            cx.listener(move |s, _, _, cx| {
                if s.store.as_ref().and_then(|s| s.selected()) == owner.as_deref() {
                    if let Some(route) = &new_route {
                        cx.emit(NewModelRoute(route.clone()));
                    } else {
                        s.action(
                            cx,
                            ox_desk_core::actions::Action::ModelOptions(
                                owner.clone(),
                                choice.clone(),
                            ),
                        );
                    }
                }
                s.model_picker = None;
                cx.notify();
            }),
        )
        .into_any_element()
    }
    fn open_model_picker(&mut self, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        let owner = self
            .store
            .as_ref()
            .and_then(|s| s.selected())
            .map(str::to_owned);
        if self
            .model_picker
            .as_ref()
            .is_some_and(|p| p.owner == owner && p.kind == kind)
        {
            self.model_picker = None;
            cx.notify();
            return;
        }
        let search = field("", "Search models", false, window, cx);
        cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();
        if kind == Kind::Model {
            search.read(cx).focus_handle(cx).focus(window, cx);
        }
        self.model_picker = Some(Picker {
            owner,
            kind,
            search: search.clone(),
            favorites: vec![],
        });
        if kind == Kind::Model {
            let task = cx.background_spawn(async { crate::settings::load().map(|s| s.favorites) });
            cx.spawn(async move |s, cx| {
                let Ok(favorites) = task.await else { return };
                let _ = s.update(cx, |s, cx| {
                    if let Some(p) = s.model_picker.as_mut()
                        && p.search == search
                    {
                        p.favorites = favorites;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }
    pub(super) fn model_controls(&self, width: Pixels, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(store) = &self.store else {
            return vec![];
        };
        let owner = store.selected();
        if owner.is_some_and(|id| store.job(id).is_some()) {
            return vec![];
        }
        let p = Palette::get(cx);
        let settings = store.options(owner);
        let enabled = store.can_send(owner);
        let route = match &store.profile().route.billing {
            Billing::Subscription if settings.model.is_empty() => self.route.label(),
            Billing::Subscription => {
                format!("{} · {}", self.route.runtime.account(), settings.model)
            }
            Billing::Api(original) => format!(
                "OX API · {}",
                if settings.model.is_empty() {
                    original
                } else {
                    &settings.model
                }
            ),
        };
        let mut chips = Vec::new();
        for (index, (kind, label)) in [
            (Kind::Model, route),
            (
                Kind::Effort,
                settings
                    .effort
                    .clone()
                    .map_or("Effort".into(), |s| format!("Effort · {s}")),
            ),
            (
                Kind::Speed,
                settings
                    .tier
                    .clone()
                    .map_or("Speed".into(), |s| format!("Speed · {s}")),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            if store.has_command_schema() {
                let caps = store.command_caps(owner);
                if (kind == Kind::Speed && !caps.speed)
                    || (kind == Kind::Effort
                        && caps
                            .models
                            .get(&settings.model)
                            .is_none_or(|m| m.efforts.is_empty()))
                {
                    continue;
                }
            }
            let opened = self
                .model_picker
                .as_ref()
                .filter(|p| p.owner.as_deref() == owner && p.kind == kind);
            let mut chip = div().child(
                button(
                    p,
                    ("model-setting", index),
                    label.clone(),
                    opened.is_some(),
                    enabled,
                    cx.listener(move |s, _, w, cx| s.open_model_picker(kind, w, cx)),
                )
                .max_w((width - px(56.)).clamp(px(84.), px(400.)))
                .tooltip(label),
            );
            if let Some(picker) = opened {
                let caps = store.command_caps(owner);
                let mut panel = col()
                    .id("model-picker-list")
                    .w(px(390.).min(width - px(20.)))
                    .max_h(px(330.))
                    .overflow_y_scroll()
                    .p_2()
                    .gap_1()
                    .rounded_md()
                    .bg(rgb(p.panel))
                    .border_1()
                    .border_color(rgb(p.line))
                    .occlude()
                    .on_key_down(cx.listener(|s, e: &KeyDownEvent, _, cx| {
                        if e.keystroke.key == "escape" {
                            s.model_picker = None;
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }));
                if kind == Kind::Model {
                    panel = panel.child(Input::new(&picker.search));
                }
                let rows = choices(
                    &caps,
                    &settings,
                    kind,
                    &picker.favorites,
                    &picker.search.read(cx).value(),
                );
                if store.commands_loading() || !store.has_command_schema() {
                    panel = panel.child(small(p, "Reading installed model capabilities…"));
                } else if rows.is_empty() {
                    panel = panel.child(small(p, "No matching choices reported for this model."));
                }
                if !rows.is_empty() {
                    let count = rows.len();
                    let selected = owner.map(str::to_owned);
                    let settings = settings.clone();
                    let list = uniform_list(
                        "model-options",
                        count,
                        cx.processor(move |workbench, range: std::ops::Range<usize>, _, cx| {
                            range
                                .map(|index| {
                                    let (label, choice) = &rows[index];
                                    workbench
                                        .choice_row(index, label, choice, &settings, &selected, cx)
                                })
                                .collect::<Vec<_>>()
                        }),
                    );
                    panel = panel.child(list.h(px((count.min(6) * 32) as f32)).w_full());
                }
                panel = panel.child(button(
                    p,
                    "close-model-picker",
                    "Close",
                    false,
                    true,
                    cx.listener(|s, _, _, cx| {
                        s.model_picker = None;
                        cx.notify();
                    }),
                ));
                chip = chip.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::BottomLeft)
                            .snap_to_window_with_margin(px(8.))
                            .child(panel),
                    )
                    .with_priority(2),
                );
            }
            chips.push(chip.into_any_element());
        }
        chips
    }
}

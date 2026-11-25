use super::model_picker::{self, ModelPicker, ModelPickerEvent};
use super::workbench::Workbench;
use crate::{
    appearance::Palette,
    kit::{button, col, small},
};
use gpui::*;
use ox_desk_core::runtime::{Billing, Runtime};
use ox_desk_core::{
    slash::Capabilities,
    store::{
        Route, Store,
        routes_thread::{Choice, Options},
    },
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Model,
    Effort,
    Speed,
}
pub(super) struct Picker {
    owner: Option<String>,
    kind: Kind,
    /// Only the model picker is a searchable shared `ModelPicker`.
    models: Option<(Entity<ModelPicker>, Subscription)>,
}
pub(super) fn model_entries(caps: &Capabilities) -> Vec<model_picker::Entry> {
    caps.models
        .iter()
        .map(|(id, m)| model_picker::Entry {
            id: id.clone(),
            label: if m.label.is_empty() || m.label == *id {
                id.clone()
            } else {
                format!("{} · {id}", m.label)
            },
            note: String::new(),
            new_session: false,
        })
        .collect()
}
/// Provider part of an alias, for the "API · provider" tag.
fn api_provider(id: &str) -> &str {
    id.split_once('/').map_or("OX", |(provider, _)| provider)
}
/// The composer's Model picker: API aliases plus the subscription routes, each tagged with its
/// runtime and what bills, and flagged when choosing it must open a new session.
pub(super) fn composer_entries(store: &Store, owner: Option<&str>) -> Vec<model_picker::Entry> {
    let runtime = store.profile().route.runtime;
    let mut entries = model_entries(&store.command_caps(owner));
    for entry in &mut entries {
        entry.note = format!("{} · API · {}", runtime.name(), api_provider(&entry.id));
        entry.new_session = !store.same_model_route(&entry.id);
    }
    let mut plans = Vec::new();
    for rt in [Runtime::Codex, Runtime::Claude] {
        if store.profile().route == rt.route(None) {
            continue;
        }
        plans.push(model_picker::Entry {
            id: format!("{}{}", model_picker::PLAN_PREFIX, rt.executable()),
            label: format!("{} plan", rt.account()),
            note: format!("{} · subscription", rt.name()),
            new_session: true,
        });
    }
    plans.extend(entries);
    plans
}
fn choices(caps: &Capabilities, settings: &Options, kind: Kind) -> Vec<(String, Choice)> {
    match kind {
        Kind::Model => vec![],
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
    /// Pick a model for the owner's thread: a new session when the route changes, otherwise an
    /// option change on the current one.
    fn choose_model(&mut self, owner: Option<String>, id: String, cx: &mut Context<Self>) {
        let plan = id.strip_prefix(model_picker::PLAN_PREFIX).map(|name| {
            if name == Runtime::Claude.executable() {
                Runtime::Claude
            } else {
                Runtime::Codex
            }
            .route(None)
        });
        let runtime = self
            .store
            .as_ref()
            .map_or(self.route.runtime, |s| s.profile().route.runtime);
        let new_route = plan.or_else(|| {
            self.store
                .as_ref()
                .is_some_and(|store| !store.same_model_route(&id))
                .then(|| runtime.route(Some(id.clone())))
        });
        if self.store.as_ref().and_then(|s| s.selected()) == owner.as_deref() {
            if let Some(route) = new_route {
                cx.emit(NewModelRoute(route));
            } else {
                self.action(
                    cx,
                    ox_desk_core::actions::Action::ModelOptions(owner, Choice::Model(id)),
                );
            }
        }
        self.model_picker = None;
        cx.notify();
    }
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
            format!("Switch model (new session) · {label}")
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
        let models = (kind == Kind::Model).then(|| {
            let entries = self
                .store
                .as_ref()
                .map(|store| composer_entries(store, owner.as_deref()))
                .unwrap_or_default();
            let picker = cx.new(|cx| ModelPicker::new(entries, window, cx));
            let chosen_owner = owner.clone();
            let subscription = cx.subscribe_in(
                &picker,
                window,
                move |s: &mut Workbench, _, event: &ModelPickerEvent, _, cx| match event {
                    ModelPickerEvent::Choose(id) => {
                        s.choose_model(chosen_owner.clone(), id.clone(), cx)
                    }
                    ModelPickerEvent::Close => {
                        s.model_picker = None;
                        cx.notify();
                    }
                    ModelPickerEvent::Starred(_) => {}
                },
            );
            (picker, subscription)
        });
        self.model_picker = Some(Picker {
            owner,
            kind,
            models,
        });
        cx.notify();
    }
    pub(super) fn model_controls(&self, width: Pixels, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(store) = &self.store else {
            return vec![];
        };
        let owner = store.selected();
        let p = Palette::get(cx);
        let settings = store.options(owner);
        let enabled = store.can_send(owner);
        // Value only: the runtime name is in the AGENT picker and the tooltip.
        let runtime = store.profile().route.runtime.name();
        let route = match &store.profile().route.billing {
            Billing::Subscription if settings.model.is_empty() => {
                format!("{} plan", store.profile().route.runtime.account())
            }
            Billing::Subscription => format!(
                "{} plan · {}",
                store.profile().route.runtime.account(),
                settings.model
            ),
            Billing::Api(original) => format!(
                "API · {}",
                if settings.model.is_empty() {
                    original
                } else {
                    &settings.model
                }
            ),
        };
        let mut chips = Vec::new();
        for (index, (kind, name, value)) in [
            (Kind::Model, "Model", route),
            (
                Kind::Effort,
                "Effort",
                capitalised(settings.effort.as_deref().unwrap_or("Default")),
            ),
            (
                Kind::Speed,
                "Speed",
                capitalised(settings.tier.as_deref().unwrap_or("Default")),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let label = match (kind, value.is_empty()) {
                (Kind::Model, _) => format!("Model · {runtime} · {value}"),
                (_, true) => name.to_owned(),
                (_, false) => format!("{name} · {value}"),
            };
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
                crate::kit::chip(
                    p,
                    ("model-setting", index),
                    if kind == Kind::Model { "" } else { name },
                    value,
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
                    .max_h(px(380.))
                    .overflow_y_scroll()
                    .p_2()
                    .gap_1()
                    .rounded(px(crate::kit::OVERLAY_RADIUS))
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
                if let Some((models, _)) = &picker.models {
                    panel = panel.child(models.clone());
                } else {
                    let rows = choices(&caps, &settings, kind);
                    if store.commands_loading() || !store.has_command_schema() {
                        panel = panel.child(small(p, "Reading installed model capabilities…"));
                    } else if rows.is_empty() {
                        panel =
                            panel.child(small(p, "No matching choices reported for this model."));
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
                                        div()
                                            .w_full()
                                            .child(workbench.choice_row(
                                                index, label, choice, &settings, &selected, cx,
                                            ))
                                            .into_any_element()
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        );
                        panel = panel.child(list.h(px((count.min(6) * 32) as f32)).w_full());
                    }
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

/// A chip value starts with a capital, so "default" and "Default" read the same.
fn capitalised(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

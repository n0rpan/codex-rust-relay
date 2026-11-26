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
/// Whether `id` is the model the session runs now. Claude's symbolic `default` is current while
/// the stream reports the model it resolves to.
fn is_current_model(caps: &Capabilities, current: &str, id: &str) -> bool {
    id == current
        || (id == "default"
            && caps
                .models
                .get("default")
                .and_then(|m| m.resolved.as_deref())
                .is_some_and(|resolved| resolved == current))
}
/// The one route value the composer's model pill and the sidebar AGENT picker both show:
/// "<runtime> · <model>", with the runtime's default model named.
pub(super) fn route_value(store: &Store, owner: Option<&str>) -> String {
    let runtime = store.profile().route.runtime.name();
    let model = store.options(owner).model;
    if model.is_empty() {
        return runtime.to_owned();
    }
    let name = match &store.profile().route.billing {
        Billing::Subscription => store.command_caps(owner).model_display(&model),
        Billing::Api(_) => ox_desk_core::slash::readable_model(&model),
    };
    format!("{runtime} · {name}")
}
/// The shared choices behind the composer's model pill and the sidebar AGENT picker: the current
/// route is always a checked row; other routes are tagged "New session".
pub(super) fn composer_entries(store: &Store, owner: Option<&str>) -> Vec<model_picker::Entry> {
    let route = &store.profile().route;
    let caps = store.command_caps(owner);
    let current = store.options(owner).model;
    let subscription = route.billing == Billing::Subscription;
    let mut entries: Vec<model_picker::Entry> = caps
        .models
        .iter()
        .map(|(id, m)| {
            let mut entry = model_picker::Entry {
                rank: m.rank,
                recommended: m.is_default,
                current: is_current_model(&caps, &current, id),
                ..model_picker::api_entry(id)
            };
            if subscription {
                // The session's own model: its bare id, never the API row of the same alias.
                entry.id.clone_from(id);
                entry.label = caps.model_display(id);
                entry.note = format!("{} subscription", route.runtime.account());
                entry.section = Some(format!("{} MODELS", route.runtime.name().to_uppercase()));
            } else {
                entry.new_session = !store.same_model_route(id);
            }
            entry
        })
        .collect();
    for rt in [Runtime::Codex, Runtime::Claude] {
        let here = route.runtime == rt && subscription;
        let mut label = rt.name().to_owned();
        if here && !current.is_empty() {
            label = format!("{label} · {}", caps.model_display(&current));
        }
        entries.push(model_picker::Entry {
            id: format!("{}{}", model_picker::PLAN_PREFIX, rt.executable()),
            label,
            note: format!("{} subscription", rt.account()),
            new_session: !here,
            current: here,
            ..Default::default()
        });
    }
    entries
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
/// What choosing a picker row does: the route to open a new session on (none when the current
/// session takes the change), and the id the in-place option change names. Plan rows open their
/// plan route unless it is the current one. An `@api:` alias always runs on its API route; only
/// an API session already on that provider takes it in place. A bare id is the session's own
/// model and changes in place when the session lists it.
pub(super) fn model_choice(
    store: &Store,
    owner: Option<&str>,
    id: &str,
    runtime: Runtime,
) -> (Option<Route>, String) {
    let current = &store.profile().route;
    if let Some(name) = id.strip_prefix(model_picker::PLAN_PREFIX) {
        let plan = if name == Runtime::Claude.executable() {
            Runtime::Claude
        } else {
            Runtime::Codex
        }
        .route(None);
        return ((&plan != current).then_some(plan), id.to_owned());
    }
    let alias = model_picker::bare(id).to_owned();
    let listed = store.command_caps(owner).models.contains_key(&alias);
    let in_place = if id.starts_with(model_picker::API_PREFIX) {
        matches!(current.billing, Billing::Api(_)) && listed && store.same_model_route(&alias)
    } else {
        listed && store.same_model_route(&alias)
    };
    let route = (!in_place).then(|| runtime.route(Some(alias.clone())));
    (route, alias)
}
pub(super) struct NewModelRoute(pub Route);
impl EventEmitter<NewModelRoute> for Workbench {}
/// The model picker asks for Settings → Providers & models.
pub(super) struct OpenProviders;
impl EventEmitter<OpenProviders> for Workbench {}

impl Workbench {
    /// Pick a model for the owner's thread: a new session when the route changes, otherwise an
    /// option change on the current one. `runtime` is the picker's "Run with" choice for API
    /// models outside the session's own list.
    pub(super) fn choose_model(
        &mut self,
        owner: Option<String>,
        id: String,
        runtime: Option<Runtime>,
        cx: &mut Context<Self>,
    ) {
        let current = self.store.as_ref().map(|s| s.profile().route.clone());
        let runtime =
            runtime.unwrap_or_else(|| current.as_ref().map_or(self.route.runtime, |r| r.runtime));
        let (new_route, id) = match self.store.as_ref() {
            Some(store) => model_choice(store, owner.as_deref(), &id, runtime),
            None => {
                let route = match id.strip_prefix(model_picker::PLAN_PREFIX) {
                    Some(name) if name == Runtime::Claude.executable() => {
                        Runtime::Claude.route(None)
                    }
                    Some(_) => Runtime::Codex.route(None),
                    None => runtime.route(Some(model_picker::bare(&id).to_owned())),
                };
                (Some(route), id)
            }
        };
        let unchanged_plan = id.starts_with(model_picker::PLAN_PREFIX) && new_route.is_none();
        if !unchanged_plan && self.store.as_ref().and_then(|s| s.selected()) == owner.as_deref() {
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
    /// Open the hand-off sheet with the route a picker row stands for.
    fn hand_off_to(
        &mut self,
        id: &str,
        runtime: Option<Runtime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (runtime, model) = match id.strip_prefix(model_picker::PLAN_PREFIX) {
            Some(name) if name == Runtime::Claude.executable() => (Runtime::Claude, None),
            Some(_) => (Runtime::Codex, None),
            None => (
                runtime.unwrap_or(self.route.runtime),
                Some(model_picker::bare(id).to_owned()),
            ),
        };
        self.model_picker = None;
        self.open_launch(super::worker_sheet::LaunchKind::Handoff, window, cx);
        self.set_launch_route(runtime, model, cx);
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
        // The press that dismissed the panel from outside is also the click on its chip.
        if self
            .picker_dismissed
            .take()
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(300))
        {
            return;
        }
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
            let runtime = self.route.runtime;
            let picker = cx.new(|cx| {
                ModelPicker::new(entries, window, cx)
                    .with_runtime(runtime)
                    .with_handoff()
                    .with_discovered_new_session()
            });
            let chosen_owner = owner.clone();
            let subscription = cx.subscribe_in(
                &picker,
                window,
                move |s: &mut Workbench, picker, event: &ModelPickerEvent, window, cx| match event {
                    ModelPickerEvent::Choose(id) => {
                        let runtime = picker.read(cx).runtime();
                        s.choose_model(chosen_owner.clone(), id.clone(), runtime, cx)
                    }
                    ModelPickerEvent::HandOff(id) => {
                        let runtime = picker.read(cx).runtime();
                        s.hand_off_to(id, runtime, window, cx)
                    }
                    ModelPickerEvent::AddProvider => cx.emit(OpenProviders),
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
        let route = route_value(store, owner);
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
                (Kind::Model, _) => format!("Model · {value}"),
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
                    .on_mouse_down_out(cx.listener(|s, _, _, cx| {
                        s.model_picker = None;
                        s.picker_dismissed = Some(std::time::Instant::now());
                        cx.notify();
                    }))
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

#[cfg(test)]
mod tests {
    use super::{model_choice, model_picker};
    use ox_desk_core::{
        links::codex::{Launch, Link},
        runtime::{Route, Runtime},
        slash::Capabilities,
        store::{Profile, Store},
    };

    /// A store on `route` whose session lists `models`.
    fn store(route: Route, models: &[&str]) -> Store {
        let directory = std::env::temp_dir();
        let link = Link::spawn(
            Launch {
                executable: "/bin/cat".into(),
                arguments: vec![],
                directory: directory.clone(),
                path: "/usr/bin:/bin".into(),
            },
            |_| {},
        )
        .unwrap();
        let mut store = Store::open(
            Profile {
                project: directory,
                route,
                provider: "openai".into(),
            },
            link,
        )
        .unwrap();
        let mut caps = Capabilities::default();
        for id in models {
            caps.add_model(
                serde_json::from_value(serde_json::json!({"id": id, "isDefault": false})).unwrap(),
            );
        }
        store.install_command_schema(caps).unwrap();
        store
    }

    #[test]
    fn an_id_in_both_lists_is_two_rows_and_each_keeps_its_billing() {
        let store = store(Route::CODEX_SUBSCRIPTION, &["fixture-model"]);
        let entries = super::composer_entries(&store, None);
        // The picker also lists discovered API aliases, which carry the namespaced id.
        let mut all = entries.clone();
        all.push(model_picker::api_entry("fixture-model"));
        let rows: Vec<_> = all
            .iter()
            .filter(|e| model_picker::bare(&e.id) == "fixture-model")
            .collect();
        assert_eq!(rows.len(), 2);
        let subscription = rows.iter().find(|e| e.id == "fixture-model").unwrap();
        let api = rows.iter().find(|e| e.id == "@api:fixture-model").unwrap();
        assert!(
            subscription.note.ends_with("subscription"),
            "{subscription:?}"
        );
        assert_eq!(api.note, "API");
        let rows = model_picker::arrange(&all, &[], "", false);
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, model_picker::Row::Model { entry, .. }
                    if model_picker::bare(&all[*entry].id) == "fixture-model"))
                .count(),
            2
        );

        // The API row opens its API route, never an in-place change of the subscription session.
        let (route, _) = model_choice(&store, None, "@api:fixture-model", Runtime::Codex);
        assert_eq!(
            route,
            Some(Runtime::Codex.route(Some("fixture-model".into())))
        );
        // The subscription row changes the model in place.
        let (route, id) = model_choice(&store, None, "fixture-model", Runtime::Codex);
        assert_eq!((route, id.as_str()), (None, "fixture-model"));
    }

    #[test]
    fn an_api_session_takes_its_own_alias_in_place() {
        let route = Route::codex_api("fixture-model".into());
        let store = store(route, &["fixture-model"]);
        let (new, id) = model_choice(&store, None, "@api:fixture-model", Runtime::Codex);
        assert_eq!((new, id.as_str()), (None, "fixture-model"));
        let entries = super::composer_entries(&store, None);
        assert_eq!(entries[0].id, "@api:fixture-model");
    }
}

//! The shared model picker: a search field, starred models in star order, subscription plans,
//! then every model by section. Stars only order the lists; any model in the catalog can be chosen.
use crate::appearance::Step;
use crate::{
    appearance::Palette,
    kit::{self, small},
    settings::{self, model, protocol::Request},
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::input::{Input, InputEvent, InputState};
use ox_desk_core::runtime::Runtime;
use std::{collections::HashSet, rc::Rc};

const ROW_HEIGHT: f32 = 28.;
const VISIBLE_ROWS: usize = 10;

/// One catalog model. `label` is what the row shows; `id` is what is chosen and starred.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Entry {
    pub id: String,
    pub label: String,
    /// Runtime and what bills, shown muted after the label ("Codex · API · openai").
    pub note: String,
    /// Choosing this row opens a new session instead of changing the current one.
    pub new_session: bool,
    /// This row is what the session uses now; it is always listed and checked.
    pub current: bool,
    /// The runtime's own recommended model: first in its section.
    pub recommended: bool,
    /// Position in the runtime's own list. When every row of a section has one, that order wins.
    pub rank: Option<u32>,
    /// Release time from the provider's catalog (seconds), when it reports one.
    pub created: Option<i64>,
    /// Section label; none derives "API MODELS · provider" from the id.
    pub section: Option<String>,
}

/// Id prefix of a subscription-route row (`@plan:codex`, `@plan:claude`); not a catalog alias.
pub(super) const PLAN_PREFIX: &str = "@plan:";
/// Id prefix of an API catalog row (`@api:openai/gpt-6`). A runtime's own model id can equal an
/// API alias, so the two kinds of row never share an id; stars still store the bare alias.
pub(super) const API_PREFIX: &str = "@api:";

/// The bare catalog alias of an API row id; any other id is returned unchanged.
pub(super) fn bare(id: &str) -> &str {
    id.strip_prefix(API_PREFIX).unwrap_or(id)
}

impl Entry {
    /// A catalog alias with no separate display name.
    #[cfg(test)]
    pub(super) fn alias(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            label: id.to_owned(),
            ..Self::default()
        }
    }
    fn is_plan(&self) -> bool {
        self.id.starts_with(PLAN_PREFIX)
    }
    /// What favourites store for this row: the bare alias.
    fn star_key(&self) -> &str {
        bare(&self.id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Row {
    Heading(String),
    /// `entry` indexes the entries slice; `starred` is whether the id is in the favourites.
    Model {
        entry: usize,
        starred: bool,
    },
    /// Expands or collapses the older versions listed after it.
    Older {
        count: usize,
        open: bool,
    },
}

/// The provider is the part of an alias before the first `/`.
fn provider(id: &str) -> &str {
    bare(id)
        .split_once('/')
        .map_or("Other", |(provider, _)| provider)
}

/// A model name split into its family (the words) and version (the numbers).
fn family_version(id: &str) -> (String, Vec<u32>) {
    let id = bare(id);
    let name = id.rsplit('/').next().unwrap_or(id).to_lowercase();
    let mut words = Vec::new();
    let mut version: Vec<u32> = Vec::new();
    // Only the first run of consecutive numeric parts is the version ("5-2", "6.2"); later
    // numbers are build stamps.
    let (mut in_run, mut done) = (false, false);
    for part in name.split(['-', '_']).filter(|p| !p.is_empty()) {
        if part.len() >= 8 && part.chars().all(|c| c.is_ascii_digit()) {
            continue; // A date stamp.
        }
        let numbers: Option<Vec<u32>> = part
            .split('.')
            .map(|n| (n.len() < 8).then(|| n.parse().ok()).flatten())
            .collect();
        match numbers {
            Some(numbers) if !done => {
                version.extend(numbers);
                in_run = true;
            }
            Some(_) => {}
            None => {
                done |= std::mem::take(&mut in_run);
                words.push(part.to_owned());
            }
        }
    }
    (words.join("-"), version)
}

/// Recency of a model: its catalog release time, then its version, newest greatest.
fn recency(entry: &Entry) -> Option<(i64, Vec<u32>)> {
    let (_, version) = family_version(&entry.id);
    (entry.created.is_some() || !version.is_empty())
        .then(|| (entry.created.unwrap_or(i64::MIN), version))
}

/// Order one section's entries (indices) and split off the older versions of crowded families.
/// Returns the visible indices and the older ones.
fn order_section(entries: &[Entry], mut section: Vec<usize>) -> (Vec<usize>, Vec<usize>) {
    if section.iter().all(|&i| entries[i].rank.is_some()) {
        section.sort_by_key(|&i| (!entries[i].recommended, entries[i].rank));
        return (section, vec![]);
    }
    // Families, newest first; models with no version or release time come last, alphabetically.
    let mut families: Vec<(String, Vec<usize>)> = Vec::new();
    let mut unknown = Vec::new();
    for index in section {
        if recency(&entries[index]).is_none() {
            unknown.push(index);
            continue;
        }
        let family = family_version(&entries[index].id).0;
        match families.iter_mut().find(|(name, _)| *name == family) {
            Some((_, members)) => members.push(index),
            None => families.push((family, vec![index])),
        }
    }
    for (_, members) in &mut families {
        members.sort_by(|&a, &b| {
            recency(&entries[b])
                .cmp(&recency(&entries[a]))
                .then_with(|| entries[a].id.cmp(&entries[b].id))
        });
    }
    families.sort_by(|(na, a), (nb, b)| {
        recency(&entries[b[0]])
            .cmp(&recency(&entries[a[0]]))
            .then_with(|| na.cmp(nb))
    });
    unknown.sort_by_key(|&i| entries[i].id.to_lowercase());
    let (mut shown, mut older) = (Vec::new(), Vec::new());
    for (_, members) in families {
        let crowded = members.len() > 2;
        for (position, index) in members.into_iter().enumerate() {
            if crowded && position >= 2 && !entries[index].current {
                older.push(index);
            } else {
                shown.push(index);
            }
        }
    }
    shown.extend(unknown);
    // The runtime's recommended model leads its section.
    let mut lead: Vec<usize> = shown
        .iter()
        .copied()
        .filter(|&i| entries[i].recommended)
        .collect();
    lead.extend(shown.into_iter().filter(|&i| !entries[i].recommended));
    (lead, older)
}

/// Rows for the picker: STARRED in star order (unknown stars are skipped), SUBSCRIPTION PLANS,
/// then every other model by section, each ordered recommended first, then families by newest
/// release, newest version first, with crowded families' older versions behind one "Older
/// models" row. A query matches id, label or note case-insensitively, filters every section and
/// opens the older versions.
pub(super) fn arrange(
    entries: &[Entry],
    favorites: &[String],
    query: &str,
    show_older: bool,
) -> Vec<Row> {
    let needle = query.trim().to_lowercase();
    let matches = |entry: &Entry| {
        needle.is_empty()
            || entry.id.to_lowercase().contains(&needle)
            || entry.label.to_lowercase().contains(&needle)
            || entry.note.to_lowercase().contains(&needle)
            || entry
                .section
                .as_ref()
                .is_some_and(|s| s.to_lowercase().contains(&needle))
    };
    let show_older = show_older || !needle.is_empty();
    // A runtime row is starred only when no API row shares its bare id.
    let is_starred = |index: usize| {
        let key = entries[index].star_key();
        favorites.iter().any(|f| f == key)
            && (entries[index].section.is_none()
                || !entries
                    .iter()
                    .any(|e| e.section.is_none() && !e.is_plan() && e.star_key() == key))
    };
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    let starred: Vec<usize> = favorites
        .iter()
        .filter_map(|id| {
            // An API row is the starred one when a runtime row shares its bare id.
            let index = entries
                .iter()
                .position(|entry| entry.star_key() == id && entry.section.is_none())
                .or_else(|| entries.iter().position(|entry| entry.star_key() == id))?;
            (!entries[index].is_plan() && matches(&entries[index]) && seen.insert(index))
                .then_some(index)
        })
        .collect();
    if !starred.is_empty() {
        rows.push(Row::Heading("STARRED".into()));
        rows.extend(starred.into_iter().map(|entry| Row::Model {
            entry,
            starred: true,
        }));
    }
    let plans: Vec<usize> = (0..entries.len())
        .filter(|&index| entries[index].is_plan() && matches(&entries[index]))
        .collect();
    if !plans.is_empty() {
        rows.push(Row::Heading("SUBSCRIPTION PLANS".into()));
        rows.extend(plans.into_iter().map(|entry| Row::Model {
            entry,
            starred: false,
        }));
    }
    // Sections: runtime lists first in the order given, then API providers by name, unknown last.
    let section_of = |entry: &Entry| {
        entry
            .section
            .clone()
            .unwrap_or_else(|| match provider(&entry.id) {
                "Other" => "API MODELS".to_owned(),
                name => format!("API MODELS · {name}"),
            })
    };
    let mut sections: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.is_plan() || !matches(entry) {
            continue;
        }
        let name = section_of(entry);
        match sections.iter_mut().find(|(n, _)| *n == name) {
            Some((_, members)) => members.push(index),
            None => sections.push((name, vec![index])),
        }
    }
    let api = |name: &str| name.starts_with("API MODELS");
    sections.sort_by_key(|(name, _)| {
        (
            api(name),
            name == "API MODELS",
            if api(name) {
                name.to_lowercase()
            } else {
                String::new()
            },
        )
    });
    for (name, members) in sections {
        let (shown, older) = order_section(entries, members);
        rows.push(Row::Heading(name));
        for index in shown {
            rows.push(Row::Model {
                entry: index,
                starred: is_starred(index),
            });
        }
        if !older.is_empty() {
            rows.push(Row::Older {
                count: older.len(),
                open: show_older,
            });
            if show_older {
                for index in older {
                    rows.push(Row::Model {
                        entry: index,
                        starred: is_starred(index),
                    });
                }
            }
        }
    }
    rows
}

/// Row positions of the selectable model rows, in order.
fn model_rows(rows: &[Row]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(ix, row)| matches!(row, Row::Model { .. }).then_some(ix))
        .collect()
}

/// Remove duplicate ids, keeping the first label seen.
pub(super) fn dedup(entries: Vec<Entry>) -> Vec<Entry> {
    let mut seen = HashSet::new();
    entries
        .into_iter()
        .filter(|entry| seen.insert(entry.id.clone()))
        .collect()
}

pub(super) enum ModelPickerEvent {
    Choose(String),
    /// Open the hand-off sheet on this model's route instead of switching in place.
    HandOff(String),
    Starred(Vec<String>),
    /// Open Settings on the providers page.
    AddProvider,
    Close,
}
impl EventEmitter<ModelPickerEvent> for ModelPicker {}

/// What the picker offers below the list when no API model is available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Footer {
    None,
    /// No API provider is configured.
    AddProvider,
    /// Providers exist but discovery listed no models.
    Refresh,
}

/// The footer for a catalog: an API provider to add, or a model list to refresh.
pub(super) fn footer(entries: &[Entry], providers_configured: bool) -> Footer {
    let has_api = entries
        .iter()
        .any(|entry| !entry.is_plan() && entry.section.is_none());
    match (has_api, providers_configured) {
        (true, _) => Footer::None,
        (false, false) => Footer::AddProvider,
        (false, true) => Footer::Refresh,
    }
}

pub(super) struct ModelPicker {
    link: crate::platform::HostLink,
    entries: Vec<Entry>,
    favorites: Vec<String>,
    search: Entity<InputState>,
    /// Index into the model rows (headings are skipped).
    cursor: usize,
    scroll: UniformListScrollHandle,
    loading: bool,
    notice: Option<String>,
    show_older: bool,
    /// Star writes started and not yet finished.
    pending_stars: usize,
    /// The runtime API rows run with; none hides the control.
    runtime: Option<Runtime>,
    /// Whether any API provider is configured; assumed until settings load.
    providers: bool,
    handoff: bool,
    /// Aliases found in settings that the session's own list lacks need a new session.
    discovered_new_session: bool,
    target_catalog: bool,
    _subscriptions: Vec<Subscription>,
}

impl ModelPicker {
    pub(super) fn new(
        link: crate::platform::HostLink,
        entries: Vec<Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = kit::field("", "Search models", false, window, cx);
        let subscriptions = vec![cx.subscribe_in(
            &search,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.cursor = 0;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.choose_current(window, cx),
                _ => {}
            },
        )];
        search.read(cx).focus_handle(cx).focus(window, cx);
        let host = link.remote().cloned();
        let task = cx.background_spawn(async move {
            host.map_or_else(settings::picker_settings, |h| {
                h.snapshot().map(|s| settings::PickerSettings::from(&s))
            })
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(settings) => {
                        this.favorites = settings.favorites;
                        if !this.target_catalog {
                            this.providers = settings.providers;
                            this.add_aliases(settings.aliases);
                        }
                    }
                    Err(error) => this.notice = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        Self {
            link,
            entries: dedup(entries),
            favorites: vec![],
            search,
            cursor: 0,
            scroll: UniformListScrollHandle::new(),
            loading: false,
            notice: None,
            show_older: false,
            pending_stars: 0,
            runtime: None,
            providers: true,
            handoff: false,
            discovered_new_session: false,
            target_catalog: false,
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn with_target_aliases(mut self, aliases: Vec<String>) -> Self {
        self.target_catalog = true;
        if aliases.is_empty() {
            self.notice = Some("This worker host reports no API models for this runtime.".into());
        }
        self.add_aliases(aliases);
        self
    }

    /// Show the "Run with" control, starting on `runtime`.
    pub(super) fn with_runtime(mut self, runtime: Runtime) -> Self {
        self.runtime = Some(if runtime.supports_api() {
            runtime
        } else {
            Runtime::Codex
        });
        self
    }

    /// Offer "Hand off" on rows that need a new session.
    pub(super) fn with_handoff(mut self) -> Self {
        self.handoff = true;
        self
    }

    /// Keep this picker in sync with the window's existing route discovery, including
    /// providers that answer after the user opens it. Retain native model labels and choices.
    pub(super) fn with_route_catalog(
        mut self,
        catalog: &Entity<Vec<String>>,
        cx: &mut Context<Self>,
    ) -> Self {
        self.discovered_new_session = true;
        let native = self.entries.clone();
        self.add_aliases(catalog.read(cx).iter().cloned());
        self._subscriptions
            .push(cx.observe(catalog, move |s, catalog, cx| {
                s.entries.clone_from(&native);
                s.add_aliases(catalog.read(cx).iter().cloned());
                cx.notify();
            }));
        self
    }

    /// Also list the full discovered catalog, not only the settings' static and starred aliases.
    pub(super) fn with_catalog(mut self, cx: &mut Context<Self>) -> Self {
        self.refresh(cx);
        self
    }

    /// The runtime API rows run with, when the control is shown.
    pub(super) fn runtime(&self) -> Option<Runtime> {
        self.runtime
    }

    /// Replace the catalog, keeping the search text and cursor position.
    pub(super) fn set_entries(&mut self, entries: Vec<Entry>, cx: &mut Context<Self>) {
        self.entries = dedup(entries);
        self.cursor = 0;
        cx.notify();
    }

    pub(super) fn set_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        self.loading = loading;
        cx.notify();
    }

    pub(super) fn set_notice(&mut self, notice: Option<String>, cx: &mut Context<Self>) {
        self.notice = notice;
        cx.notify();
    }

    fn query(&self, cx: &App) -> String {
        self.search.read(cx).value().to_string()
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        arrange(
            &self.entries,
            &self.favorites,
            &self.query(cx),
            self.show_older,
        )
    }

    /// List API aliases the catalog lacks, so every configured model can be chosen.
    fn add_aliases(&mut self, aliases: impl IntoIterator<Item = String>) {
        let known: HashSet<String> = self.entries.iter().map(|e| e.id.clone()).collect();
        let aliases = aliases.into_iter().map(|id| format!("{API_PREFIX}{id}"));
        let new_session = self.discovered_new_session;
        self.entries.extend(
            aliases
                .into_iter()
                .filter(|id| !known.contains(id))
                .map(|id| Entry {
                    new_session,
                    ..api_entry(&id)
                }),
        );
    }

    /// Refresh the API model list from the installed `ox`.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        let link = self.link.clone();
        let task = cx.background_spawn(async move { link.models() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(ids) => {
                        this.add_aliases(ids);
                        this.notice = None;
                    }
                    Err(error) => this.notice = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn choose_current(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let models = model_rows(&rows);
        if let Some(Row::Model { entry, .. }) = models
            .get(self.cursor.min(models.len().saturating_sub(1)))
            .map(|&ix| &rows[ix])
        {
            cx.emit(ModelPickerEvent::Choose(self.entries[*entry].id.clone()));
        }
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let models = model_rows(&rows);
        if models.is_empty() {
            return;
        }
        let last = models.len() - 1;
        self.cursor = self.cursor.min(last).saturating_add_signed(delta).min(last);
        self.scroll
            .scroll_to_item(models[self.cursor], ScrollStrategy::Nearest);
        cx.notify();
    }

    fn toggle_star(&mut self, id: &str, cx: &mut Context<Self>) {
        if id.starts_with(PLAN_PREFIX) {
            return;
        }
        // The click is a delta against what the row shows now, which already includes the
        // clicks still being written, so a fast double click on one row is add then remove.
        let id = bare(id);
        let add = !self.favorites.iter().any(|f| f == id);
        self.favorites = apply_delta(&self.favorites, id, add);
        self.pending_stars += 1;
        let id = id.to_owned();
        let link = self.link.clone();
        let task = cx.background_spawn(async move { write_star(link, &id, add) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.pending_stars -= 1;
                match result {
                    Ok(saved) => {
                        // Later clicks are still in flight: their optimistic state stands until
                        // the last write reports the saved list.
                        if this.pending_stars == 0 {
                            this.favorites = saved.clone();
                        }
                        this.notice = None;
                        cx.emit(ModelPickerEvent::Starred(saved));
                    }
                    Err(error) => {
                        this.notice = Some(error);
                        if this.pending_stars == 0 {
                            let link = this.link.clone();
                            let reload = cx.background_spawn(async move { link.settings() });
                            cx.spawn(async move |this, cx| {
                                if let Ok(snapshot) = reload.await {
                                    let _ = this.update(cx, |s, cx| {
                                        if s.pending_stars == 0 {
                                            s.favorites = snapshot.favorites;
                                            cx.notify()
                                        }
                                    });
                                }
                            })
                            .detach();
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn row(
        &self,
        rows: &[Row],
        ix: usize,
        selected: Option<usize>,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &rows[ix] {
            Row::Heading(text) => div()
                .h(px(ROW_HEIGHT))
                .px_2()
                .flex()
                .items_center()
                .child(kit::micro_label(p, text.clone()))
                .into_any_element(),
            Row::Older { count, open } => div()
                .h(px(ROW_HEIGHT))
                .w_full()
                .child(kit::toolbar_button(
                    p,
                    ("model-older", ix),
                    if *open {
                        format!("Hide {count} older models")
                    } else {
                        format!("Older models ({count})")
                    },
                    true,
                    cx.listener(|this, _, _, cx| {
                        this.show_older = !this.show_older;
                        cx.notify();
                    }),
                ))
                .into_any_element(),
            Row::Model { entry, starred } => {
                let entry = &self.entries[*entry];
                let id = entry.id.clone();
                let plan = entry.is_plan();
                let star = *starred;
                let active = selected == Some(ix);
                let probe_id: gpui::ElementId = ("model-row", ix).into();
                let probe_label: SharedString = format!("{} · {}", entry.label, id).into();
                let row = kit::row()
                    .id(probe_id.clone())
                    .h(px(ROW_HEIGHT))
                    .px_2()
                    .gap_1()
                    .w_full()
                    .text_size(p.text(Step::Ui))
                    .text_color(rgb(p.fg))
                    .bg(if active {
                        rgb(p.raised)
                    } else {
                        rgba(0x0000_0000)
                    })
                    .hover(|row| row.bg(rgb(p.hover)))
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |_, _, _, cx| cx.emit(ModelPickerEvent::Choose(id.clone()))
                    }))
                    .child(
                        div()
                            .id(("model-star", ix))
                            .flex_shrink_0()
                            .cursor_pointer()
                            .child(if plan {
                                div().w(px(14.)).into_any_element()
                            } else {
                                kit::star_column(p, star).into_any_element()
                            })
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle_star(&id, cx);
                                }
                            })),
                    )
                    .child(
                        div()
                            .w(px(12.))
                            .flex_shrink_0()
                            .text_color(rgb(p.accent))
                            .child(if entry.current { "✓" } else { "" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(entry.label.clone()),
                    )
                    .when(!entry.note.is_empty(), |row| {
                        row.child(
                            div()
                                .flex_shrink_0()
                                .text_size(p.text(Step::Label))
                                .text_color(rgb(p.muted))
                                .child(entry.note.clone()),
                        )
                    })
                    .when(entry.current, |row| row.child(tag(p, "Current", p.quiet())))
                    .when(entry.new_session && !entry.current, |row| {
                        row.child(tag(p, "New session", p.warning))
                    })
                    .when(entry.new_session && !entry.current && self.handoff, |row| {
                        row.child(kit::toolbar_button(
                            p,
                            ("model-handoff", ix),
                            "Hand off",
                            true,
                            cx.listener(move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(ModelPickerEvent::HandOff(id.clone()));
                            }),
                        ))
                    });
                crate::platform::layout_probe::mark_element(row, &probe_id, &probe_label, true)
                    .into_any_element()
            }
        }
    }
}

/// A small outlined tag after a row's label.
fn tag(p: Palette, text: &'static str, color: u32) -> Div {
    div()
        .flex_shrink_0()
        .px_1()
        .border_1()
        .border_color(rgb(p.line))
        .text_size(p.text(Step::Label))
        .text_color(rgb(color))
        .child(text)
}

/// A bare catalog alias as an API row: the value first, the provider in its section.
pub(super) fn api_entry(id: &str) -> Entry {
    let id = bare(id);
    Entry {
        id: format!("{API_PREFIX}{id}"),
        label: ox_desk_core::slash::readable_model(id),
        note: provider_note(id),
        ..Entry::default()
    }
}

/// "API · provider" for an alias, "API" without one.
pub(super) fn provider_note(id: &str) -> String {
    match provider(id) {
        "Other" => "API".to_owned(),
        name => format!("API · {name}"),
    }
}

/// Add or remove one alias in a favourites list, keeping the others and their order. Adding a
/// starred alias or removing an absent one changes nothing.
pub(super) fn apply_delta(list: &[String], id: &str, add: bool) -> Vec<String> {
    let mut next: Vec<String> = list.iter().filter(|f| *f != id).cloned().collect();
    if add {
        match list.iter().position(|f| f == id) {
            Some(index) => next.insert(index, id.to_owned()),
            None => next.push(id.to_owned()),
        }
    }
    next
}

/// The favourites document as the picker writes it: read the freshest list and revision,
/// replace the list against that revision.
pub(super) trait Favorites {
    fn read(&mut self) -> Result<(Vec<String>, String), String>;
    /// `Ok(None)`: the revision is stale.
    fn write(&mut self, revision: &str, list: Vec<String>) -> Result<Option<Vec<String>>, String>;
}

/// Apply one star as a delta on the freshest list. A stale revision re-reads and applies the
/// same delta again, so overlapping stars from other windows are kept.
pub(super) fn star_delta(
    store: &mut impl Favorites,
    id: &str,
    add: bool,
) -> Result<Vec<String>, String> {
    for _ in 0..8 {
        let (list, revision) = store.read()?;
        let next = apply_delta(&list, id, add);
        if next == list {
            return Ok(list);
        }
        if let Some(saved) = store.write(&revision, next)? {
            return Ok(saved);
        }
    }
    Err("Starred models changed elsewhere. Try again.".into())
}

impl Favorites for crate::platform::HostLink {
    fn read(&mut self) -> Result<(Vec<String>, String), String> {
        let s = self.settings()?;
        Ok((s.favorites, s.revisions.favorites))
    }
    fn write(&mut self, revision: &str, list: Vec<String>) -> Result<Option<Vec<String>>, String> {
        let request = Request::Edit {
            revision: revision.to_owned(),
            edit: model::Edit::Favorites { models: list },
        };
        Ok(self.apply(&request)?.ok().map(|saved| saved.favorites))
    }
}

/// Writes in one window go one at a time, each reading the list the previous one saved.
static STAR_WRITES: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn write_star(
    mut link: crate::platform::HostLink,
    id: &str,
    add: bool,
) -> Result<Vec<String>, String> {
    let _serial = STAR_WRITES.lock().unwrap_or_else(|e| e.into_inner());
    star_delta(&mut link, id, add)
}

impl Render for ModelPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let rows = self.rows(cx);
        let models = model_rows(&rows);
        self.cursor = self.cursor.min(models.len().saturating_sub(1));
        let selected = models.get(self.cursor).copied();
        let count = rows.len();
        let rows = Rc::new(rows);
        let mut view = kit::col()
            .id("model-picker")
            .w_full()
            .gap_1()
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                match e.keystroke.key.as_str() {
                    "up" => this.move_cursor(-1, cx),
                    "down" => this.move_cursor(1, cx),
                    "escape" => cx.emit(ModelPickerEvent::Close),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(
                kit::row()
                    .gap_1()
                    .child(div().flex_1().min_w_0().child(Input::new(&self.search)))
                    .child(kit::icon_button(
                        p,
                        "close-model-picker",
                        "×",
                        "Close",
                        cx.listener(|_, _, _, cx| cx.emit(ModelPickerEvent::Close)),
                    )),
            );
        if let Some(runtime) = self.runtime {
            let mut control = kit::row().gap_1().child(small(p, "Run with"));
            for (id, choice) in [
                ("model-run-codex", Runtime::Codex),
                ("model-run-claude", Runtime::Claude),
            ] {
                control = control.child(kit::button(
                    p,
                    id,
                    choice.name(),
                    runtime == choice,
                    true,
                    cx.listener(move |this, _, _, cx| {
                        this.runtime = Some(choice);
                        cx.notify();
                    }),
                ));
            }
            view = view.child(control).child(small(
                p,
                "API models run with this runtime; subscription rows use their own.",
            ));
        }
        if self.loading {
            view = view.child(small(p, "Loading the full catalog…"));
        }
        if let Some(notice) = &self.notice {
            view = view.child(small(p, notice.clone()));
        }
        if models.is_empty() {
            let text = if self.entries.is_empty() {
                "No models in the catalog yet."
            } else {
                "No models match."
            };
            view = view.child(small(p, text));
        } else {
            let list = uniform_list(
                "model-picker-rows",
                count,
                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .map(|ix| this.row(&rows, ix, selected, p, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll)
            .h(px(ROW_HEIGHT * count.min(VISIBLE_ROWS) as f32))
            .w_full();
            view = view.child(list);
        }
        match if self.target_catalog {
            Footer::None
        } else {
            footer(&self.entries, self.providers)
        } {
            Footer::None => {}
            Footer::AddProvider => {
                view = view.child(kit::toolbar_button(
                    p,
                    "model-add-provider",
                    "Add an API provider…",
                    true,
                    cx.listener(|_, _, _, cx| cx.emit(ModelPickerEvent::AddProvider)),
                ));
            }
            Footer::Refresh => {
                view = view.child(kit::toolbar_button(
                    p,
                    "model-refresh",
                    "Refresh model list",
                    !self.loading,
                    cx.listener(|this, _, _, cx| this.refresh(cx)),
                ));
            }
        }
        if self.entries.iter().any(|e| e.new_session) {
            view = view.child(small(
                p,
                "New session: starts a fresh session on that route.",
            ));
        }
        view
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Entry, Favorites, Footer, Row, api_entry, apply_delta, arrange, dedup, footer, model_rows,
        star_delta,
    };
    use std::{cell::RefCell, rc::Rc};

    /// A favourites document shared by several windows. `race` runs once between a read and
    /// the next write, standing for another window saving in that gap.
    type Race = Box<dyn FnOnce(&mut (Vec<String>, u32))>;
    #[derive(Clone, Default)]
    struct Doc {
        state: Rc<RefCell<(Vec<String>, u32)>>,
        race: Rc<RefCell<Vec<Race>>>,
        conflicts: Rc<RefCell<u32>>,
    }
    impl Favorites for Doc {
        fn read(&mut self) -> Result<(Vec<String>, String), String> {
            let s = self.state.borrow();
            Ok((s.0.clone(), s.1.to_string()))
        }
        fn write(
            &mut self,
            revision: &str,
            list: Vec<String>,
        ) -> Result<Option<Vec<String>>, String> {
            let race = self.race.borrow_mut().pop();
            if let Some(race) = race {
                race(&mut self.state.borrow_mut());
            }
            let mut s = self.state.borrow_mut();
            if s.1.to_string() != revision {
                *self.conflicts.borrow_mut() += 1;
                return Ok(None);
            }
            *s = (list.clone(), s.1 + 1);
            Ok(Some(list))
        }
    }
    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn delta_keeps_order_and_is_idempotent() {
        let base = list(&["a", "b", "c"]);
        assert_eq!(apply_delta(&base, "d", true), list(&["a", "b", "c", "d"]));
        assert_eq!(apply_delta(&base, "b", true), base);
        assert_eq!(apply_delta(&base, "b", false), list(&["a", "c"]));
        assert_eq!(apply_delta(&base, "z", false), base);
    }

    #[test]
    fn overlapping_clicks_on_different_models_both_land() {
        let doc = Doc::default();
        // Click two reads the list before click one saves, then both write.
        doc.race.borrow_mut().push(Box::new(|s| {
            s.0.push("two".into());
            s.1 += 1;
        }));
        let saved = star_delta(&mut doc.clone(), "one", true).unwrap();
        assert_eq!(saved, list(&["two", "one"]));
        assert_eq!(*doc.conflicts.borrow(), 1);
        let saved = star_delta(&mut doc.clone(), "two", false).unwrap();
        assert_eq!(saved, list(&["one"]));
    }

    #[test]
    fn two_windows_unstar_and_star_independently() {
        let doc = Doc::default();
        *doc.state.borrow_mut() = (list(&["a", "b"]), 4);
        // Window two removes "a" while window one is adding "c" against the old revision.
        doc.race.borrow_mut().push(Box::new(|s| {
            s.0.retain(|f| f != "a");
            s.1 += 1;
        }));
        let saved = star_delta(&mut doc.clone(), "c", true).unwrap();
        assert_eq!(saved, list(&["b", "c"]));
        assert_eq!(doc.state.borrow().0, saved);
    }

    #[test]
    fn a_persistent_conflict_gives_up_with_a_message() {
        struct Never;
        impl Favorites for Never {
            fn read(&mut self) -> Result<(Vec<String>, String), String> {
                Ok((vec![], "1".into()))
            }
            fn write(&mut self, _: &str, _: Vec<String>) -> Result<Option<Vec<String>>, String> {
                Ok(None)
            }
        }
        assert!(star_delta(&mut Never, "a", true).is_err());
    }

    fn entry(id: &str) -> Entry {
        Entry::alias(id)
    }

    fn ids(entries: &[Entry], rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Heading(text) => format!("#{text}"),
                Row::Older { count, open } => {
                    format!("older{count}{}", if *open { "+" } else { "" })
                }
                Row::Model { entry: i, starred } => {
                    format!("{}{}", entries[*i].id, if *starred { "*" } else { "" })
                }
            })
            .collect()
    }

    #[test]
    fn starred_first_then_providers() {
        let entries = vec![
            entry("openai/b"),
            entry("anthropic/a"),
            entry("openai/a"),
            entry("solo"),
        ];
        let favorites = vec![
            "openai/a".to_owned(),
            "solo".to_owned(),
            "missing".to_owned(),
        ];
        let rows = arrange(&entries, &favorites, "", false);
        assert_eq!(
            ids(&entries, &rows),
            [
                "#STARRED",
                "openai/a*",
                "solo*",
                "#API MODELS · anthropic",
                "anthropic/a",
                "#API MODELS · openai",
                "openai/a*",
                "openai/b",
                "#API MODELS",
                "solo*",
            ]
        );
    }

    #[test]
    fn newest_version_leads_and_families_order_by_recency() {
        let entries: Vec<Entry> = [
            "x/fable-5",
            "x/fable-5-2",
            "x/gpt-6.2-sol",
            "x/gpt-6.1-sol",
            "x/zeta",
            "x/alpha",
        ]
        .into_iter()
        .map(entry)
        .collect();
        let rows = arrange(&entries, &[], "", false);
        assert_eq!(
            ids(&entries, &rows),
            [
                "#API MODELS · x",
                "x/gpt-6.2-sol",
                "x/gpt-6.1-sol",
                "x/fable-5-2",
                "x/fable-5",
                "x/alpha",
                "x/zeta",
            ]
        );
    }

    #[test]
    fn crowded_families_collapse_older_versions() {
        let entries: Vec<Entry> = ["x/m-3", "x/m-1", "x/m-2", "x/m-4"]
            .into_iter()
            .map(entry)
            .collect();
        let rows = arrange(&entries, &[], "", false);
        assert_eq!(
            ids(&entries, &rows),
            ["#API MODELS · x", "x/m-4", "x/m-3", "older2"]
        );
        let rows = arrange(&entries, &[], "", true);
        assert_eq!(
            ids(&entries, &rows),
            [
                "#API MODELS · x",
                "x/m-4",
                "x/m-3",
                "older2+",
                "x/m-2",
                "x/m-1"
            ]
        );
        // A search opens them, and a current model is never hidden.
        assert_eq!(model_rows(&arrange(&entries, &[], "m-", false)).len(), 4);
        let mut entries = entries;
        entries[1].current = true;
        let rows = arrange(&entries, &[], "", false);
        assert_eq!(
            ids(&entries, &rows),
            ["#API MODELS · x", "x/m-4", "x/m-3", "x/m-1", "older1"]
        );
    }

    #[test]
    fn runtime_order_is_kept_with_the_recommended_model_first() {
        let mut entries: Vec<Entry> = [("sonnet", 2), ("default", 0), ("opus", 1)]
            .into_iter()
            .map(|(id, rank)| Entry {
                section: Some("CLAUDE CODE MODELS".into()),
                rank: Some(rank),
                recommended: id == "default",
                ..entry(id)
            })
            .collect();
        entries.push(Entry {
            id: "@plan:claude".into(),
            current: true,
            ..Entry::default()
        });
        let rows = arrange(&entries, &[], "", false);
        assert_eq!(
            ids(&entries, &rows),
            [
                "#SUBSCRIPTION PLANS",
                "@plan:claude",
                "#CLAUDE CODE MODELS",
                "default",
                "opus",
                "sonnet",
            ]
        );
    }

    #[test]
    fn catalog_release_time_beats_version_numbers() {
        let entries = vec![
            Entry {
                created: Some(200),
                ..entry("x/a-1")
            },
            Entry {
                created: Some(100),
                ..entry("x/b-9")
            },
        ];
        let rows = arrange(&entries, &[], "", false);
        assert_eq!(ids(&entries, &rows), ["#API MODELS · x", "x/a-1", "x/b-9"]);
    }

    #[test]
    fn query_matches_label_provider_and_family() {
        let entries = vec![
            Entry {
                label: "Bravo".into(),
                ..entry("openai/b")
            },
            api_entry("opencode-go/gemini-4-argon"),
            entry("anthropic/a"),
        ];
        let favorites = vec!["openai/b".to_owned(), "anthropic/a".to_owned()];
        let rows = arrange(&entries, &favorites, "  BRA ", false);
        assert_eq!(
            ids(&entries, &rows),
            ["#STARRED", "openai/b*", "#API MODELS · openai", "openai/b*"]
        );
        assert!(arrange(&entries, &favorites, "zzz", false).is_empty());
        for query in ["argon", "opencode", "gemini", "Gemini 4"] {
            assert_eq!(
                model_rows(&arrange(&entries, &[], query, false)).len(),
                1,
                "{query}"
            );
        }
        assert_eq!(
            api_entry("opencode-go/gemini-4-argon").label,
            "Gemini 4 Argon"
        );
    }

    #[test]
    fn duplicate_ids_are_listed_once() {
        let entries = dedup(vec![entry("x/1"), entry("x/1"), entry("y/2")]);
        assert_eq!(entries.len(), 2);
        let rows = arrange(&entries, &["x/1".into(), "x/1".into()], "", false);
        assert_eq!(model_rows(&rows).len(), 3);
    }

    #[test]
    fn stars_store_the_bare_alias_and_pick_the_api_row() {
        let runtime = Entry {
            id: "shared".into(),
            label: "Shared".into(),
            section: Some("CODEX MODELS".into()),
            ..Entry::default()
        };
        let entries = vec![runtime, api_entry("shared")];
        assert_eq!(entries[1].id, "@api:shared");
        let rows = arrange(&entries, &["shared".into()], "", false);
        assert_eq!(
            ids(&entries, &rows),
            [
                "#STARRED",
                "@api:shared*",
                "#CODEX MODELS",
                "shared",
                "#API MODELS",
                "@api:shared*"
            ]
        );
        assert_eq!(model_rows(&rows).len(), 3);
    }

    #[test]
    fn footer_offers_a_provider_or_a_refresh_when_no_api_model_is_listed() {
        let plan = Entry {
            id: "@plan:codex".into(),
            ..Entry::default()
        };
        assert_eq!(
            footer(std::slice::from_ref(&plan), false),
            Footer::AddProvider
        );
        assert_eq!(footer(std::slice::from_ref(&plan), true), Footer::Refresh);
        assert_eq!(footer(&[plan, entry("x/y")], false), Footer::None);
    }
}

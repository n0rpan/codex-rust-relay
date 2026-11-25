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
    id.split_once('/').map_or("Other", |(provider, _)| provider)
}

/// A model name split into its family (the words) and version (the numbers).
fn family_version(id: &str) -> (String, Vec<u32>) {
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
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    let starred: Vec<usize> = favorites
        .iter()
        .filter_map(|id| {
            let index = entries.iter().position(|entry| &entry.id == id)?;
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
                starred: favorites.contains(&entries[index].id),
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
                        starred: favorites.contains(&entries[index].id),
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
    entries: Vec<Entry>,
    favorites: Vec<String>,
    search: Entity<InputState>,
    /// Index into the model rows (headings are skipped).
    cursor: usize,
    scroll: UniformListScrollHandle,
    loading: bool,
    notice: Option<String>,
    show_older: bool,
    /// The runtime API rows run with; none hides the control.
    runtime: Option<Runtime>,
    /// Whether any API provider is configured; assumed until settings load.
    providers: bool,
    handoff: bool,
    /// Aliases found in settings that the session's own list lacks need a new session.
    discovered_new_session: bool,
    _subscriptions: Vec<Subscription>,
}

impl ModelPicker {
    pub(super) fn new(entries: Vec<Entry>, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        let task = cx.background_spawn(async {
            settings::load().map(|s| (s.favorites, s.models, s.api_provider.is_some()))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((favorites, aliases, providers)) => {
                        this.favorites = favorites;
                        this.providers = providers;
                        this.add_aliases(aliases);
                    }
                    Err(error) => this.notice = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        Self {
            entries: dedup(entries),
            favorites: vec![],
            search,
            cursor: 0,
            scroll: UniformListScrollHandle::new(),
            loading: false,
            notice: None,
            show_older: false,
            runtime: None,
            providers: true,
            handoff: false,
            discovered_new_session: false,
            _subscriptions: subscriptions,
        }
    }

    /// Show the "Run with" control, starting on `runtime`.
    pub(super) fn with_runtime(mut self, runtime: Runtime) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Offer "Hand off" on rows that need a new session.
    pub(super) fn with_handoff(mut self) -> Self {
        self.handoff = true;
        self
    }

    pub(super) fn with_discovered_new_session(mut self) -> Self {
        self.discovered_new_session = true;
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
        let task = cx.background_spawn(async { settings::discover_models() });
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
        let mut favorites = self.favorites.clone();
        match favorites.iter().position(|f| f == id) {
            Some(index) => {
                favorites.remove(index);
            }
            None => favorites.push(id.to_owned()),
        }
        let task = cx.background_spawn(async move { write_favorites(favorites) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(saved) => {
                        this.favorites = saved.clone();
                        this.notice = None;
                        cx.emit(ModelPickerEvent::Starred(saved));
                    }
                    Err(error) => this.notice = Some(error),
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
                kit::row()
                    .id(("model-row", ix))
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
                    })
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
    Entry {
        id: id.to_owned(),
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

/// Persist the favourites with the same edit the Favorites page uses. A stale revision is
/// reloaded and retried once.
fn write_favorites(favorites: Vec<String>) -> Result<Vec<String>, String> {
    let mut revision = settings::load()?.revisions.favorites;
    for _ in 0..2 {
        let request = Request::Edit {
            revision: revision.clone(),
            edit: model::Edit::Favorites {
                models: favorites.clone(),
            },
        };
        match settings::apply(&request)? {
            Ok(saved) => return Ok(saved.favorites),
            Err(_) => revision = settings::load()?.revisions.favorites,
        }
    }
    Err("Starred models changed elsewhere. Try again.".into())
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
        match footer(&self.entries, self.providers) {
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
    use super::{Entry, Footer, Row, api_entry, arrange, dedup, footer, model_rows};

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

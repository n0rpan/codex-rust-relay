//! The shared model picker: a search field, starred models in star order, then every model
//! grouped by provider. Stars only order the lists; any model in the catalog can be chosen.
use crate::{
    appearance::Palette,
    kit::{self, small},
    settings::{self, model, protocol::Request},
};
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState};
use std::{collections::HashSet, rc::Rc};

const ROW_HEIGHT: f32 = 28.;
const VISIBLE_ROWS: usize = 10;

/// One catalog model. `label` is what the row shows; `id` is what is chosen and starred.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub id: String,
    pub label: String,
}

impl Entry {
    /// A catalog alias with no separate display name.
    pub(super) fn alias(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            label: id.to_owned(),
        }
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
}

/// The provider is the part of an alias before the first `/`.
fn provider(id: &str) -> &str {
    id.split_once('/').map_or("Other", |(provider, _)| provider)
}

/// Rows for the picker: a STARRED section in star order (unknown stars are skipped), then every
/// entry grouped by provider and sorted by id. A query matches id or label, case-insensitively,
/// and filters both sections.
pub(super) fn arrange(entries: &[Entry], favorites: &[String], query: &str) -> Vec<Row> {
    let needle = query.trim().to_lowercase();
    let matches = |entry: &Entry| {
        needle.is_empty()
            || entry.id.to_lowercase().contains(&needle)
            || entry.label.to_lowercase().contains(&needle)
    };
    let mut seen = HashSet::new();
    let starred: Vec<usize> = favorites
        .iter()
        .filter_map(|id| {
            let index = entries.iter().position(|entry| &entry.id == id)?;
            (matches(&entries[index]) && seen.insert(index)).then_some(index)
        })
        .collect();
    let mut rows = Vec::new();
    if !starred.is_empty() {
        rows.push(Row::Heading("STARRED".into()));
        rows.extend(starred.into_iter().map(|entry| Row::Model {
            entry,
            starred: true,
        }));
    }
    let mut all: Vec<usize> = (0..entries.len())
        .filter(|&index| matches(&entries[index]))
        .collect();
    // Unknown providers go last; otherwise providers are compared case-insensitively.
    let group = |id: &str| {
        let name = provider(id);
        (name == "Other", name.to_lowercase())
    };
    all.sort_by(|&a, &b| {
        group(&entries[a].id)
            .cmp(&group(&entries[b].id))
            .then_with(|| entries[a].id.cmp(&entries[b].id))
    });
    let mut current = None;
    for index in all {
        let name = provider(&entries[index].id);
        if current != Some(name) {
            rows.push(Row::Heading(name.to_owned()));
            current = Some(name);
        }
        let starred = favorites.contains(&entries[index].id);
        rows.push(Row::Model {
            entry: index,
            starred,
        });
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
    Starred(Vec<String>),
    Close,
}
impl EventEmitter<ModelPickerEvent> for ModelPicker {}

pub(super) struct ModelPicker {
    entries: Vec<Entry>,
    favorites: Vec<String>,
    search: Entity<InputState>,
    /// Index into the model rows (headings are skipped).
    cursor: usize,
    scroll: UniformListScrollHandle,
    loading: bool,
    notice: Option<String>,
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
        let task = cx.background_spawn(async { settings::load().map(|s| s.favorites) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(favorites) => this.favorites = favorites,
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
            _subscriptions: subscriptions,
        }
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

    fn choose_current(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let rows = arrange(&self.entries, &self.favorites, &self.query(cx));
        let models = model_rows(&rows);
        if let Some(Row::Model { entry, .. }) = models
            .get(self.cursor.min(models.len().saturating_sub(1)))
            .map(|&ix| &rows[ix])
        {
            cx.emit(ModelPickerEvent::Choose(self.entries[*entry].id.clone()));
        }
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = arrange(&self.entries, &self.favorites, &self.query(cx));
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
                .text_size(px(11.))
                .text_color(rgb(p.muted))
                .child(text.clone())
                .into_any_element(),
            Row::Model { entry, starred } => {
                let entry = &self.entries[*entry];
                let id = entry.id.clone();
                let star = *starred;
                let active = selected == Some(ix);
                kit::row()
                    .id(("model-row", ix))
                    .h(px(ROW_HEIGHT))
                    .px_2()
                    .gap_1()
                    .w_full()
                    .text_size(px(14.))
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
                            .child(kit::star_column(p, star))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_star(&id, cx);
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(entry.label.clone()),
                    )
                    .into_any_element()
            }
        }
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
        let rows = arrange(&self.entries, &self.favorites, &self.query(cx));
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
            .child(Input::new(&self.search));
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
        view
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, Row, arrange, dedup, model_rows};

    fn entry(id: &str) -> Entry {
        Entry::alias(id)
    }

    fn ids(entries: &[Entry], rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Heading(text) => format!("#{text}"),
                Row::Model { entry: i, starred } => {
                    format!("{}{}", entries[*i].id, if *starred { "*" } else { "" })
                }
            })
            .collect()
    }

    #[test]
    fn starred_first_in_star_order_then_grouped_by_provider() {
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
        let rows = arrange(&entries, &favorites, "");
        assert_eq!(
            ids(&entries, &rows),
            [
                "#STARRED",
                "openai/a*",
                "solo*",
                "#anthropic",
                "anthropic/a",
                "#openai",
                "openai/a*",
                "openai/b",
                "#Other",
                "solo*",
            ]
        );
    }

    #[test]
    fn query_filters_both_sections_and_matches_labels() {
        let entries = vec![
            Entry {
                id: "openai/b".into(),
                label: "Bravo".into(),
            },
            entry("openai/a"),
            entry("anthropic/a"),
        ];
        let favorites = vec!["openai/b".to_owned(), "anthropic/a".to_owned()];
        let rows = arrange(&entries, &favorites, "  BRA ");
        assert_eq!(
            ids(&entries, &rows),
            ["#STARRED", "openai/b*", "#openai", "openai/b*"]
        );
        let rows = arrange(&entries, &favorites, "zzz");
        assert!(rows.is_empty());
        assert_eq!(model_rows(&arrange(&entries, &favorites, "a")).len(), 5);
    }

    #[test]
    fn duplicate_ids_are_listed_once() {
        let entries = dedup(vec![entry("x/1"), entry("x/1"), entry("y/2")]);
        assert_eq!(entries.len(), 2);
        let rows = arrange(&entries, &["x/1".into(), "x/1".into()], "");
        // One starred row plus one row in each of the two provider groups.
        assert_eq!(model_rows(&rows).len(), 3);
    }
}

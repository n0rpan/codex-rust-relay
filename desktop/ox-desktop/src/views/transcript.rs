mod cards;
mod tools;
use super::workbench::Workbench;
use crate::{appearance::Palette, col, kit::button, small};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::text::{TextView, TextViewState};
use gpui_component::{ActiveTheme, menu::ContextMenuExt};
use ox_desk_core::actions::Action as DeskAction;
use ox_desk_core::{
    transcript::item::Item,
    transcript::{ItemKey, Row},
};
use std::collections::HashSet;

const OVERDRAW: f32 = 240.;

struct CachedMessage {
    key: ItemKey,
    revision: u64,
    state: Entity<TextViewState>,
    scrollable: bool,
}

pub(super) struct TranscriptView {
    list: ListState,
    expanded: HashSet<ItemKey>,
    tool_limits: std::collections::HashMap<ItemKey, usize>,
    all_expanded: bool,
    hide_thinking: bool,
    find: Option<tools::Find>,
    prepended: usize,
    following: bool,
    document_height: Pixels,
    markdown: std::collections::VecDeque<CachedMessage>,
}

impl TranscriptView {
    fn message(
        &mut self,
        key: &ItemKey,
        revision: u64,
        text: &str,
        project: Option<std::path::PathBuf>,
        cx: &mut App,
    ) -> TextView {
        // Visible rows can be measured more than once per frame. Reuse their parser,
        // selections and layout. Evicted rows reconstruct directly from the store.
        let mut cached = self
            .markdown
            .iter()
            .position(|entry| &entry.key == key)
            .and_then(|index| self.markdown.remove(index))
            .unwrap_or_else(|| CachedMessage {
                key: key.clone(),
                revision,
                scrollable: false,
                state: cx.new(|cx| TextViewState::markdown(text, cx)),
            });
        if cached.revision != revision {
            cached.state.update(cx, |s, cx| s.set_text(text, cx));
            cached.revision = revision;
        }
        // A document with thousands of paragraphs would otherwise measure every
        // paragraph on every append. The native text view owns a virtual block list.
        let scrollable = text.len() >= 16 * 1024;
        if scrollable && !cached.scrollable && self.following {
            cached
                .state
                .read(cx)
                .list_state()
                .set_follow_mode(FollowMode::Tail);
            cached.state.read(cx).list_state().scroll_to_end();
        }
        cached.scrollable = scrollable;
        let height = self.document_height;
        let element = cards::markdown(TextView::new(&cached.state), project)
            .selectable(true)
            .when(scrollable, |view| view.scrollable(true).h(height));
        self.markdown.push_front(cached);
        self.markdown.truncate(48);
        element
    }
    pub(super) fn uniform_row_budget(&self, row: f32) -> Option<usize> {
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        (row > 0.).then(|| ((viewport + OVERDRAW) / row).ceil() as usize + 1)
    }
}

impl Workbench {
    pub(super) fn transcript_panel(&mut self, cx: &mut Context<Self>) -> Div {
        let p = Palette::get(cx);
        let Some(store) = &mut self.store else {
            return col().flex_1().min_h_0();
        };
        let Some(id) = store.selected().map(str::to_owned) else {
            return col().flex_1().min_h_0();
        };
        let changed_rows = store.take_remeasure(&id);
        let Some(transcript) = store.transcript(&id) else {
            return col().flex_1().min_h_0();
        };
        let count = transcript.len();
        let view = self.transcript_views.entry(id.clone()).or_insert_with(|| {
            let list = ListState::new(count, ListAlignment::Bottom, px(OVERDRAW));
            list.set_follow_mode(FollowMode::Tail);
            let owner = id.clone();
            list.set_scroll_handler(cx.listener(move |s, event: &ListScrollEvent, _, cx| {
                if let Some(view) = s.transcript_views.get_mut(&owner)
                    && view.following != event.is_following_tail
                {
                    view.following = event.is_following_tail;
                    cx.notify();
                }
                if event.visible_range.start <= 2
                    && !event.is_following_tail
                    && s.store
                        .as_ref()
                        .and_then(|store| store.transcript(&owner))
                        .is_some_and(|t| t.has_older() && !t.is_loading())
                {
                    s.action(cx, DeskAction::LoadOlder(owner.clone()));
                }
            }));
            TranscriptView {
                list,
                expanded: HashSet::new(),
                tool_limits: Default::default(),
                all_expanded: false,
                hide_thinking: false,
                find: None,
                prepended: transcript.prepended(),
                following: true,
                document_height: px(360.),
                markdown: Default::default(),
            }
        });
        let inserted = transcript.prepended().saturating_sub(view.prepended);
        if inserted > 0 {
            view.list.splice(0..0, inserted);
        }
        view.prepended = transcript.prepended();
        let previous = view.list.item_count();
        if count > previous {
            view.list.splice(previous..previous, count - previous);
        }
        for row in changed_rows {
            view.list.remeasure_items(row..row + 1);
        }
        let height = view.list.viewport_bounds().size.height;
        if height > px(0.) {
            view.document_height = (height * 0.8).max(px(140.));
        }
        if let Some(find) = &mut view.find
            && find.paging
            && !transcript.is_loading()
        {
            find.paging = false;
            let owner = id.clone();
            let workbench = cx.weak_entity();
            cx.defer(move |cx| {
                let _ = workbench.update(cx, |s, cx| {
                    if s.store.as_ref().and_then(|store| store.selected()) == Some(&owner) {
                        s.search_transcript(if inserted == 0 { 0 } else { 1 }, cx);
                    }
                });
            });
        }
        let list_state = view.list.clone();
        let find_bar = tools::find_bar(view, &id, p, cx);
        col()
            .flex_1()
            .min_h_0()
            .children(find_bar)
            .when(!view.following, |d| {
                let state = view.list.clone();
                d.child(crate::kit::icon_button(
                    p,
                    "jump-latest",
                    "↓",
                    "Jump to latest",
                    cx.listener(move |_, _, _, cx| {
                        state.scroll_to_end();
                        cx.notify();
                    }),
                ))
            })
            .when(transcript.is_loading(), |d| {
                d.child(
                    crate::row()
                        .gap_2()
                        .when_some(self.runner.clone(), |r, ox| r.child(ox))
                        .child(small(p, "Loading history…")),
                )
            })
            .when_some(transcript.error(), |d, error| {
                d.child(
                    crate::row()
                        .flex_wrap()
                        .gap_2()
                        .child(small(p, error.to_owned()))
                        .child(crate::kit::copy_message(p, "copy-transcript-error", error)),
                )
            })
            .when(transcript.has_older() && !transcript.is_loading(), |d| {
                let id = id.clone();
                d.child(button(
                    p,
                    "older-turns",
                    "Earlier messages",
                    false,
                    true,
                    cx.listener(move |s, _, _, cx| s.action(cx, DeskAction::LoadOlder(id.clone()))),
                ))
            })
            .child(
                list(
                    list_state,
                    cx.processor(move |s, index, _, cx| {
                        crate::app::guard(cx, |cx| s.transcript_row(&id, index, cx))
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
    }

    fn transcript_row(&mut self, id: &str, index: usize, cx: &mut Context<Self>) -> AnyElement {
        if let Some(stats) = &mut self.stats {
            stats.rows += 1;
        }
        let p = Palette::get(cx);
        let Some(store) = &self.store else {
            return div().into_any_element();
        };
        let runtime = store.runtime_name();
        let Some(t) = store.transcript(id) else {
            return div().into_any_element();
        };
        let Some(row) = t.row(index) else {
            return div().into_any_element();
        };
        let view = self.transcript_views.get_mut(id).unwrap();
        let owner = id.to_owned();
        let mut frame = col().w_full().min_w_0().px_3().py_1().gap_1();
        if let Some(stats) = &mut self.stats {
            if stats
                .tail_owner
                .as_ref()
                .is_none_or(|(thread, size)| thread != id || *size != t.len())
            {
                stats.tail_owner = Some((id.into(), t.len()));
                stats.painted = Default::default();
                stats.tail_height = Default::default();
                stats.measured_items.clear();
            }
            stats.measured_items.insert(index);
        }
        if let Some(stats) = &self.stats
            && index + 1 == t.len()
        {
            let painted = stats.painted.clone();
            let height = stats.tail_height.clone();
            frame = frame.relative().child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, _, _| {
                        height.set(f32::from(bounds.size.height));
                        if painted.get() == 0 {
                            painted.set(ox_desk_core::perf::monotonic_ns());
                        }
                    },
                )
                .absolute()
                .size_full(),
            );
        }
        match row {
            Row::Message(key) => {
                let entry = t.entry(key).expect("transcript row has an entry");
                if view.find.as_ref().and_then(|f| f.selected.as_ref()) == Some(key) {
                    frame = frame.border_l_2().border_color(rgb(p.accent));
                }
                let element = match &entry.item {
                    Item::Reasoning { summary } if summary.is_empty() || view.hide_thinking => {
                        div()
                    }
                    Item::Reasoning { summary } => {
                        let expanded = view.all_expanded ^ view.expanded.contains(key);
                        let key = key.clone();
                        let body = if expanded {
                            summary.join("\n\n")
                        } else {
                            summary
                                .first()
                                .and_then(|s| s.lines().next())
                                .unwrap_or("Thinking")
                                .to_owned()
                        };
                        frame.child(
                            div()
                                .id(SharedString::from(format!("reasoning-{index}")))
                                .cursor_pointer()
                                .italic()
                                .text_xs()
                                .text_color(rgb(p.muted))
                                .when(!expanded, |d| d.truncate())
                                .child(body)
                                .on_click(cx.listener(move |s, _, _, cx| {
                                    s.toggle_row(&owner, index, &key, cx)
                                })),
                        )
                    }
                    Item::UserMessage { content } => {
                        let text = content
                            .iter()
                            .filter_map(|c| c.text.as_deref().or(c.path.as_deref()))
                            .collect::<Vec<_>>()
                            .join("\n");
                        frame.items_end().py_2().child(
                            col()
                                .min_w_0()
                                .max_w(relative(0.88))
                                .gap_1()
                                .rounded_lg()
                                .border_r_2()
                                .border_color(rgb(p.accent))
                                .bg(rgb(crate::appearance::mix(p.panel, p.accent, 16)))
                                .px_3()
                                .py_2()
                                .child(
                                    small(p, "You")
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(p.fg)),
                                )
                                .child(markdown(id, key, text)),
                        )
                    }
                    Item::AgentMessage { text, is_update } => frame
                        .child(small(p, if *is_update { "Update" } else { runtime }))
                        .child(view.message(
                            key,
                            entry.revision,
                            text,
                            t.active_turn().is_none().then(|| {
                                store
                                    .catalog_row(id)
                                    .map(|r| &r.cwd)
                                    .unwrap_or(&self.project)
                                    .clone()
                            }),
                            cx,
                        )),
                    Item::Plan { text } => {
                        frame
                            .child(small(p, "Plan"))
                            .child(markdown(id, key, text.clone()))
                    }
                    _ => frame,
                };
                let target = key.clone();
                let owner = id.to_owned();
                let view = cx.weak_entity();
                element
                    .context_menu(move |menu, _, cx| {
                        let Some(view) = view.upgrade() else {
                            return menu;
                        };
                        match view
                            .read(cx)
                            .store
                            .as_ref()
                            .and_then(|s| s.transcript(&owner))
                        {
                            Some(t) => tools::copy_menu(menu, t, &target),
                            None => menu,
                        }
                    })
                    .into_any_element()
            }
            Row::Tools(keys) => {
                let mono = cx.theme().mono_font_family.clone();
                let key = keys.front().unwrap().clone();
                let expanded = view.all_expanded ^ view.expanded.contains(&key);
                let binding = key.clone();
                let group_size = keys.len();
                let visible = view.tool_limits.get(&key).copied().unwrap_or(cards::PAGE);
                let mut group = col()
                    .id(("tool-card", index))
                    .min_w_0()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(p.line));
                group = group.child(cards::header(
                    index,
                    t.entry(keys.back().expect("tool group has entries"))
                        .unwrap(),
                    group_size,
                    expanded,
                    p,
                    &mono,
                    cx.listener({
                        let owner = owner.clone();
                        move |s, _, _, cx| s.toggle_row(&owner, index, &binding, cx)
                    }),
                ));
                if expanded {
                    for (offset, item_key) in keys.iter().take(visible).enumerate() {
                        if let Some(entry) = t.entry(item_key) {
                            let local_review = t
                                .active_turn()
                                .is_none()
                                .then(|| {
                                    super::terminal_review::Review::failed(entry, &self.project)
                                })
                                .flatten();
                            let view = cx.weak_entity();
                            let (id, item) = (owner.clone(), item_key.clone());
                            group = group.child(cards::detail(
                                entry,
                                offset,
                                group_size > 1,
                                p,
                                &mono,
                                move |_, _, cx| {
                                    if let Some(view) = view.upgrade()
                                        && let Some(entry) = view
                                            .read(cx)
                                            .store
                                            .as_ref()
                                            .and_then(|s| s.transcript(&id))
                                            .and_then(|t| t.entry(&item))
                                    {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            cards::full_text(entry),
                                        ));
                                    }
                                },
                            ));
                            if let Some(review) = local_review {
                                let source = super::terminal_review::Source::Failed {
                                    thread: owner.clone(),
                                    item: item_key.clone(),
                                };
                                group =
                                    group
                                        .child(small(p, review.directory_label()))
                                        .child(button(
                                            p,
                                            ("terminal-tool", offset),
                                            "Run in OX terminal",
                                            false,
                                            true,
                                            cx.listener(move |s, _, _, cx| {
                                                s.review_tool_command(
                                                    source.clone(),
                                                    review.clone(),
                                                    cx,
                                                );
                                            }),
                                        ));
                            }
                        }
                    }
                    if group_size > visible {
                        group = group.child(cards::more(
                            group_size - visible,
                            cx.listener(move |s, _, _, cx| {
                                if let Some(view) = s.transcript_views.get_mut(&owner) {
                                    view.tool_limits.insert(key.clone(), visible + cards::PAGE);
                                    view.list.remeasure_items(index..index + 1);
                                    cx.notify();
                                }
                            }),
                        ));
                    }
                }
                frame.child(group).into_any_element()
            }
        }
    }

    fn toggle_row(&mut self, owner: &str, index: usize, key: &ItemKey, cx: &mut Context<Self>) {
        if let Some(state) = self.transcript_views.get_mut(owner) {
            if !state.expanded.remove(key) {
                state.expanded.insert(key.clone());
            }
            state.list.remeasure_items(index..index + 1);
            cx.notify();
        }
    }
}

fn markdown(owner: &str, key: &ItemKey, text: String) -> impl IntoElement {
    cards::markdown(
        TextView::markdown(
            SharedString::from(format!("{owner}/{}/{}", key.turn, key.item)),
            text,
        ),
        None,
    )
}

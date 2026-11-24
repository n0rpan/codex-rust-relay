//! Transcript operations work on display items, independently of the owning runtime.
use super::{TranscriptView, Workbench, cards};
use crate::{
    appearance::Palette,
    kit::{PopupMenuItem, button, field},
    row,
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    input::{Input, InputEvent, InputState},
    menu::PopupMenu,
};
use ox_desk_core::transcript::{Entry, ItemKey, Row, Transcript, item::Item};
use std::borrow::Cow;

pub(super) struct Find {
    input: Entity<InputState>,
    pub(super) selected: Option<ItemKey>,
    result: String,
    pub(super) paging: bool,
    _events: Subscription,
}

fn entry_text(entry: &Entry) -> Cow<'_, str> {
    match &entry.item {
        Item::UserMessage { content } => content
            .iter()
            .filter_map(|c| c.text.as_deref().or(c.path.as_deref()))
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
        Item::Reasoning { summary } => summary.join("\n\n").into(),
        Item::Plan { text } | Item::AgentMessage { text, .. } => text.as_str().into(),
        _ => cards::full_text(entry).into(),
    }
}

fn matches(t: &Transcript, query: &str, hide_thinking: bool) -> Vec<(usize, ItemKey)> {
    let query = query.to_lowercase();
    let mut found = Vec::new();
    if query.is_empty() {
        return found;
    }
    for index in 0..t.len() {
        let has_text = |k: &ItemKey| {
            t.entry(k).is_some_and(|entry| {
                !(hide_thinking && matches!(entry.item, Item::Reasoning { .. }))
                    && entry_text(entry).to_lowercase().contains(&query)
            })
        };
        let key = match t.row(index) {
            Some(Row::Message(k)) if has_text(k) => Some(k),
            Some(Row::Tools(keys)) if keys.iter().any(has_text) => keys.front(),
            _ => None,
        };
        if let Some(key) = key {
            found.push((index, key.clone()))
        }
    }
    found
}

fn code_blocks(text: &str) -> Vec<(String, String)> {
    use markdown::mdast::Node;
    let mut result = Vec::new();
    if let Ok(node) = markdown::to_mdast(text, &markdown::ParseOptions::default()) {
        let mut nodes = vec![&node];
        while let Some(n) = nodes.pop() {
            if let Node::Code(c) = n {
                let i = result.len() + 1;
                let label = c.lang.as_deref().filter(|s| !s.is_empty()).map_or_else(
                    || format!("Copy code block {i}"),
                    |lang| format!("Copy {lang} block {i}"),
                );
                result.push((label, c.value.clone()));
            } else if let Some(children) = n.children() {
                nodes.extend(children.iter().rev());
            }
        }
    }
    result
}

pub(super) fn copy_menu(mut menu: PopupMenu, t: &Transcript, key: &ItemKey) -> PopupMenu {
    let Some(entry) = t.entry(key) else {
        return menu;
    };
    let text = entry_text(entry).into_owned();
    let blocks = code_blocks(&text);
    menu = menu.item(
        PopupMenuItem::new("Copy message").on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
        }),
    );
    for (label, value) in blocks {
        menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(value.clone()))
        }));
    }
    menu
}

impl Workbench {
    pub(in crate::views) fn find_in_transcript(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(owner) = self
            .store
            .as_ref()
            .and_then(|s| s.selected())
            .map(str::to_owned)
        else {
            return;
        };
        let Some(view) = self.transcript_views.get_mut(&owner) else {
            return;
        };
        if let Some(find) = &view.find {
            find.input.read(cx).focus_handle(cx).focus(window, cx);
            return;
        }
        let input = field("", "Find in transcript", false, window, cx);
        let events = cx.subscribe(&input, move |s, _, event: &InputEvent, cx| match event {
            InputEvent::Change => s.search_transcript(0, cx),
            InputEvent::PressEnter { shift, .. } => {
                s.search_transcript(if *shift { -1 } else { 1 }, cx)
            }
            _ => {}
        });
        let focus = input.read(cx).focus_handle(cx);
        window.on_next_frame(move |w, cx| focus.focus(w, cx));
        view.find = Some(Find {
            input,
            selected: None,
            result: String::new(),
            paging: false,
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn search_transcript(&mut self, direction: i32, cx: &mut Context<Self>) {
        let Some(store) = &self.store else { return };
        let Some(id) = store.selected() else { return };
        let Some(t) = store.transcript(id) else {
            return;
        };
        let Some(view) = self.transcript_views.get_mut(id) else {
            return;
        };
        let Some(find) = &mut view.find else { return };
        let query = find.input.read(cx).value();
        let hits = matches(t, query.trim(), view.hide_thinking);
        find.paging = false;
        if hits.is_empty() {
            find.selected = None;
            find.result = if query.trim().is_empty() {
                String::new()
            } else if t.has_older() {
                "No match in loaded history".into()
            } else {
                "No matches".into()
            };
            if direction != 0 && !query.trim().is_empty() && t.has_older() && !t.is_loading() {
                find.paging = true;
                self.action(cx, ox_desk_core::actions::Action::LoadOlder(id.to_owned()));
            }
        } else {
            let prev = find
                .selected
                .as_ref()
                .and_then(|k| hits.iter().position(|(_, key)| key == k));
            let cursor = match (prev, direction) {
                (Some(n), 1) => (n + 1) % hits.len(),
                (Some(n), -1) => (n + hits.len() - 1) % hits.len(),
                _ => 0,
            };
            let (index, key) = &hits[cursor];
            find.result = format!("{} of {}", cursor + 1, hits.len());
            find.selected = Some(key.clone());
            if matches!(t.row(*index), Some(Row::Tools(_))) {
                if view.all_expanded {
                    view.expanded.remove(key);
                } else {
                    view.expanded.insert(key.clone());
                }
                view.list.remeasure_items(*index..*index + 1);
            }
            view.list.scroll_to_reveal_item(*index);
        }
        cx.notify();
    }

    pub(in crate::views) fn transcript_folding(
        &mut self,
        expand: Option<bool>,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.store.as_ref().and_then(|s| s.selected()) else {
            return;
        };
        if let Some(v) = self.transcript_views.get_mut(id) {
            if let Some(expand) = expand {
                v.all_expanded = expand;
                v.expanded.clear();
            } else {
                v.hide_thinking = !v.hide_thinking;
            }
            v.list.remeasure_items(0..v.list.item_count());
            self.search_transcript(0, cx);
            cx.notify();
        }
    }
}

pub(super) fn find_bar(
    view: &TranscriptView,
    owner: &str,
    p: Palette,
    cx: &mut Context<Workbench>,
) -> Option<Div> {
    let find = view.find.as_ref()?;
    let id = owner.to_owned();
    Some(
        row()
            .gap_1()
            .child(div().flex_1().child(Input::new(&find.input)))
            .child(crate::kit::small(p, find.result.clone()))
            .child(button(
                p,
                "find-prev",
                "Previous",
                false,
                true,
                cx.listener(|s, _, _, cx| s.search_transcript(-1, cx)),
            ))
            .child(button(
                p,
                "find-next",
                "Next",
                false,
                true,
                cx.listener(|s, _, _, cx| s.search_transcript(1, cx)),
            ))
            .when_some(find.selected.clone(), |r, k| {
                use crate::kit::DropdownMenu;
                let view = cx.weak_entity();
                let id = owner.to_owned();
                r.child(
                    button(p, "find-copy", "Copy", false, true, |_, _, _| {}).dropdown_menu(
                        move |menu, _, cx| {
                            let Some(view) = view.upgrade() else {
                                return menu;
                            };
                            match view.read(cx).store.as_ref().and_then(|s| s.transcript(&id)) {
                                Some(t) => copy_menu(menu, t, &k),
                                None => menu,
                            }
                        },
                    ),
                )
            })
            .child(button(
                p,
                "find-close",
                "Close",
                false,
                true,
                cx.listener(move |s, _, _, cx| {
                    if let Some(v) = s.transcript_views.get_mut(&id) {
                        v.find = None;
                        cx.notify();
                    }
                }),
            )),
    )
}

#[cfg(test)]
mod tests {
    use super::code_blocks;
    #[test]
    fn unclosed_fences_copy_the_visible_code_and_unmatched_inline_ticks_stay_in_prose() {
        let text = "An unmatched `inline marker.\n\n```bash\nprintf 'partial'\n";
        assert_eq!(
            code_blocks(text),
            [("Copy bash block 1".into(), "printf 'partial'".into())]
        );
        assert!(code_blocks("Unfinished `inline").is_empty());
        assert_eq!(
            code_blocks("~~~python\nprint(1)\n```"),
            [("Copy python block 1".into(), "print(1)\n```".into())]
        );
    }
    #[test]
    fn copy_uses_markdown_blocks_in_document_order() {
        let blocks = code_blocks(
            "Inline `untouched`.\n\n> ~~~rust\n>     let x = 1;\n> ~~~\n\n    echo second",
        );
        assert_eq!(
            blocks,
            [
                ("Copy rust block 1".into(), "    let x = 1;".into()),
                ("Copy code block 2".into(), "echo second".into())
            ]
        );
    }
}

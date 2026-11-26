use super::*;
use ox_desk_core::keys::Command;

impl Host {
    pub(super) fn shortcut(
        &mut self,
        key: Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if matches!(
            key,
            Command::Composer | Command::New | Command::Changes | Command::Find
        ) {
            self.close_drawer(window, cx);
        }
        match key {
            Command::Palette => unreachable!("handled in the key interceptor"),
            Command::Settings => self.show_settings(window, cx),
            Command::Sidebar => self.toggle_sidebar(window, cx),
            _ if self.preferences.is_some() => return false,
            Command::Previous | Command::Next => {
                self.index_catalog(cx);
                let active = self.panes[self.active].read(cx);
                let selected = active.store.as_ref().and_then(|s| s.selected());
                let current = self.catalog.iter().position(|e| {
                    self.located_entry(e, cx)
                        .is_some_and(|(r, project, scope)| {
                            project == active.project
                                && scope.matches(&active.route)
                                && Some(r.id.as_str()) == selected
                        })
                });
                let mut rows = self
                    .catalog
                    .iter()
                    .enumerate()
                    .filter_map(|(i, e)| Some((i, self.located_entry(e, cx)?)));
                let choice = if key == Command::Next {
                    rows.find(|(i, _)| current.is_none_or(|pos| *i > pos))
                } else {
                    rows.rev().find(|(i, _)| current.is_none_or(|pos| *i < pos))
                };
                if let Some((i, (row, project, scope))) = choice {
                    self.activate_row(project.to_owned(), scope, row.clone(), window, cx);
                    self.list_scroll.scroll_to_item(i, ScrollStrategy::Nearest);
                }
            }
            _ => self.panes[self.active].update(cx, |pane, cx| {
                let Some(store) = &pane.store else {
                    return;
                };
                let owner = store.selected().map(str::to_owned);
                match key {
                    Command::New if store.state() == &Connection::Ready => {
                        pane.action(cx, DeskAction::NewDraft)
                    }
                    Command::Find => pane.find_in_transcript(window, cx),
                    Command::Composer => {
                        if let Some(editor) = pane.editors.get(&owner) {
                            editor.read(cx).focus_handle(cx).focus(window, cx);
                        }
                    }
                    Command::Changes => pane.toggle_review(cx),
                    Command::Stop => {
                        if let Some(id) = owner
                            && store
                                .transcript(&id)
                                .and_then(|t| t.active_turn())
                                .is_some()
                        {
                            pane.action(cx, DeskAction::Stop(id));
                        }
                    }
                    Command::Answer(choice) => {
                        if let Some(ask) = store.asks().get(pane.visible_ask_index(store))
                            && choice < ask.decisions().len()
                        {
                            pane.action(cx, DeskAction::AnswerAsk(ask.id.clone(), choice));
                        }
                    }
                    Command::SubmitAnswers => {
                        if let Some(ask) = store.asks().get(pane.visible_ask_index(store))
                            && let Some(fields) = pane.answer_fields.get(&ask.id)
                        {
                            pane.action(
                                cx,
                                DeskAction::AnswerQuestions(
                                    ask.id.clone(),
                                    fields
                                        .iter()
                                        .map(|f| f.read(cx).value().to_string())
                                        .collect(),
                                ),
                            );
                        }
                    }
                    _ => {}
                }
            }),
        }
        true
    }
}

//! Editable library over the same store used by the worker gateway and the session host.
use super::forms;
use crate::{
    appearance::{Palette, Step},
    kit::{button, col, row, small},
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::input::{Input, InputState};
use ox_memory::{Kind, MemoryEntry, MemoryStore, NewMemory, Scope, Sensitivity};
use std::path::PathBuf;

pub(super) struct Library {
    project: PathBuf,
    entries: Vec<MemoryEntry>,
    host: String,
    text: Entity<InputState>,
    filter: Entity<InputState>,
    editing: Option<MemoryEntry>,
    pending_removal: Option<String>,
    loading: bool,
    error: Option<String>,
}

enum Change {
    Reload,
    Add(String),
    Edit { previous: MemoryEntry, text: String },
    Pin(MemoryEntry),
    Forget(String),
}

impl Change {
    fn apply(self, project: PathBuf) -> Result<(String, Vec<MemoryEntry>), ox_memory::MemoryError> {
        let store = MemoryStore::open_default()?;
        let host = ox_memory::default_host_id();
        match self {
            Self::Reload => {}
            Self::Add(text) => {
                store.upsert(NewMemory {
                    id: None,
                    scope: Scope::Project(ox_memory::canonical_project_id(&project)),
                    kind: Kind::Preference,
                    sensitivity: Sensitivity::Private,
                    text,
                    tags: Vec::new(),
                    pin: false,
                    source_host: host.clone(),
                })?;
            }
            Self::Edit { previous, text } => {
                store.edit_text(&previous.id, previous.seq, &text, &host)?;
            }
            Self::Pin(entry) => {
                store.set_pin(&entry.id, !entry.pin, &host)?;
            }
            Self::Forget(id) => {
                store.forget(&id, &host)?;
            }
        }
        let entries = store.list_for_project(&project, &host, Sensitivity::LocalOnly)?;
        Ok((host, entries))
    }
}

impl Library {
    pub(super) fn new(project: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = forms::input("", "Find a saved memory", window, cx);
        cx.observe(&filter, |_, _, cx| cx.notify()).detach();
        let mut this = Self {
            project,
            entries: Vec::new(),
            host: String::new(),
            text: forms::input(
                "",
                "For example, run the tests against the sandbox account",
                window,
                cx,
            ),
            filter,
            editing: None,
            pending_removal: None,
            loading: false,
            error: None,
        };
        this.change(Change::Reload, window, cx);
        this
    }

    fn change(&mut self, change: Change, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        self.error = None;
        let reset = matches!(&change, Change::Add(_) | Change::Edit { .. });
        let project = self.project.clone();
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { change.apply(project) })
                .await;
            let _ = view.update_in(cx, |s, window, cx| {
                s.loading = false;
                match result {
                    Ok((host, entries)) => {
                        s.host = host;
                        s.entries = entries;
                        if reset {
                            s.editing = None;
                            s.text
                                .update(cx, |input, cx| input.set_value("", window, cx));
                        }
                    }
                    Err(error) => s.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for Library {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let project = self.project.file_name().map_or_else(
            || self.project.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let editing = self.editing.is_some();
        let needle = self.filter.read(cx).value().trim().to_lowercase();
        let mut view = col().gap_3()
            .child(row().flex_wrap().justify_between().items_center()
                .child(div().text_size(p.text(Step::Title)).child("OX Memory"))
                .child(button(p, "memory-reload", "Refresh", false, !self.loading,
                    cx.listener(|s, _, window, cx| s.change(Change::Reload, window, cx)))))
            .child(small(p, format!("{} and shared memories on this computer", project)))
            .child(small(p, "Pin a note to make it eligible for new sessions. Agents can recall other saved memories as needed."))
            .child(div().mt_1().child(if editing { "Edit memory" } else { "Add a project preference" }))
            .child(Input::new(&self.text).disabled(self.loading))
            .child(row().gap_2()
                .child(button(p, "memory-save", if editing { "Save changes" } else { "Save for this project" },
                    true, !self.loading, cx.listener(|s, _, window, cx| {
                        let text = s.text.read(cx).value().trim().to_owned();
                        if text.is_empty() {
                            s.error = Some("Enter a preference to save.".into());
                            cx.notify();
                            return;
                        }
                        let change = s.editing.clone().map_or_else(|| Change::Add(text.clone()), |previous| Change::Edit { previous, text: text.clone() });
                        s.change(change, window, cx);
                    })))
                .when(editing, |r| r.child(button(p, "memory-cancel", "Cancel", false, !self.loading,
                    cx.listener(|s, _, window, cx| {
                        s.editing = None;
                        s.text.update(cx, |input, cx| input.set_value("", window, cx));
                        cx.notify();
                    })))))
            .when_some(self.error.clone(), |view, error| {
                view.child(div().text_color(rgb(p.error)).child(error))
            })
            .when(!self.entries.is_empty(), |view| view.child(Input::new(&self.filter)));
        let mut matches = 0_usize;
        for entry in &self.entries {
            if !needle.is_empty() && !entry.text.to_lowercase().contains(&needle) {
                continue;
            }
            matches += 1;
            let remove_id = entry.id.clone();
            let cancel_id = entry.id.clone();
            let edit = entry.clone();
            let pin = entry.clone();
            let scope = match &entry.scope {
                Scope::Global => "All projects",
                Scope::Project(_) => "This project",
                Scope::Host(_) => "This host",
            };
            let sensitivity = match entry.sensitivity {
                Sensitivity::Public => "Public",
                Sensitivity::Private => "Private",
                Sensitivity::LocalOnly => "Local only",
            };
            let pinned = if entry.pin { " · Pinned" } else { "" };
            let source = if self.host == entry.source_host {
                "Last saved on this computer".into()
            } else {
                format!("Last saved on host {}", entry.source_host)
            };
            let removing = self.pending_removal.as_deref() == Some(&entry.id);
            view = view.child(
                col()
                    .p_3()
                    .gap_1()
                    .bg(rgb(p.panel))
                    .border_1()
                    .border_color(rgb(p.line))
                    .child(div().child(entry.text.clone()))
                    .child(small(p, format!("{scope} · {sensitivity}{pinned}")))
                    .child(small(p, source))
                    .child(
                        row()
                            .gap_2()
                            .flex_wrap()
                            .child(button(
                                p,
                                ("memory-edit", matches),
                                "Edit",
                                false,
                                !self.loading,
                                cx.listener(move |s, _, window, cx| {
                                    s.editing = Some(edit.clone());
                                    s.text.update(cx, |input, cx| {
                                        input.set_value(edit.text.clone(), window, cx);
                                        input.focus_handle(cx).focus(window, cx);
                                    });
                                    cx.notify();
                                }),
                            ))
                            .child(button(
                                p,
                                ("memory-pin", matches),
                                if entry.pin { "Unpin" } else { "Pin" },
                                false,
                                !self.loading,
                                cx.listener(move |s, _, window, cx| {
                                    s.change(Change::Pin(pin.clone()), window, cx)
                                }),
                            ))
                            .child(button(
                                p,
                                ("memory-forget", matches),
                                if removing {
                                    "Forget this memory"
                                } else {
                                    "Forget…"
                                },
                                false,
                                !self.loading,
                                cx.listener(move |s, _, window, cx| {
                                    if s.pending_removal.as_deref() == Some(&remove_id) {
                                        s.pending_removal = None;
                                        s.change(Change::Forget(remove_id.clone()), window, cx);
                                    } else {
                                        s.pending_removal = Some(remove_id.clone());
                                        cx.notify();
                                    }
                                }),
                            ))
                            .when(removing, |r| {
                                r.child(button(
                                    p,
                                    ("memory-keep", matches),
                                    "Keep memory",
                                    false,
                                    !self.loading,
                                    cx.listener(move |s, _, _, cx| {
                                        if s.pending_removal.as_deref() == Some(&cancel_id) {
                                            s.pending_removal = None;
                                            cx.notify();
                                        }
                                    }),
                                ))
                            }),
                    ),
            );
        }
        if self.loading && self.entries.is_empty() {
            view = view.child(small(p, "Loading memory…"));
        } else if matches == 0 && self.error.is_none() {
            view = view.child(small(
                p,
                if needle.is_empty() {
                    "No saved memories for this project yet."
                } else {
                    "No matching memories."
                },
            ));
        }
        view
    }
}

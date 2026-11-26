use super::workbench::Workbench;
use crate::{
    appearance::Palette,
    kit::{button, col, row, small},
};
use gpui::*;
use gpui_component::input::{Textarea, TextareaState};
use ox_desk_core::{
    mentions::{ImageInput, token},
    store::Store,
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

fn file_image(path: &Path) -> bool {
    path.is_absolute()
        && path.extension().and_then(|v| v.to_str()).is_some_and(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif"
            )
        })
}
fn mime(format: ImageFormat) -> Option<&'static str> {
    Some(match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::Webp => "image/webp",
        _ => return None,
    })
}
impl Workbench {
    pub(super) fn search_mention(
        &mut self,
        owner: Option<String>,
        editor: Entity<TextareaState>,
        cx: &mut Context<Self>,
    ) {
        let read = editor.read(cx);
        let value = read.value();
        let Some(store) = &mut self.store else {
            return;
        };
        if store.selected() != owner.as_deref() {
            return;
        }
        let range = token(&value, read.cursor());
        self.mention_index = 0;
        self.mention_task = None;
        let Some(range) = range else {
            let _ = store.find_mention(owner.as_deref(), None);
            return;
        };
        let query = value[range.start + 1..range.end].to_owned();
        self.mention_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(110))
                .await;
            let _ = this.update(cx, |s, cx| {
                if let Some(store) = &mut s.store
                    && store.selected() == owner.as_deref()
                {
                    if let Err(err) = store.find_mention(owner.as_deref(), Some(&query)) {
                        s.launch_error = Some(err.to_string());
                    }
                    let _ = s.wake.try_send(());
                    cx.notify();
                }
            });
        }));
    }
    pub(super) fn mention_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(store) = &self.store else {
            return false;
        };
        let owner = store.selected().map(str::to_owned);
        let Some(editor) = self.editors.get(&owner) else {
            return false;
        };
        if !editor.read(cx).focus_handle(cx).is_focused(window)
            || editor.update(cx, |f, cx| f.marked_text_range(window, cx).is_some())
        {
            return false;
        }
        let state = editor.read(cx);
        let value = state.value();
        let Some(range) = token(&value, state.cursor()) else {
            return false;
        };
        let query = &value[range.start + 1..range.end];
        let (matches, _) = store.mention_matches(owner.as_deref(), query);
        if matches.is_empty() {
            return false;
        };
        let last = matches.len() - 1;
        self.mention_index = self.mention_index.min(last);
        match key {
            "up" => self.mention_index = self.mention_index.saturating_sub(1),
            "down" => self.mention_index = (self.mention_index + 1).min(last),
            "tab" => {
                let path = matches[self.mention_index].to_string_lossy().into_owned();
                insert_mention(editor, query, &path, window, cx);
            }
            _ => return false,
        }
        cx.notify();
        true
    }
    fn attach_files(&mut self, owner: Option<&str>, paths: &[PathBuf], cx: &mut Context<Self>) {
        let Some(store) = &mut self.store else {
            return;
        };
        if store.selected() != owner {
            return;
        }
        if self.link.remote().is_some() {
            let paths = paths.to_vec();
            let task = cx
                .background_spawn(async move { crate::platform::attachments::read_images(&paths) });
            self.prepare_images(owner.map(str::to_owned), task, cx);
            return;
        }
        for path in paths {
            if file_image(path) {
                store.attach_image(owner, ImageInput::File(path.clone()));
            } else {
                self.launch_error = Some("Choose a PNG, JPEG, GIF or WebP image.".into());
            }
        }
        cx.notify();
    }
    fn paste_images(
        &mut self,
        owner: Option<String>,
        clip: &ClipboardItem,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .store
            .as_ref()
            .is_none_or(|s| s.selected() != owner.as_deref())
        {
            return false;
        }
        let mut images = vec![];
        let mut handled = false;
        for entry in clip.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    handled = true;
                    if let Some(mime) = mime(image.format) {
                        images.push((mime, image.bytes.clone()));
                    } else {
                        self.launch_error = Some("Paste a PNG, JPEG, GIF or WebP image.".into());
                    }
                }
                ClipboardEntry::ExternalPaths(paths)
                    if paths.paths().iter().any(|p| file_image(p)) =>
                {
                    self.attach_files(owner.as_deref(), paths.paths(), cx);
                    handled = true;
                }
                _ => {}
            }
        }
        if !images.is_empty() {
            let remote = self.link.remote().is_some();
            let convert = cx.background_spawn(async move {
                if remote {
                    crate::platform::attachments::check_limits(
                        images.len(),
                        images.iter().map(|(_, b)| b.len()).sum(),
                    )?;
                }
                Ok(images
                    .into_iter()
                    .map(|(mime, bytes)| crate::platform::attachments::encode(mime, &bytes))
                    .collect())
            });
            self.prepare_images(owner, convert, cx);
        }
        if handled {
            cx.notify();
        }
        handled
    }
    fn prepare_images(
        &mut self,
        owner: Option<String>,
        task: Task<Result<Vec<ImageInput>, String>>,
        cx: &mut Context<Self>,
    ) {
        *self.image_work.entry(owner.clone()).or_default() += 1;
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                if let Some(store) = &mut s.store {
                    let result = result.and_then(|added| {
                        if s.link.remote().is_some() {
                            crate::platform::attachments::check_draft(
                                store.draft_images(owner.as_deref()),
                                &added,
                            )?;
                        }
                        Ok(added)
                    });
                    match result {
                        Ok(images) => {
                            for image in images {
                                store.attach_image(owner.as_deref(), image);
                            }
                        }
                        Err(error) => s.launch_error = Some(error),
                    }
                }
                if let Some(count) = s.image_work.get_mut(&owner) {
                    *count -= 1;
                    if *count == 0 {
                        s.image_work.remove(&owner);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
fn insert_mention(
    input: &Entity<TextareaState>,
    expected: &str,
    chosen: &str,
    window: &mut Window,
    cx: &mut App,
) {
    input.update(cx, |edit, cx| {
        let text = edit.value();
        let Some(range) = token(&text, edit.cursor()) else {
            return;
        };
        if &text[range.start + 1..range.end] != expected {
            return;
        }
        let tag = if chosen.chars().any(char::is_whitespace) {
            format!("@{} ", serde_json::json!(chosen))
        } else {
            format!("@{chosen} ")
        };
        edit.set_selected_range(range, cx);
        edit.replace(tag, window, cx);
        edit.focus_handle(cx).focus(window, cx);
    });
}
pub(super) fn composer_input(
    editor: Entity<TextareaState>,
    owner: Option<String>,
    store: &Store,
    encoding: bool,
    current_match: usize,
    cx: &mut Context<Workbench>,
) -> Div {
    let p = Palette::get(cx);
    let view = cx.weak_entity();
    let dest = owner.clone();
    let pasted = owner.clone();
    let dragging_owner = owner.clone();
    let mut panel = col()
        .relative()
        .gap_1()
        .on_drag_move::<ExternalPaths>(cx.listener(
            move |s, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                let inside = event.bounds.contains(&event.event.position);
                if inside {
                    let files = event.drag(cx).paths();
                    if s.file_hover
                        .as_ref()
                        .is_none_or(|(id, paths)| id != &dragging_owner || paths != files)
                    {
                        s.file_hover = Some((dragging_owner.clone(), files.to_vec()));
                        cx.notify();
                    }
                } else if s.file_hover.take().is_some() {
                    cx.notify();
                }
            },
        ))
        .on_drop(cx.listener(move |s, paths: &ExternalPaths, _, cx| {
            s.file_hover = None;
            s.attach_files(dest.as_deref(), paths.paths(), cx);
        }))
        .child(
            Textarea::new(&editor)
                .appearance(false)
                .on_paste(move |clip, _, cx| {
                    view.update(cx, |s, cx| s.paste_images(pasted.clone(), clip, cx))
                        .unwrap_or(false)
                }),
        );
    let value = editor.read(cx).value();
    if let Some(range) = token(&value, editor.read(cx).cursor()) {
        let query = value[range.start + 1..range.end].to_owned();
        let (files, error) = store.mention_matches(owner.as_deref(), &query);
        if let Some(error) = error {
            panel = panel.child(small(p, error.to_owned()));
        }
        let mut matches = col()
            .id("mention-results")
            .max_h(px(130.))
            .overflow_y_scroll()
            .gap_1();
        for (i, path) in files.iter().enumerate() {
            let name = path.to_string_lossy().into_owned();
            let chosen = name.clone();
            let input = editor.clone();
            let expected = query.clone();
            let owner = owner.clone();
            matches = matches.child(button(
                p,
                ("mention", i),
                name,
                i == current_match,
                true,
                cx.listener(move |s, _, window, cx| {
                    if s.store
                        .as_ref()
                        .is_none_or(|store| store.selected() != owner.as_deref())
                    {
                        return;
                    }
                    insert_mention(&input, &expected, &chosen, window, cx);
                }),
            ));
        }
        if !files.is_empty() {
            panel = panel.child(matches);
        }
    }
    if encoding {
        panel = panel.child(small(p, "Preparing attachment…"));
    }
    let mut chips = row().gap_1().flex_wrap();
    for (i, img) in store.draft_images(owner.as_deref()).iter().enumerate() {
        let target = owner.clone();
        chips = chips.child(button(
            p,
            ("attached-image", i),
            format!("{}   ×", img.label()),
            false,
            true,
            cx.listener(move |s, _, _, cx| {
                if let Some(store) = &mut s.store {
                    store.remove_image(target.as_deref(), i);
                    cx.notify();
                }
            }),
        ));
    }
    if !store.draft_images(owner.as_deref()).is_empty() {
        panel = panel.child(chips);
        if !store.has_command_schema() || store.commands_loading() {
            panel = panel.child(small(p, "Checking model image support…"));
        }
        if let Some(warning) = store.image_model_warning(owner.as_deref()) {
            panel = panel.child(small(p, warning));
        }
    }
    let view = cx.weak_entity();
    panel.child(
        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                let view = view.clone();
                // A native Wayland drag can release over the surface without a regular
                // wl_pointer enter. The file-drag coordinates still identify this field.
                window.on_mouse_event(move |up: &MouseUpEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture
                        || up.button != MouseButton::Left
                        || !bounds.contains(&up.position)
                        || !cx.has_active_drag()
                    {
                        return;
                    }
                    let _ = view.update(cx, |s, cx| {
                        if let Some((owner, paths)) = s.file_hover.take() {
                            cx.stop_active_drag(window);
                            s.attach_files(owner.as_deref(), &paths, cx);
                            cx.stop_propagation();
                        }
                    });
                });
            },
        )
        .absolute()
        .inset_0(),
    )
}

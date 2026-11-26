use super::*;
use crate::appearance::Step;
use crate::settings::protocol::Document;
use gpui_component::input::{Textarea, TextareaState};

pub(super) struct Advanced {
    documents: Vec<(Document, Entity<TextareaState>)>,
    selected: usize,
    conflict_revision: Option<String>,
}
impl Preferences {
    pub(super) fn open_advanced(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.goto(Page::Advanced, cx);
        self.busy = true;
        let link = self.link.clone();
        cx.spawn_in(w, async move |view, cx| {
            let docs = cx
                .background_executor()
                .spawn(async move { link.request::<Vec<Document>>(Request::ReadAdvanced) })
                .await;
            let _ = view.update_in(cx, |s, w, cx| {
                s.busy = false;
                match docs {
                    Ok(docs) => {
                        s.install_documents(docs, None, w, cx);
                        cx.notify();
                    }
                    Err(e) => s.report(Err(e), "", cx),
                }
            });
        })
        .detach();
    }
    fn install_documents(
        &mut self,
        docs: Vec<Document>,
        saved: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut previous = self.advanced.take();
        let selected = previous
            .as_ref()
            .and_then(|a| a.documents.get(a.selected))
            .map(|(d, _)| d.id.clone());
        let mut documents = Vec::with_capacity(docs.len());
        for doc in docs {
            if let Some(previous) = previous.as_mut()
                && let Some(index) = previous
                    .documents
                    .iter()
                    .position(|(old, _)| old.id == doc.id)
            {
                let (old, field) = previous.documents.remove(index);
                if saved != Some(&doc.id) && field.read(cx).value().as_ref() != old.text.as_str() {
                    documents.push((old, field));
                    continue;
                }
                field.update(cx, |f, cx| f.set_value(doc.text.clone(), window, cx));
                documents.push((doc, field));
            } else {
                let field =
                    cx.new(|cx| TextareaState::new(window, cx).default_value(doc.text.clone()));
                documents.push((doc, field));
            }
        }
        let selected = documents
            .iter()
            .position(|(d, _)| Some(&d.id) == selected.as_ref())
            .unwrap_or(0);
        self.advanced = Some(Advanced {
            documents,
            selected,
            conflict_revision: None,
        });
    }
    fn save_advanced(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(a) = &self.advanced else { return };
        let Some((d, field)) = a.documents.get(a.selected) else {
            return;
        };
        let req = Request::SaveAdvanced {
            id: d.id.clone(),
            revision: a
                .conflict_revision
                .clone()
                .unwrap_or_else(|| d.revision.clone()),
            original: d.text.clone(),
            text: field.read(cx).value().to_string(),
        };
        let id = d.id.clone();
        self.busy = true;
        cx.notify();
        let link = self.link.clone();
        cx.spawn_in(w, async move |view, cx| {
            let result = cx.background_executor().spawn(async move {
                match link.request::<model::Outcome>(req)? {
                    model::Outcome::ChangedOnDisk { revision } => Ok(Err(revision)),
                    model::Outcome::Applied => {
                        let docs = link.request::<Vec<Document>>(Request::ReadAdvanced)?;
                        Ok(Ok((docs, link.settings()?)))
                    }
                }
            }).await;
            let _ = view.update_in(cx, |s, window, cx| {
                s.busy = false;
                match result {
                    Ok(Ok((docs, rows))) => {
                        s.install_documents(docs, Some(&id), window, cx);
                        s.install(rows, Some(Page::Advanced), window, cx);
                        if !s.error {
                            s.report(Ok(()), "Document saved.", cx);
                        }
                    }
                    Ok(Err(revision)) => {
                        if let Some(a) = &mut s.advanced {
                            a.conflict_revision = Some(revision);
                        }
                        s.report(Err("Changed on disk. Your text is still here. Save again to reapply your edits to the new revision, or discard the text and reload.".into()), "", cx);
                    }
                    Err(e) => s.report(Err(e), "", cx),
                }
            });
        }).detach();
    }
    pub(super) fn advanced_page(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let Some(a) = &self.advanced else {
            return div().child("Settings documents unavailable.");
        };
        let mut tabs = div().flex().flex_wrap().gap_1();
        for (i, (doc, _)) in a.documents.iter().enumerate() {
            tabs = tabs.child(crate::kit::button(
                p,
                ("document", i),
                doc.title.clone(),
                i == a.selected,
                true,
                cx.listener(move |s, _, _, cx| {
                    if s.busy {
                        return;
                    }
                    if let Some(a) = s.advanced.as_mut() {
                        a.selected = i;
                        a.conflict_revision = None;
                    }
                    s.message.clear();
                    cx.notify();
                }),
            ));
        }
        let mut view=div().flex().flex_col().gap_3().child(tabs)
            .child(div().text_size(p.text(Step::Ui)).text_color(rgb(p.muted)).child("Edit provider, search and model settings as text. Private values and unrecognized registry fields stay in the saved file."));
        if let Some((doc, field)) = a.documents.get(a.selected) {
            view = view
                .child(doc.id.clone())
                .child(
                    div()
                        .h(px(330.))
                        .child(Textarea::new(field).readonly(self.busy).h_full()),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(crate::kit::button(
                            p,
                            "save-document",
                            "Save document",
                            true,
                            true,
                            cx.listener(|s, _, w, cx| s.save_advanced(w, cx)),
                        ))
                        .child(crate::kit::button(
                            p,
                            "reload-documents",
                            "Discard text and reload",
                            false,
                            true,
                            cx.listener(|s, _, w, cx| {
                                if s.busy {
                                    return;
                                }
                                if let Some((doc, field)) = s
                                    .advanced
                                    .as_ref()
                                    .and_then(|a| a.documents.get(a.selected))
                                {
                                    field.update(cx, |e, cx| e.set_value(doc.text.clone(), w, cx));
                                }
                                s.open_advanced(w, cx)
                            }),
                        )),
                );
        }
        view
    }
}

//! Server-side palette searches have a separate cursor and lifetime from the sidebar.
use super::{Connection, Pending, Store, catalog::Catalog};
use crate::codex::{history::ThreadPage, request::Call};
use std::{collections::HashSet, io, path::Path};

#[derive(Default)]
pub(super) struct Search {
    pub(super) generation: u64,
    pub(super) query: String,
    pub(super) rows: Catalog,
    pub(super) cursor: Option<String>,
    seen: HashSet<String>,
    pending: bool,
    pub(super) error: Option<String>,
}
impl Search {
    pub(super) fn add(&mut self, project: &Path, page: ThreadPage) {
        self.pending = false;
        for row in page.data {
            if row.cwd == project {
                self.rows.insert(row);
            }
        }
        self.cursor = page
            .next_cursor
            .filter(|cursor| self.seen.insert(cursor.clone()));
    }
    pub(super) fn failed(&mut self, message: String) {
        self.pending = false;
        self.error = Some(message);
    }
}
impl Store {
    pub fn palette_query(&self) -> &str {
        &self.palette_search.query
    }
    pub fn palette_contains(&self, id: &str) -> bool {
        self.palette_search.rows.get(id).is_some()
    }
    pub fn palette_rows(&self) -> &[super::ThreadRow] {
        &self.palette_search.rows.rows
    }
    pub fn palette_search_error(&self) -> Option<&str> {
        self.palette_search.error.as_deref()
    }
    pub fn palette_has_more(&self) -> bool {
        !self.palette_search.pending && self.palette_search.cursor.is_some()
    }
    pub fn palette_search(&mut self, query: &str) -> io::Result<()> {
        if self.remote().is_some() {
            if self.palette_search.query == query {
                return Ok(());
            }
            self.palette_search.query = query.into();
            return self.forward(crate::actions::Action::PaletteSearch(query.into()));
        }
        if query == self.palette_search.query {
            return Ok(());
        }
        self.palette_search = Search {
            generation: self.palette_search.generation + 1,
            query: query.into(),
            ..Default::default()
        };
        if query.is_empty() || !matches!(self.state, Connection::Ready | Connection::SignInRequired)
        {
            return Ok(());
        }
        if self.is_session_scoped() {
            let rows = self.claude_palette(query);
            for row in rows {
                self.palette_search.rows.insert(row);
            }
            return Ok(());
        }
        self.fetch_palette_page()
    }
    pub fn palette_more(&mut self) -> io::Result<()> {
        if self.remote().is_some() {
            return self.forward(crate::actions::Action::PaletteMore);
        }
        if self.palette_has_more() {
            self.fetch_palette_page()?;
        }
        Ok(())
    }
    fn fetch_palette_page(&mut self) -> io::Result<()> {
        let search = &self.palette_search;
        self.request(
            Call::List {
                project: self.profile.project.clone(),
                query: search.query.clone(),
                cursor: search.cursor.clone(),
            },
            Pending::PaletteSearch(search.generation),
        )?;
        self.palette_search.pending = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::{
        frame::{Frame, RequestId},
        reply::Reply,
    };
    use crate::links::codex::{Launch, Link, LinkEvent};
    use crate::store::{Profile, Route};
    use serde_json::json;

    #[test]
    fn late_palette_pages_never_replace_the_sidebar_or_current_query() {
        let project = std::env::temp_dir();
        let link = Link::spawn(
            Launch {
                executable: "/bin/cat".into(),
                arguments: vec![],
                directory: project.clone(),
                path: "/usr/bin:/bin".into(),
            },
            |_| {},
        )
        .unwrap();
        let mut s = Store::open(
            Profile {
                project: project.clone(),
                route: Route::CODEX_SUBSCRIPTION,
                provider: "openai".into(),
            },
            link,
        )
        .unwrap();
        s.state = Connection::Ready;
        let page = |seq, name: &str, cursor: Option<&str>| {
            LinkEvent::Frame(Frame::Reply{id:RequestId::Number(seq),result:Ok(Reply::Threads(serde_json::from_value(json!({
            "data":[{"id":name,"cwd":project,"modelProvider":"openai","name":name}],"nextCursor":cursor
        })).unwrap()))})
        };
        s.search("sidebar".into()).unwrap();
        s.apply(page(s.seq, "sidebar", None));
        s.edit(None, "keep my draft".into());
        s.palette_search("first").unwrap();
        let stale = s.seq;
        s.palette_search("second").unwrap();
        s.apply(page(stale, "obsolete", None));
        assert!(s.palette_rows().is_empty());
        s.apply(page(s.seq, "result", Some("next")));
        assert!(s.palette_contains("result"));
        s.palette_more().unwrap();
        let closing = s.seq;
        s.palette_search("").unwrap();
        s.apply(page(closing, "late", None));
        assert!(s.palette_rows().is_empty());
        assert_eq!(s.query(), "sidebar");
        assert_eq!(s.threads()[0].id, "sidebar");
        assert_eq!(s.selected(), None);
        s.dispatch(crate::actions::Action::PaletteCommand(
            None,
            "/status".into(),
        ))
        .unwrap();
        assert_eq!(s.draft(None), "keep my draft");
    }
}

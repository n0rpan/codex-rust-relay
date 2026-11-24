use super::{Pending, Store};
use crate::codex::request::Call;
use std::{io, path::PathBuf};
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Search {
    pub(super) version: u64,
    owner: Option<String>,
    query: Option<String>,
    pub(super) paths: Vec<PathBuf>,
    pub(super) error: Option<String>,
}
impl Store {
    pub fn mention_matches(&self, id: Option<&str>, query: &str) -> (&[PathBuf], Option<&str>) {
        if self.mention.owner.as_deref() == id && self.mention.query.as_deref() == Some(query) {
            (&self.mention.paths, self.mention.error.as_deref())
        } else {
            (&[], None)
        }
    }
    pub fn find_mention(&mut self, id: Option<&str>, query: Option<&str>) -> io::Result<()> {
        if self.remote().is_some() {
            return self.forward(crate::actions::Action::Mention(
                id.map(str::to_owned),
                query.map(str::to_owned),
            ));
        }
        if self.mention.owner.as_deref() == id && self.mention.query.as_deref() == query {
            return Ok(());
        }
        self.mention = Search {
            owner: id.map(str::to_owned),
            query: query.map(str::to_owned),
            version: self.mention.version + 1,
            ..Default::default()
        };
        if let Some(query) = query {
            self.request(
                Call::FindFile {
                    project: self.profile.project.clone(),
                    query: query.into(),
                },
                Pending::Mention(self.mention.version),
            )?;
        }
        Ok(())
    }
}

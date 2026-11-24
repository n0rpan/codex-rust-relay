use super::{Store, ThreadRow};
use crate::{
    changed::Changed,
    links::LinkEvent,
    runtime::Billing,
    titles::{self, Index, Key, Launcher, Title},
    transcript::{Row, item::Item},
};
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, OnceLock},
    thread,
};

type Deliver = Arc<dyn Fn(LinkEvent) + Send + Sync>;
pub(super) struct Service {
    launch: Arc<Launcher>,
    deliver: Deliver,
    first: HashMap<String, String>,
    running: HashMap<String, bool>,
}

impl Store {
    /// Installed on the background connection opener, alongside the native catalog.
    pub fn enable_naming(
        &mut self,
        launch: Launcher,
        deliver: impl Fn(LinkEvent) + Send + Sync + 'static,
    ) {
        self.catalog.titles = Index::load(&launch.directory).unwrap_or_default();
        for row in &mut self.catalog.rows {
            self.catalog.titles.decorate(row);
        }
        self.naming = Some(Service {
            launch: Arc::new(launch),
            deliver: Arc::new(deliver),
            first: Default::default(),
            running: Default::default(),
        });
    }
    pub(super) fn first_title_prompt(&mut self, id: &str, text: &str) {
        if let Some(service) = &mut self.naming
            && !text.trim().is_empty()
            && !text.trim_start().starts_with('/')
        {
            service
                .first
                .entry(id.into())
                .or_insert_with(|| text.chars().take(1800).collect());
        }
    }
    pub(super) fn title_after_turn(&mut self, id: &str) {
        if let Some(text) = self.naming.as_mut().and_then(|s| s.first.remove(id)) {
            let _ = self.request_title(id, text, false);
        }
    }
    fn unnamed(&self, id: &str) -> Option<&ThreadRow> {
        let row = self.catalog.get(id)?;
        (self
            .naming
            .as_ref()
            .is_some_and(|s| !s.running.contains_key(id))
            && !row.worker
            && row.parent_thread_id.is_none()
            && row.agent_nickname.is_none()
            && row.name.as_deref().is_none_or(|n| n.trim().is_empty())
            && row.generated_title.is_none()
            && row.cwd == self.profile.project
            && row.model_provider == self.profile.provider)
            .then_some(row)
    }
    fn first_text(&self, id: &str) -> Option<&str> {
        let row = self.catalog.get(id)?;
        if let Some(t) = self.transcripts.get(id)
            && !t.has_older()
        {
            for ix in 0..t.len() {
                if let Some(Row::Message(key)) = t.row(ix)
                    && let Some(entry) = t.entry(key)
                    && let Item::UserMessage { content } = &entry.item
                    && let Some(text) = content
                        .iter()
                        .find_map(|part| part.text.as_deref().filter(|s| !s.trim().is_empty()))
                {
                    return Some(text);
                }
            }
        }
        self.catalog
            .has_first_preview(id)
            .then_some(row.preview.trim())
            .filter(|s| !s.is_empty() && *s != "Untitled thread" && *s != "Untitled Claude session")
    }
    pub fn can_generate_title(&self, id: &str) -> bool {
        if let Some(remote) = self.remote() {
            return remote.can(id, "generate_title");
        }
        self.transcripts.get(id).is_some_and(|t| t.verified)
            && self.unnamed(id).is_some()
            && self.naming_model(id).is_some()
            && self.first_text(id).is_some()
    }
    pub fn generate_thread_title(&mut self, id: &str) -> io::Result<()> {
        if !self.can_generate_title(id) {
            return Err(io::Error::other(
                "Open and verify this unnamed session before generating its title",
            ));
        }
        let text = self
            .first_text(id)
            .ok_or_else(|| io::Error::other("No first message is available for this session"))?
            .chars()
            .take(1800)
            .collect();
        self.request_title(id, text, true)
    }
    fn naming_model(&self, id: &str) -> Option<String> {
        let row = self.catalog.get(id)?;
        self.transcripts
            .get(id)
            .filter(|t| t.verified)
            .and_then(|t| t.model.clone())
            .or_else(|| row.model.clone())
            .filter(|s| !s.trim().is_empty())
    }
    fn request_title(&mut self, id: &str, text: String, manual: bool) -> io::Result<()> {
        self.unnamed(id).ok_or_else(|| {
            io::Error::other("This session already has a title or a pending title request")
        })?;
        let model = self.naming_model(id).ok_or_else(|| {
            io::Error::other("Open this session to verify its model before naming it")
        })?;
        let route = self.thread_route(Some(id));
        let alias = match &route.billing {
            Billing::Api(alias) if alias == &model => Some(alias.clone()),
            Billing::Api(_) => {
                return Err(io::Error::other(
                    "Session model differs from the authorized billing route",
                ));
            }
            Billing::Subscription => None,
        };
        let key = Key {
            project: self.profile.project.clone(),
            runtime: route.runtime,
            provider: self.profile.provider.clone(),
            alias,
            id: id.into(),
        };
        let service = self.naming.as_mut().unwrap();
        service.running.insert(id.into(), manual);
        let launch = service.launch.clone();
        let deliver = service.deliver.clone();
        // Multiple open windows can request the same title. Share the generation and wake each.
        type Jobs = HashMap<(std::path::PathBuf, Key), Vec<Deliver>>;
        static JOBS: OnceLock<Mutex<Jobs>> = OnceLock::new();
        let jobs = JOBS.get_or_init(Default::default);
        let job = (launch.directory.clone(), key.clone());
        let mut pending = jobs.lock().unwrap();
        if let Some(listeners) = pending.get_mut(&job) {
            listeners.push(deliver);
            return Ok(());
        }
        pending.insert(job.clone(), vec![deliver]);
        drop(pending);
        self.changed.mark(Changed::THREADS);
        thread::spawn(move || {
            let name = titles::generate(&launch, &route, &model, &text)
                .and_then(|name| {
                    Title {
                        key: key.clone(),
                        name,
                    }
                    .persist(&launch.directory)
                    .ok()
                })
                .map(|t| t.name);
            let listeners = jobs.lock().unwrap().remove(&job).unwrap_or_default();
            for deliver in listeners {
                deliver(LinkEvent::Title {
                    key: key.clone(),
                    name: name.clone(),
                });
            }
        });
        Ok(())
    }
    pub(super) fn generated_title(&mut self, key: Key, name: Option<String>) {
        if key.project != self.profile.project
            || key.runtime != self.profile.route.runtime
            || key.provider != self.profile.provider
        {
            return;
        }
        let id = key.id.clone();
        let manual = self.naming.as_mut().and_then(|s| s.running.remove(&id)) == Some(true);
        if let Some(name) = name {
            let index = self.catalog.titles.clone();
            index.insert(Title {
                key: key.clone(),
                name: name.clone(),
            });
            if let Some(row) = self.catalog.get_mut(&id) {
                // A stock list can omit the optional model for the live thread.
                if row.model.is_none()
                    && let Some(alias) = &key.alias
                {
                    row.model = Some(alias.clone());
                }
                index.decorate(row);
            }
            self.catalog_revision += 1;
        } else if manual {
            self.notice = Some("The session’s runtime could not generate a title.".into());
            self.changed.mark(Changed::NOTICE);
        }
        self.changed.mark(Changed::THREADS);
    }
}

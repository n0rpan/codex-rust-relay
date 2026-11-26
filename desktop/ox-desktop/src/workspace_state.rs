//! Desktop-owned recent projects. Only the OX helper knows where the legacy layout lives.
use ox_desk_core::runtime::Billing;
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct ProjectRoute {
    pub project: PathBuf,
    pub api_model: Option<String>,
    #[serde(default)]
    pub runtime: ox_desk_core::runtime::Runtime,
}
use ox_desk_core::store::Route;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct LastWorkspace {
    pub project: PathBuf,
    pub recent: Vec<ProjectRoute>,
}
fn path() -> Option<PathBuf> {
    ox_desk_core::desktop_behavior::directory().map(|p| p.join("workspace.json"))
}

pub fn read_preference<T: serde::de::DeserializeOwned>(name: &str) -> io::Result<T> {
    let path = path()
        .ok_or_else(|| io::Error::other("Desktop state directory unavailable"))?
        .with_file_name(name);
    serde_json::from_reader(fs::File::open(path)?).map_err(io::Error::from)
}
pub(crate) use ox_desk_core::state_file::{update_preference_at, write_json};

enum WriteTask {
    Layout(String, crate::views::host::layout::Layout),
    Save(Option<String>, PathBuf, Route),
    Host(Option<String>),
    Flush(flume::Sender<()>),
}
fn writer() -> &'static std::sync::mpsc::Sender<WriteTask> {
    static WRITER: OnceLock<std::sync::mpsc::Sender<WriteTask>> = OnceLock::new();
    WRITER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for task in rx {
                match task {
                    WriteTask::Layout(name, layout) => {
                        if let Some(path) = path()
                            && let Err(e) = crate::views::host::layout::SavedLayouts::save_at(
                                &path.with_file_name(name),
                                layout,
                            )
                        {
                            eprintln!("Could not save desktop layout: {e}")
                        }
                    }
                    WriteTask::Host(id) => {
                        if let Some(path) = path()
                            && let Err(e) =
                                write_json(&path.with_file_name("selected-host.json"), &id)
                        {
                            eprintln!("Could not remember selected host: {e}");
                        }
                    }
                    WriteTask::Flush(done) => {
                        let _ = done.try_send(());
                    }
                    WriteTask::Save(name, project, route) => {
                        if let Some(path) = path()
                            && let Err(err) = LastWorkspace::save_at(
                                &name
                                    .as_ref()
                                    .map_or(path.clone(), |name| path.with_file_name(name)),
                                &project,
                                &route,
                                || {
                                    if name.is_some() {
                                        LastWorkspace::default()
                                    } else {
                                        LastWorkspace::previous()
                                    }
                                },
                            )
                        {
                            eprintln!("Could not remember workspace selection: {err}");
                        }
                    }
                }
            }
        });
        tx
    })
}
pub fn remember_host(id: Option<String>) {
    let _ = writer().send(WriteTask::Host(id));
}

pub fn remember_layout(name: String, layout: crate::views::host::layout::Layout) {
    let _ = writer().send(WriteTask::Layout(name, layout));
}
pub fn finish_writes(cx: &gpui::App) {
    cx.on_app_quit(|_| {
        let (done, rx) = flume::bounded(1);
        let _ = writer().send(WriteTask::Flush(done));
        async move {
            let _ = rx.recv_async().await;
        }
    })
    .detach();
}

impl LastWorkspace {
    fn read(path: &Path) -> io::Result<Self> {
        serde_json::from_reader(fs::File::open(path)?).map_err(io::Error::from)
    }
    fn previous() -> Self {
        static PREVIOUS: OnceLock<LastWorkspace> = OnceLock::new();
        PREVIOUS
            .get_or_init(|| {
                crate::settings::previous_workspace()
                    .ok()
                    .flatten()
                    .filter(|r| r.project.is_absolute())
                    .map(|r| Self {
                        project: r.project.clone(),
                        recent: vec![ProjectRoute {
                            project: r.project,
                            api_model: r.api_model,
                            ..Default::default()
                        }],
                    })
                    .unwrap_or_default()
            })
            .clone()
    }
    pub fn load() -> Self {
        path()
            .and_then(|p| Self::read(&p).ok())
            .unwrap_or_else(Self::previous)
    }
    /// The route saved for this project, if the user has ever opened it.
    pub fn saved_route(&self, project: &Path) -> Option<Route> {
        self.recent
            .iter()
            .find(|r| r.project == project)
            .map(|r| r.runtime.route(r.api_model.clone()))
    }
    pub fn route(&self, project: &Path) -> Route {
        self.recent
            .iter()
            .find(|r| r.project == project)
            .map(|r| r.runtime.route(r.api_model.clone()))
            .unwrap_or(Route::CODEX_SUBSCRIPTION)
    }
    pub fn remember(&mut self, project: &Path, route: &Route) {
        self.project = project.into();
        self.recent.retain(|r| r.project != project);
        self.recent.insert(
            0,
            ProjectRoute {
                runtime: route.runtime,
                project: project.into(),
                api_model: match &route.billing {
                    Billing::Subscription => None,
                    Billing::Api(m) => Some(m.clone()),
                },
            },
        );
    }
    pub fn save(name: Option<String>, project: &Path, route: &Route) -> io::Result<()> {
        writer()
            .send(WriteTask::Save(name, project.into(), route.clone()))
            .map_err(|_| io::Error::other("Workspace writer closed"))
    }
    fn save_at(
        path: &Path,
        project: &Path,
        route: &Route,
        initial: impl FnOnce() -> Self,
    ) -> io::Result<()> {
        update_preference_at(path, initial, |mut state| {
            state.remember(project, route);
            state
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_saves_keep_each_project_and_the_migrated_route() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("workspace.json");
        std::thread::scope(|scope| {
            for i in 0..5 {
                let path = &path;
                scope.spawn(move || {
                    LastWorkspace::save_at(
                        path,
                        Path::new(&format!("/p{i}")),
                        &Route::codex_api(format!("models/{i}")),
                        || LastWorkspace {
                            project: "/prior".into(),
                            recent: vec![ProjectRoute {
                                runtime: Default::default(),
                                project: "/prior".into(),
                                api_model: Some("old/model".into()),
                            }],
                        },
                    )
                    .unwrap()
                });
            }
        });
        let state = LastWorkspace::read(&path).unwrap();
        assert_eq!(state.recent.len(), 6);
        assert_eq!(
            state.route(Path::new("/prior")),
            Route::codex_api("old/model".into())
        );
        assert_eq!(
            state.route(Path::new("/p3")),
            Route::codex_api("models/3".into())
        );
    }
}

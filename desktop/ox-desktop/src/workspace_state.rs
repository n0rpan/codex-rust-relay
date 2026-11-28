//! Desktop-owned recent projects. Only the OX helper knows where the legacy layout lives.
use ox_desk_core::host_protocol::{Scope, SessionResult};
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
    Checkout(String, Checkout),
    Sidebar(String, crate::sidebar_state::SidebarState),
    SeenResult(String, Scope, String, SessionResult),
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
                    WriteTask::Checkout(repo, checkout) => {
                        if let Some(path) = path()
                            && let Err(e) = update_preference_at(
                                &path.with_file_name("project-hosts.json"),
                                ProjectHosts::new,
                                |mut hosts| {
                                    hosts.insert(repo, checkout);
                                    hosts
                                },
                            )
                        {
                            eprintln!("Could not remember project host: {e}");
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
                    WriteTask::Sidebar(name, state) => {
                        if let Some(path) = path()
                            && let Err(e) = crate::sidebar_state::SidebarState::save_at(
                                &path.with_file_name(name),
                                state,
                            )
                        {
                            eprintln!("Could not save sidebar metadata: {e}")
                        }
                    }
                    WriteTask::SeenResult(name, scope, thread, result) => {
                        if let Some(path) = path()
                            && let Err(e) = update_preference_at(
                                &path.with_file_name(name),
                                SeenResults::default,
                                |mut results| {
                                    results.record(&scope, &thread, &result);
                                    results
                                },
                            )
                        {
                            eprintln!("Could not remember viewed session result: {e}");
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
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct Checkout {
    pub host: Option<String>,
    pub path: PathBuf,
}
pub(crate) type ProjectHosts = std::collections::BTreeMap<String, Checkout>;
pub(crate) fn remember_checkout(repo: String, checkout: Checkout) {
    let _ = writer().send(WriteTask::Checkout(repo, checkout));
}
pub fn remember_host(id: Option<String>) {
    let _ = writer().send(WriteTask::Host(id));
}

pub fn remember_layout(name: String, layout: crate::views::host::layout::Layout) {
    let _ = writer().send(WriteTask::Layout(name, layout));
}
pub fn remember_sidebar(name: String, state: crate::sidebar_state::SidebarState) {
    let _ = writer().send(WriteTask::Sidebar(name, state));
}

/// The most recent result viewed in each thread on this host and desktop.
#[derive(Default, Deserialize, Serialize)]
pub(crate) struct SeenResults(std::collections::BTreeMap<String, SessionResult>);

impl SeenResults {
    pub(crate) const FILE_NAME: &'static str = "seen-results.json";

    fn key(scope: &Scope, thread: &str) -> String {
        // Route includes the full API alias, which can differ for identical native IDs.
        serde_json::to_string(&(scope, thread)).expect("session identity serializes")
    }

    pub(crate) fn contains(&self, scope: &Scope, thread: &str, result: &SessionResult) -> bool {
        self.0
            .get(&Self::key(scope, thread))
            .is_some_and(|seen| seen.turn == result.turn || seen.completed_at > result.completed_at)
    }

    pub(crate) fn record(&mut self, scope: &Scope, thread: &str, result: &SessionResult) -> bool {
        if self.contains(scope, thread, result) {
            return false;
        }
        self.0.insert(Self::key(scope, thread), result.clone());
        true
    }
}

pub(crate) fn remember_seen_result(
    name: String,
    scope: Scope,
    thread: String,
    result: SessionResult,
) {
    let _ = writer().send(WriteTask::SeenResult(name, scope, thread, result));
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
    use ox_desk_core::host_protocol::SessionOutcome;

    #[test]
    fn viewed_results_survive_reload_and_stay_specific_to_the_turn_and_route() {
        let scope = Scope {
            project: "/work".into(),
            route: Route::codex_api("provider/one".into()),
        };
        let other = Scope {
            route: Route::codex_api("provider/two".into()),
            ..scope.clone()
        };
        let first = SessionResult {
            turn: "first".into(),
            completed_at: 100,
            outcome: SessionOutcome::Failed,
        };
        let next = SessionResult {
            turn: "next".into(),
            completed_at: 101,
            outcome: SessionOutcome::Completed,
        };
        let mut seen = SeenResults::default();
        assert!(seen.record(&scope, "session", &first));
        let mut reloaded: SeenResults =
            serde_json::from_value(serde_json::to_value(seen).unwrap()).unwrap();
        assert!(reloaded.contains(&scope, "session", &first));
        assert!(!reloaded.contains(&other, "session", &first));
        assert!(!reloaded.contains(&scope, "another", &first));
        assert!(!reloaded.contains(&scope, "session", &next));
        assert!(reloaded.record(&scope, "session", &next));
        assert!(!reloaded.record(&scope, "session", &first));
    }

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

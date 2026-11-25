use ox_desk_core::{
    claude::catalog::Profiles,
    host_protocol::Scope,
    links::{LinkEvent, codex},
    ports::ox,
    runtime::{Billing, Route, Runtime},
    settings_protocol::{ClaudeProfiles, Request},
    store::{
        Profile, Store,
        claude::{API_PROVIDER, Launcher, SUBSCRIPTION_PROVIDER},
    },
};
use std::{
    env,
    ffi::OsString,
    io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

type FileIdentity = (u64, u64, u64, i64, i64, i64, i64);
pub(crate) struct Prepared {
    alias: Vec<String>,
    provider: Option<String>,
    pub routes: Vec<Route>,
    pub yolo_grant_projects: Vec<PathBuf>,
    inputs: Option<[Option<FileIdentity>; 4]>,
    when: std::time::Instant,
}
fn route_inputs() -> Option<[Option<FileIdentity>; 4]> {
    let home = env::var_os("CODEX_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".codex")))?;
    let mut ids = [None; 4];
    for (i, name) in [
        "codeox.toml",
        "config.toml",
        "ox-api-keys.json",
        "ox-favorites.json",
    ]
    .into_iter()
    .enumerate()
    {
        match std::fs::metadata(home.join(name)) {
            Ok(m) => {
                ids[i] = Some((
                    m.dev(),
                    m.ino(),
                    m.len(),
                    m.mtime(),
                    m.mtime_nsec(),
                    m.ctime(),
                    m.ctime_nsec(),
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(_) => return None,
        }
    }
    Some(ids)
}
impl Prepared {
    fn current(&self) -> bool {
        self.when.elapsed() < std::time::Duration::from_secs(5)
            && self.inputs.is_some()
            && self.inputs == route_inputs()
    }
}
#[derive(Clone)]
pub(crate) struct RuntimeConfig {
    pub ox: PathBuf,
    pub path: OsString,
    pub(crate) desktop: PathBuf,
}
impl RuntimeConfig {
    pub(crate) fn discover() -> io::Result<Self> {
        let mut paths: Vec<_> = env::var_os("PATH")
            .as_deref()
            .map(env::split_paths)
            .into_iter()
            .flatten()
            .filter(|p| p.is_absolute())
            .collect();
        let home = env::var_os("HOME").map(PathBuf::from);
        if let Some(home) = &home {
            paths.extend(
                [
                    ".local/bin",
                    ".cargo/bin",
                    ".local/share/mise/shims",
                    ".local/share/mise/command-wrappers/bin",
                    ".npm-global/bin",
                    ".volta/bin",
                ]
                .map(|p| home.join(p)),
            );
        }
        paths
            .extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from));
        let sibling = env::current_exe()?.with_file_name("ox");
        let ox = std::iter::once(sibling)
            .chain(paths.iter().map(|p| p.join("ox")))
            .find(|p| {
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Install the matching OX helper next to ox-host",
                )
            })?;
        let base = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                home.map(|p| {
                    p.join(if cfg!(target_os = "macos") {
                        "Library/Application Support"
                    } else {
                        ".local/state"
                    })
                })
            })
            .ok_or_else(|| io::Error::other("No OX state directory"))?;
        Ok(Self {
            ox,
            path: env::join_paths(paths).map_err(io::Error::other)?,
            desktop: base.join("ox/desktop"),
        })
    }
    pub(crate) fn projects(&self) -> Vec<PathBuf> {
        let Ok(f) = std::fs::File::open(self.desktop.join("workspace.json")) else {
            return vec![];
        };
        let Ok(v) = serde_json::from_reader::<_, serde_json::Value>(f) else {
            return vec![];
        };
        std::iter::once(&v)
            .chain(v["recent"].as_array().into_iter().flatten())
            .filter_map(|r| r["project"].as_str())
            .map(PathBuf::from)
            .filter_map(|p| p.canonicalize().ok())
            .filter(|p| p.is_dir())
            .collect()
    }
    pub(crate) fn prepare(&self) -> io::Result<Prepared> {
        let before = route_inputs();
        let s = ox::snapshot(&self.ox, &self.path).map_err(io::Error::other)?;
        let after = route_inputs();
        let routes = Runtime::routes(&s.models)
            .filter(|r| {
                r.is_api()
                    || match r.runtime {
                        Runtime::Codex => s.chatgpt_account.is_some(),
                        Runtime::Claude => s.claude_account,
                    }
            })
            .collect();
        Ok(Prepared {
            alias: s.models,
            provider: s.api_provider,
            routes,
            yolo_grant_projects: s.yolo_grant_projects,
            inputs: if before == after { after } else { None },
            when: std::time::Instant::now(),
        })
    }
    pub(crate) fn open(
        &self,
        scope: &Scope,
        prepared: Option<Prepared>,
        receive: impl Fn(LinkEvent) + Send + Sync + 'static,
    ) -> io::Result<Store> {
        let settings = if scope.route.is_api() || scope.route.runtime == Runtime::Claude {
            Some(match prepared.filter(|s| s.current()) {
                Some(s) => s,
                None => self.prepare()?,
            })
        } else {
            None
        };
        if scope.route.is_api()
            && !settings
                .as_ref()
                .is_some_and(|s| s.routes.contains(&scope.route))
        {
            return Err(io::Error::other(
                "The selected API billing route is no longer configured.",
            ));
        }
        let receive = Arc::new(receive);
        let deliver = receive.clone();
        let project = scope.project.clone();
        let route = scope.route.clone();
        let mut store = match route.runtime {
            Runtime::Codex => {
                let provider = match &route.billing {
                    Billing::Subscription => "openai".to_owned(),
                    Billing::Api(_) => settings.and_then(|s| s.provider).ok_or_else(|| {
                        io::Error::other("Configure an OX API provider in Settings first.")
                    })?,
                };
                let mut args = ox_desk_core::codex::launch_args(&route, &provider);
                args.extend(
                    ["--no-daemon", "app-server", "--listen", "stdio://"].map(String::from),
                );
                let sub = route == Route::CODEX_SUBSCRIPTION;
                let saw = AtomicBool::new(false);
                let config = self.clone();
                let mut command = std::process::Command::new(&self.ox);
                command
                    .args(args)
                    .current_dir(&project)
                    .env("PATH", &self.path)
                    .env("OX_HOST_WORKER_ROUTE", serde_json::to_string(&route)?)
                    .env_remove("OX_HOST_WORKER_PARENT");
                let link = codex::Link::from_command(command, move |e| {
                    match &e {
                        LinkEvent::Frame(_) => saw.store(true, Ordering::Release),
                        LinkEvent::Closed { .. }
                            if sub
                                && !saw.load(Ordering::Acquire)
                                && ox::snapshot(&config.ox, &config.path)
                                    .is_ok_and(|s| s.chatgpt_account.is_none()) =>
                        {
                            deliver(LinkEvent::SignInRequired)
                        }
                        _ => (),
                    }
                    deliver(e);
                })?;
                Store::open(
                    Profile {
                        project,
                        route,
                        provider,
                    },
                    link,
                )
            }
            Runtime::Claude => {
                let paths: ClaudeProfiles =
                    ox::request(&self.ox, &self.path, Request::ClaudeProfiles)
                        .map_err(io::Error::other)?;
                let aliases = settings.expect("Claude preflight").alias;
                let provider = if route.is_api() {
                    API_PROVIDER
                } else {
                    SUBSCRIPTION_PROVIDER
                }
                .into();
                Store::open_claude(
                    Profile {
                        project,
                        route,
                        provider,
                    },
                    Launcher {
                        ox: self.ox.clone(),
                        path: self.path.clone(),
                        profiles: Profiles {
                            subscription: paths.subscription,
                            api: paths.api,
                        },
                        index: self.desktop.join("claude-routes.json"),
                        aliases,
                    },
                    move |e| deliver(e),
                )
            }
        }?;
        store.enable_naming(
            ox_desk_core::titles::Launcher {
                ox: self.ox.clone(),
                path: self.path.clone(),
                directory: self.desktop.join("titles"),
            },
            move |e| receive(e),
        );
        Ok(store)
    }
}
pub(crate) fn canonical_scope(scope: Scope) -> io::Result<Scope> {
    if !scope.project.is_absolute() {
        return Err(io::Error::other("Open an absolute project directory"));
    }
    let project = scope.project.canonicalize()?;
    if !project.is_dir() {
        return Err(io::Error::other("Select a project directory"));
    }
    Ok(Scope { project, ..scope })
}

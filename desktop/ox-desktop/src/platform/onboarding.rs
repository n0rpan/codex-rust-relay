//! Check the official CLI used by OX and start repairs only on an explicit click.
use super::*;
use ox_desk_core::store::Route;
pub fn missing(runtime: ox_desk_core::runtime::Runtime) -> bool {
    find(runtime.executable(), &child_path()).is_none()
}
pub fn installer(
    runtime: ox_desk_core::runtime::Runtime,
    update: bool,
) -> Option<(String, Vec<OsString>)> {
    if runtime == ox_desk_core::runtime::Runtime::Claude {
        return None;
    }
    let path = child_path();
    let installed = find("codex", &path).and_then(|p| p.canonicalize().ok());
    let managed_by = |name: &str| {
        installed
            .as_ref()
            .is_some_and(|p| p.components().any(|c| c.as_os_str() == name))
    };
    if update
        && managed_by("mise")
        && let Some(mise) = find("mise", &path)
    {
        return Some((
            "Update with mise".into(),
            vec![
                mise.into(),
                "use".into(),
                "--global".into(),
                "codex@latest".into(),
            ],
        ));
    }
    #[cfg(target_os = "macos")]
    if (!update || managed_by("Caskroom"))
        && let Some(brew) = find("brew", &path)
    {
        let action = if update { "upgrade" } else { "install" };
        return Some((
            format!(
                "{} with Homebrew",
                if update { "Update" } else { "Install" }
            ),
            vec![brew.into(), action.into(), "--cask".into(), "codex".into()],
        ));
    }
    if update && !managed_by("node_modules") {
        return None;
    }
    find("npm", &path).map(|npm| {
        (
            format!("{} with npm", if update { "Update" } else { "Install" }),
            vec![
                npm.into(),
                "install".into(),
                "-g".into(),
                "@openai/codex@latest".into(),
            ],
        )
    })
}
pub fn repair(project: &Path, args: &[OsString]) -> Result<(), String> {
    let path = child_path();
    let mut assignment = OsString::from("PATH=");
    assignment.push(&path);
    let mut command = vec!["/usr/bin/env".into(), assignment];
    command.extend_from_slice(args);
    open_terminal(project, &command, &path)
}

/// What the first-launch probe learned about one official CLI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    pub installed: bool,
    pub signed_in: bool,
}
impl Probe {
    pub fn ready(self) -> bool {
        self.installed && self.signed_in
    }
}
/// Whether the CLI is installed and has local sign-in state. Only file presence is checked;
/// no credential is read or sent anywhere.
pub fn probe(runtime: ox_desk_core::runtime::Runtime) -> Probe {
    Probe {
        installed: !missing(runtime),
        signed_in: signed_in(runtime),
    }
}
/// A CLI that can start a session now: installed, and signed in unless the route uses an API key.
pub fn ready(runtime: ox_desk_core::runtime::Runtime, api: bool) -> bool {
    !missing(runtime) && (api || signed_in(runtime))
}
fn signed_in(runtime: ox_desk_core::runtime::Runtime) -> bool {
    use ox_desk_core::runtime::Runtime;
    let home = env::var_os("HOME").map(PathBuf::from);
    match runtime {
        Runtime::Codex => env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".codex")))
            .is_some_and(|d| d.join("auth.json").is_file()),
        Runtime::Claude => {
            let config = env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
            let credentials = config
                .clone()
                .or_else(|| home.clone().map(|h| h.join(".claude")))
                .map(|d| d.join(".credentials.json"));
            let account = config
                .map(|d| d.join(".claude.json"))
                .or_else(|| home.map(|h| h.join(".claude.json")));
            credentials.is_some_and(|p| p.is_file())
                || account.is_some_and(|p| {
                    std::fs::read_to_string(p).is_ok_and(|t| t.contains("\"oauthAccount\""))
                })
        }
    }
}
/// A fresh install uses Codex until the operator selects another runtime.
pub fn initial_route(codex: Probe, first_api_model: impl FnOnce() -> Option<String>) -> Route {
    if !codex.ready()
        && let Some(alias) = first_api_model()
    {
        return Route::codex_api(alias);
    }
    Route::CODEX_SUBSCRIPTION
}
/// Whether `project` is the user's home directory.
pub fn is_home(project: &Path) -> bool {
    env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|h| h.canonicalize().ok())
        .is_some_and(|h| project.canonicalize().is_ok_and(|p| p == h))
}
#[cfg(test)]
mod tests {
    use super::*;
    const READY: Probe = Probe {
        installed: true,
        signed_in: true,
    };
    const OUT: Probe = Probe {
        installed: true,
        signed_in: false,
    };
    const NONE: Probe = Probe {
        installed: false,
        signed_in: false,
    };
    #[test]
    fn first_launch_keeps_codex_even_if_its_cli_is_missing() {
        assert_eq!(initial_route(NONE, || None), Route::CODEX_SUBSCRIPTION);
        assert_eq!(
            initial_route(READY, || Some("alias".into())),
            Route::CODEX_SUBSCRIPTION
        );
        assert_eq!(
            initial_route(OUT, || Some("alias".into())),
            Route::codex_api("alias".into())
        );
    }
}

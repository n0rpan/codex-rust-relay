//! Check the stock executable used by OX and start repairs only on an explicit click.
use super::*;
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

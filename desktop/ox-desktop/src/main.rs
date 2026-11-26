mod activity;
mod app;
mod appearance;
mod keymap;
mod kit;
mod platform;
mod preferences;
mod settings;
mod sky;
mod terminal;
mod updates;
#[path = "../../../src/version.rs"]
mod version;
mod views;
mod workspace_state;

use gpui::*;
use gpui_component::Root;
use kit::{col, row, small};
use std::path::PathBuf;
use workspace_state::LastWorkspace;

fn main() {
    platform::init_companion();
    let _package = ox_host_service::managed_install::hold_generation();
    ox_host_service::configure_allocator();
    app::install_panic_hook();
    let started = std::time::Instant::now();
    let mut args = std::env::args_os().skip(1).peekable();
    let mut client_only = false;
    let mut smoke = false;
    let mut stats = false;
    while let Some(arg) = args.peek() {
        if arg == "--desk-smoke" || arg == "--desk-exit-after-first-frame" {
            smoke = true;
        } else if arg == "--client-only" {
            client_only = true;
        } else if arg == "--desk-stats" {
            stats = true;
        } else if arg == "--version" || arg == "-V" {
            println!("ox-desktop {}", version::VERSION);
            return;
        } else if arg == "--desk-next" {
            // Accepted for scripts written while the workbench was opt-in.
        } else {
            break;
        }
        args.next();
    }
    let explicit = args.next().map(PathBuf::from);
    let restore_host = explicit.is_none() && !smoke && !stats;
    let project = match if client_only {
        Ok(PathBuf::new())
    } else {
        startup_project(explicit)
    } {
        Ok(project) => project,
        Err(message) => {
            eprintln!("ox-desktop: {message}");
            std::process::exit(2);
        }
    };
    gpui_platform::application()
        .with_assets(gpui_kit_assets::Assets)
        .run(move |cx| {
            #[cfg(target_os = "macos")]
            platform::set_application_icon();
            gpui_component::init(cx);
            appearance::init(cx);
            sky::start(cx);
            cx.set_global(app::ClientOnly(client_only));
            cx.set_global(preferences::HostDirectory(
                platform::hosts::list().unwrap_or_default(),
            ));
            updates::start(smoke, cx);
            workspace_state::finish_writes(cx);
            let layouts = views::host::layout::SavedLayouts::load();
            let bounds = Bounds::centered(None, layouts.for_project(&project).window_size(), cx);
            // Let AppKit finish its launch notification before creating the first NSWindow.
            cx.spawn(async move |cx| -> gpui::Result<()> {
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        titlebar: Some(TitlebarOptions {
                            title: Some("OX Desktop".into()),
                            ..Default::default()
                        }),
                        window_min_size: Some(size(px(360.), px(480.))),
                        app_id: Some("dev.ox.desktop".into()),
                        ..Default::default()
                    },
                    |window, cx| {
                        appearance::observe_window(window);
                        let view = (!client_only).then(|| {
                            cx.new(|cx| {
                                views::host::Host::new(
                                    platform::HostLink::Local,
                                    None,
                                    project,
                                    layouts,
                                    smoke.then_some(started),
                                    stats,
                                    window,
                                    cx,
                                )
                            })
                        });
                        let workspace =
                            cx.new(|cx| app::Workspace::new(view, restore_host, window, cx));
                        cx.new(|cx| Root::new(workspace, window, cx))
                    },
                )?;
                cx.update(|cx| cx.activate(true));
                Ok(())
            })
            .detach_and_log_err(cx);
            cx.on_window_closed(|cx, window| {
                platform::layout_probe::forget(window);
                if cx.windows().is_empty() {
                    cx.quit()
                }
            })
            .detach();
        });
}

fn startup_project(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return path
            .canonicalize()
            .map_err(|err| format!("Cannot open project {}: {err}", path.display()))
            .and_then(|project| {
                project
                    .is_dir()
                    .then_some(project)
                    .ok_or_else(|| format!("Project {} is not a directory", path.display()))
            });
    }
    let saved = LastWorkspace::load().project;
    [
        Some(saved),
        std::env::current_dir()
            .ok()
            .filter(|path| path.parent().is_some()),
        std::env::var_os("HOME").map(PathBuf::from),
    ]
    .into_iter()
    .flatten()
    .find_map(|path| path.canonicalize().ok().filter(|p| p.is_dir()))
    .ok_or_else(|| "No project directory is available. Pass an existing directory.".into())
}

#[cfg(test)]
mod startup_tests {
    use super::startup_project;

    #[test]
    fn explicit_project_must_be_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            startup_project(Some(dir.path().join("."))).unwrap(),
            dir.path().canonicalize().unwrap()
        );
        let missing = dir.path().join("missing");
        assert!(
            startup_project(Some(missing.clone()))
                .unwrap_err()
                .contains(&missing.display().to_string())
        );
        let file = dir.path().join("regular-file");
        std::fs::write(&file, "").unwrap();
        assert!(
            startup_project(Some(file))
                .unwrap_err()
                .contains("directory")
        );
    }
}

//! Watch parent directories so atomic replacements and Omarchy theme symlinks survive.
use notify::{EventKind, RecursiveMode, Watcher};
use std::{collections::HashSet, env, path::PathBuf};

#[derive(Clone)]
pub struct ThemeFiles {
    pub colors: [PathBuf; 2],
    pub reduced_motion: PathBuf,
    roots: [PathBuf; 2],
}
impl ThemeFiles {
    pub fn discover() -> Option<Self> {
        let home = PathBuf::from(env::var_os("HOME")?);
        let xdg = |key, fallback| {
            env::var_os(key)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
        };
        Some(Self::new(
            xdg("XDG_STATE_HOME", ".local/state"),
            xdg("XDG_CONFIG_HOME", ".config"),
        ))
    }
    pub fn new(state: PathBuf, config: PathBuf) -> Self {
        let roots = [state, config];
        Self {
            colors: roots
                .clone()
                .map(|dir| dir.join("omarchy/current/theme/colors.toml")),
            reduced_motion: roots[0].join("omarchy/toggles/hypr/no-animations.lua"),
            roots,
        }
    }
    pub fn watch(
        &self,
        wake: impl Fn() + Send + 'static,
    ) -> notify::Result<notify::RecommendedWatcher> {
        let mut dirs = HashSet::new();
        let mut targets = HashSet::new();
        for (path, root) in [
            (&self.colors[0], &self.roots[0]),
            (&self.colors[1], &self.roots[1]),
            (&self.reduced_motion, &self.roots[0]),
        ] {
            targets.insert(path.clone());
            // Watch the nearest existing ancestor even on a fresh installation.
            if let Some(parent) = root.ancestors().find(|p| p.is_dir()) {
                dirs.insert(parent.to_owned());
            }
            for parent in path.ancestors().skip(1).take_while(|p| p.starts_with(root)) {
                if let Ok(real) = parent.canonicalize() {
                    if let Ok(suffix) = path.strip_prefix(parent) {
                        targets.insert(real.join(suffix));
                    }
                    dirs.insert(real);
                }
            }
            if let Ok(real) = path.canonicalize() {
                if let Some(parent) = real.parent() {
                    dirs.insert(parent.into());
                }
                targets.insert(real);
            }
        }
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                if let Ok(event) = result
                    && !matches!(event.kind, EventKind::Access(_))
                    && (event.need_rescan()
                        || event.paths.iter().any(|changed| {
                            targets.iter().any(|target| target.starts_with(changed))
                        }))
                {
                    wake();
                }
            })?;
        for dir in dirs {
            watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        }
        Ok(watcher)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink, sync::mpsc, time::Duration};
    #[test]
    fn wakes_on_palette_edits_and_atomic_theme_switches() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let current = state.join("omarchy/current");
        fs::create_dir_all(&current).unwrap();
        let theme = root.path().join("tokyo");
        fs::create_dir(&theme).unwrap();
        fs::write(theme.join("colors.toml"), "dark").unwrap();
        symlink(&theme, current.join("theme")).unwrap();
        let paths = ThemeFiles::new(state, root.path().join("config"));
        let (tx, rx) = mpsc::channel();
        let watcher = paths
            .watch(move || {
                let _ = tx.send(());
            })
            .unwrap();
        fs::write(theme.join("colors.toml"), "light").unwrap();
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(watcher);
        let (tx, rx) = mpsc::channel();
        let _watcher = paths
            .watch(move || {
                let _ = tx.send(());
            })
            .unwrap();
        symlink(root.path().join("future"), current.join("next")).unwrap();
        fs::rename(current.join("next"), current.join("theme")).unwrap();
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
    }
}

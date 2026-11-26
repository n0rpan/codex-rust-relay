//! Operator controls shared by local administration and signed remote settings.
use serde::{Deserialize, Serialize};
#[path = "../../../src/update_preferences.rs"]
pub mod preferences;
pub use preferences::{Channel, Mode, Preferences};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Status,
    SetMode { mode: Mode },
    SetChannel { channel: Channel },
    UpdateWhenIdle,
    Cancel,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub installed: String,
    pub managed: bool,
    pub mode: Mode,
    pub channel: Channel,
    pub scheduled: bool,
    pub message: String,
}
// Terminal sessions live in the web service, outside the agent engine's turn count.
// The shared lease serializes starting a PTY with reserving an idle host for update.
#[cfg(feature = "server")]
pub static TERMINALS: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

#[cfg(feature = "server")]
pub use controller::*;
#[cfg(feature = "server")]
mod controller {
    use super::*;
    use std::{
        fs::File,
        io,
        path::{Path, PathBuf},
        sync::{Condvar, Mutex, OnceLock},
        time::Duration,
    };
    struct State {
        prefs: Preferences,
        message: String,
        revision: u64,
    }
    pub struct Controller {
        path: PathBuf,
        installed: String,
        managed: bool,
        state: Mutex<State>,
        wake: Condvar,
    }
    pub static UPDATES: OnceLock<Controller> = OnceLock::new();
    impl Controller {
        pub fn open(dir: &Path, store: Option<&Path>, version: &str) -> io::Result<Self> {
            let path = store.unwrap_or(dir).join("update-preferences.json");
            let mut prefs = Preferences {
                mode: if store.is_some_and(|s| s.join("headless").is_file()) {
                    Mode::Automatic
                } else {
                    Mode::Download
                },
                channel: preferences::installed_channel(version),
                scheduled: false,
                generation: store.and_then(preferences::generation),
            };
            let mut message = if store.is_some() {
                "Ready"
            } else {
                "Install OX with install.sh --host or --desktop to manage updates."
            };
            if let Some(store) = store {
                match preferences::read(store) {
                    Ok(Some(saved)) => {
                        prefs.mode = saved.mode;
                        prefs.channel = preferences::effective_channel(&saved, store, version);
                        prefs.scheduled = saved.scheduled && prefs.generation == saved.generation;
                    }
                    Ok(None) => (),
                    Err(_) => {
                        prefs.mode = Mode::Manual;
                        message = "Could not read update settings. Choose an update mode to restore them.";
                    }
                }
            }
            Ok(Self {
                path,
                managed: store.is_some(),
                installed: version.to_owned(),
                state: Mutex::new(State {
                    prefs,
                    revision: 0,
                    message: message.into(),
                }),
                wake: Condvar::new(),
            })
        }
        fn save(&self, prefs: &Preferences) -> io::Result<()> {
            let mut file = tempfile::NamedTempFile::new_in(self.path.parent().unwrap())?;
            serde_json::to_writer(&mut file, prefs)?;
            file.as_file().sync_all()?;
            file.persist(&self.path)?;
            File::open(self.path.parent().unwrap())?.sync_all()?;
            Ok(())
        }
        pub fn command(&self, op: Command) -> io::Result<Status> {
            let mut s = self.state.lock().unwrap();
            if !matches!(op, Command::Status) {
                if !self.managed {
                    return Err(io::Error::other(s.message.clone()));
                }
                let mut next = s.prefs.clone();
                match op {
                    Command::SetMode { mode } => {
                        next.mode = mode;
                        next.scheduled = false;
                    }
                    Command::SetChannel { channel } => next.channel = channel,
                    Command::UpdateWhenIdle => next.scheduled = true,
                    Command::Cancel => next.scheduled = false,
                    Command::Status => unreachable!(),
                }
                self.save(&next)?;
                s.prefs = next.clone();
                s.revision += 1;
                s.message = if next.scheduled {
                    "Update scheduled for the next idle point"
                } else {
                    "Preferences saved"
                }
                .into();
                self.wake.notify_all();
            }
            Ok(Status {
                installed: self.installed.clone(),
                managed: self.managed,
                mode: s.prefs.mode,
                channel: s.prefs.channel,
                scheduled: s.prefs.scheduled,
                message: s.message.clone(),
            })
        }
        pub fn current(&self) -> (u64, Preferences) {
            let s = self.state.lock().unwrap();
            (s.revision, s.prefs.clone())
        }
        pub fn wait(&self, rev: u64, delay: Duration) {
            drop(
                self.wake
                    .wait_timeout_while(self.state.lock().unwrap(), delay, |s| s.revision == rev)
                    .unwrap(),
            );
        }
        pub fn message(&self, rev: u64, message: String) {
            let mut s = self.state.lock().unwrap();
            if s.revision == rev {
                s.message = message;
            }
        }
        /// Serialize the final package switch with an operator's channel or mode change.
        pub fn finish<T>(
            &self,
            rev: u64,
            message: &str,
            apply: impl FnOnce() -> io::Result<T>,
        ) -> io::Result<Option<T>> {
            let mut s = self.state.lock().unwrap();
            if s.revision != rev {
                return Ok(None);
            }
            let result = apply()?;
            if s.prefs.scheduled {
                let next = Preferences {
                    scheduled: false,
                    ..s.prefs.clone()
                };
                self.save(&next)?;
                s.prefs = next.clone();
            }
            s.message = message.into();
            Ok(Some(result))
        }
    }
    pub fn control(command: Command) -> io::Result<Status> {
        UPDATES
            .get()
            .ok_or_else(|| io::Error::other("Host update service is starting"))?
            .command(command)
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    #[test]
    fn headless_defaults_to_auto_and_remembers_the_operators_channel_and_mode() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        std::fs::write(dir.join("headless"), "").unwrap();
        let open = || Controller::open(dir, Some(dir), "1.0.0-dev.1").unwrap();
        let controller = open();
        let status = controller.command(Command::Status).unwrap();
        assert_eq!(
            (status.mode, status.channel),
            (Mode::Automatic, Channel::Dev)
        );
        controller
            .command(Command::SetMode { mode: Mode::Manual })
            .unwrap();
        controller
            .command(Command::SetChannel {
                channel: Channel::Stable,
            })
            .unwrap();
        controller.command(Command::UpdateWhenIdle).unwrap();
        let restarted = open();
        let status = restarted.command(Command::Status).unwrap();
        assert!(status.scheduled);
        assert_eq!(
            (status.mode, status.channel),
            (Mode::Manual, Channel::Stable)
        );
        let (rev, _) = restarted.current();
        restarted.command(Command::Cancel).unwrap();
        assert!(
            restarted
                .finish(rev, "Installed", || -> std::io::Result<()> {
                    panic!("cancelled update ran")
                })
                .unwrap()
                .is_none()
        );
        assert!(!open().current().1.scheduled);
    }
}

#[cfg(all(test, feature = "server"))]
mod recovery_tests {
    use super::*;
    #[test]
    fn broken_preferences_preserve_the_host_and_can_be_repaired_in_settings() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        std::fs::write(dir.join("update-preferences.json"), "broken").unwrap();
        let c = Controller::open(dir, Some(dir), "1.0.0").unwrap();
        let initial = c.command(Command::Status).unwrap();
        assert_eq!(initial.mode, Mode::Manual);
        assert!(initial.message.contains("restore"));
        c.command(Command::SetMode {
            mode: Mode::Automatic,
        })
        .unwrap();
        assert_eq!(
            Controller::open(dir, Some(dir), "1.0.0")
                .unwrap()
                .current()
                .1
                .mode,
            Mode::Automatic
        );
    }

    #[test]
    fn a_new_installer_generation_supersedes_an_earlier_channel_choice() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        for name in ["one", "two"] {
            std::fs::create_dir(dir.join(name)).unwrap();
        }
        std::os::unix::fs::symlink("one", dir.join("current")).unwrap();
        let c = Controller::open(dir, Some(dir), "1.0.0").unwrap();
        c.command(Command::SetChannel {
            channel: Channel::Stable,
        })
        .unwrap();
        std::fs::remove_file(dir.join("current")).unwrap();
        std::os::unix::fs::symlink("two", dir.join("current")).unwrap();
        assert_eq!(
            Controller::open(dir, Some(dir), "1.1.0-dev.1")
                .unwrap()
                .current()
                .1
                .channel,
            Channel::Dev
        );
    }
}

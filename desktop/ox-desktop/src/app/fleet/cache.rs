//! Desktop-owned last-seen navigation metadata for unavailable paired machines.
use super::Inventory;
use crate::platform::HostLink;
use gpui::{AppContext, Context};
use ox_desk_core::host_protocol::{HostInfo, ProjectDirectory, Scope};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

type Row = (Scope, Option<String>, String);
#[derive(Default, Serialize, Deserialize)]
pub(super) struct Snapshot {
    origin: String,
    pairing: String,
    seen: i64,
    info: Option<HostInfo>,
    directory: Option<ProjectDirectory>,
    rows: Vec<Row>,
}

impl Snapshot {
    fn path(link: &HostLink) -> Option<PathBuf> {
        let host = link.remote()?;
        // Pairing IDs are public hex values. Keep untrusted directory data out of filenames.
        if host.pairing_id().len() != 32
            || !host.pairing_id().bytes().all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        Some(
            ox_desk_core::desktop_behavior::directory()?
                .join("host-catalog")
                .join(link.state_file(&format!("{}-catalog.json", host.pairing_id()))),
        )
    }
    pub(super) fn read(link: &HostLink) -> Option<Self> {
        let host = link.remote()?;
        let file = std::fs::File::open(Self::path(link)?).ok()?;
        let snapshot: Self = serde_json::from_reader(file).ok()?;
        (snapshot.origin == host.origin && snapshot.pairing == host.pairing_id())
            .then_some(snapshot)
    }
    pub(super) fn restore(self, inv: &mut Inventory) {
        inv.last_seen = chrono::DateTime::from_timestamp_millis(self.seen).map(Into::into);
        inv.directory = self.directory;
        inv.info = self.info;
        inv.previous = self.rows;
    }
}
impl Inventory {
    pub(super) fn add_last_seen(&self, rows: &mut Vec<Row>) {
        let seen: HashSet<_> = rows
            .iter()
            .map(|(scope, id, _)| (scope.clone(), id.clone()))
            .collect();
        rows.extend(
            self.previous
                .iter()
                .filter(|(scope, id, _)| {
                    !seen.contains(&(scope.clone(), id.clone()))
                        && !id.as_ref().is_some_and(|id| {
                            self.projects
                                .get(scope)
                                .is_some_and(|p| p.archived.contains(id))
                        })
                })
                .cloned(),
        );
    }
    pub(super) fn schedule_cache(&mut self, cx: &mut Context<Self>) {
        if self.cache_scheduled || self.link.remote().is_none() || !self.received_catalog {
            return;
        }
        self.cache_scheduled = true;
        let delay = self.last_write.map_or(Duration::from_millis(500), |t| {
            Duration::from_secs(30).saturating_sub(t.elapsed())
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |s, cx| {
                s.cache_scheduled = false;
                if s.online {
                    s.save_cache(cx);
                }
            });
        })
        .detach();
    }
    pub(super) fn save_cache(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.link.remote() else {
            return;
        };
        let Some(path) = Snapshot::path(&self.link) else {
            return;
        };
        if !self.received_catalog {
            return;
        }
        let mut rows = self.rows(false);
        if self.online && !self.complete_library {
            self.add_last_seen(&mut rows);
        }
        self.previous.clone_from(&rows);
        let value = Snapshot {
            origin: host.origin.clone(),
            pairing: host.pairing_id().to_owned(),
            seen: self.last_seen.map_or_else(
                || chrono::Local::now().timestamp_millis(),
                |t| t.timestamp_millis(),
            ),
            info: self.info.clone().map(|mut info| {
                info.subscription_limits.clear();
                info
            }),
            directory: self.directory.clone(),
            rows,
        };
        self.last_write = Some(Instant::now());
        cx.background_spawn(async move {
            let res = with_cache_directory(path.parent().unwrap(), || {
                let current = crate::platform::hosts::list()?;
                if !current
                    .iter()
                    .any(|h| h.origin == value.origin && h.pairing_id() == value.pairing)
                {
                    return Ok(());
                }
                let prior = std::fs::File::open(&path)
                    .ok()
                    .and_then(|f| serde_json::from_reader::<_, Snapshot>(f).ok());
                if prior.is_none_or(|p| p.seen <= value.seen) {
                    crate::workspace_state::write_json(&path, &value)?;
                }
                Ok(())
            });
            if let Err(e) = res {
                eprintln!("Could not save the last host catalog: {e}");
            }
        })
        .detach();
    }
}

fn with_cache_directory(
    dir: &std::path::Path,
    f: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("catalog.lock"))?;
    lock.lock()?;
    f()
}

pub(super) fn remove_unpaired() {
    crate::platform::clean_handoff_pairings();
    let Some(dir) = ox_desk_core::desktop_behavior::directory()
        .map(|d| d.join("host-catalog"))
        .filter(|d| d.is_dir())
    else {
        return;
    };
    let _ = with_cache_directory(&dir, || {
        let current: HashSet<_> = crate::platform::hosts::list()?
            .into_iter()
            .filter_map(|h| Snapshot::path(&HostLink::Remote(h)))
            .collect();
        for item in dir.read_dir()? {
            let path = item?.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("-catalog.json"))
                && !current.contains(&path)
            {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    });
}

use super::*;
use crate::kit;
use crate::platform::HostLink;
use gpui_component::input::InputState;
use ox_desk_core::{host_protocol::HostResources, origin::WorkerNeeds};
use serde::Deserialize;
use std::{path::PathBuf, time::Duration};

#[derive(Deserialize)]
struct Host {
    id: String,
    name: String,
    resources: Option<HostResources>,
}
#[derive(Deserialize)]
struct Entry {
    host: Host,
    parent_host: bool,
    paired_name: Option<String>,
    status: String,
    #[serde(default)]
    routes: Vec<serde_json::Value>,
}
#[derive(Deserialize)]
struct Inventory {
    hosts: Vec<Entry>,
    warning: Option<String>,
    project_placement: Option<ProjectRule>,
    placement_error: Option<String>,
}

#[derive(Deserialize)]
struct ProjectRule {
    host: Option<String>,
    os: Option<String>,
}

pub(super) struct Picker {
    target: String,
    inventory: Option<Result<Option<Inventory>, String>>,
    memory: Entity<InputState>,
    cpus: Entity<InputState>,
    os: Entity<InputState>,
    gpu: bool,
    _refresh: Task<()>,
}

impl Picker {
    pub(super) fn new(
        link: HostLink,
        project: PathBuf,
        window: &mut Window,
        cx: &mut Context<Workbench>,
    ) -> Self {
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                let link = link.clone();
                let project = project.clone();
                let result = cx
                    .background_spawn(async move {
                        let data = link.worker_hosts(project).map_err(|e| e.to_string())?;
                        data.map(serde_json::from_value)
                            .transpose()
                            .map_err(|e| e.to_string())
                    })
                    .await;
                let legacy = matches!(result, Ok(None));
                let _ = this.update(cx, |s, cx| {
                    if let Some(Launch::Worker(d)) = &mut s.launch {
                        d.hosts.inventory = Some(result);
                        cx.notify();
                    }
                });
                if legacy {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
            }
        });
        Self {
            target: "parent".into(),
            inventory: None,
            memory: kit::field("", "Any", false, window, cx),
            cpus: kit::field("", "Any", false, window, cx),
            os: kit::field("", "Any", false, window, cx),
            gpu: false,
            _refresh: refresh,
        }
    }

    pub(super) fn same_as_parent(&self) -> bool {
        self.target == "parent"
    }
    pub(super) fn aliases(&self, runtime: Runtime) -> Option<Vec<String>> {
        if self.target == "parent" {
            return None;
        }
        let inventory = self.inventory.as_ref()?.as_ref().ok()?.as_ref()?;
        Some(
            inventory
                .hosts
                .iter()
                .filter(|e| {
                    matches!(self.target.as_str(), "auto" | "project") || self.target == e.host.id
                })
                .flat_map(|e| e.routes.iter())
                .filter_map(|v| serde_json::from_value::<Route>(v.clone()).ok())
                .filter(|r| r.runtime == runtime)
                .filter_map(|r| match r.billing {
                    Billing::Api(alias) => Some(alias),
                    _ => None,
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
        )
    }

    pub(super) fn apply(&self, start: &mut WorkerStart, cx: &App) -> Result<(), String> {
        if self.target == "project" {
            return Ok(());
        }
        start.host = Some(self.target.clone());
        if self.target == "auto" {
            let number = |field: &Entity<InputState>, name: &str| {
                let text = field.read(cx).value();
                let text = text.trim();
                if text.is_empty() {
                    Ok(None)
                } else {
                    text.parse::<u32>()
                        .ok()
                        .filter(|n| *n > 0)
                        .map(Some)
                        .ok_or_else(|| format!("Enter a positive whole number for {name}."))
                }
            };
            let os = self.os.read(cx).value().trim().to_owned();
            start.needs = Some(WorkerNeeds {
                memory_gb: number(&self.memory, "memory")?,
                cpus: number(&self.cpus, "CPUs")?,
                os: (!os.is_empty()).then_some(os),
                gpu: self.gpu,
            });
        }
        start.validate()
    }
}

impl Workbench {
    fn set_worker_host(&mut self, host: String, cx: &mut Context<Self>) {
        self.launch_picker = None;
        if let Some(Launch::Worker(d)) = &mut self.launch {
            d.hosts.target = host;
            d.choice.error = None;
        }
        cx.notify();
    }
    pub(super) fn host_row(&self, p: Palette, draft: &WorkerDraft, cx: &mut Context<Self>) -> Div {
        let picker = &draft.hosts;
        let inventory = picker
            .inventory
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .and_then(Option::as_ref);
        let mut choices = row().flex_wrap().gap_2();
        let mut entries = vec![("parent".to_owned(), "Same as parent".to_owned(), true)];
        if let Some(inventory) = inventory {
            for entry in &inventory.hosts {
                let id = if entry.parent_host {
                    "parent"
                } else {
                    &entry.host.id
                };
                let name = if entry.parent_host {
                    "Same as parent"
                } else {
                    entry.paired_name.as_ref().unwrap_or(&entry.host.name)
                };
                let label = if let Some(r) = &entry.host.resources {
                    format!(
                        "{name} · load {:.1} · {:.1} GB free",
                        r.load_one_minute,
                        r.available_memory as f64 / 1e9
                    )
                } else {
                    format!("{name} · {}", entry.status.replace('_', " "))
                };
                if entry.parent_host {
                    entries[0].1 = label;
                } else {
                    entries.push((id.to_owned(), label, entry.status == "online"));
                }
            }
            if let Some(rule) = &inventory.project_placement {
                let label = [
                    "Project rule",
                    rule.host.as_deref().unwrap_or("Prefer parent"),
                ]
                .into_iter()
                .chain(rule.os.as_deref())
                .collect::<Vec<_>>()
                .join(" · ");
                entries.push(("project".into(), label, true));
            }
            entries.push(("auto".into(), "Auto".into(), true));
        }
        for (i, (id, label, enabled)) in entries.into_iter().enumerate() {
            choices = choices.child(button(
                p,
                ("worker-host", i),
                label,
                picker.target == id,
                enabled,
                cx.listener(move |s, _, _, cx| s.set_worker_host(id.clone(), cx)),
            ));
        }
        let mut content = col().gap_1().child(choices);
        let status = match picker.inventory.as_ref() {
            None => Some("Loading worker hosts…"),
            Some(Err(e)) => Some(e.as_str()),
            Some(Ok(Some(v))) => v.warning.as_deref().or(v.placement_error.as_deref()),
            _ => None,
        };
        if let Some(status) = status {
            content = content.child(small(p, status.to_owned()));
        }
        if picker.target == "auto" {
            let mut inputs = row().gap_2().flex_wrap();
            for (name, input) in [
                ("Memory (GB)", &picker.memory),
                ("CPUs", &picker.cpus),
                ("OS", &picker.os),
            ] {
                inputs = inputs.child(div().w(px(108.)).child(field(p, name, kit::input(input))));
            }
            content = content.child(inputs).child(button(
                p,
                "worker-needs-gpu",
                "Require GPU",
                picker.gpu,
                true,
                cx.listener(|s, _, _, cx| {
                    if let Some(Launch::Worker(d)) = &mut s.launch {
                        d.hosts.gpu = !d.hosts.gpu;
                        cx.notify();
                    }
                }),
            ));
        }
        field(p, "Worker host", content)
    }
}

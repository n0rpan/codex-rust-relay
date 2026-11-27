use super::{Inventory, Peer, check_peer};
use ox_desk_core::{
    host_protocol::HostResources,
    modes::Level,
    origin::{WorkerNeeds, WorkerStart},
};
use serde_json::Value;
use std::cmp::Ordering;

/// Compare pressure relative to the machine's capacity. A busy machine remains eligible.
fn rank(a: &HostResources, b: &HostResources) -> Ordering {
    let cpu = |r: &HostResources| 1.0 - r.load_one_minute / r.cpus.unwrap_or(1).max(1) as f64;
    let headroom =
        |r: &HostResources| cpu(r).min(r.available_memory as f64 / r.total_memory.max(1) as f64);
    headroom(a)
        .total_cmp(&headroom(b))
        .then_with(|| cpu(a).total_cmp(&cpu(b)))
        .then_with(|| a.available_memory.cmp(&b.available_memory))
        .then_with(|| b.running_workers.cmp(&a.running_workers))
}

fn hardware(r: &HostResources, needs: &WorkerNeeds) -> Result<(), String> {
    if let Some(gb) = needs.memory_gb
        && r.total_memory < u64::from(gb) * 1_000_000_000
    {
        return Err(format!("needs {gb} GB of total memory"));
    }
    if let Some(cpus) = needs.cpus
        && r.cpus.unwrap_or(0) < cpus as usize
    {
        return Err(format!("needs {cpus} CPUs"));
    }
    if needs.gpu && r.gpus.is_empty() {
        return Err("needs a GPU".into());
    }
    if let Some(os) = &needs.os
        && !r.os.eq_ignore_ascii_case(os)
    {
        return Err(format!("needs {os}"));
    }
    Ok(())
}

/// Resolve once against the parent and the parent's signed worker links.
pub fn choose(
    list: &Value,
    selector: &str,
    needs: &WorkerNeeds,
    start: &WorkerStart,
    parent: Level,
    prefer_parent: bool,
) -> Result<String, String> {
    let rows = list["hosts"]
        .as_array()
        .ok_or("Worker host inventory is unavailable.")?;
    let label = selector.strip_prefix("label:");
    let any = selector == "auto";
    let mut best: Option<(String, HostResources)> = None;
    let mut reasons = Vec::new();
    let mut matches = 0;
    for row in rows {
        let h = &row["host"];
        let id = h["id"].as_str().unwrap_or_default();
        let name = row["paired_name"]
            .as_str()
            .or(h["name"].as_str())
            .unwrap_or(id);
        let local = row["parent_host"] == true;
        let selected = if let Some(label) = label {
            h["labels"].as_array().is_some_and(|ls| {
                ls.iter()
                    .any(|l| l.as_str().is_some_and(|s| s.eq_ignore_ascii_case(label)))
            })
        } else {
            any || selector == id
                || (if selector == "parent" {
                    local
                } else {
                    selector.eq_ignore_ascii_case(name)
                        || h["name"]
                            .as_str()
                            .is_some_and(|s| s.eq_ignore_ascii_case(selector))
                })
        };
        if !selected {
            continue;
        }
        matches += 1;
        let candidate = || -> Result<HostResources, String> {
            if row["status"] != "online" {
                return Err(row["status"]
                    .as_str()
                    .unwrap_or("unavailable")
                    .replace('_', " "));
            }
            let inv: Inventory = serde_json::from_value(row.clone())
                .map_err(|_| "update the host to report worker inventory")?;
            if local {
                if !inv.runtimes.contains(&start.runtime) {
                    return Err(format!("{} is unavailable", start.runtime.name()));
                }
                if !inv.routes.contains(&start.route()) {
                    return Err(format!("{} is unavailable", start.route().label()));
                }
            } else {
                let peer: Peer = serde_json::from_value(row.clone())
                    .map_err(|_| "update the host to report worker permissions")?;
                check_peer(&peer, name, start, parent).map_err(|e| e.to_string())?;
            }
            let r = inv.host.resources.ok_or("resource sample unavailable")?;
            hardware(&r, needs)?;
            Ok(r)
        };
        match candidate() {
            Ok(_) if local && prefer_parent && any => return Ok(id.to_owned()),
            Ok(r) if best.as_ref().is_none_or(|(_, old)| rank(&r, old).is_gt()) => {
                best = Some((id.to_owned(), r))
            }
            Ok(_) => (),
            Err(e) => reasons.push(format!("{name} ({e})")),
        }
    }
    if !any && label.is_none() && matches > 1 {
        return Err(format!(
            "Several hosts match {selector:?}. Choose a stable host ID."
        ));
    }
    if let Some((id, _)) = best {
        return Ok(id);
    }
    if reasons.is_empty() {
        return Err(format!(
            "No known worker host matches {selector:?}. See list_ox_hosts."
        ));
    }
    Err(format!(
        "No eligible worker host for {selector}: {}.",
        reasons.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str, local: bool, load: f64) -> Value {
        json!({
            "host":{"id":id,"name":id,"version":"current","epoch":"one","labels":["build"],"resources":{
                "os":"linux","arch":"x86_64","cpus":8,"total_memory":64_000_000_000u64,
                "available_memory":40_000_000_000u64,"load_one_minute":load,"running_workers":12,"gpus":[]}},
            "parent_host":local, "status":"online", "has_project":true,"linked_read":true,"routes_known":true,
            "runtimes":["claude"], "routes":[{"runtime":"claude","billing":"subscription"}],
            "grant":{"source":"source","may_launch":true,"ceiling":"read"}
        })
    }
    fn select(rows: Vec<Value>, target: &str, needs: WorkerNeeds) -> Result<String, String> {
        let start =
            WorkerStart::parse_command("parent", "/worker claude --mode read review the tests")
                .unwrap()
                .unwrap();
        choose(
            &json!({"hosts":rows}),
            target,
            &needs,
            &start,
            Level::Write,
            false,
        )
    }

    #[test]
    fn pressure_ranks_eligible_hosts_but_never_disqualifies_them() {
        let parent = row("laptop", true, 12.0);
        let mut studio = row("studio", false, 1.0);
        let needs = WorkerNeeds {
            memory_gb: Some(64),
            cpus: Some(8),
            os: Some("linux".into()),
            gpu: false,
        };
        assert_eq!(
            select(vec![parent.clone(), studio.clone()], "auto", needs.clone()).unwrap(),
            "studio"
        );
        for (key, value) in [
            ("has_project", json!(false)),
            ("runtimes", json!([])),
            ("routes", json!([])),
            ("status", json!("unavailable")),
        ] {
            let mut unavailable = studio.clone();
            unavailable[key] = value;
            assert_eq!(
                select(vec![parent.clone(), unavailable], "auto", needs.clone()).unwrap(),
                "laptop"
            );
        }
        studio["grant"]["may_launch"] = false.into();
        assert_eq!(
            select(vec![parent.clone(), studio], "label:build", needs.clone()).unwrap(),
            "laptop"
        );
        assert!(
            select(
                vec![parent.clone()],
                "parent",
                WorkerNeeds {
                    gpu: true,
                    ..needs.clone()
                }
            )
            .unwrap_err()
            .contains("GPU")
        );
        assert!(
            select(
                vec![parent],
                "auto",
                WorkerNeeds {
                    memory_gb: Some(65),
                    ..needs
                }
            )
            .unwrap_err()
            .contains("65 GB")
        );
    }

    #[test]
    fn an_os_only_project_rule_keeps_a_qualified_parent() {
        let mut laptop = row("laptop", true, 20.);
        let studio = row("studio", false, 0.);
        let start = WorkerStart::parse_command("p", "/worker claude --mode read review")
            .unwrap()
            .unwrap();
        let needs = WorkerNeeds {
            os: Some("linux".into()),
            ..Default::default()
        };
        let route = |local: &Value| {
            choose(
                &json!({"hosts":[local,studio]}),
                "auto",
                &needs,
                &start,
                Level::Write,
                true,
            )
            .unwrap()
        };
        assert_eq!(route(&laptop), "laptop");
        laptop["host"]["resources"]["os"] = "macos".into();
        assert_eq!(route(&laptop), "studio");
    }

    #[test]
    fn labels_filter_and_a_tie_preserves_saved_order() {
        let parent = row("laptop", true, 2.0);
        let mut studio = row("studio", false, 2.0);
        studio["host"]["labels"] = json!(["big"]);
        let needs = WorkerNeeds::default();
        assert_eq!(
            select(
                vec![parent.clone(), studio.clone()],
                "label:BIG",
                needs.clone()
            )
            .unwrap(),
            "studio"
        );
        assert_eq!(
            select(vec![parent.clone(), studio.clone()], "auto", needs.clone()).unwrap(),
            "laptop"
        );
        studio["paired_name"] = "laptop".into();
        assert!(
            select(vec![parent, studio], "laptop", needs)
                .unwrap_err()
                .contains("Several hosts")
        );
    }
}

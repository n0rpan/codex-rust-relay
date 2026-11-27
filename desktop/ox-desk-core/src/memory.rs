//! Budgeted Tier-1 memory injection for first turns of Codex and Claude sessions/workers.

use crate::runtime::{Billing, Route};
use ox_memory::{DEFAULT_BRIEF_CHARS, MemoryStore, Sensitivity, canonical_project_id};
use std::path::{Path, PathBuf};

/// Host-supplied configuration for Tier-1 memory injection on a [`crate::store::Store`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryConfig {
    pub store_path: PathBuf,
    pub host_id: String,
    /// Optional pre-filtered brief supplied by a parent host for a remote worker job.
    pub brief_override: Option<String>,
}

/// Safe sensitivity ceiling for a resolved runtime/billing route.
///
/// `LocalOnly` memories are only eligible when inference itself stays local
/// (`ollama/`, `lmstudio/`, or `local/` API aliases). Cloud subscriptions and
/// external API providers are capped at `Private`. If `OX_MEMORY_MAX_SENSITIVITY`
/// is set, it can only tighten (never raise) this ceiling.
pub fn route_sensitivity_ceiling(route: &Route) -> Sensitivity {
    let base = match &route.billing {
        Billing::Api(alias) if is_local_inference_alias(alias) => Sensitivity::LocalOnly,
        Billing::Api(_) | Billing::Subscription => Sensitivity::Private,
    };
    match std::env::var("OX_MEMORY_MAX_SENSITIVITY")
        .ok()
        .and_then(|v| Sensitivity::parse(&v).ok())
    {
        Some(env_cap) => base.min(env_cap),
        None => base,
    }
}

fn is_local_inference_alias(alias: &str) -> bool {
    let lower = alias.trim().to_ascii_lowercase();
    lower.starts_with("ollama/")
        || lower.starts_with("lmstudio/")
        || lower.starts_with("local/")
        || lower.starts_with("llama.cpp/")
}

/// Build the Tier-1 pinned memory brief to attach once on the first turn of a session or worker.
///
/// Returns `None` if no eligible pinned memories exist or if the store cannot be read.
pub fn first_turn_brief(config: &MemoryConfig, project: &Path, route: &Route) -> Option<String> {
    if let Some(override_brief) = config
        .brief_override
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let capped: String = override_brief.chars().take(DEFAULT_BRIEF_CHARS).collect();
        return Some(format!(
            "{capped}\n(Use recall_ox_memory for additional saved preferences or project facts.)"
        ));
    }

    let store = MemoryStore::open(&config.store_path);
    let project_id = canonical_project_id(project);
    let ceiling = route_sensitivity_ceiling(route);
    let brief = store
        .tier1_brief(
            Some(&project_id),
            Some(&config.host_id),
            ceiling,
            DEFAULT_BRIEF_CHARS,
        )
        .ok()??;
    Some(format!(
        "{brief}\n(Use recall_ox_memory for additional saved preferences or project facts.)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Runtime;
    use ox_memory::{Kind, NewMemory, Scope};

    #[test]
    fn route_ceiling_restricts_local_only_to_local_inference() {
        assert_eq!(
            route_sensitivity_ceiling(&Route::CODEX_SUBSCRIPTION),
            Sensitivity::Private
        );
        assert_eq!(
            route_sensitivity_ceiling(&Runtime::Claude.route(None)),
            Sensitivity::Private
        );
        assert_eq!(
            route_sensitivity_ceiling(&Route::codex_api("openrouter/openai/gpt-6.2-astra".into())),
            Sensitivity::Private
        );
        assert_eq!(
            route_sensitivity_ceiling(&Route::codex_api("ollama/qwen3".into())),
            Sensitivity::LocalOnly
        );
        assert_eq!(
            route_sensitivity_ceiling(&Route::codex_api("local/llama".into())),
            Sensitivity::LocalOnly
        );
    }

    #[test]
    fn first_turn_brief_respects_route_sensitivity_and_override() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("memory.json");
        let store = MemoryStore::open(&store_path);
        store
            .upsert(NewMemory {
                id: None,
                scope: Scope::Global,
                kind: Kind::RoutingPolicy,
                sensitivity: Sensitivity::Private,
                text: "Use Claude subscription for private work.".into(),
                tags: vec!["routing".into()],
                pin: true,
                source_host: "host-a".into(),
            })
            .unwrap();
        store
            .upsert(NewMemory {
                id: None,
                scope: Scope::Host("host-a".into()),
                kind: Kind::Fact,
                sensitivity: Sensitivity::LocalOnly,
                text: "Local-only GPU calibration note.".into(),
                tags: vec![],
                pin: true,
                source_host: "host-a".into(),
            })
            .unwrap();

        let cfg = MemoryConfig {
            store_path,
            host_id: "host-a".into(),
            brief_override: None,
        };

        // Cloud route gets Private policy but NOT LocalOnly fact
        let cloud_brief = first_turn_brief(&cfg, dir.path(), &Route::CODEX_SUBSCRIPTION).unwrap();
        assert!(cloud_brief.contains("Use Claude subscription for private work."));
        assert!(!cloud_brief.contains("Local-only GPU calibration note."));

        // Local inference route gets both
        let local_brief =
            first_turn_brief(&cfg, dir.path(), &Route::codex_api("ollama/qwen3".into())).unwrap();
        assert!(local_brief.contains("Use Claude subscription for private work."));
        assert!(local_brief.contains("Local-only GPU calibration note."));

        // Brief override takes precedence (used by remote worker jobs)
        let override_cfg = MemoryConfig {
            brief_override: Some("OX memory (pinned):\n- [policy/global] Custom brief.".into()),
            ..cfg
        };
        let brief =
            first_turn_brief(&override_cfg, dir.path(), &Route::CODEX_SUBSCRIPTION).unwrap();
        assert!(brief.contains("Custom brief."));
    }
}

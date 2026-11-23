//! Model discovery parsing and the Codex-facing model catalog.
//!
//! Upstream `/models` payloads vary between Responses API vendors, so they are
//! normalised into [`ModelSpec`] values. Static `[[providers.<name>.models]]`
//! entries from `codeox.toml` either add models (useful when discovery is
//! unavailable) or override capabilities such as the context window, input
//! modalities, or supported reasoning levels.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::provider::ModelEntry;

#[derive(Deserialize)]
pub struct ProviderModels {
    data: Vec<ProviderModel>,
}

#[derive(Default, Deserialize)]
pub struct BundledModels {
    models: Vec<Value>,
}

#[derive(Deserialize)]
struct ProviderModel {
    id: String,
    name: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    modalities: Modalities,
    #[serde(default)]
    capabilities: Capabilities,
    #[serde(default)]
    limit: Limits,
    #[serde(default)]
    context_length: Value,
    #[serde(default)]
    max_input_tokens: Value,
}

#[derive(Default, Deserialize)]
struct Modalities {
    #[serde(default)]
    input: Vec<String>,
    #[serde(default)]
    output: Vec<String>,
}

#[derive(Default, Deserialize)]
struct Capabilities {
    #[serde(default)]
    tools: ToolCapabilities,
    #[serde(default)]
    reasoning: ReasoningCapabilities,
}

#[derive(Default, Deserialize)]
struct ToolCapabilities {
    supported: Option<bool>,
}

#[derive(Default, Deserialize)]
struct ReasoningCapabilities {
    #[serde(default)]
    effort: Vec<String>,
}

#[derive(Default, Deserialize)]
struct Limits {
    context: Option<u64>,
    input: Option<u64>,
}

/// A model after discovery and static overrides, still identified by its
/// upstream ID. The alias is resolved separately so routing and catalog
/// rendering use exactly the same string.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelSpec {
    pub id: String,
    pub alias: Option<String>,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    pub input_modalities: Vec<String>,
    pub reasoning_efforts: Vec<String>,
}

/// One row of the Codex model catalog.
pub struct CatalogModel {
    pub slug: String,
    pub display_name: String,
    pub description: String,
    pub context_window: Option<u64>,
    pub input_modalities: Vec<String>,
    pub reasoning_efforts: Vec<String>,
    /// Offer Codex's freeform `apply_patch` tool. Enabled for translated
    /// vendors, where the adapter normalizes it to a function tool; Responses
    /// vendors keep their previous tool surface.
    pub apply_patch: bool,
}

/// Merges discovered models with the vendor's static entries.
///
/// Static entries with the same upstream ID act as capability overrides;
/// entries for IDs discovery did not return are added as models, which keeps
/// aliases (and therefore resumable sessions) available when a vendor's model
/// list is temporarily unavailable. The first entry wins for duplicate IDs.
pub fn merge_models(
    discovered: Option<ProviderModels>,
    overrides: &[ModelEntry],
) -> Vec<ModelSpec> {
    let mut specs: Vec<ModelSpec> = Vec::new();
    let mut used = vec![false; overrides.len()];
    if let Some(models) = discovered {
        let mut seen = HashSet::new();
        for model in models.data {
            if !model.supports_coding() || !seen.insert(model.id.clone()) {
                continue;
            }
            let mut spec = ModelSpec::from_upstream(model);
            if let Some(index) = overrides.iter().position(|entry| entry.id == spec.id) {
                spec.apply_override(&overrides[index]);
                used[index] = true;
            }
            specs.push(spec);
        }
    }
    for (index, entry) in overrides.iter().enumerate() {
        if used[index] || specs.iter().any(|spec| spec.id == entry.id) {
            continue;
        }
        specs.push(ModelSpec::from_entry(entry));
    }
    specs
}

impl ModelSpec {
    fn from_upstream(model: ProviderModel) -> Self {
        let context_window = model
            .limit
            .context
            .into_iter()
            .chain(model.limit.input)
            .chain(model.context_length.as_u64())
            .chain(model.max_input_tokens.as_u64())
            .filter(|limit| *limit > 0)
            .min();
        let mut input_modalities = vec!["text".to_owned()];
        if model
            .modalities
            .input
            .iter()
            .any(|input| input.starts_with("image"))
        {
            input_modalities.push("image".to_owned());
        }
        Self {
            id: model.id,
            alias: None,
            name: model.name,
            context_window,
            input_modalities,
            reasoning_efforts: model.capabilities.reasoning.effort,
        }
    }

    fn from_entry(entry: &ModelEntry) -> Self {
        let mut spec = Self {
            id: entry.id.clone(),
            alias: None,
            name: None,
            context_window: None,
            input_modalities: vec!["text".to_owned()],
            reasoning_efforts: Vec::new(),
        };
        spec.apply_override(entry);
        spec
    }

    fn apply_override(&mut self, entry: &ModelEntry) {
        if let Some(alias) = &entry.alias {
            self.alias = Some(alias.clone());
        }
        if let Some(name) = &entry.name {
            self.name = Some(name.clone());
        }
        if let Some(context_window) = entry.context_window {
            self.context_window = Some(context_window);
        }
        if let Some(input_modalities) = &entry.input_modalities {
            self.input_modalities = normalise_modalities(input_modalities);
        }
        if let Some(reasoning_efforts) = &entry.reasoning_efforts {
            self.reasoning_efforts = reasoning_efforts.clone();
        }
    }
}

fn normalise_modalities(input: &[String]) -> Vec<String> {
    let mut modalities = Vec::new();
    for modality in input {
        if (modality == "text" || modality == "image") && !modalities.contains(modality) {
            modalities.push(modality.clone());
        }
    }
    if !modalities.iter().any(|modality| modality == "text") {
        modalities.insert(0, "text".to_owned());
    }
    modalities
}

impl ProviderModels {
    /// Legacy single-provider translation, keeping upstream model IDs as Codex
    /// slugs exactly as the original relay did.
    pub fn legacy_catalog(self, provider_name: &str, bundled: &BundledModels) -> Option<Value> {
        let specs = merge_models(Some(self), &[]);
        if specs.is_empty() {
            return None;
        }
        let models = specs
            .into_iter()
            .map(|spec| CatalogModel {
                slug: spec.id.clone(),
                display_name: spec.name.clone().unwrap_or_else(|| spec.id.clone()),
                description: provider_name.to_owned(),
                context_window: spec.context_window,
                input_modalities: spec.input_modalities,
                reasoning_efforts: spec.reasoning_efforts,
                apply_patch: false,
            })
            .collect::<Vec<_>>();
        Some(catalog_json(&models, bundled))
    }
}

impl ProviderModel {
    fn supports_coding(&self) -> bool {
        if self.id.is_empty() || self.capabilities.tools.supported == Some(false) {
            return false;
        }
        if !self.modalities.output.is_empty() {
            return self.modalities.output.iter().any(|output| output == "text");
        }

        !matches!(
            self.kind.as_deref(),
            Some("embedding" | "embeddings" | "image" | "audio" | "video" | "speech" | "rerank")
        )
    }
}

/// Renders the Codex catalog. Codex merges remote entries with its built-in
/// catalog by slug, so built-in models that a vendor did not list are emitted
/// as hidden entries to prevent selecting a model that would not route.
pub fn catalog_json(models: &[CatalogModel], bundled: &BundledModels) -> Value {
    let mut seen = HashSet::new();
    let mut entries = models
        .iter()
        .filter(|model| seen.insert(model.slug.clone()))
        .enumerate()
        .map(|(priority, model)| model.entry(priority))
        .collect::<Vec<_>>();
    entries.extend(bundled.models.iter().filter_map(|model| {
        let slug = model.get("slug")?.as_str()?;
        if seen.contains(slug) {
            return None;
        }
        let mut hidden = model.clone();
        hidden["visibility"] = json!("hide");
        hidden["supported_in_api"] = json!(false);
        Some(hidden)
    }));
    json!({ "models": entries })
}

pub(crate) fn prioritize_favorites(catalog: &mut Value, favorites: &[String]) {
    if favorites.is_empty() {
        return;
    }
    let Some(models) = catalog["models"].as_array_mut() else {
        return;
    };
    models.sort_by_cached_key(|entry| {
        if entry["visibility"] == "list"
            && let Some(model) = entry["slug"].as_str()
            && let Some(rank) = favorites.iter().position(|favorite| favorite == model)
        {
            rank
        } else {
            usize::MAX
        }
    });
    for (priority, entry) in models
        .iter_mut()
        .filter(|entry| entry["visibility"] == "list")
        .enumerate()
    {
        entry["priority"] = json!(priority);
    }
}

impl CatalogModel {
    fn entry(&self, priority: usize) -> Value {
        let levels = self
            .reasoning_efforts
            .iter()
            .filter_map(|effort| {
                let description = match effort.as_str() {
                    "none" => "Reasoning off",
                    "minimal" => "Minimal reasoning",
                    "low" => "Faster responses",
                    "medium" => "Balanced reasoning",
                    "high" => "Deeper reasoning",
                    "xhigh" => "Extra-high reasoning",
                    "max" => "Maximum reasoning",
                    _ => return None,
                };
                Some(json!({ "effort": effort, "description": description }))
            })
            .collect::<Vec<_>>();
        let default_effort = ["medium", "low", "high", "xhigh", "max", "minimal", "none"]
            .into_iter()
            .find(|effort| levels.iter().any(|level| level["effort"] == *effort));

        let mut entry = json!({
            "slug": self.slug,
            "display_name": self.display_name,
            "description": self.description,
            "base_instructions": "Help the user with their coding tasks. Use the available tools to inspect, edit, and verify their work.",
            "default_reasoning_level": default_effort,
            "supported_reasoning_levels": levels,
            "shell_type": "shell_command",
            "visibility": "list",
            "supported_in_api": true,
            "priority": priority,
            "support_verbosity": false,
            "supports_parallel_tool_calls": false,
            "truncation_policy": { "mode": "tokens", "limit": 10000 },
            "experimental_supported_tools": [],
            "input_modalities": self.input_modalities,
            "context_window": self.context_window
        });
        if self.apply_patch {
            // The translated adapters normalize this freeform tool to a
            // function tool and map calls back to custom_tool_call items.
            entry["apply_patch_tool_type"] = json!("freeform");
        }
        entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bundled() -> BundledModels {
        serde_json::from_value::<BundledModels>(json!({
            "models": [
                {"slug": "demo-coding", "visibility": "list"},
                {"slug": "bundled-model", "visibility": "list"}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn keeps_provider_models_and_hides_only_unlisted_defaults() {
        let upstream = serde_json::from_value::<ProviderModels>(json!({
            "data": [
                {"id": "demo-coding", "name": "Demo Coding"},
                {"id": "demo-embeddings", "type": "embedding"},
                {"id": "demo-media", "modalities": {"output": ["video"]}}
            ]
        }))
        .unwrap();

        let catalog = upstream
            .legacy_catalog("Demo Provider", &bundled())
            .unwrap();
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["display_name"], "Demo Coding");
        assert_eq!(models[0]["visibility"], "list");
        assert_eq!(models[1]["slug"], "bundled-model");
        assert_eq!(models[1]["visibility"], "hide");
        assert_eq!(models[1]["supported_in_api"], false);
    }

    #[test]
    fn reads_context_from_each_catalog_and_uses_the_tightest_limit() {
        let upstream = serde_json::from_value::<ProviderModels>(json!({
            "data": [
                {"id": "go", "limit": {"context": 1_000_000, "output": 131_072}},
                {"id": "router", "context_length": 256_000},
                {"id": "anthropic", "max_input_tokens": 200_000},
                {"id": "bounded", "limit": {"context": 256_000, "input": 200_000}, "context_length": 128_000},
                {"id": "unknown", "context_length": "unspecified", "max_input_tokens": 0}
            ]
        }))
        .unwrap();
        let limits: Vec<_> = merge_models(Some(upstream), &[])
            .into_iter()
            .map(|model| model.context_window)
            .collect();
        assert_eq!(
            limits,
            [
                Some(1_000_000),
                Some(256_000),
                Some(200_000),
                Some(128_000),
                None
            ]
        );
    }

    #[test]
    fn static_entries_override_capabilities_and_add_models() {
        let discovered = serde_json::from_value::<ProviderModels>(json!({
            "data": [{"id": "coder", "limit": {"context": 8192}}]
        }))
        .unwrap();
        let overrides = vec![
            ModelEntry {
                id: "coder".to_owned(),
                alias: Some("acme/stable-coder".to_owned()),
                name: Some("Stable Coder".to_owned()),
                context_window: Some(200_000),
                input_modalities: Some(vec!["text".to_owned(), "image".to_owned()]),
                reasoning_efforts: Some(vec!["low".to_owned(), "high".to_owned()]),
            },
            ModelEntry {
                id: "offline-model".to_owned(),
                ..ModelEntry::default()
            },
        ];

        let specs = merge_models(Some(discovered), &overrides);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].id, "coder");
        assert_eq!(specs[0].context_window, Some(200_000));
        assert_eq!(specs[0].alias.as_deref(), Some("acme/stable-coder"));
        assert_eq!(specs[0].input_modalities, ["text", "image"]);
        assert_eq!(specs[1].id, "offline-model");
        assert_eq!(specs[1].input_modalities, ["text"]);
    }

    #[test]
    fn coding_only_models_are_filtered_from_discovery() {
        let discovered = serde_json::from_value::<ProviderModels>(json!({
            "data": [
                {"id": "with-tools", "capabilities": {"tools": {"supported": true}}},
                {"id": "no-tools", "capabilities": {"tools": {"supported": false}}},
                {"id": "embedder", "type": "embeddings"}
            ]
        }))
        .unwrap();
        let ids = merge_models(Some(discovered), &[])
            .into_iter()
            .map(|spec| spec.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["with-tools"]);
    }

    #[test]
    fn favorites_follow_saved_order_across_providers() {
        let mut catalog = json!({"models": [
            {"slug": "alibaba/a", "priority": 0, "visibility": "list"},
            {"slug": "openrouter/b", "priority": 1, "visibility": "list"},
            {"slug": "opencode-go/c", "priority": 2, "visibility": "list"},
            {"slug": "built-in", "priority": 0, "visibility": "hide"},
        ]});
        prioritize_favorites(
            &mut catalog,
            &[
                "opencode-go/c".to_owned(),
                "built-in".to_owned(),
                "alibaba/a".to_owned(),
            ],
        );
        let rows = catalog["models"].as_array().unwrap();
        assert_eq!(rows[0]["slug"], "opencode-go/c");
        assert_eq!(rows[0]["priority"], 0);
        assert_eq!(rows[1]["slug"], "alibaba/a");
        assert_eq!(rows[1]["priority"], 1);
        assert_eq!(rows[2]["slug"], "openrouter/b");
        assert_eq!(rows[3]["visibility"], "hide");
    }

    #[test]
    fn catalog_uses_alias_slugs_and_keeps_hidden_bundled_models() {
        let models = vec![CatalogModel {
            slug: "alpha/coder".to_owned(),
            display_name: "Coder".to_owned(),
            description: "Alpha".to_owned(),
            context_window: Some(128_000),
            input_modalities: vec!["text".to_owned()],
            reasoning_efforts: vec!["medium".to_owned()],
            apply_patch: true,
        }];
        let catalog = catalog_json(&models, &bundled());
        let entries = catalog["models"].as_array().unwrap();
        assert_eq!(entries[0]["slug"], "alpha/coder");
        assert_eq!(entries[0]["description"], "Alpha");
        assert_eq!(entries[0]["context_window"], 128_000);
        assert_eq!(entries[0]["default_reasoning_level"], "medium");
        assert_eq!(entries[0]["apply_patch_tool_type"], "freeform");
        assert_eq!(entries[1]["slug"], "demo-coding");
        assert_eq!(entries[1]["visibility"], "hide");
        assert_eq!(entries[2]["slug"], "bundled-model");
    }
}

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

/// Reasoning effort names in increasing strength. Providers publish them in
/// their own order and vocabulary, so OX ranks by this list instead.
pub const REASONING_LEVELS: [&str; 7] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

pub(crate) fn reasoning_rank(level: &str) -> Option<usize> {
    REASONING_LEVELS.iter().position(|known| *known == level)
}

/// Picks the effort to send when the requested one is not available.
///
/// A provider's own default wins because it is what the provider would have
/// used had the request stayed silent. Otherwise the closest supported level is
/// used, preferring the lower one on a tie so a clamp never quietly spends more
/// reasoning tokens than the caller asked for.
pub fn clamp_reasoning_effort(
    requested: &str,
    supported: &[String],
    default: Option<&str>,
) -> Option<String> {
    if supported.iter().any(|level| level == requested) {
        return None;
    }
    if let Some(default) = default
        && supported.iter().any(|level| level == default)
    {
        return Some(default.to_owned());
    }
    let target = reasoning_rank(requested).unwrap_or(3);
    supported
        .iter()
        .min_by_key(|level| {
            let rank = reasoning_rank(level).unwrap_or(target);
            // Equal distances pick the lower level, never the more expensive one.
            (rank.abs_diff(target), rank)
        })
        .cloned()
}

#[derive(Deserialize)]
pub struct ProviderModels {
    #[serde(deserialize_with = "parse_model_rows")]
    data: Vec<ModelSpec>,
    #[serde(default)]
    has_more: Value,
    #[serde(default)]
    last_id: Value,
}

impl ProviderModels {
    pub fn next_cursor(&self) -> Option<&str> {
        self.last_id.as_str().filter(|cursor| {
            self.has_more.as_bool() == Some(true) && !cursor.is_empty() && cursor.len() <= 1024
        })
    }

    pub fn append_page(&mut self, page: Self) {
        self.data.extend(page.data);
        self.has_more = page.has_more;
        self.last_id = page.last_id;
    }

    /// Removes discovered models whose IDs `keep` rejects, returning the
    /// rejected IDs in upstream order.
    pub fn retain_ids(&mut self, mut keep: impl FnMut(&str) -> bool) -> Vec<String> {
        let mut dropped = Vec::new();
        self.data.retain(|model| {
            let kept = keep(&model.id);
            if !kept {
                dropped.push(model.id.clone());
            }
            kept
        });
        dropped
    }

    /// Drops models the provider does not serve on this API, keeping every
    /// model whose endpoint list is unknown.
    pub fn retain_wire(&mut self, wire: crate::provider::Wire) {
        let path = match wire {
            crate::provider::Wire::Responses => "/api/v1/responses",
            crate::provider::Wire::ChatCompletions => "/api/v1/chat/completions",
            crate::provider::Wire::AnthropicMessages => "/api/v1/messages",
        };
        self.data.retain(|model| {
            model.endpoints.is_empty()
                || model
                    .endpoints
                    .iter()
                    .any(|endpoint| endpoint.trim_end_matches('/').ends_with(path))
        });
    }
}

#[derive(Default, Deserialize)]
pub struct BundledModels {
    models: Vec<Value>,
}

/// Providers disagree on optional field types. A bad field is unknown for
/// this model, and a row without a usable ID does not affect its neighbors.
fn parse_model_rows<'de, D>(deserializer: D) -> Result<Vec<ModelSpec>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let rows = Vec::<Value>::deserialize(deserializer)?;
    Ok(rows.iter().filter_map(ModelSpec::from_upstream).collect())
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
    pub max_output_tokens: Option<u64>,
    pub input_modalities: Vec<String>,
    pub reasoning_efforts: Vec<String>,
    /// The effort to use when the caller does not pick one, as published by the
    /// provider. Codex otherwise falls back to its own global default, which may
    /// be `none` on a model that cannot disable reasoning.
    pub default_reasoning_effort: Option<String>,
    /// The model rejects every attempt to disable reasoning. OX must never send
    /// `effort: "none"`, `enabled: false` or `thinking: {"type":"disabled"}`.
    pub reasoning_mandatory: bool,
    /// API paths the provider says this model serves, when it says so. Used to
    /// hide models that cannot answer the wire this route is configured for.
    pub endpoints: Vec<String>,
    /// `None` when the provider does not say. `Some(false)` hides the model,
    /// because every OX session runs a coding agent that needs tools.
    pub supports_tools: Option<bool>,
}

/// One row of the Codex model catalog.
pub struct CatalogModel {
    pub slug: String,
    pub display_name: String,
    pub description: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub input_modalities: Vec<String>,
    pub reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: Option<String>,
    pub reasoning_mandatory: bool,
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
        for mut spec in models.data {
            if !seen.insert(spec.id.clone()) {
                continue;
            }
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
    fn from_upstream(model: &Value) -> Option<Self> {
        let id = model["id"].as_str()?;
        if id.is_empty() {
            return None;
        }
        if model.pointer("/capabilities/tools/supported") == Some(&Value::Bool(false)) {
            return None;
        }
        // OpenRouter publishes tool support as a `supported_parameters` entry;
        // Model Studio-style vendors publish a boolean capability.
        let supports_tools = match model.get("supported_parameters").and_then(Value::as_array) {
            Some(parameters) => Some(
                parameters
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|parameter| parameter == "tools"),
            ),
            None => model
                .pointer("/capabilities/tools/supported")
                .and_then(Value::as_bool),
        };
        if supports_tools == Some(false) {
            return None;
        }
        let strings = |path: &str| -> Vec<&str> {
            model
                .pointer(path)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect()
        };
        let mut output = strings("/modalities/output");
        output.extend(strings("/architecture/output_modalities"));
        let non_coding = if output.is_empty() {
            matches!(
                model["type"].as_str(),
                Some(
                    "embedding" | "embeddings" | "image" | "audio" | "video" | "speech" | "rerank"
                )
            )
        } else {
            !output.contains(&"text")
        };
        if non_coding {
            return None;
        }
        let limit = |paths: &[&str]| {
            paths
                .iter()
                .filter_map(|path| model.pointer(path)?.as_u64())
                .filter(|tokens| *tokens > 0)
                .min()
        };
        let mut inputs = strings("/modalities/input");
        inputs.extend(strings("/architecture/input_modalities"));
        let mut input_modalities = vec!["text".to_owned()];
        if inputs.iter().any(|input| input.starts_with("image")) {
            input_modalities.push("image".to_owned());
        }
        let (reasoning_efforts, default_reasoning_effort, reasoning_mandatory) =
            reasoning_capabilities(model, &strings("/capabilities/reasoning/effort"));
        Some(Self {
            id: id.to_owned(),
            alias: None,
            name: model["name"]
                .as_str()
                .or_else(|| model["display_name"].as_str())
                .map(str::to_owned),
            context_window: limit(&[
                "/limit/context",
                "/limit/input",
                "/context_length",
                "/max_input_tokens",
                "/context_window",
                "/max_model_len",
                "/n_ctx",
            ]),
            max_output_tokens: limit(&[
                "/limit/output",
                "/max_output",
                "/max_output_tokens",
                "/max_tokens",
                "/top_provider/max_completion_tokens",
            ]),
            input_modalities,
            reasoning_efforts,
            default_reasoning_effort,
            reasoning_mandatory,
            endpoints: strings("/endpoints")
                .into_iter()
                .map(str::to_owned)
                .collect(),
            supports_tools,
        })
    }

    fn from_entry(entry: &ModelEntry) -> Self {
        let mut spec = Self {
            id: entry.id.clone(),
            alias: None,
            name: None,
            context_window: None,
            max_output_tokens: None,
            input_modalities: vec!["text".to_owned()],
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            reasoning_mandatory: false,
            endpoints: Vec::new(),
            supports_tools: None,
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
        if let Some(default) = &entry.default_reasoning_effort {
            self.default_reasoning_effort = Some(default.clone());
        }
        if let Some(mandatory) = entry.reasoning_mandatory {
            self.reasoning_mandatory = mandatory;
        }
    }
}

/// Reads the reasoning capabilities a provider publishes.
///
/// OpenRouter describes them in a `reasoning` object: `supported_efforts` (a
/// descending list, or `null` for "every gateway effort"), `default_effort`,
/// `mandatory` and `supports_max_tokens`. Other vendors such as Alibaba's Model
/// Studio use `capabilities.reasoning.effort`; that list is the fallback.
fn reasoning_capabilities(model: &Value, fallback: &[&str]) -> (Vec<String>, Option<String>, bool) {
    let mandatory = model
        .pointer("/reasoning/mandatory")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut efforts: Vec<String> = match model.pointer("/reasoning/supported_efforts") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        // `null` means every gateway effort is accepted.
        Some(Value::Null) => REASONING_LEVELS
            .iter()
            .map(|level| (*level).to_owned())
            .collect(),
        _ => fallback.iter().map(|level| (*level).to_owned()).collect(),
    };
    efforts.retain(|effort| reasoning_rank(effort).is_some());
    let mut seen = HashSet::new();
    efforts.retain(|effort| seen.insert(effort.clone()));
    // A mandatory model rejects every way of turning reasoning off, so it must
    // not be offered in the catalog either.
    if mandatory {
        efforts.retain(|effort| effort != "none");
    }
    let default = model
        .pointer("/reasoning/default_effort")
        .or_else(|| model.pointer("/capabilities/reasoning/default_effort"))
        .and_then(Value::as_str)
        .filter(|effort| efforts.iter().any(|level| level == effort))
        .map(str::to_owned);
    (efforts, default, mandatory)
}

/// Model Studio's compatibility catalog omits capacity for this snapshot.
pub(crate) fn published_context_window(vendor_url: &reqwest::Url, model: &str) -> Option<u64> {
    // https://www.alibabacloud.com/notice/detail?_p_lc=1&id=2225
    (model == "deepseek-v4.1-flash-1017"
        && vendor_url.scheme() == "https"
        && vendor_url
            .domain()
            .is_some_and(|host| host.ends_with(".aliyuncs.com")))
    .then_some(1_000_000)
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
                max_output_tokens: spec.max_output_tokens,
                input_modalities: spec.input_modalities,
                reasoning_efforts: spec.reasoning_efforts,
                default_reasoning_effort: spec.default_reasoning_effort,
                reasoning_mandatory: spec.reasoning_mandatory,
                apply_patch: false,
            })
            .collect::<Vec<_>>();
        Some(catalog_json(&models, bundled))
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
            .filter(|effort| !(self.reasoning_mandatory && effort.as_str() == "none"))
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
        // Prefer what the provider says it uses by default, then the levels
        // most clients expect. A mandatory model never defaults to `none`.
        let default_effort = self
            .default_reasoning_effort
            .as_deref()
            .filter(|effort| levels.iter().any(|level| level["effort"] == *effort))
            .or_else(|| {
                ["medium", "low", "high", "xhigh", "max", "minimal", "none"]
                    .into_iter()
                    .find(|effort| levels.iter().any(|level| level["effort"] == *effort))
            });

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
        if let Some(tokens) = self.max_output_tokens {
            entry["max_output_tokens"] = json!(tokens);
        }
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
                default_reasoning_effort: Some("high".to_owned()),
                reasoning_mandatory: Some(true),
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
    fn openrouter_reasoning_metadata_drives_the_offered_levels() {
        let discovered = serde_json::from_value::<ProviderModels>(json!({
            "data": [{
                "id": "openai/gpt-6.1-sol",
                "name": "GPT-6.1 Sol",
                "context_length": 1_050_000,
                "reasoning": {
                    "mandatory": true,
                    "default_enabled": true,
                    "supported_efforts": ["max", "xhigh", "high", "medium", "low"],
                    "default_effort": "medium"
                }
            }]
        }))
        .unwrap();
        let specs = merge_models(Some(discovered), &[]);
        assert_eq!(
            specs[0].reasoning_efforts,
            ["max", "xhigh", "high", "medium", "low"]
        );
        assert_eq!(specs[0].default_reasoning_effort.as_deref(), Some("medium"));
        assert!(specs[0].reasoning_mandatory);

        let models = vec![CatalogModel {
            slug: "openrouter/openai/gpt-6.1-sol".to_owned(),
            display_name: "GPT-6.1 Sol".to_owned(),
            description: "OpenRouter".to_owned(),
            context_window: specs[0].context_window,
            max_output_tokens: None,
            input_modalities: specs[0].input_modalities.clone(),
            reasoning_efforts: specs[0].reasoning_efforts.clone(),
            default_reasoning_effort: specs[0].default_reasoning_effort.clone(),
            reasoning_mandatory: specs[0].reasoning_mandatory,
            apply_patch: false,
        }];
        let catalog = catalog_json(&models, &bundled());
        let entry = &catalog["models"][0];
        assert_eq!(entry["default_reasoning_level"], "medium");
        let levels = entry["supported_reasoning_levels"].as_array().unwrap();
        assert!(
            levels.iter().all(|level| level["effort"] != "none"),
            "{levels:?}"
        );
    }

    #[test]
    fn a_mandatory_model_drops_none_even_when_the_provider_lists_it() {
        let discovered = serde_json::from_value::<ProviderModels>(json!({
            "data": [{
                "id": "gpt",
                "reasoning": {
                    "mandatory": true,
                    "supported_efforts": ["high", "low", "none"],
                    "default_effort": "none"
                }
            }]
        }))
        .unwrap();
        let spec = merge_models(Some(discovered), &[]).remove(0);
        assert_eq!(spec.reasoning_efforts, ["high", "low"]);
        assert_eq!(spec.default_reasoning_effort, None, "none is not a default");
    }

    #[test]
    fn a_null_effort_list_means_every_gateway_level() {
        let discovered = serde_json::from_value::<ProviderModels>(json!({
            "data": [{
                "id": "gpt",
                "reasoning": {"mandatory": false, "supported_efforts": null, "default_effort": "low"}
            }]
        }))
        .unwrap();
        let spec = merge_models(Some(discovered), &[]).remove(0);
        assert_eq!(spec.reasoning_efforts, REASONING_LEVELS);
        assert_eq!(spec.default_reasoning_effort.as_deref(), Some("low"));
    }

    #[test]
    fn openrouter_metadata_fills_modalities_output_tools_and_endpoints() {
        let upstream = serde_json::from_value::<ProviderModels>(json!({
            "data": [
                {
                    "id": "openai/gpt-6.1-sol",
                    "name": "GPT-6.1 Sol",
                    "context_length": 1_050_000,
                    "architecture": {
                        "input_modalities": ["file", "image", "text"],
                        "output_modalities": ["text"]
                    },
                    "top_provider": {"context_length": 1_050_000, "max_completion_tokens": 128_000},
                    "supported_parameters": ["tools", "reasoning", "temperature"],
                    "endpoints": ["/api/v1/chat/completions", "/api/v1/responses"]
                },
                {
                    "id": "no-tools",
                    "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                    "supported_parameters": ["temperature"]
                },
                {
                    "id": "chat-only",
                    "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                    "supported_parameters": ["tools"],
                    "endpoints": ["/api/v1/chat/completions"]
                }
            ]
        }))
        .unwrap();
        let specs = merge_models(Some(upstream), &[]);
        assert_eq!(
            specs.len(),
            2,
            "a model without tools is not a coding model"
        );
        assert_eq!(specs[0].input_modalities, ["text", "image"]);
        assert_eq!(specs[0].max_output_tokens, Some(128_000));
        assert_eq!(specs[0].supports_tools, Some(true));
        assert_eq!(specs[0].endpoints.len(), 2);
        assert_eq!(specs[1].id, "chat-only");
    }

    #[test]
    fn models_are_hidden_from_a_wire_they_do_not_serve() {
        let mut upstream = serde_json::from_value::<ProviderModels>(json!({
            "data": [
                {"id": "responses", "endpoints": ["/api/v1/responses", "/api/v1/chat/completions"]},
                {"id": "batch", "endpoints": ["/api/v1/chat/completions"]},
                {"id": "unknown"}
            ]
        }))
        .unwrap();
        upstream.retain_wire(crate::provider::Wire::Responses);
        let ids: Vec<_> = merge_models(Some(upstream), &[])
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(ids, ["responses", "unknown"]);
    }

    #[test]
    fn reasoning_clamping_prefers_the_provider_default_then_the_nearest_level() {
        let supported = ["low", "medium", "high"].map(str::to_owned).to_vec();
        assert_eq!(
            clamp_reasoning_effort("none", &supported, Some("high")).as_deref(),
            Some("high")
        );
        assert_eq!(
            clamp_reasoning_effort("none", &supported, None).as_deref(),
            Some("low")
        );
        assert_eq!(
            clamp_reasoning_effort("max", &supported, None).as_deref(),
            Some("high")
        );
        assert_eq!(clamp_reasoning_effort("medium", &supported, None), None);
        // `medium` sits exactly between `low` and `high`; the cheaper one wins.
        let gap = ["low", "high"].map(str::to_owned).to_vec();
        assert_eq!(
            clamp_reasoning_effort("medium", &gap, None).as_deref(),
            Some("low")
        );
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
    fn invalid_discovered_ids_are_dropped_before_routes_are_built() {
        let mut discovered = serde_json::from_value::<ProviderModels>(json!({
            "data": [
                {"id": "Qwen 2.5 Coder.gguf"},
                {"id": "good-model"},
                {"id": "quoted\"id"}
            ]
        }))
        .unwrap();
        let dropped =
            discovered.retain_ids(|id| crate::provider::alias_for("local", id, None).is_ok());
        assert_eq!(dropped, ["Qwen 2.5 Coder.gguf", "quoted\"id"]);
        let ids = merge_models(Some(discovered), &[])
            .into_iter()
            .map(|spec| spec.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["good-model"]);
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
            max_output_tokens: Some(16384),
            input_modalities: vec!["text".to_owned()],
            reasoning_efforts: vec!["medium".to_owned()],
            default_reasoning_effort: None,
            reasoning_mandatory: false,
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

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
pub struct ProviderModels {
    data: Vec<ProviderModel>,
}

#[derive(Deserialize)]
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

impl ProviderModels {
    pub fn codex_catalog(self, provider_name: &str, bundled: &BundledModels) -> Option<Value> {
        let mut seen = std::collections::HashSet::new();
        let mut models = self
            .data
            .into_iter()
            .filter(|model| model.supports_coding() && seen.insert(model.id.clone()))
            .enumerate()
            .map(|(priority, model)| model.codex_entry(priority, provider_name))
            .collect::<Vec<_>>();
        if models.is_empty() {
            return None;
        }

        // Codex merges remote entries with its built-in catalog by slug.
        // Shadow built-in models that this provider did not actually list.
        models.extend(bundled.models.iter().filter_map(|model| {
            let slug = model.get("slug")?.as_str()?;
            if seen.contains(slug) {
                return None;
            }
            let mut hidden = model.clone();
            hidden["visibility"] = json!("hide");
            hidden["supported_in_api"] = json!(false);
            Some(hidden)
        }));
        Some(json!({ "models": models }))
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

    fn codex_entry(self, priority: usize, provider_name: &str) -> Value {
        let efforts = self
            .capabilities
            .reasoning
            .effort
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
            .find(|effort| efforts.iter().any(|level| level["effort"] == *effort));
        let mut input_modalities = vec!["text"];
        if self
            .modalities
            .input
            .iter()
            .any(|input| input.starts_with("image"))
        {
            input_modalities.push("image");
        }
        let context_window = self
            .limit
            .context
            .into_iter()
            .chain(self.limit.input)
            .filter(|limit| *limit > 0)
            .min();

        json!({
            "slug": self.id,
            "display_name": self.name.unwrap_or_else(|| self.id.clone()),
            "description": provider_name,
            "base_instructions": "Help the user with their coding tasks. Use the available tools to inspect, edit, and verify their work.",
            "default_reasoning_level": default_effort,
            "supported_reasoning_levels": efforts,
            "shell_type": "shell_command",
            "visibility": "list",
            "supported_in_api": true,
            "priority": priority,
            "support_verbosity": false,
            "supports_parallel_tool_calls": false,
            "truncation_policy": { "mode": "tokens", "limit": 10000 },
            "experimental_supported_tools": [],
            "input_modalities": input_modalities,
            "context_window": context_window
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let bundled = serde_json::from_value::<BundledModels>(json!({
            "models": [
                {"slug": "demo-coding", "visibility": "list"},
                {"slug": "bundled-model", "visibility": "list"}
            ]
        }))
        .unwrap();

        let catalog = upstream.codex_catalog("Demo Provider", &bundled).unwrap();
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["display_name"], "Demo Coding");
        assert_eq!(models[0]["visibility"], "list");
        assert_eq!(models[1]["slug"], "bundled-model");
        assert_eq!(models[1]["visibility"], "hide");
        assert_eq!(models[1]["supported_in_api"], false);
    }
}

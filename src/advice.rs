//! Provider advice: what OX had to work around, and the safe configuration fix.
//!
//! The relay records one record per provider/model quirk it corrected. The same
//! file is read by `ox doctor` and by OX Desktop, so a fix never requires a
//! terminal: Desktop asks the shared `ox` helper to apply it, and the helper
//! uses the same locked, atomic writes as `ox setup`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table};

use crate::provider::RelayError;

const FILE: &str = "ox-advice.json";
const MAX_RECORDS: usize = 32;
const MAX_BYTES: u64 = 64 * 1024;

/// A safe, reversible change to the user's provider configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fix {
    /// Remove the relay Codex profile's global `model_reasoning_effort`, so a
    /// choice made for one model cannot follow the user to another.
    ClearRelayReasoningEffort,
    /// Record a model's reasoning default (and whether reasoning is mandatory)
    /// in `codeox.toml`, so the catalog never has to guess again.
    SetModelReasoning {
        provider: String,
        model: String,
        effort: String,
        mandatory: bool,
    },
    /// Point an OpenRouter provider at the in-region host its key may use.
    UseRegionalHost { provider: String, region: String },
}

impl Advice {
    /// Keeps a hand-edited or provider-derived record bounded.
    fn limited(mut self) -> Self {
        self.id = self.id.chars().take(256).collect();
        self.message = self.message.chars().take(1000).collect();
        if let Some(model) = &mut self.model {
            *model = model.chars().take(200).collect();
        }
        self.provider = self.provider.chars().take(64).collect();
        self
    }
}

impl Fix {
    /// One line describing the change, for the desktop Fix button.
    pub fn label(&self) -> String {
        match self {
            Self::ClearRelayReasoningEffort => {
                "stop forcing one reasoning effort for every API model".to_owned()
            }
            Self::SetModelReasoning {
                provider,
                model,
                effort,
                mandatory,
            } => format!(
                "record {model} on {provider} as {effort}{}",
                if *mandatory {
                    " with mandatory reasoning"
                } else {
                    ""
                }
            ),
            Self::UseRegionalHost { provider, region } => {
                format!("use {region}.openrouter.ai for {provider}")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advice {
    /// Stable identity: the same condition is recorded once.
    pub id: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub message: String,
    pub fix: Fix,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    advice: Vec<Advice>,
}

pub fn path(home: &Path) -> PathBuf {
    home.join(FILE)
}

/// Reads the recorded advice. A missing or unreadable file is empty, never an
/// error: advice is a convenience, not a startup dependency.
pub fn read(home: &Path) -> Vec<Advice> {
    let Ok(bytes) = std::fs::read(path(home)) else {
        return Vec::new();
    };
    if bytes.len() as u64 > MAX_BYTES {
        return Vec::new();
    }
    serde_json::from_slice::<File>(&bytes)
        .map(|file| file.advice)
        .unwrap_or_default()
}

/// Records advice, replacing an older record with the same id. Returns true when
/// this is the first time the condition was seen.
pub fn record(home: &Path, advice: Advice) -> bool {
    let _lock = match crate::setup::lock_config(home) {
        Ok(lock) => lock,
        Err(_) => return false,
    };
    let mut records = read(home);
    if records.iter().any(|existing| existing.id == advice.id) {
        return false;
    }
    records.push(advice.limited());
    if records.len() > MAX_RECORDS {
        let excess = records.len() - MAX_RECORDS;
        records.drain(0..excess);
    }
    let text = serde_json::to_string_pretty(&File { advice: records });
    match text {
        Ok(text) => crate::setup::write_config_atomic(&path(home), &text).is_ok(),
        Err(_) => false,
    }
}

pub fn remove(home: &Path, id: &str) -> Result<(), RelayError> {
    let _lock = crate::setup::lock_config(home)?;
    let records = read(home);
    let kept: Vec<_> = records.iter().filter(|record| record.id != id).collect();
    if kept.len() == records.len() {
        return Ok(());
    }
    let text = serde_json::to_string_pretty(&File {
        advice: kept.into_iter().cloned().collect(),
    })?;
    crate::setup::write_config_atomic(&path(home), &text)
}

/// Applies one advice record and forgets it. Only the documented safe changes
/// are performed; nothing else in the file is reformatted.
pub fn apply(home: &Path, id: &str) -> Result<Fix, RelayError> {
    let advice = read(home)
        .into_iter()
        .find(|advice| advice.id == id)
        .ok_or("that advice is no longer recorded; reload the provider page")?;
    match &advice.fix {
        Fix::ClearRelayReasoningEffort => {
            let config = home.join("ox-clients/codex/config.toml");
            let _lock = crate::setup::lock_config(home)?;
            if let Some(text) = crate::setup::read_optional_text_limited(&config, "Codex profile")?
                && let Some(updated) = strip_key(&text, "model_reasoning_effort")?
            {
                crate::setup::write_config_atomic(&config, &updated)?;
            }
        }
        Fix::SetModelReasoning {
            provider,
            model,
            effort,
            mandatory,
        } => {
            if crate::catalog::reasoning_rank(effort).is_none() {
                return Err(format!("{effort:?} is not a reasoning effort").into());
            }
            let config = crate::provider::CodeoxConfig::path(home);
            let _lock = crate::setup::lock_config(home)?;
            let text = crate::setup::read_optional_text_limited(&config, "codeox.toml")?
                .ok_or("codeox.toml is missing; run `ox setup`")?;
            let updated = set_model_reasoning(&text, provider, model, effort, *mandatory)?;
            crate::setup::write_config_atomic(&config, &updated)?;
        }
        Fix::UseRegionalHost { provider, region } => {
            if !matches!(region.as_str(), "eu" | "us") {
                return Err(format!("{region:?} is not an OpenRouter data region").into());
            }
            let config = crate::provider::CodeoxConfig::path(home);
            let _lock = crate::setup::lock_config(home)?;
            let text = crate::setup::read_optional_text_limited(&config, "codeox.toml")?
                .ok_or("codeox.toml is missing; run `ox setup`")?;
            // Saved keys are scoped to their destination, so the move has to
            // take the credential with it or the route looks keyless.
            let before = crate::provider::CodeoxConfig::load_from_str(&text)?;
            let vendor = before
                .vendors
                .iter()
                .find(|vendor| vendor.key == *provider)
                .ok_or_else(|| format!("codeox.toml has no providers.{provider} table"))?;
            // Advice can outlive the provider entry it was recorded for. Moving
            // a credential to OpenRouter after the entry was repointed would
            // send the wrong service's key there.
            if vendor.base_url.host_str() != Some("openrouter.ai")
                || !crate::provider::is_openrouter_url(&vendor.base_url)
            {
                return Err(format!(
                    "providers.{provider} no longer points at the global OpenRouter route; discard this advice"
                )
                .into());
            }
            let old_base = vendor.base_url.as_str().to_owned();
            let env_key = vendor.env_key.clone();
            // A saved key is valid only while every provider that references it
            // shares one destination. Moving one of two providers would leave
            // the others unreachable, so refuse and let the user split them.
            if let Some(env_key) = &env_key
                && before.vendors.iter().any(|vendor| {
                    vendor.key != *provider && vendor.env_key.as_deref() == Some(env_key.as_str())
                })
            {
                return Err(format!(
                    "providers.{provider} shares {env_key} with another provider; give each route its own env_key (or update both by hand) before moving to {region}.openrouter.ai"
                )
                .into());
            }
            let base = format!("https://{region}.openrouter.ai/api/v1");
            let updated = set_provider_base(&text, provider, &base)?;
            crate::setup::write_config_atomic(&config, &updated)?;
            if let Some(env_key) = env_key {
                let mut keys = crate::provider::SavedKeys::load(home)?;
                if let Some(text) = keys.rescope(&env_key, &old_base, &base)? {
                    crate::setup::write_config_atomic(
                        &crate::provider::SavedKeys::path(home),
                        &text,
                    )?;
                }
            }
        }
    }
    remove(home, id)?;
    Ok(advice.fix)
}

/// Removes one top-level key from a TOML document, keeping everything else
/// byte-for-byte where the parser allows it. `None` means the key was absent.
fn strip_key(text: &str, key: &str) -> Result<Option<String>, RelayError> {
    let mut document: DocumentMut = text
        .parse()
        .map_err(|_| "could not parse the Codex profile config.toml")?;
    if document.remove(key).is_none() {
        return Ok(None);
    }
    Ok(Some(document.to_string()))
}

fn set_model_reasoning(
    text: &str,
    provider: &str,
    model: &str,
    effort: &str,
    mandatory: bool,
) -> Result<String, RelayError> {
    let mut document: DocumentMut = text.parse().map_err(|_| "could not parse codeox.toml")?;
    let providers = document
        .get_mut("providers")
        .and_then(Item::as_table_mut)
        .ok_or("codeox.toml has no [providers] table")?;
    let provider_table = providers
        .get_mut(provider)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| format!("codeox.toml has no providers.{provider} table"))?;
    if !provider_table.contains_key("models") {
        provider_table["models"] = Item::ArrayOfTables(Default::default());
    }
    let models = provider_table
        .get_mut("models")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| {
            format!("providers.{provider}.models must be [[providers.{provider}.models]] tables")
        })?;
    let existing = models
        .iter()
        .position(|entry| entry.get("id").and_then(Item::as_str) == Some(model));
    let index = match existing {
        Some(index) => index,
        None => {
            let mut entry = Table::new();
            entry["id"] = toml_edit::value(model);
            models.push(entry);
            models.len() - 1
        }
    };
    let entry = models.get_mut(index).expect("the entry exists");
    entry["default_reasoning_effort"] = toml_edit::value(effort);
    entry["reasoning_mandatory"] = toml_edit::value(mandatory);
    Ok(document.to_string())
}

fn set_provider_base(text: &str, provider: &str, base: &str) -> Result<String, RelayError> {
    let mut document: DocumentMut = text.parse().map_err(|_| "could not parse codeox.toml")?;
    let providers = document
        .get_mut("providers")
        .and_then(Item::as_table_mut)
        .ok_or("codeox.toml has no [providers] table")?;
    let provider_table = providers
        .get_mut(provider)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| format!("codeox.toml has no providers.{provider} table"))?;
    provider_table["base_url"] = toml_edit::value(base);
    // The default is derived from base_url; a stale explicit URL would keep
    // discovery on the old host.
    provider_table.remove("models_url");
    Ok(document.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let home = std::env::temp_dir().join(format!("ox-advice-{tag}-{}", hex::encode(bytes)));
        std::fs::create_dir_all(home.join("ox-clients/codex")).unwrap();
        home
    }

    fn advice(id: &str, fix: Fix) -> Advice {
        Advice {
            id: id.to_owned(),
            provider: "openrouter".to_owned(),
            model: Some("openai/gpt-6.1-sol".to_owned()),
            message: "reasoning cannot be disabled".to_owned(),
            fix,
        }
    }

    #[test]
    fn advice_is_recorded_once_and_read_back() {
        let home = temp_home("record");
        let entry = advice("clear-reasoning", Fix::ClearRelayReasoningEffort);
        assert!(record(&home, entry.clone()));
        assert!(
            !record(&home, entry.clone()),
            "the same condition is not repeated"
        );
        assert_eq!(read(&home), vec![entry]);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn clearing_the_global_effort_keeps_the_rest_of_the_profile() {
        let text = "model = \"openrouter/openai/gpt-6.1-sol\"\nmodel_reasoning_effort = \"none\"\n\n[tui]\nsidebar_width_percent = 20\n";
        let updated = strip_key(text, "model_reasoning_effort").unwrap().unwrap();
        assert!(!updated.contains("model_reasoning_effort"));
        assert!(updated.contains("sidebar_width_percent = 20"));
        assert_eq!(
            strip_key("model = \"x\"\n", "model_reasoning_effort").unwrap(),
            None
        );
    }

    #[test]
    fn setting_model_reasoning_adds_a_static_entry_or_updates_one() {
        let text = r#"
[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"

[[providers.openrouter.models]]
id = "openai/gpt-6.1-sol"
reasoning_efforts = ["low", "high"]
"#;
        let updated =
            set_model_reasoning(text, "openrouter", "openai/gpt-6.1-sol", "high", true).unwrap();
        assert!(updated.contains("default_reasoning_effort = \"high\""));
        assert!(updated.contains("reasoning_mandatory = true"));

        let updated =
            set_model_reasoning(text, "openrouter", "openai/gpt-6.2-astra", "medium", false)
                .unwrap();
        assert!(updated.contains("id = \"openai/gpt-6.2-astra\""));
        assert!(updated.contains("default_reasoning_effort = \"medium\""));
    }

    #[test]
    fn applying_an_advice_writes_the_file_and_forgets_the_record() {
        let home = temp_home("apply");
        std::fs::write(
            home.join("ox-clients/codex/config.toml"),
            "model = \"m\"\nmodel_reasoning_effort = \"none\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join("codeox.toml"),
            "[providers.openrouter]\nbase_url = \"https://openrouter.ai/api/v1\"\n\n[[providers.openrouter.models]]\nid = \"openai/gpt-6.1-sol\"\n",
        )
        .unwrap();
        record(
            &home,
            advice(
                "model-reasoning:openrouter:openai/gpt-6.1-sol",
                Fix::SetModelReasoning {
                    provider: "openrouter".to_owned(),
                    model: "openai/gpt-6.1-sol".to_owned(),
                    effort: "high".to_owned(),
                    mandatory: true,
                },
            ),
        );
        apply(&home, "model-reasoning:openrouter:openai/gpt-6.1-sol").unwrap();
        let config = std::fs::read_to_string(home.join("codeox.toml")).unwrap();
        assert!(config.contains("default_reasoning_effort = \"high\""));
        assert!(config.contains("reasoning_mandatory = true"));

        record(
            &home,
            advice("clear-reasoning", Fix::ClearRelayReasoningEffort),
        );
        apply(&home, "clear-reasoning").unwrap();
        let profile = std::fs::read_to_string(home.join("ox-clients/codex/config.toml")).unwrap();
        assert!(!profile.contains("model_reasoning_effort"));
        assert!(read(&home).is_empty());

        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_hand_edited_fix_cannot_write_an_unknown_region_or_effort() {
        let home = temp_home("validate");
        std::fs::write(
            home.join("codeox.toml"),
            "[providers.openrouter]\nbase_url = \"https://openrouter.ai/api/v1\"\n",
        )
        .unwrap();
        record(
            &home,
            advice(
                "region",
                Fix::UseRegionalHost {
                    provider: "openrouter".to_owned(),
                    region: "evil.example".to_owned(),
                },
            ),
        );
        assert!(apply(&home, "region").is_err());
        record(
            &home,
            advice(
                "effort",
                Fix::SetModelReasoning {
                    provider: "openrouter".to_owned(),
                    model: "m".to_owned(),
                    effort: "extreme".to_owned(),
                    mandatory: false,
                },
            ),
        );
        assert!(apply(&home, "effort").is_err());
        let config = std::fs::read_to_string(home.join("codeox.toml")).unwrap();
        assert!(config.contains("https://openrouter.ai/api/v1"), "{config}");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_regional_fix_is_refused_when_another_route_shares_the_key() {
        let home = temp_home("shared-key");
        std::fs::write(
            home.join("codeox.toml"),
            "[providers.openrouter]\nbase_url = \"https://openrouter.ai/api/v1\"\nenv_key = \"OPENROUTER_API_KEY\"\n\n[providers.openrouter-chat]\nbase_url = \"https://openrouter.ai/api/v1\"\nenv_key = \"OPENROUTER_API_KEY\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join("ox-api-keys.json"),
            r#"{"OPENROUTER_API_KEY":{"scope":"https://openrouter.ai/api/v1","value":"sk-test"}}"#,
        )
        .unwrap();
        record(
            &home,
            advice(
                "region",
                Fix::UseRegionalHost {
                    provider: "openrouter".to_owned(),
                    region: "eu".to_owned(),
                },
            ),
        );
        let error = apply(&home, "region").unwrap_err().to_string();
        assert!(error.contains("shares OPENROUTER_API_KEY"), "{error}");
        let config = std::fs::read_to_string(home.join("codeox.toml")).unwrap();
        assert!(!config.contains("eu.openrouter.ai"), "{config}");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_stale_regional_fix_is_refused_after_the_provider_changed() {
        let home = temp_home("stale-region");
        std::fs::write(
            home.join("codeox.toml"),
            "[providers.openrouter]\nbase_url = \"https://other.example/v1\"\nenv_key = \"OTHER_API_KEY\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join("ox-api-keys.json"),
            r#"{"OTHER_API_KEY":{"scope":"https://other.example/v1","value":"sk-other"}}"#,
        )
        .unwrap();
        record(
            &home,
            advice(
                "region",
                Fix::UseRegionalHost {
                    provider: "openrouter".to_owned(),
                    region: "eu".to_owned(),
                },
            ),
        );
        assert!(apply(&home, "region").is_err());
        let config = std::fs::read_to_string(home.join("codeox.toml")).unwrap();
        assert!(config.contains("https://other.example/v1"), "{config}");
        let keys = std::fs::read_to_string(home.join("ox-api-keys.json")).unwrap();
        assert!(keys.contains("https://other.example/v1"), "{keys}");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_regional_fix_moves_a_saved_key_to_the_new_destination() {
        let home = temp_home("rescope");
        std::fs::write(
            home.join("codeox.toml"),
            "[providers.openrouter]\nbase_url = \"https://openrouter.ai/api/v1\"\nenv_key = \"OPENROUTER_API_KEY\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join("ox-api-keys.json"),
            r#"{"OPENROUTER_API_KEY":{"scope":"https://openrouter.ai/api/v1","value":"sk-test"}}"#,
        )
        .unwrap();
        record(
            &home,
            advice(
                "region",
                Fix::UseRegionalHost {
                    provider: "openrouter".to_owned(),
                    region: "eu".to_owned(),
                },
            ),
        );
        apply(&home, "region").unwrap();
        let keys = std::fs::read_to_string(home.join("ox-api-keys.json")).unwrap();
        assert!(keys.contains("https://eu.openrouter.ai/api/v1"), "{keys}");
        let config = crate::provider::CodeoxConfig::load(&home.join("codeox.toml")).unwrap();
        let saved = crate::provider::SavedKeys::for_config(&home, &config).unwrap();
        assert!(
            saved.scopes().any(|(name, _)| name == "OPENROUTER_API_KEY"),
            "the key must still resolve after the move"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_regional_fix_moves_the_base_url_and_drops_a_stale_models_url() {
        let text = r#"
[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
models_url = "https://openrouter.ai/api/v1/models/user"
"#;
        let updated =
            set_provider_base(text, "openrouter", "https://eu.openrouter.ai/api/v1").unwrap();
        assert!(updated.contains("https://eu.openrouter.ai/api/v1"));
        assert!(!updated.contains("models_url"));
    }
}

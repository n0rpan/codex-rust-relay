//! Edit only the selected legacy relay's model and connection settings. Never send
//! the native Codex configuration (which can contain headers and tokens) to the UI.
use super::{Document, RelayError, Value, fs, setup};
use crate::provider::{MAX_CONFIG_BYTES, Provider};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table, value};

pub(super) const ID: &str = "config.toml";
const FIELDS: [&str; 5] = [
    "name",
    "base_url",
    "model_catalog_url",
    "env_key",
    "wire_api",
];
const PRIVATE: &str = "<existing private value>";

fn source(home: &Path) -> Result<Option<(String, DocumentMut, String)>, RelayError> {
    let Some(raw) = setup::read_optional_text_limited(&home.join(ID), ID)? else {
        return Ok(None);
    };
    let config: DocumentMut = raw
        .parse()
        .map_err(|_| "Could not parse Codex config.toml.")?;
    let Some(id) = config.get("model_provider").and_then(Item::as_str) else {
        return Ok(None);
    };
    if !id
        .bytes()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_'))
        || id.is_empty()
        || crate::provider::looks_like_credential(id)
    {
        return Ok(None);
    }
    if config
        .get("model_providers")
        .and_then(|p| p.get(id))
        .and_then(Item::as_table_like)
        .is_none()
    {
        return Ok(None);
    }
    let id = id.to_owned();
    Ok(Some((raw, config, id)))
}

fn display(field: &str, text: &str) -> String {
    let private = crate::provider::looks_like_credential(text)
        || match field {
            "base_url" | "model_catalog_url" => reqwest::Url::parse(text).map_or(true, |u| {
                !u.username().is_empty()
                    || u.password().is_some()
                    || u.query().is_some()
                    || u.fragment().is_some()
            }),
            _ => false,
        };
    if private {
        PRIVATE.to_owned()
    } else {
        text.to_owned()
    }
}

pub(super) fn projection(
    home: &Path,
) -> Result<Option<(Document, String, Option<String>)>, RelayError> {
    let Some((raw, cfg, id)) = source(home)? else {
        return Ok(None);
    };
    let mut out = DocumentMut::new();
    let model = cfg
        .get("model")
        .and_then(Item::as_str)
        .map(|v| display("model", v));
    if let Some(model) = &model {
        out["model"] = value(model);
    }
    let mut table = Table::new();
    for field in FIELDS {
        if let Some(v) = cfg["model_providers"][&id]
            .get(field)
            .and_then(Item::as_str)
        {
            table.insert(field, value(display(field, v)));
        }
    }
    out["provider"] = Item::Table(table);
    let revision =
        hex::encode(ring::digest::digest(&ring::digest::SHA256, raw.as_bytes()).as_ref());
    Ok(Some((
        Document {
            id: ID.into(),
            title: format!("Legacy · {id}"),
            revision,
            text: out.to_string(),
        },
        id,
        model.filter(|s| s != PRIVATE),
    )))
}

pub(super) fn key_destination(home: &Path) -> Result<Option<(String, String)>, RelayError> {
    let Some((raw, _, _)) = source(home)? else {
        return Ok(None);
    };
    let mut destination = None;
    Provider::from_config(&raw, |name, scope| {
        destination = Some((name.to_owned(), scope.to_owned()));
        Ok(Some("validation-only".into()))
    })?;
    Ok(destination)
}

pub(super) fn save(home: &Path, revision: &str, text: &str) -> Result<(), RelayError> {
    let Some((raw, mut config, id)) = source(home)? else {
        return Err("The legacy provider is no longer selected.".into());
    };
    if home.join(ID).is_symlink() {
        return Err("Edit the target of this linked config.toml directly.".into());
    }
    if revision != hex::encode(ring::digest::digest(&ring::digest::SHA256, raw.as_bytes()).as_ref())
    {
        return Err("Codex settings changed on disk. Reload before saving.".into());
    }
    let draft: DocumentMut = text
        .parse()
        .map_err(|_| "Invalid TOML in legacy settings.")?;
    let provider = draft
        .get("provider")
        .and_then(Item::as_table_like)
        .ok_or("Include a [provider] table.")?;
    if draft
        .iter()
        .any(|(f, _)| !matches!(f, "model" | "provider"))
        || provider.iter().any(|(f, _)| !FIELDS.contains(&f))
    {
        return Err("Use only the model and legacy connection fields in this editor.".into());
    }
    if let Some(model) = draft.get("model") {
        let model = model.as_str().ok_or("Expected a model ID.")?;
        if model != PRIVATE {
            config["model"] = value(model)
        }
    } else {
        config.remove("model");
    }
    let table = config["model_providers"][&id]
        .as_table_like_mut()
        .ok_or("The selected provider is invalid.")?;
    for f in FIELDS {
        if let Some(item) = provider.get(f) {
            let v = item
                .as_str()
                .ok_or("Expected a text value in legacy provider settings.")?;
            if v != PRIVATE {
                if matches!(f, "base_url" | "model_catalog_url") && display(f, v) == PRIVATE {
                    return Err("Enter an endpoint URL without embedded credentials, a query or a fragment.".into());
                }
                if f == "wire_api" && v != "responses" {
                    return Err("The legacy relay requires the Responses API.".into());
                }
                table.insert(f, value(v));
            }
        } else {
            table.remove(f);
        }
    }
    let merged = config.to_string();
    if merged.len() as u64 > MAX_CONFIG_BYTES {
        return Err("Codex configuration exceeds the size limit.".into());
    }
    Provider::from_config(&merged, |_, _| Ok(Some("validation-only".into())))?
        .ok_or("Invalid legacy provider settings.")?;
    // A native CLI can update its config without holding ox's settings lock.
    if fs::read_to_string(home.join(ID))? != raw {
        return Err("Codex settings changed while saving. Reload and try again.".into());
    }
    setup::write_config_atomic(&home.join(ID), &merged)
}

pub(super) async fn models(home: &Path) -> Result<Value, RelayError> {
    let Some(provider) = Provider::load_if_configured(home, &crate::provider::process_env)? else {
        return Ok(serde_json::json!([]));
    };
    let response = crate::upstream_client()?
        .get(provider.models_url.clone())
        .header(
            reqwest::header::AUTHORIZATION,
            provider.upstream_auth.clone(),
        )
        .timeout(crate::CATALOG_TIMEOUT)
        .send()
        .await
        .map_err(|_| "Legacy model discovery failed.")?;
    if !response.status().is_success() {
        return Err(format!(
            "Legacy model discovery returned HTTP {}.",
            response.status()
        )
        .into());
    }
    let bytes = crate::read_upstream(response, crate::MAX_CATALOG_BYTES).await?;
    let models: crate::ProviderModels =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid legacy model catalog.")?;
    let catalog = models
        .legacy_catalog(&provider.name, &crate::BundledModels::default())
        .ok_or("Empty legacy model catalog.")?;
    let mut models = catalog["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["visibility"] == "list")
        .filter_map(|v| v["slug"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    super::prioritize(&mut models, home);
    Ok(serde_json::json!(models))
}

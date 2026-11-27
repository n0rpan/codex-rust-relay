//! Translates the public rows and edits at the OX storage boundary.
use super::*;
use crate::settings_protocol::model::*;
use toml_edit::{Array, DocumentMut, Item, Table, value};

fn parse(raw: &str) -> Result<DocumentMut, RelayError> {
    raw.parse()
        .map_err(|_| "Could not parse settings. Correct the file and reload.".into())
}
fn private(key: Key, s: &str) -> bool {
    crate::provider::looks_like_credential(s)
        || s == "<existing private value>"
        || matches!(key, Key::BaseUrl | Key::ModelsUrl | Key::ModelCatalogUrl)
            && reqwest::Url::parse(s).map_or(true, |u| {
                !u.username().is_empty()
                    || u.password().is_some()
                    || u.query().is_some()
                    || u.fragment().is_some()
            })
}
fn cell(key: Key, item: &Item) -> Option<Cell> {
    if let Some(s) = item.as_str() {
        Some(if private(key, s) {
            Cell::Private
        } else {
            Cell::Text(s.to_string())
        })
    } else if let Some(n) = item.as_integer() {
        Some(Cell::Integer(n))
    } else if let Some(b) = item.as_bool() {
        Some(Cell::Bool(b))
    } else {
        item.as_array().map(|a| {
            let strings: Vec<_> = a
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_owned)
                .collect();
            if strings.iter().any(|s| private(key, s)) {
                Cell::Private
            } else {
                Cell::List(strings)
            }
        })
    }
}
fn values(table: &Table, keys: &[Key]) -> Values {
    keys.iter()
        .filter_map(|&k| Some((k, cell(k, table.get(k.name())?)?)))
        .collect()
}
pub(super) fn registry(raw: &str) -> Result<Registry, RelayError> {
    let doc = parse(raw)?;
    let mut out = Registry::default();
    if let Some(item) = doc.get("providers") {
        for (id, item) in item.as_table().ok_or("Invalid providers table")?.iter() {
            if crate::provider::looks_like_credential(id) {
                return Err("Invalid provider ID".into());
            }
            let p = item.as_table().ok_or("Invalid provider entry")?;
            let models = match p.get("models") {
                None => Vec::new(),
                Some(m) => m
                    .as_array_of_tables()
                    .ok_or("Invalid model table")?
                    .iter()
                    .map(|m| values(m, MODEL_KEYS))
                    .collect(),
            };
            out.providers.push(ProviderRow {
                id: id.into(),
                fields: values(p, PROVIDER_KEYS),
                models,
            });
        }
    }
    if let Some(item) = doc.get("search") {
        out.search = values(
            item.as_table().ok_or("Invalid search settings")?,
            SEARCH_KEYS,
        );
        out.search.entry(Key::Enabled).or_insert(Cell::Bool(true));
    }
    Ok(out)
}

pub(super) fn key_revision(home: &Path) -> Result<String, RelayError> {
    // Hash the canonical store directly, without creating another copy of its credentials.
    struct Hash(ring::digest::Context);
    impl Write for Hash {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.update(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut hash = Hash(ring::digest::Context::new(&ring::digest::SHA256));
    serde_json::to_writer(&mut hash, &SavedKeys::load(home)?)?;
    Ok(hex::encode(hash.0.finish().as_ref()))
}
pub(super) fn read(home: &Path) -> Result<Snapshot, RelayError> {
    let (s, legacy_id) = snapshot(home)?;
    let mut out = Snapshot {
        version: s.version,
        api_provider: s.api_provider,
        chatgpt_account: s.chatgpt_account,
        claude_account: s.claude_account,
        credentials: s.credentials,
        models: s.models,
        ..Snapshot::default()
    };
    for doc in s.documents {
        match doc.id.as_str() {
            REGISTRY => {
                out.revisions.registry = doc.revision;
                if let Ok(config) = crate::provider::CodeoxConfig::load_from_str(&doc.text) {
                    out.yolo_grant_projects = config.yolo_grant_projects;
                }
                match registry(&doc.text) {
                    Ok(r) => out.registry = Some(r),
                    Err(_) => out.issues.push(
                        "Could not read provider settings. Correct the provider file and reload."
                            .into(),
                    ),
                }
            }
            FAVORITES => {
                out.revisions.favorites = doc.revision;
                match serde_json::from_str::<Vec<String>>(&doc.text) {
                    Ok(f) => out.favorites = f,
                    Err(_) => out
                        .issues
                        .push("Could not read favorites. Save a new list to replace them.".into()),
                }
            }
            legacy::ID => {
                out.revisions.legacy = doc.revision;
                let raw = parse(&doc.text)?;
                let table = raw["provider"]
                    .as_table()
                    .ok_or("Invalid legacy provider")?;
                let id = legacy_id.as_deref().ok_or("Legacy provider unavailable")?;
                out.legacy = Some(Legacy {
                    id: id.into(),
                    model: raw.get("model").and_then(|m| cell(Key::Model, m)),
                    fields: values(table, LEGACY_KEYS),
                });
            }
            _ => {}
        }
    }
    out.revisions.keys = key_revision(home)?;
    for (name, template) in s.presets {
        if let Some(row) = registry(&template)?.providers.into_iter().next() {
            out.presets.push((name, row))
        }
    }
    Ok(out)
}
fn patch(table: &mut Table, changes: Changes, allowed: &[Key]) -> Result<(), RelayError> {
    for (k, v) in changes {
        if !allowed.contains(&k) {
            return Err("This field is not available for this setting.".into());
        }
        let item = match v {
            None => {
                table.remove(k.name());
                continue;
            }
            Some(Cell::Private) => {
                return Err("Private fields may be left unchanged or replaced.".into());
            }
            Some(Cell::Text(s)) => value(s),
            Some(Cell::Bool(b)) => value(b),
            Some(Cell::Integer(i)) => value(i),
            Some(Cell::List(list)) => value(list.into_iter().collect::<Array>()),
        };
        table.insert(k.name(), item);
    }
    Ok(())
}
fn edit_models(p: &mut Table, changes: Vec<RowEdit>) -> Result<(), RelayError> {
    if changes.is_empty() {
        return Ok(());
    }
    let mut models = p
        .remove("models")
        .map(|i| i.into_array_of_tables().map_err(|_| "Invalid model table"))
        .transpose()?
        .unwrap_or_default();
    for change in changes {
        let index = change
            .original
            .as_deref()
            .map(|id| {
                models
                    .iter()
                    .position(|t| t.get("id").and_then(Item::as_str) == Some(id))
                    .ok_or("Model changed on disk. Review the current model list.")
            })
            .transpose()?;
        if let Some(id) = change.id {
            let duplicate = models
                .iter()
                .enumerate()
                .any(|(i, t)| Some(i) != index && t.get("id").and_then(Item::as_str) == Some(&id));
            if duplicate {
                return Err("A model with that ID already exists.".into());
            }
            let t = if let Some(i) = index {
                models.get_mut(i).unwrap()
            } else {
                models.push(Table::new());
                let i = models.len() - 1;
                models.get_mut(i).unwrap()
            };
            patch(t, change.fields, MODEL_KEYS)?;
            if t.get("id").and_then(Item::as_str) != Some(&id) {
                t["id"] = value(id)
            }
        } else if let Some(i) = index {
            models.remove(i);
        } else {
            return Err("Missing model ID".into());
        }
    }
    if !models.is_empty() {
        p["models"] = Item::ArrayOfTables(models);
    }
    Ok(())
}
fn edit_providers(doc: &mut DocumentMut, changes: Vec<ProviderEdit>) -> Result<(), RelayError> {
    let mut table = doc
        .remove("providers")
        .map(|i| i.into_table().map_err(|_| "Invalid providers table"))
        .transpose()?
        .unwrap_or_else(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            t
        });
    for change in changes {
        let row = change.row;
        let existing = row
            .original
            .as_ref()
            .map(|old| {
                table
                    .remove(old)
                    .ok_or("Provider changed on disk. Review the current provider list.")
            })
            .transpose()?;
        if let Some(id) = row.id {
            if table.contains_key(&id) {
                return Err("A provider with that ID already exists.".into());
            }
            let mut p = existing
                .map(|i| i.into_table().map_err(|_| "Invalid provider"))
                .transpose()?
                .unwrap_or_default();
            patch(&mut p, row.fields, PROVIDER_KEYS)?;
            edit_models(&mut p, change.models)?;
            table.insert(&id, Item::Table(p));
        } else if row.original.is_none() {
            return Err("Missing provider ID".into());
        }
    }
    doc["providers"] = Item::Table(table);
    Ok(())
}
pub(super) fn apply(home: &Path, revision: &str, edit: Edit) -> Result<Outcome, RelayError> {
    let id = match &edit {
        Edit::Providers { .. } | Edit::Search { .. } => REGISTRY,
        Edit::Favorites { .. } => FAVORITES,
        Edit::Legacy { .. } => legacy::ID,
    };
    let (current, legacy_id) = if id == legacy::ID {
        let (doc, id, _) =
            legacy::projection(home)?.ok_or("The legacy provider is no longer selected.")?;
        (doc, Some(id))
    } else {
        (document(home, id)?, None)
    };
    if current.revision != revision {
        return Ok(Outcome::ChangedOnDisk {
            revision: current.revision,
        });
    }
    match edit {
        Edit::Favorites { models } => {
            save_document(home, id, &serde_json::to_string_pretty(&models)?)?
        }
        Edit::Legacy { id, mut fields } => {
            if legacy_id.as_ref() != Some(&id) {
                return Err("The selected legacy provider changed.".into());
            }
            let mut raw = parse(&current.text)?;
            if let Some(model) = fields.remove(&Key::Model) {
                patch(
                    raw.as_table_mut(),
                    [(Key::Model, model)].into(),
                    &[Key::Model],
                )?;
            }
            patch(
                raw["provider"]
                    .as_table_mut()
                    .ok_or("Invalid legacy provider")?,
                fields,
                LEGACY_KEYS,
            )?;
            legacy::save(home, revision, &raw.to_string())?;
        }
        edit => {
            let mut raw = parse(&current.text)?;
            match edit {
                Edit::Providers { changes } => edit_providers(&mut raw, changes)?,
                Edit::Search { fields } => {
                    if !fields.is_empty() {
                        if raw.get("search").is_none() {
                            raw["search"] = Item::Table(Table::new());
                            raw["search"]["enabled"] = value(false);
                        }
                        patch(
                            raw["search"]
                                .as_table_mut()
                                .ok_or("Invalid search configuration")?,
                            fields,
                            SEARCH_KEYS,
                        )?
                    }
                }
                _ => unreachable!(),
            }
            let empty = raw
                .get("providers")
                .and_then(Item::as_table)
                .is_none_or(Table::is_empty);
            if empty
                && raw
                    .get("search")
                    .is_some_and(|s| s.get("enabled").and_then(Item::as_bool) != Some(false))
            {
                return Err("Add a provider before enabling web search.".into());
            }
            if empty
                && raw
                    .iter()
                    .any(|(k, _)| !matches!(k, "providers" | "search"))
            {
                return Err("Removing the last provider would also remove other configuration. Keep a provider or move those settings first.".into());
            }
            save_document(
                home,
                REGISTRY,
                &if empty {
                    String::new()
                } else {
                    raw.to_string()
                },
            )?;
        }
    }
    Ok(Outcome::Applied)
}
pub(super) async fn test_provider(
    home: &Path,
    id: &str,
    revision: &str,
) -> Result<usize, RelayError> {
    let current = document(home, REGISTRY)?;
    if current.revision != revision {
        return Err("Provider settings changed on disk. Reload before testing.".into());
    }
    let config = CodeoxConfig::load_from_str(&current.text)?;
    let vendor = config
        .vendors
        .iter()
        .find(|v| v.key == id)
        .ok_or("Save this provider before testing.")?;
    let keys = SavedKeys::for_config(home, &config)?;
    let key = vendor
        .api_key(&|n| keys.get(n))
        .ok_or("This provider needs a configured API key.")?;
    let deadline = tokio::time::Instant::now() + crate::CATALOG_TIMEOUT;
    let list = crate::discover(
        &upstream_client()?,
        vendor,
        &key,
        deadline,
        // The provider page lists models for both Codex and Claude sessions.
        None,
    )
    .await?;
    Ok(crate::catalog::merge_models(Some(list), &[]).len())
}

//! Text editing for the original settings documents. Project private and unrecognized
//! registry data out of the editor; merge edits into the original tables while locked.
use super::*;
use crate::settings_protocol::model::{Cell, Key, Outcome, Values};
use std::collections::HashSet;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

const PRIVATE: &str = "<existing private value>";
fn table(cells: Values) -> Table {
    let mut t = Table::new();
    for (k, c) in cells {
        let v = match c {
            Cell::Private => value(PRIVATE),
            Cell::Text(s) => value(s),
            Cell::List(v) => value(v.into_iter().collect::<Array>()),
            Cell::Bool(b) => value(b),
            Cell::Integer(n) => value(n),
        };
        t.insert(k.name(), v);
    }
    t
}
fn projection(text: &str) -> Result<DocumentMut, RelayError> {
    let rows = rows::registry(text)?;
    let mut out = DocumentMut::new();
    if !rows.providers.is_empty() {
        let mut providers = Table::new();
        providers.set_implicit(true);
        for p in rows.providers {
            let mut t = table(p.fields);
            if !p.models.is_empty() {
                t["models"] = Item::ArrayOfTables(p.models.into_iter().map(table).collect());
            }
            providers.insert(&p.id, Item::Table(t));
        }
        out["providers"] = Item::Table(providers);
    }
    if !rows.search.is_empty() {
        out["search"] = Item::Table(table(rows.search));
    }
    Ok(out)
}
pub(super) fn documents(home: &Path) -> Result<Vec<Document>, RelayError> {
    let mut docs = vec![document(home, REGISTRY)?, document(home, FAVORITES)?];
    docs[0].text = projection(&docs[0].text)?.to_string();
    if let Some((mut legacy, id, _)) = legacy::projection(home)? {
        // Keep the selected provider bound to this draft across a retry. The
        // ordinary v1 projection remains unchanged for existing consumers.
        legacy.text = format!("model_provider = {id:?}\n{}", legacy.text);
        docs.push(legacy)
    }
    Ok(docs)
}
fn model_id(table: &Table) -> Result<&str, RelayError> {
    table
        .get(Key::Id.name())
        .and_then(Item::as_str)
        .ok_or_else(|| "Each model needs its ID.".into())
}
fn merge_models(
    original: &ArrayOfTables,
    before: &ArrayOfTables,
    after: &ArrayOfTables,
) -> Result<ArrayOfTables, RelayError> {
    let mut models = ArrayOfTables::new();
    for target in after {
        let id = model_id(target)?;
        let old = before.iter().find(|m| model_id(m).ok() == Some(id));
        let current = original.iter().find(|m| model_id(m).ok() == Some(id));
        if let (None, Some(base)) = (current, old) {
            if same_table(base, target) {
                continue;
            }
            return Err("This model was removed on disk. Reload before editing it.".into());
        }
        let mut t = current.cloned().unwrap_or_default();
        merge(&mut t, old.unwrap_or(&Table::new()), target)?;
        models.push(t);
    }
    for current in original {
        let id = model_id(current)?;
        if !after.iter().any(|t| model_id(t).ok() == Some(id)) {
            if let Some(old) = before.iter().find(|t| model_id(t).ok() == Some(id)) {
                if !deletable(current, old) {
                    return Err("This model contains private or additional settings. Remove it from the native provider form.".into());
                }
            } else {
                models.push(current.clone());
            }
        }
    }
    Ok(models)
}
// Ignore formatting here, but never clear an unknown field or a redacted value.
fn deletable(actual: &Table, visible: &Table) -> bool {
    actual
        .iter()
        .all(|(k, item)| visible.get(k).is_some_and(|v| deletable_item(item, v)))
}
fn deletable_item(actual: &Item, visible: &Item) -> bool {
    match (actual, visible) {
        (Item::Table(a), Item::Table(b)) => deletable(a, b),
        (Item::ArrayOfTables(a), Item::ArrayOfTables(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| deletable(x, y))
        }
        (Item::Value(_), Item::Value(_)) => visible.as_str() != Some(PRIVATE),
        _ => false,
    }
}
fn same_table(left: &Table, right: &Table) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|(k, v)| right.get(k).is_some_and(|r| same(v, r)))
}
fn same(left: &Item, right: &Item) -> bool {
    match (left, right) {
        (Item::Table(a), Item::Table(b)) => same_table(a, b),
        (Item::ArrayOfTables(a), Item::ArrayOfTables(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| same_table(x, y))
        }
        (Item::Value(a), Item::Value(b)) => a.to_string() == b.to_string(),
        _ => false,
    }
}
fn merge(original: &mut Table, before: &Table, after: &Table) -> Result<(), RelayError> {
    for (name, old) in before {
        if !after.contains_key(name) {
            if original
                .get(name)
                .is_some_and(|current| !deletable_item(current, old))
            {
                return Err("This entry contains private or additional settings. Remove it from the native provider form.".into());
            }
            original.remove(name);
        }
    }
    for (name, next) in after {
        let old = before.get(name);
        if old.is_some_and(|v| same(v, next)) {
            continue;
        }
        if let Some(next) = next.as_table() {
            let old = old.and_then(Item::as_table);
            let mut t = original
                .get(name)
                .and_then(Item::as_table)
                .cloned()
                .unwrap_or_default();
            merge(&mut t, old.unwrap_or(&Table::new()), next)?;
            original.insert(name, Item::Table(t));
        } else if let Some(next) = next.as_array_of_tables() {
            let empty = ArrayOfTables::new();
            let disk = original
                .get(name)
                .and_then(Item::as_array_of_tables)
                .unwrap_or(&empty);
            let old = old.and_then(Item::as_array_of_tables).unwrap_or(&empty);
            original.insert(name, Item::ArrayOfTables(merge_models(disk, old, next)?));
        } else {
            if next.as_str() == Some(PRIVATE) {
                return Err("A private-value marker must remain in its original field. Use a native form to rename this entry.".into());
            }
            if old.is_none() && original.contains_key(name) {
                return Err("This edit conflicts with a field that is reserved on disk.".into());
            }
            original.insert(name, next.clone());
        }
    }
    Ok(())
}
fn parse(text: &str) -> Result<DocumentMut, RelayError> {
    text.parse()
        .map_err(|_| "Invalid settings TOML. Check the document and save again.".into())
}

fn save_registry(home: &Path, current: &str, original: &str, text: &str) -> Result<(), RelayError> {
    let before = parse(original)?;
    let mut next = parse(text)?;
    let mut disk = parse(current)?;
    // Removing the providers section means removing its displayed entries.
    // A provider added by a concurrent OX writer was never in that set.
    if before.contains_key("providers") && !next.contains_key("providers") {
        next["providers"] = Item::Table(Table::new());
    }
    merge(disk.as_table_mut(), before.as_table(), next.as_table())?;
    if disk
        .get("providers")
        .and_then(Item::as_table)
        .is_some_and(Table::is_empty)
    {
        disk.remove("providers");
    }
    if disk.is_empty() {
        save_document(home, REGISTRY, "")
    } else {
        save_document(home, REGISTRY, &disk.to_string())
    }
}

fn save_legacy(
    home: &Path,
    revision: &str,
    id: &str,
    current: &str,
    original: &str,
    text: &str,
) -> Result<(), RelayError> {
    let mut before = parse(original)?;
    let mut next = parse(text)?;
    for document in [&mut before, &mut next] {
        if document
            .remove("model_provider")
            .as_ref()
            .and_then(Item::as_str)
            != Some(id)
        {
            return Err("The selected legacy provider changed. Reload this document before editing its connection.".into());
        }
    }
    let mut disk = parse(current)?;
    merge(disk.as_table_mut(), before.as_table(), next.as_table())?;
    legacy::save(home, revision, &disk.to_string())
}

fn favorites(text: &str) -> Result<Vec<String>, RelayError> {
    serde_json::from_str(text)
        .map_err(|_| "Favorites must be a JSON array of model aliases.".into())
}

fn save_favorites(
    home: &Path,
    current: &str,
    original: &str,
    text: &str,
) -> Result<(), RelayError> {
    if original == current {
        // The editor can repair a malformed file when no other writer touched it.
        return save_document(home, FAVORITES, text);
    }
    let before = favorites(original)?;
    let next = favorites(text)?;
    let disk = favorites(current)?;
    if disk == before {
        return save_document(home, FAVORITES, text);
    }
    if next == before {
        return Ok(());
    }
    let before_set: HashSet<_> = before.iter().collect();
    let disk_set: HashSet<_> = disk.iter().collect();
    let next_set: HashSet<_> = next.iter().collect();
    if disk.len() != disk_set.len()
        || next.len() != next_set.len()
        || before.len() != before_set.len()
    {
        return Err(
            "Favorites contain duplicate aliases. Reload before combining these changes.".into(),
        );
    }
    let common_order = |values: &[String]| {
        values
            .iter()
            .filter(|v| before_set.contains(*v) && disk_set.contains(*v) && next_set.contains(*v))
            .cloned()
            .collect::<Vec<_>>()
    };
    let (base_order, disk_order, next_order) = (
        common_order(&before),
        common_order(&disk),
        common_order(&next),
    );
    if disk_order != base_order && next_order != base_order && disk_order != next_order {
        return Err(
            "Favorites were reordered in both drafts. Reload before choosing their order.".into(),
        );
    }
    // Respect the order selected by the editor that reordered existing rows.
    // Keep independent additions from the other writer and honor both removals.
    let (primary, secondary) = if disk_order != base_order {
        (&disk, &next)
    } else {
        (&next, &disk)
    };
    let mut seen = HashSet::new();
    let result: Vec<_> = primary
        .iter()
        .chain(secondary)
        .filter(|v| {
            (!before_set.contains(*v) || (disk_set.contains(*v) && next_set.contains(*v)))
                && seen.insert(*v)
        })
        .collect();
    save_document(home, FAVORITES, &serde_json::to_string_pretty(&result)?)
}

pub(super) fn save(
    home: &Path,
    id: &str,
    revision: &str,
    original: &str,
    text: &str,
) -> Result<Outcome, RelayError> {
    let (doc, provider) = if id == legacy::ID {
        let (doc, id, _) =
            legacy::projection(home)?.ok_or("The selected legacy provider changed.")?;
        (doc, Some(id))
    } else {
        (document(home, id)?, None)
    };
    if doc.revision != revision {
        return Ok(Outcome::ChangedOnDisk {
            revision: doc.revision,
        });
    }
    match (id, provider) {
        (legacy::ID, Some(provider)) => {
            save_legacy(home, revision, &provider, &doc.text, original, text)?
        }
        (FAVORITES, _) => save_favorites(home, &doc.text, original, text)?,
        (REGISTRY, _) => save_registry(home, &doc.text, original, text)?,
        _ => return Err("Unknown settings document.".into()),
    }
    Ok(Outcome::Applied)
}

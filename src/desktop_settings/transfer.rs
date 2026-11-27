//! Export by reviewed provider destination. Import as one revision-checked write.
use super::*;
use crate::settings_protocol::{KeySelection, TransferredKey, model::Outcome};

fn validate(selection: &KeySelection, value: &str) -> Result<(), RelayError> {
    let name = &selection.name;
    let endpoint =
        reqwest::Url::parse(&selection.scope).map_err(|_| "Invalid provider endpoint.")?;
    if crate::provider::credential_env_error(name).is_some()
        || !matches!(endpoint.scheme(), "https" | "http")
        || endpoint.host().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || value.is_empty()
        || value.len() > 8192
        || !value.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err("Invalid provider credential or destination.".into());
    }
    Ok(())
}

pub(super) fn export(
    home: &Path,
    revision: &str,
    selected: Vec<KeySelection>,
) -> Result<Vec<TransferredKey>, RelayError> {
    if rows::key_revision(home)? != revision {
        return Err("Source credentials changed. Review the transfer again.".into());
    }
    let saved = SavedKeys::load(home)?;
    let available = snapshot(home)?.0.credentials;
    selected
        .into_iter()
        .map(|selection| {
            let row = available
                .iter()
                .find(|r| r.name == selection.name && r.scope == selection.scope)
                .ok_or("This credential is no longer available.")?;
            // Use the credential the local provider would actually use.
            let value = row
                .configured
                .then(|| process_env(&row.name))
                .flatten()
                .map(|v| v.trim().to_owned())
                .filter(|v| crate::provider::usable_key(v))
                .or_else(|| saved.scoped_value(&row.name, &row.scope))
                .ok_or("This provider has no transferable API key.")?;
            validate(&selection, &value)?;
            Ok(TransferredKey {
                selection,
                value: value.into(),
            })
        })
        .collect()
}

pub(super) fn import(
    home: &Path,
    revision: &str,
    keys: Vec<TransferredKey>,
) -> Result<Outcome, RelayError> {
    let current = rows::key_revision(home)?;
    if revision != current {
        return Ok(Outcome::ChangedOnDisk { revision: current });
    }
    if keys.is_empty() {
        return Err("Select at least one credential.".into());
    }
    let mut saved = SavedKeys::load(home)?;
    let config = home
        .join(REGISTRY)
        .try_exists()?
        .then(|| CodeoxConfig::load(&home.join(REGISTRY)))
        .transpose()?;
    let legacy = if config.is_none() {
        legacy::key_destination(home)?
    } else {
        None
    };
    let mut names = BTreeSet::new();
    for key in keys {
        validate(&key.selection, key.value.expose())?;
        let KeySelection { name, scope } = &key.selection;
        if config.as_ref().is_some_and(|cfg| {
            cfg.env_refs().contains(name.as_str()) && !SavedKeys::scope_matches(cfg, name, scope)
        }) || legacy
            .as_ref()
            .is_some_and(|(n, s)| n == name && s != scope)
            || saved.scopes().any(|(n, s)| n == name && s != scope)
        {
            return Err("The host uses this credential name for a different endpoint.".into());
        }
        if !names.insert(key.selection.name.clone()) {
            return Err("Choose one provider endpoint per credential name.".into());
        }
        saved.insert_scoped(
            &key.selection.name,
            &key.selection.scope,
            key.value.into_string(),
        );
    }
    let encoded = serde_json::to_string(&saved)?;
    if encoded.len() as u64 > MAX_CONFIG_BYTES {
        return Err("The credential store would exceed its size limit.".into());
    }
    setup::write_config_atomic(&SavedKeys::path(home), &encoded)?;
    Ok(Outcome::Applied)
}

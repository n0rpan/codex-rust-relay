use std::path::Path;

use reqwest::Url;
use reqwest::header::HeaderValue;
use toml_edit::{Document, Item};

use crate::{RelayError, TOKEN_ENV, bearer};

pub struct Provider {
    pub id: String,
    pub name: String,
    pub env_key: String,
    pub upstream_auth: HeaderValue,
    pub models_url: Url,
    inference_url: String,
}

impl Provider {
    pub fn load(codex_home: &Path) -> Result<Self, RelayError> {
        let raw = std::fs::read_to_string(codex_home.join("config.toml"))?;
        let config = Document::parse(raw).map_err(|_| "could not parse Codex config.toml")?;
        if config.get("model_catalog_json").is_some() {
            return Err("remove model_catalog_json to enable live model discovery".into());
        }
        let id = config
            .get("model_provider")
            .and_then(Item::as_str)
            .filter(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_' || ch == b'-')
            })
            .ok_or("set model_provider to a provider ID using letters, numbers, _ or -")?;
        let provider = config
            .get("model_providers")
            .and_then(|table| table.get(id))
            .ok_or("the selected model_provider is missing from model_providers")?;
        let setting = |name: &str| provider.get(name).and_then(Item::as_str);
        let raw_base = setting("base_url").ok_or("the selected provider needs a base_url")?;
        let base = parse_url(raw_base)?;
        if base.query().is_some() {
            return Err("base_url must not contain a query string".into());
        }
        let inference_url = base.as_str().trim_end_matches('/').to_owned();
        let models_url = match setting("model_catalog_url") {
            Some(url) => parse_url(url)?,
            None => Url::parse(&format!("{inference_url}/models"))?,
        };
        if models_url.origin() != base.origin() {
            return Err("model_catalog_url must have the same origin as base_url".into());
        }
        let env_key = setting("env_key")
            .filter(|name| !name.is_empty() && *name != TOKEN_ENV)
            .ok_or("set env_key to the name of the provider API key environment variable")?;
        let key = std::env::var(env_key)
            .ok()
            .filter(|key| !key.is_empty() && !key.chars().any(char::is_whitespace))
            .ok_or_else(|| format!("set a valid API key in {env_key}"))?;
        let mut upstream_auth = bearer(&key)?;
        upstream_auth.set_sensitive(true);
        Ok(Self {
            id: id.to_owned(),
            name: setting("name").unwrap_or(id).to_owned(),
            env_key: env_key.to_owned(),
            upstream_auth,
            models_url,
            inference_url,
        })
    }

    pub fn responses_url(&self, path: &str, query: Option<&str>) -> Url {
        let mut url = Url::parse(&format!("{}{path}", self.inference_url))
            .expect("the configured base URL and the response path are valid");
        url.set_query(query);
        url
    }
}

fn parse_url(raw: &str) -> Result<Url, RelayError> {
    let url = Url::parse(raw).map_err(|_| "expected an absolute provider URL")?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err("provider URLs must be HTTPS or HTTP on loopback".into());
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err("provider URLs cannot contain credentials or fragments".into());
    }
    Ok(url)
}

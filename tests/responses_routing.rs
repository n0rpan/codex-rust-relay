//! Contract tests for codeox milestone 1: the `codeox.toml` surface, stable
//! provider-qualified aliases, and the routing table.
//!
//! These tests are socket-free. The HTTP behaviour of the relay (alias routing,
//! upstream ID rewriting, per-vendor credentials, discovery failures, request
//! limits and redirect handling) is covered against mocked loopback vendors by
//! the `tests::*` unit tests in `src/main.rs`, because this package is a
//! binary-only target with no library surface to link an integration test to.
#![allow(dead_code)]

#[path = "../src/catalog.rs"]
mod catalog;
#[path = "../src/provider.rs"]
mod provider;
#[path = "../src/worker_placement.rs"]
mod worker_placement;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use catalog::merge_models;
use provider::{CodeoxConfig, Provider, Resolution, Routes};

fn config_text(alpha_base: &str, beta_base: &str) -> String {
    format!(
        r#"
[providers.alpha]
name = "Alpha"
base_url = "{alpha_base}"
env_key = "ALPHA_KEY"

[[providers.alpha.models]]
id = "alpha-coder"
name = "Alpha Coder"
context_window = 128000
input_modalities = ["text", "image"]
reasoning_efforts = ["low", "medium", "high"]

[providers.beta]
name = "Beta"
base_url = "{beta_base}"
env_key = "BETA_KEY"
discovery = false

[[providers.beta.models]]
id = "meta/llama-3.3-70b"
name = "Llama 3.3 70B"
"#
    )
}

fn routes_for(config: &CodeoxConfig) -> Routes {
    let models = config
        .vendors
        .iter()
        .map(|vendor| merge_models(None, &vendor.models))
        .collect::<Vec<_>>();
    Routes::build(&config.vendors, &models).unwrap()
}

#[test]
fn aliases_are_stable_provider_qualified_and_resume_safe() {
    let text = config_text("https://alpha.example/v1", "https://beta.example/v1");
    let first = routes_for(&CodeoxConfig::load_from_str(&text).unwrap());
    let second = routes_for(&CodeoxConfig::load_from_str(&text).unwrap());
    let aliases = first.aliases().map(str::to_owned).collect::<Vec<_>>();
    assert_eq!(
        aliases,
        second.aliases().map(str::to_owned).collect::<Vec<_>>()
    );
    assert_eq!(
        aliases,
        ["alpha/alpha-coder", "beta/meta/llama-3.3-70b"],
        "aliases depend only on the provider key and upstream model id"
    );

    // A slug persisted by a previous Codex session keeps routing to the same
    // vendor and upstream model after a restart.
    match first.resolve("beta/meta/llama-3.3-70b") {
        Resolution::Route(route) => {
            assert_eq!(route.vendor, 1);
            assert_eq!(route.upstream_model, "meta/llama-3.3-70b");
        }
        _ => panic!("expected the qualified alias to route"),
    }
}

#[test]
fn the_same_model_id_on_two_vendors_does_not_collide() {
    let text = r#"
[providers.alpha]
base_url = "https://alpha.example/v1"

[[providers.alpha.models]]
id = "shared-model"

[providers.beta]
base_url = "https://beta.example/v1"

[[providers.beta.models]]
id = "shared-model"
"#;
    let config = CodeoxConfig::load_from_str(text).unwrap();
    let routes = routes_for(&config);
    assert_eq!(
        routes.aliases().collect::<Vec<_>>(),
        ["alpha/shared-model", "beta/shared-model"]
    );
    match routes.resolve("shared-model") {
        Resolution::Ambiguous(aliases) => {
            assert_eq!(aliases, ["alpha/shared-model", "beta/shared-model"]);
        }
        _ => panic!("a bare id owned by two vendors must be ambiguous"),
    }
}

#[test]
fn explicit_aliases_cannot_use_another_providers_prefix() {
    let raw = r#"
[providers.alpha]
base_url = "https://alpha.example/v1"
[[providers.alpha.models]]
id = "alpha-model"
alias = "beta/unlisted-model"
[providers.beta]
base_url = "https://beta.example/v1"
"#;
    let err = CodeoxConfig::load_from_str(raw).unwrap_err();
    assert!(
        err.to_string()
            .contains("uses the reserved prefix of provider \"beta\""),
        "{err}"
    );
    assert!(
        CodeoxConfig::load_from_str(&raw.replace("beta/unlisted-model", "alpha/unlisted-model",))
            .is_ok()
    );
}

#[test]
fn an_explicit_alias_collision_is_rejected() {
    let text = r#"
[providers.alpha]
base_url = "https://alpha.example/v1"

[[providers.alpha.models]]
id = "x"
alias = "shared"

[providers.beta]
base_url = "https://beta.example/v1"

[[providers.beta.models]]
id = "y"
alias = "shared"
"#;
    let config = CodeoxConfig::load_from_str(text).unwrap();
    let models = config
        .vendors
        .iter()
        .map(|vendor| merge_models(None, &vendor.models))
        .collect::<Vec<_>>();
    let error = Routes::build(&config.vendors, &models)
        .unwrap_err()
        .to_string();
    assert!(error.contains("\"shared\""), "{error}");
    assert!(
        error.contains("\"alpha\"") && error.contains("\"beta\""),
        "{error}"
    );
}

#[test]
fn explicit_wire_formats_are_accepted_and_aliases_are_not() {
    for wire in ["responses", "anthropic_messages", "chat_completions"] {
        let config = CodeoxConfig::load_from_str(&format!(
            "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nwire_api = \"{wire}\"\n"
        ))
        .unwrap();
        assert_eq!(config.vendors[0].wire.as_str(), wire);
    }
    for wire in ["anthropic", "chat", "messages"] {
        let error = CodeoxConfig::load_from_str(&format!(
            "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nwire_api = \"{wire}\"\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown wire_api"), "{error}");
    }
}

#[test]
fn per_vendor_env_refs_are_collected_for_the_child_environment() {
    let config =
        CodeoxConfig::load_from_str(&config_text("https://a.example/v1", "https://b.example/v1"))
            .unwrap();
    assert_eq!(
        config.env_refs(),
        BTreeSet::from(["ALPHA_KEY".to_owned(), "BETA_KEY".to_owned()])
    );
}

#[test]
fn cross_origin_catalog_and_insecure_urls_are_rejected() {
    for text in [
        "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nmodels_url = \"https://other.example/v1/models\"\n",
        "[providers.alpha]\nbase_url = \"http://alpha.example/v1\"\n",
        "[providers.alpha]\nbase_url = \"https://alpha.example/v1?token=secret\"\n",
        "[providers.alpha]\nbase_url = \"https://user:pass@alpha.example/v1\"\n",
    ] {
        assert!(
            CodeoxConfig::load_from_str(text).is_err(),
            "expected config rejection for {text:?}"
        );
    }
}

#[test]
fn legacy_config_still_loads_without_a_codeox_config() {
    let home = TempHome::new("legacy");
    std::fs::write(
        home.path().join("config.toml"),
        r#"
model = "demo-coding"
model_provider = "demo"

[model_providers.demo]
name = "Demo Provider"
base_url = "https://api.example.com/v1"
env_key = "DEMO_API_KEY"
wire_api = "responses"
"#,
    )
    .unwrap();
    let env = |name: &str| (name == "DEMO_API_KEY").then(|| "fixture-key".to_owned());
    let provider = Provider::load(home.path(), &env).unwrap();
    assert_eq!(provider.id, "demo");
    assert_eq!(
        provider.models_url.as_str(),
        "https://api.example.com/v1/models"
    );
    assert_eq!(
        provider.responses_url("/responses", None).as_str(),
        "https://api.example.com/v1/responses"
    );
    assert_eq!(
        provider.upstream_auth.to_str().unwrap(),
        "Bearer fixture-key"
    );
}

struct TempHome(PathBuf);

impl TempHome {
    fn new(tag: &str) -> Self {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let path =
            std::env::temp_dir().join(format!("codeox-contract-{tag}-{}", hex::encode(bytes)));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

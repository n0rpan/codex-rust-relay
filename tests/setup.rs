//! Offline tests for `codeox provider add` and `codeox doctor`.
//!
//! The setup module and its dependencies are included by relative path, like
//! the adapter tests, so these tests do not depend on `main.rs` wiring the
//! command yet. Every test uses an isolated temporary codex home; nothing here
//! reads the real `$HOME`, `auth.json`, or the network.

#![allow(dead_code)]

#[path = "../src/advice.rs"]
mod advice;
#[path = "../src/catalog.rs"]
mod catalog;
#[path = "../src/provider.rs"]
mod provider;
#[path = "../src/setup.rs"]
mod setup;
#[path = "../src/ui.rs"]
mod ui;

use std::ffi::OsString;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let path = std::env::temp_dir().join(format!("codeox-setup-{}", hex::encode(bytes)));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, contents: &str) {
        fs::write(self.0.join(name), contents).unwrap();
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.0.join(name)).unwrap()
    }

    fn exists(&self, name: &str) -> bool {
        self.0.join(name).exists()
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_add(
    home: &Path,
    items: &[&str],
    input: &str,
    interactive: bool,
    env_pairs: &[(&str, &str)],
) -> Result<(u8, String), String> {
    let args = args(items);
    let mut input = Cursor::new(input.as_bytes().to_vec());
    let mut output = Vec::new();
    let env = |name: &str| {
        env_pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    };
    match setup::run_provider_add(&args, home, &mut input, &mut output, interactive, &env) {
        Ok(code) => Ok((code, String::from_utf8(output).unwrap())),
        Err(error) => Err(error.to_string()),
    }
}

fn add_ok(home: &Path, items: &[&str], env_pairs: &[(&str, &str)]) -> (u8, String) {
    run_add(home, items, "", false, env_pairs).unwrap()
}

fn run_doctor(home: &Path, flags: &[&str], env_pairs: &[(&str, &str)]) -> (u8, String) {
    let args = args(flags);
    let mut output = Vec::new();
    let env = |name: &str| {
        env_pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    };
    let code = setup::run_doctor(&args, home, &mut output, &env).unwrap();
    (code, String::from_utf8(output).unwrap())
}

const ALPHA_CONFIG: &str = r#"# keep this comment exactly

[providers.alpha]
name = "Alpha"
base_url = "https://alpha.example/v1"
env_key = "ALPHA_KEY"

[[providers.alpha.models]]
id = "alpha-coder"
name = "Alpha Coder"
context_window = 64000
input_modalities = ["text"]
"#;

// ---------------------------------------------------------------------------
// provider add
// ---------------------------------------------------------------------------

#[test]
fn adds_provider_with_flags_and_preserves_existing_bytes() {
    let home = TempHome::new();
    home.write("codeox.toml", ALPHA_CONFIG);
    let before = home.read("codeox.toml");

    let (code, output) = add_ok(
        home.path(),
        &[
            "beta",
            "--name",
            "Beta",
            "--base-url",
            "https://beta.example/v1",
            "--wire",
            "chat",
            "--env-key",
            "BETA_KEY",
            "--no-discovery",
            "--model",
            "beta-1",
            "--model-name",
            "Beta One",
            "--context-window",
            "128000",
            "--alias",
            "beta/stable",
            "--input-modalities",
            "text,image",
            "--reasoning-efforts",
            "low,high",
        ],
        &[("BETA_KEY", "super-secret-value")],
    );
    assert_eq!(code, 0, "{output}");

    let after = home.read("codeox.toml");
    assert!(
        after.starts_with(&before),
        "the existing file must be preserved byte for byte"
    );
    assert!(after.contains("[providers.beta]"), "{after}");
    assert!(after.contains("name = \"Beta\""), "{after}");
    assert!(
        after.contains("base_url = \"https://beta.example/v1\""),
        "{after}"
    );
    assert!(after.contains("env_key = \"BETA_KEY\""), "{after}");
    assert!(after.contains("wire_api = \"chat_completions\""), "{after}");
    assert!(after.contains("discovery = false"), "{after}");
    assert!(after.contains("[[providers.beta.models]]"), "{after}");
    assert!(after.contains("id = \"beta-1\""), "{after}");
    assert!(after.contains("name = \"Beta One\""), "{after}");
    assert!(after.contains("context_window = 128000"), "{after}");
    assert!(after.contains("alias = \"beta/stable\""), "{after}");
    assert!(output.contains("model alias: beta/stable"), "{output}");
    assert!(
        output.contains("ox codex --model 'beta/stable'"),
        "{output}"
    );
    assert!(
        output.contains("ox claude --model 'beta/stable'"),
        "{output}"
    );
    assert!(
        after.contains("input_modalities = [\"text\", \"image\"]"),
        "{after}"
    );
    assert!(
        after.contains("reasoning_efforts = [\"low\", \"high\"]"),
        "{after}"
    );

    // The candidate still goes through the provider module's URL/model rules.
    setup::parse_config_for_validation(&after).unwrap();

    // The credential value is not stored or echoed.
    assert!(!after.contains("super-secret-value"), "{after}");
    assert!(!output.contains("super-secret-value"), "{output}");
    assert!(output.contains("references $BETA_KEY"), "{output}");
    assert!(
        output.contains("no key value was read or stored"),
        "{output}"
    );
}

#[test]
fn creates_a_new_config_when_none_exists() {
    let home = TempHome::new();
    let (code, output) = add_ok(
        home.path(),
        &[
            "solo",
            "--base-url",
            "https://solo.example/v1",
            "--wire",
            "responses",
        ],
        &[],
    );
    assert_eq!(code, 0, "{output}");
    let config = home.read("codeox.toml");
    assert!(config.starts_with("[providers.solo]\n"), "{config}");
    // The default credential reference is derived from the provider key.
    assert!(config.contains("env_key = \"SOLO_API_KEY\""), "{config}");
    assert!(!config.contains("discovery = true"), "{config}");
    let parsed = setup::parse_config_for_validation(&config).unwrap();
    assert_eq!(parsed.vendors.len(), 1);
    assert_eq!(parsed.vendors[0].key, "solo");
}

#[test]
fn provider_key_flag_is_equivalent_to_the_positional_key() {
    let home = TempHome::new();
    let (code, output) = add_ok(
        home.path(),
        &[
            "--provider-key",
            "flag-key",
            "--base-url",
            "https://flag.example/v1",
            "--env-key",
            "FLAG_KEY",
        ],
        &[],
    );
    assert_eq!(code, 0, "{output}");
    assert!(home.read("codeox.toml").contains("[providers.flag-key]"));
}

#[test]
fn appends_cleanly_when_the_existing_file_lacks_a_final_newline() {
    let home = TempHome::new();
    let original =
        "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nenv_key = \"ALPHA_KEY\"";
    home.write("codeox.toml", original);
    let (code, output) = add_ok(
        home.path(),
        &[
            "beta",
            "--base-url",
            "https://beta.example/v1",
            "--env-key",
            "BETA_KEY",
        ],
        &[],
    );
    assert_eq!(code, 0, "{output}");
    let after = home.read("codeox.toml");
    assert!(after.starts_with(original), "{after}");
    assert!(after.contains("\n[providers.beta]"), "{after}");
    setup::parse_config_for_validation(&after).unwrap();
}

#[test]
fn duplicate_provider_key_is_rejected_without_writing() {
    let home = TempHome::new();
    home.write("codeox.toml", ALPHA_CONFIG);
    let before = home.read("codeox.toml");

    let (code, output) = add_ok(
        home.path(),
        &[
            "alpha",
            "--base-url",
            "https://other.example/v1",
            "--env-key",
            "OTHER_KEY",
        ],
        &[],
    );
    assert_eq!(code, 2, "{output}");
    assert!(output.contains("already exists"), "{output}");
    assert_eq!(
        home.read("codeox.toml"),
        before,
        "duplicate must not edit the file"
    );
}

#[test]
fn invalid_existing_config_is_not_overwritten() {
    let home = TempHome::new();
    home.write("codeox.toml", "[providers.alpha\nbase_url = 1\n");

    let error = run_add(
        home.path(),
        &[
            "beta",
            "--base-url",
            "https://beta.example/v1",
            "--env-key",
            "BETA_KEY",
        ],
        "",
        false,
        &[],
    )
    .unwrap_err();
    assert!(error.contains("not valid TOML"), "{error}");
    assert_eq!(
        home.read("codeox.toml"),
        "[providers.alpha\nbase_url = 1\n",
        "invalid input must be left untouched"
    );
}

#[test]
fn semantically_invalid_existing_provider_is_not_overwritten() {
    let home = TempHome::new();
    let original = r#"
[providers.alpha]
base_url = "http://not-loopback.example/v1"
env_key = "ALPHA_KEY"
"#;
    home.write("codeox.toml", original);

    let (code, output) = add_ok(
        home.path(),
        &[
            "beta",
            "--base-url",
            "https://beta.example/v1",
            "--env-key",
            "BETA_KEY",
        ],
        &[],
    );
    assert_eq!(code, 2, "{output}");
    assert!(
        output.contains("HTTPS or HTTP on loopback"),
        "the provider module's URL rule is reused: {output}"
    );
    assert_eq!(home.read("codeox.toml"), original);
}

#[test]
fn credential_values_are_rejected_and_never_echoed() {
    let home = TempHome::new();
    let (code, output) = run_add(
        home.path(),
        &[
            "alpha",
            "--base-url",
            "https://alpha.example/v1",
            "--env-key",
            "sk-live-this-is-a-secret",
        ],
        "",
        false,
        &[],
    )
    .unwrap();
    assert_eq!(code, 2);
    assert!(output.contains("environment variable name"), "{output}");
    assert!(!output.contains("sk-live-this-is-a-secret"), "{output}");
    assert!(!home.exists("codeox.toml"));

    let (code, output) = run_add(
        home.path(),
        &[
            "alpha",
            "--base-url",
            "https://alpha.example/v1",
            "--api-key",
            "another-secret-value",
        ],
        "",
        false,
        &[],
    )
    .unwrap();
    assert_eq!(code, 2);
    assert!(output.contains("--env-key NAME"), "{output}");
    assert!(!output.contains("another-secret-value"), "{output}");
    assert!(!home.exists("codeox.toml"));
}

#[test]
fn gateway_and_launch_keys_are_rejected_as_provider_credentials() {
    let home = TempHome::new();
    for key in ["CODEOX_TOKEN", "OX_GATEWAY_KEY"] {
        let (code, output) = add_ok(
            home.path(),
            &[
                "alpha",
                "--base-url",
                "https://alpha.example/v1",
                "--env-key",
                key,
            ],
            &[],
        );
        assert_eq!(code, 2);
        assert!(output.contains("reserved"), "{output}");
        assert!(!home.exists("codeox.toml"));
    }
    home.write(
        "codeox.toml",
        "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nenv_key = \"OX_GATEWAY_KEY\"\n",
    );
    let (code, output) = run_doctor(home.path(), &[], &[("OX_GATEWAY_KEY", "fixture-secret")]);
    assert_eq!(code, 1);
    assert!(output.contains("OX_GATEWAY_KEY is reserved for the shared gateway key"));
    assert!(!output.contains("fixture-secret"));
}

#[test]
fn pasted_credential_values_are_rejected_without_echoing_them() {
    let home = TempHome::new();
    let secret = "sk-or-fixture-pasted-credential";
    let base = "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\n";
    for config in [
        format!("{base}env_key = \"{secret}\"\n"),
        format!("{base}\n[search]\nenv_key = \"{secret}\"\n"),
    ] {
        home.write("codeox.toml", &config);
        let (code, output) = run_doctor(home.path(), &[], &[]);
        assert_eq!(code, 1);
        assert!(output.contains("environment variable name"), "{output}");
        assert!(!output.contains(secret));
    }
}

#[test]
fn credential_shaped_provider_name_is_rejected_without_echoing_it() {
    let home = TempHome::new();
    let secret = "sk-or-fixture-lowercase";
    let (code, output) = add_ok(home.path(), &[secret], &[]);
    assert_eq!(code, 2);
    assert!(output.contains("looks like a credential"));
    assert!(!output.contains(secret));
    assert!(!home.exists("codeox.toml"));
}

#[test]
fn anthropic_wire_and_version_are_recorded() {
    let home = TempHome::new();
    let (code, output) = add_ok(
        home.path(),
        &[
            "claude",
            "--base-url",
            "https://api.anthropic.com/v1",
            "--wire",
            "anthropic",
            "--env-key",
            "ANTHROPIC_API_KEY",
            "--anthropic-version",
            "2023-06-01",
        ],
        &[],
    );
    assert_eq!(code, 0, "{output}");
    let config = home.read("codeox.toml");
    assert!(
        config.contains("wire_api = \"anthropic_messages\""),
        "{config}"
    );
    assert!(
        config.contains("anthropic_version = \"2023-06-01\""),
        "{config}"
    );

    let (code, output) = add_ok(
        home.path(),
        &[
            "claude-default",
            "--base-url",
            "https://api.anthropic.com/v1",
            "--wire",
            "messages",
            "--env-key",
            "ANTHROPIC_API_KEY",
        ],
        &[],
    );
    assert_eq!(code, 0, "{output}");
    let config = home.read("codeox.toml");
    let default_section = config
        .split("[providers.claude-default]")
        .nth(1)
        .expect("default anthropic section");
    assert!(
        !default_section.contains("anthropic_version"),
        "the adapter default must apply when no version is requested: {default_section}"
    );
}

#[test]
fn anthropic_version_is_rejected_for_other_wires() {
    let home = TempHome::new();
    let (code, output) = add_ok(
        home.path(),
        &[
            "alpha",
            "--base-url",
            "https://alpha.example/v1",
            "--wire",
            "chat",
            "--anthropic-version",
            "2023-06-01",
        ],
        &[],
    );
    assert_eq!(code, 2);
    assert!(
        output.contains("only valid with --wire anthropic_messages"),
        "{output}"
    );
    assert!(!home.exists("codeox.toml"));
}

#[test]
fn discovery_off_requires_a_static_model() {
    let home = TempHome::new();
    let (code, output) = add_ok(
        home.path(),
        &[
            "alpha",
            "--base-url",
            "https://alpha.example/v1",
            "--no-discovery",
        ],
        &[],
    );
    assert_eq!(code, 2);
    assert!(output.contains("requires at least one --model"), "{output}");
    assert!(!home.exists("codeox.toml"));

    let (code, output) = add_ok(
        home.path(),
        &[
            "alpha",
            "--base-url",
            "https://alpha.example/v1",
            "--discovery",
            "false",
            "--model",
            "only-model",
        ],
        &[],
    );
    assert_eq!(code, 0, "{output}");
    assert!(home.read("codeox.toml").contains("discovery = false"));
}

#[test]
fn duplicate_model_ids_and_single_model_flags_are_rejected() {
    let home = TempHome::new();
    let (code, output) = add_ok(
        home.path(),
        &[
            "alpha",
            "--base-url",
            "https://alpha.example/v1",
            "--model",
            "same",
            "--model",
            "same",
        ],
        &[],
    );
    assert_eq!(code, 2);
    assert!(output.contains("more than once"), "{output}");

    let (code, output) = add_ok(
        home.path(),
        &[
            "alpha",
            "--base-url",
            "https://alpha.example/v1",
            "--model",
            "one",
            "--model",
            "two",
            "--context-window",
            "1000",
        ],
        &[],
    );
    assert_eq!(code, 2);
    assert!(output.contains("exactly one --model"), "{output}");
    assert!(!home.exists("codeox.toml"));
}

#[test]
fn direct_openai_setup_explains_codex_api_access_and_chatgpt_sign_in() {
    for (name, url, direct_openai) in [
        ("openai-direct", "https://api.openai.com/v1", true),
        ("openai", "https://api.openai.com.example/v1", false),
    ] {
        let home = TempHome::new();
        let (code, output) = add_ok(home.path(), &[name, "--base-url", url], &[]);
        assert_eq!(code, 0, "{output}");
        assert_eq!(
            output.contains("requires Codex access in your OpenAI API organization"),
            direct_openai,
            "{output}"
        );
        assert_eq!(
            output.contains("ox codex --chatgpt"),
            direct_openai,
            "{output}"
        );
    }
}

#[test]
fn interactive_setup_prompts_for_references_only() {
    let home = TempHome::new();
    let input = "\
cli
CLI Test

https://cli.example/v1
chat

n
cli-model
";
    let (code, output) = run_add(home.path(), &[], input, true, &[]).unwrap();
    assert_eq!(code, 0, "{output}");
    assert!(
        output.contains("Provider name (e.g. openrouter, anthropic, openai):"),
        "{output}"
    );
    assert!(
        output.contains("API key variable name [CLI_API_KEY]"),
        "{output}"
    );
    assert!(
        output.contains("Enter an environment variable name"),
        "the prompt makes the reference-only rule explicit: {output}"
    );
    assert!(!output.to_ascii_lowercase().contains("api key value:"));

    let config = home.read("codeox.toml");
    assert!(config.contains("[providers.cli]"), "{config}");
    assert!(config.contains("name = \"CLI Test\""), "{config}");
    assert!(config.contains("env_key = \"CLI_API_KEY\""), "{config}");
    assert!(
        config.contains("wire_api = \"chat_completions\""),
        "{config}"
    );
    assert!(config.contains("discovery = false"), "{config}");
    assert!(config.contains("id = \"cli-model\""), "{config}");
}

#[test]
fn interactive_prompts_reprompt_after_invalid_values() {
    let home = TempHome::new();
    let input = "\
Bad Key
good-key

http://not-loopback.example/v1
https://good.example/v1
responses

y

";
    let (code, output) = run_add(home.path(), &[], input, true, &[]).unwrap();
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("lowercase ASCII"), "{output}");
    assert!(output.contains("HTTPS or HTTP on loopback"), "{output}");
    let config = home.read("codeox.toml");
    assert!(config.contains("[providers.good-key]"), "{config}");
}

#[test]
fn adding_a_provider_with_a_colliding_alias_is_rejected() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        r#"
[providers.alpha]
base_url = "https://alpha.example/v1"
env_key = "ALPHA_KEY"

[[providers.alpha.models]]
id = "alpha-model"
alias = "shared"
"#,
    );
    let before = home.read("codeox.toml");

    let (code, output) = add_ok(
        home.path(),
        &[
            "beta",
            "--base-url",
            "https://beta.example/v1",
            "--env-key",
            "BETA_KEY",
            "--no-discovery",
            "--model",
            "beta-model",
            "--alias",
            "shared",
        ],
        &[],
    );
    assert_eq!(code, 2, "{output}");
    assert!(output.contains("used by both provider"), "{output}");
    assert_eq!(home.read("codeox.toml"), before);
}

#[cfg(unix)]
#[test]
fn atomic_write_preserves_permissions_and_leaves_no_temp_files() {
    use std::os::unix::fs::PermissionsExt;

    let home = TempHome::new();
    home.write("codeox.toml", ALPHA_CONFIG);
    let path = home.path().join("codeox.toml");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

    let (code, output) = add_ok(
        home.path(),
        &[
            "beta",
            "--base-url",
            "https://beta.example/v1",
            "--env-key",
            "BETA_KEY",
        ],
        &[],
    );
    assert_eq!(code, 0, "{output}");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let mut entries = fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    entries.sort();
    assert_eq!(
        entries,
        [".codeox.toml.lock", "codeox.toml"],
        "no temporary file may remain: {entries:?}"
    );
}

#[test]
fn simultaneous_provider_adds_preserve_every_provider() {
    use std::sync::Barrier;
    use std::thread;

    const WRITERS: usize = 8;
    let home = TempHome::new();
    home.write("codeox.toml", ALPHA_CONFIG);
    let barrier = Barrier::new(WRITERS);
    thread::scope(|scope| {
        for index in 0..WRITERS {
            let home = home.path();
            let barrier = &barrier;
            scope.spawn(move || {
                let key = format!("worker-{index}");
                barrier.wait();
                let (code, output) =
                    add_ok(home, &[&key, "--base-url", "https://example.com/v1"], &[]);
                assert_eq!(code, 0, "{output}");
            });
        }
    });
    let stored = home.read("codeox.toml");
    assert!(stored.starts_with(ALPHA_CONFIG));
    let config = provider::CodeoxConfig::load_from_str(&stored).unwrap();
    assert_eq!(config.vendors.len(), WRITERS + 1);
}

#[cfg(unix)]
#[test]
fn symlinked_config_is_refused() {
    let home = TempHome::new();
    let target = home.path().join("real.toml");
    fs::write(&target, ALPHA_CONFIG).unwrap();
    std::os::unix::fs::symlink(&target, home.path().join("codeox.toml")).unwrap();

    let error = run_add(
        home.path(),
        &[
            "beta",
            "--base-url",
            "https://beta.example/v1",
            "--env-key",
            "BETA_KEY",
        ],
        "",
        false,
        &[],
    )
    .unwrap_err();
    assert!(error.contains("symbolic link"), "{error}");
    assert_eq!(fs::read_to_string(target).unwrap(), ALPHA_CONFIG);
}

#[test]
fn dispatch_only_claims_its_own_commands() {
    let home = TempHome::new();
    assert!(setup::dispatch(&args(&["run", "--model", "x"]), home.path()).is_none());
    assert!(setup::dispatch(&[], home.path()).is_none());
    assert!(setup::dispatch(&args(&["provider", "add", "--help"]), home.path()).is_some());
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[test]
fn doctor_reports_and_fixes_recorded_provider_advice() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        "[providers.openrouter]\nbase_url = \"https://openrouter.ai/api/v1\"\n",
    );
    fs::create_dir_all(home.path().join("ox-clients/codex")).unwrap();
    fs::write(
        home.path().join("ox-clients/codex/config.toml"),
        "model = \"openrouter/openai/gpt-6.1-sol\"\nmodel_reasoning_effort = \"none\"\n",
    )
    .unwrap();
    advice::record(
        home.path(),
        advice::Advice {
            id: "model-reasoning:openrouter:openai/gpt-6.1-sol".to_owned(),
            provider: "openrouter".to_owned(),
            model: Some("openai/gpt-6.1-sol".to_owned()),
            message: "reasoning cannot be disabled on this model; using \"high\"".to_owned(),
            fix: advice::Fix::ClearRelayReasoningEffort,
        },
    );

    let (code, output) = run_doctor(home.path(), &["--strict"], &[]);
    assert_eq!(code, 1, "{output}");
    assert!(output.contains("advice"), "{output}");
    assert!(output.contains("openai/gpt-6.1-sol"), "{output}");
    assert!(output.contains("ox doctor --fix"), "{output}");

    let (_, output) = run_doctor(home.path(), &["--fix"], &[]);
    assert!(output.contains("applied:"), "{output}");
    let profile = fs::read_to_string(home.path().join("ox-clients/codex/config.toml")).unwrap();
    assert!(!profile.contains("model_reasoning_effort"), "{profile}");
    assert!(advice::read(home.path()).is_empty(), "the fix is forgotten");
}

#[test]
fn doctor_reports_each_provider_without_secret_values() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        r#"
[providers.alpha]
name = "Alpha"
base_url = "https://alpha.example/v1"
env_key = "ALPHA_KEY"

[providers.beta]
name = "Beta"
base_url = "https://beta.example/v1"
env_key = "BETA_KEY"

[[providers.beta.models]]
id = "beta-1"

[providers.gamma]
name = "Gamma"
base_url = "https://gamma.example/v1"
env_key = "GAMMA_KEY"
discovery = false
"#,
    );
    home.write(
        "config.toml",
        r#"
model_catalog_json = "/tmp/catalog.json"
model_provider = "alpha"

[model_providers.codeox]
name = "Shadow"
base_url = "https://shadow.example/v1"
env_key = "SHADOW_KEY"
"#,
    );
    home.write(
        "auth.json",
        r#"{"OPENAI_API_KEY":"AUTH_JSON_SENTINEL","tokens":{"access":"AUTH_JSON_SENTINEL"}}"#,
    );

    let (code, output) = run_doctor(
        home.path(),
        &[],
        &[("ALPHA_KEY", "super-secret-alpha"), ("GAMMA_KEY", "")],
    );
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("mode: multi-vendor (codeox.toml)"),
        "{output}"
    );
    assert!(output.contains("provider alpha"), "{output}");
    assert!(output.contains("wire: responses"), "{output}");
    assert!(
        output.contains("credentials: ALPHA_KEY is available to ox"),
        "{output}"
    );
    assert!(
        output.contains("credential reference BETA_KEY is not set"),
        "{output}"
    );
    assert!(
        output.contains("credential reference GAMMA_KEY is set but empty"),
        "{output}"
    );
    assert!(output.contains("discovery off"), "{output}");
    assert!(
        output.contains("discovery is off and no static models"),
        "{output}"
    );
    assert!(output.contains("model_catalog_json is set"), "{output}");
    assert!(
        output.contains("[model_providers.codeox] is defined"),
        "{output}"
    );
    assert!(
        output.contains("model_provider = \"alpha\" is replaced at launch"),
        "{output}"
    );
    assert!(output.contains("summary:"), "{output}");

    // No credential value, and nothing from auth.json, may appear.
    assert!(!output.contains("super-secret-alpha"), "{output}");
    assert!(!output.contains("AUTH_JSON_SENTINEL"), "{output}");
    assert!(!output.contains("SHADOW_KEY is set"), "{output}");
}

#[test]
fn doctor_healthy_multi_vendor_config_exits_zero() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        r#"
[providers.alpha]
base_url = "https://alpha.example/v1"
env_key = "ALPHA_KEY"

[[providers.alpha.models]]
id = "alpha-coder"
context_window = 64000
"#,
    );
    let (code, output) = run_doctor(home.path(), &[], &[("ALPHA_KEY", "secret")]);
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("validation: ok"), "{output}");
    assert!(
        output.contains("config: codeox.toml parses and validates"),
        "{output}"
    );
    assert!(output.contains("0 error(s)"), "{output}");
    assert!(!output.contains("secret"), "{output}");
}

#[test]
fn doctor_strict_promotes_warnings_to_failure() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        r#"
[providers.alpha]
base_url = "https://alpha.example/v1"
"#,
    );
    let (code, output) = run_doctor(home.path(), &[], &[]);
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("no env_key is configured"), "{output}");

    let (code, output) = run_doctor(home.path(), &["--strict"], &[]);
    assert_eq!(code, 1, "{output}");
    assert!(output.contains("warning(s)"), "{output}");
}

#[test]
fn doctor_reports_anthropic_wire_version_and_unknown_wires() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        r#"
[providers.claude]
base_url = "https://api.anthropic.com/v1"
env_key = "ANTHROPIC_API_KEY"
wire_api = "anthropic_messages"
anthropic_version = "2023-06-01"

[providers.broken]
base_url = "https://broken.example/v1"
env_key = "BROKEN_KEY"
wire_api = "grpc"
"#,
    );
    let (code, output) = run_doctor(
        home.path(),
        &[],
        &[("ANTHROPIC_API_KEY", "secret"), ("BROKEN_KEY", "secret")],
    );
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("anthropic_messages (anthropic_version: 2023-06-01)"),
        "{output}"
    );
    assert!(output.contains("unknown wire_api \"grpc\""), "{output}");
}

#[test]
fn doctor_rejects_anthropic_options_on_the_responses_wire() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        r#"
[providers.alpha]
base_url = "https://alpha.example/v1"
env_key = "ALPHA_KEY"
wire_api = "responses"
anthropic_version = "2023-06-01"
"#,
    );
    let (code, output) = run_doctor(home.path(), &[], &[("ALPHA_KEY", "secret")]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("sets adapter options but wire_api is"),
        "{output}"
    );
}

#[test]
fn doctor_reports_default_anthropic_version() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        r#"
[providers.claude]
base_url = "https://api.anthropic.com/v1"
env_key = "ANTHROPIC_API_KEY"
wire_api = "anthropic_messages"
"#,
    );
    let (code, output) = run_doctor(home.path(), &[], &[("ANTHROPIC_API_KEY", "secret")]);
    assert_eq!(code, 0, "{output}");
    assert!(
        output.contains("anthropic_messages (anthropic_version: adapter default)"),
        "{output}"
    );
}

#[test]
fn doctor_detects_cross_provider_alias_and_credential_collisions() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        r#"
[providers.alpha]
base_url = "https://alpha.example/v1"
env_key = "SHARED_KEY"

[[providers.alpha.models]]
id = "alpha-model"
alias = "shared"

[providers.beta]
base_url = "https://beta.example/v1"
env_key = "SHARED_KEY"

[[providers.beta.models]]
id = "beta-model"
alias = "shared"
"#,
    );
    let (code, output) = run_doctor(home.path(), &[], &[("SHARED_KEY", "secret")]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("model alias \"shared\" is used by both provider"),
        "{output}"
    );
    assert!(output.contains("share env_key SHARED_KEY"), "{output}");
    assert!(
        output.contains("same credential would be sent to multiple origins"),
        "{output}"
    );
}

#[test]
fn doctor_checks_the_claude_example_and_its_brave_credential() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        include_str!("../examples/claude-providers.toml"),
    );
    let provider_env = ("ANTHROPIC_API_KEY", "fixture-anthropic");
    let (code, output) = run_doctor(home.path(), &[], &[provider_env]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("credential reference BRAVE_SEARCH_API_KEY is not set"),
        "{output}"
    );

    let (code, output) = run_doctor(
        home.path(),
        &["--strict"],
        &[provider_env, ("BRAVE_SEARCH_API_KEY", "fixture-brave")],
    );
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("Brave Search: up to 8 results"), "{output}");
    assert!(
        output.contains("model alias: anthropic/claude-sonnet-5"),
        "{output}"
    );
    assert!(!output.contains("fixture-brave"), "{output}");
}

#[test]
fn doctor_reports_the_selected_profile_blocking_brave() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        include_str!("../examples/claude-providers.toml"),
    );
    let env = [
        ("ANTHROPIC_API_KEY", "fixture-anthropic"),
        ("BRAVE_SEARCH_API_KEY", "fixture-brave"),
    ];
    home.write(
        "config.toml",
        "profile = 'work'\nweb_search = 'live'\n[profiles.work]\nweb_search = 'cached'\n",
    );
    let (code, output) = run_doctor(home.path(), &["--strict"], &env);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("web_search = \"cached\" keeps Brave search off"),
        "{output}"
    );
    assert!(
        output.contains("ox codex -c 'web_search=\"live\"'"),
        "{output}"
    );

    home.write(
        "config.toml",
        "profile = 'work'\nweb_search = 'cached'\n[profiles.work]\nweb_search = 'live'\n",
    );
    let (code, output) = run_doctor(home.path(), &["--strict"], &env);
    assert_eq!(code, 0, "{output}");
}

#[test]
fn doctor_warns_when_discovery_is_on_but_static_models_are_absent() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        r#"
[providers.alpha]
base_url = "https://alpha.example/v1"
env_key = "ALPHA_KEY"
"#,
    );
    let (code, output) = run_doctor(home.path(), &[], &[("ALPHA_KEY", "secret")]);
    assert_eq!(code, 0, "{output}");
    assert!(
        output.contains("the catalog depends on discovery at launch"),
        "doctor should explain that the catalog depends on discovery: {output}"
    );
    assert!(
        output.contains("doctor does not contact the provider"),
        "{output}"
    );
}

#[test]
fn doctor_detects_duplicate_static_model_ids() {
    let home = TempHome::new();
    home.write(
        "codeox.toml",
        r#"
[providers.alpha]
base_url = "https://alpha.example/v1"
env_key = "ALPHA_KEY"

[[providers.alpha.models]]
id = "same"

[[providers.alpha.models]]
id = "same"
"#,
    );
    let (code, output) = run_doctor(home.path(), &[], &[("ALPHA_KEY", "secret")]);
    assert_eq!(code, 0, "{output}");
    assert!(
        output.contains("listed more than once; the first entry wins"),
        "{output}"
    );
}

#[test]
fn doctor_legacy_mode_checks_config_toml() {
    let home = TempHome::new();
    home.write(
        "config.toml",
        r#"
model = "demo"
model_provider = "demo"

[model_providers.demo]
name = "Demo"
base_url = "https://demo.example/v1"
env_key = "DEMO_KEY"
wire_api = "responses"
"#,
    );
    let (code, output) = run_doctor(home.path(), &[], &[("DEMO_KEY", "secret")]);
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("legacy (no codeox.toml"), "{output}");
    assert!(output.contains("selected provider: demo"), "{output}");
    assert!(
        output.contains("credentials: DEMO_KEY is available to ox"),
        "{output}"
    );
    assert!(output.contains("legacy relay can start"), "{output}");
    assert!(!output.contains("secret"), "{output}");

    let (code, output) = run_doctor(home.path(), &[], &[]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("credential reference DEMO_KEY is not set"),
        "{output}"
    );
}

#[test]
fn doctor_legacy_mode_reports_catalog_and_wire_conflicts() {
    let home = TempHome::new();
    home.write(
        "config.toml",
        r#"
model_catalog_json = "/tmp/catalog.json"
model_provider = "demo"

[model_providers.demo]
base_url = "https://demo.example/v1"
env_key = "DEMO_KEY"
wire_api = "chat_completions"
"#,
    );
    let (code, output) = run_doctor(home.path(), &[], &[("DEMO_KEY", "secret")]);
    assert_eq!(code, 1, "{output}");
    assert!(output.contains("model_catalog_json is set"), "{output}");
    assert!(
        output.contains("legacy mode supports wire_api = \"responses\" only"),
        "{output}"
    );
}

#[test]
fn doctor_reports_missing_home_configs_without_panicking() {
    let home = TempHome::new();
    let (code, output) = run_doctor(home.path(), &[], &[]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("neither codeox.toml nor config.toml exists"),
        "{output}"
    );
}

#[test]
fn doctor_rejects_unknown_flags_with_usage_exit_code() {
    let home = TempHome::new();
    let (code, output) = run_doctor(home.path(), &["--online"], &[]);
    assert_eq!(code, 2, "{output}");
    assert!(output.contains("unknown doctor option"), "{output}");
    assert!(output.contains("offline"), "{output}");
}

// ---------------------------------------------------------------------------
// doctor: presentation, next step, and self-repair
// ---------------------------------------------------------------------------

#[test]
fn doctor_marks_each_line_and_ends_with_the_next_command() {
    let home = TempHome::new();
    let (code, output) = run_doctor(home.path(), &[], &[]);
    assert_eq!(code, 1, "{output}");
    assert!(output.contains("  ✗ neither codeox.toml"), "{output}");
    assert!(output.trim_end().ends_with("next: ox setup"), "{output}");
    assert!(
        !output.contains('\x1b'),
        "plain output has no colour: {output}"
    );

    home.write("codeox.toml", ALPHA_CONFIG);
    let (code, output) = run_doctor(home.path(), &[], &[]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("  ✗ credential reference ALPHA_KEY is not set"),
        "{output}"
    );
    assert!(
        output.contains("next: ox setup   (choose the same service again to paste its key)"),
        "{output}"
    );

    let (code, output) = run_doctor(home.path(), &[], &[("ALPHA_KEY", "fixture-value")]);
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("\nclients\n"), "{output}");
    assert!(output.contains("\nprovider alpha\n"), "{output}");
    assert!(
        output.contains("  ✓ credentials: ALPHA_KEY is available"),
        "{output}"
    );
    assert!(output.contains("  ✓ validation: ok"), "{output}");
    assert!(output.contains("  · wire: responses"), "{output}");
    let last = output.trim_end().lines().last().unwrap();
    assert!(last.starts_with("next: "), "{output}");
    assert!(
        !output.contains("info:") && !output.contains("error:"),
        "{output}"
    );
}

#[test]
fn find_on_path_returns_the_first_matching_file() {
    let first = TempHome::new();
    let second = TempHome::new();
    second.write("codex", "");
    let path = std::env::join_paths([first.path(), second.path()]).unwrap();
    assert_eq!(
        setup::find_on_path("codex", Some(path.as_os_str())),
        Some(second.path().join("codex"))
    );
    assert_eq!(setup::find_on_path("claude", Some(path.as_os_str())), None);
    assert_eq!(setup::find_on_path("codex", None), None);
}

#[test]
fn doctor_fix_moves_unusable_files_aside_and_is_idempotent() {
    let home = TempHome::new();
    home.write("codeox.toml", "[providers.alpha\n");
    home.write("ox-favorites.json", "[1,");

    let (code, output) = run_doctor(home.path(), &[], &[]);
    assert_eq!(code, 1, "{output}");
    assert!(output.contains("could not parse codeox.toml"), "{output}");
    assert!(
        output.trim_end().ends_with("next: ox doctor --fix"),
        "{output}"
    );
    assert!(
        home.exists("codeox.toml"),
        "a plain doctor run changes nothing"
    );

    let (code, output) = run_doctor(home.path(), &["--fix"], &[]);
    assert_eq!(code, 1, "{output}");
    assert!(output.starts_with("repairs\n"), "{output}");
    assert!(
        output.contains("fixed: codeox.toml could not be used"),
        "{output}"
    );
    assert!(
        output.contains("fixed: ox-favorites.json could not be read"),
        "{output}"
    );
    assert!(output.trim_end().ends_with("next: ox setup"), "{output}");
    assert!(!home.exists("codeox.toml") && !home.exists("ox-favorites.json"));
    let backups: Vec<String> = fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.contains(".broken-"))
        .collect();
    assert_eq!(backups.len(), 2, "{backups:?}");
    let config_backup = backups
        .iter()
        .find(|name| name.starts_with("codeox.toml.broken-"))
        .unwrap();
    assert_eq!(home.read(config_backup), "[providers.alpha\n");

    let (_, output) = run_doctor(home.path(), &["--fix"], &[]);
    assert!(output.contains("nothing needed repair"), "{output}");
}

#[test]
fn doctor_fix_leaves_a_valid_setup_untouched() {
    let home = TempHome::new();
    home.write("codeox.toml", ALPHA_CONFIG);
    home.write("ox-favorites.json", "[\"alpha/alpha-coder\"]");
    home.write(".codeox.toml.tmp-0011223344556677", "fresh temporary file");
    let (code, output) = run_doctor(home.path(), &["--fix"], &[("ALPHA_KEY", "fixture-value")]);
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("nothing needed repair"), "{output}");
    assert_eq!(home.read("codeox.toml"), ALPHA_CONFIG);
    assert_eq!(home.read("ox-favorites.json"), "[\"alpha/alpha-coder\"]");
    assert!(home.exists(".codeox.toml.tmp-0011223344556677"));
}

#[cfg(unix)]
#[test]
fn saved_key_file_is_repaired_or_reported_but_never_fatal() {
    use std::os::unix::fs::PermissionsExt;

    let home = TempHome::new();
    home.write("codeox.toml", ALPHA_CONFIG);
    let keys = home.path().join("ox-api-keys.json");
    home.write(
        "ox-api-keys.json",
        r#"{"ALPHA_KEY":{"scope":"https://alpha.example/v1","value":"fixture-saved-value"}}"#,
    );
    fs::set_permissions(&keys, fs::Permissions::from_mode(0o644)).unwrap();
    let loaded = provider::SavedKeys::load(home.path()).unwrap();
    assert_eq!(
        loaded.get("ALPHA_KEY").as_deref(),
        Some("fixture-saved-value")
    );
    assert_eq!(
        fs::metadata(&keys).unwrap().permissions().mode() & 0o777,
        0o600
    );

    home.write("ox-api-keys.json", "{not json");
    let (code, output) = run_doctor(home.path(), &[], &[("ALPHA_KEY", "fixture-value")]);
    assert_eq!(code, 1, "{output}");
    assert!(
        output.contains("\nsaved keys\n  ✗ could not parse ox-api-keys.json"),
        "{output}"
    );
    assert!(
        output.trim_end().ends_with("next: ox doctor --fix"),
        "{output}"
    );

    let (code, output) = run_doctor(home.path(), &["--fix"], &[("ALPHA_KEY", "fixture-value")]);
    assert_eq!(code, 0, "{output}");
    assert!(
        output.contains("fixed: ox-api-keys.json could not be read"),
        "{output}"
    );
    assert!(!home.exists("ox-api-keys.json"));
    assert!(!output.contains("fixture-saved-value") && !output.contains("fixture-value"));
}

// Codex bundled catalog diagnostics

#[test]
fn catalog_failure_keeps_its_first_line_and_explains_the_cause() {
    let home = TempHome::new();
    let message = setup::codex_catalog_failure(
        "`codex debug models --bundled` exited with status 2",
        b"\x1b[31merror:\x1b[0m unrecognized subcommand 'models'\n\nUsage: codex debug <COMMAND>\n",
        Some(Path::new("/usr/local/bin/codex")),
        home.path(),
        Some("codex-cli 0.172.0"),
    );
    assert!(
        message.starts_with("could not read the bundled Codex model catalog\n  why: "),
        "{message}"
    );
    assert!(message.contains("exited with status 2"), "{message}");
    assert!(message.contains("codex: /usr/local/bin/codex"), "{message}");
    assert!(
        message.contains("unrecognized subcommand 'models'"),
        "{message}"
    );
    assert!(
        !message.contains('\x1b'),
        "control characters must be dropped: {message:?}"
    );
    assert!(message.contains("\n  fix: "), "{message}");
    assert!(message.contains(setup::min_codex()), "{message}");
    assert!(message.contains("config.toml"), "{message}");
    // render_error must not add a second, generic fix line.
    let rendered = ui::render_error("ox", &message, false);
    assert_eq!(rendered.matches("fix:").count(), 1, "{rendered}");
}

#[test]
fn stderr_tail_keeps_the_last_lines_and_caps_the_size() {
    let many = (1..=20).map(|n| format!("line {n}\n")).collect::<String>();
    let tail = setup::stderr_tail(many.as_bytes()).unwrap();
    assert!(
        tail.starts_with("line 16") && tail.ends_with("line 20"),
        "{tail}"
    );
    assert!(!tail.contains("line 15"), "{tail}");

    let long = format!("{}\n", "é".repeat(2000));
    let tail = setup::stderr_tail(long.as_bytes()).unwrap();
    assert!(tail.starts_with('…') && tail.len() <= 504, "{}", tail.len());

    assert_eq!(setup::stderr_tail(b" \n\t\n"), None);
}

#[cfg(unix)]
#[test]
fn doctor_probe_reports_a_codex_without_the_bundled_catalog() {
    use std::os::unix::fs::PermissionsExt;

    let home = TempHome::new();
    let fake = |name: &str, body: &str| {
        let path = home.path().join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    };

    let old = fake(
        "codex-old",
        "[ \"$1\" = --version ] && { echo 'codex-cli 0.98.0'; exit 0; }\necho \"error: unrecognized subcommand 'models'\" >&2; exit 2",
    );
    let error = setup::probe_codex_catalog(&old, home.path()).unwrap_err();
    assert!(
        error.starts_with("Codex 0.98.0 is older than the required"),
        "{error}"
    );
    assert!(error.contains("exited with status 2"), "{error}");
    assert!(
        error.contains("unrecognized subcommand 'models'"),
        "{error}"
    );

    let garbled = fake("codex-garbled", "echo not-json");
    let error = setup::probe_codex_catalog(&garbled, home.path()).unwrap_err();
    assert!(error.contains("could not parse"), "{error}");

    // Larger than a pipe buffer, so a probe that waits before reading would hang.
    let good = fake(
        "codex-good",
        "printf '{\"models\":['; i=0; while [ $i -lt 4000 ]; do printf '{\"slug\":\"m%s\",\"pad\":\"xxxxxxxxxxxxxxxxxxxx\"},' $i; i=$((i+1)); done; printf '{\"slug\":\"last\"}]}'",
    );
    assert_eq!(setup::probe_codex_catalog(&good, home.path()), Ok(()));
}

#[cfg(unix)]
#[test]
fn find_all_on_path_lists_each_binary_once_in_order() {
    use std::os::unix::fs::PermissionsExt;

    let home = TempHome::new();
    let first = home.path().join("first");
    let second = home.path().join("second");
    let alias = home.path().join("alias");
    for dir in [&first, &second] {
        fs::create_dir_all(dir).unwrap();
        let bin = dir.join("codex");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::os::unix::fs::symlink(&first, &alias).unwrap();
    let path = std::env::join_paths([&first, &alias, &second]).unwrap();
    let found = setup::find_all_on_path("codex", Some(&path));
    assert_eq!(found, vec![first.join("codex"), second.join("codex")]);
    assert_eq!(
        setup::find_on_path("codex", Some(&path)),
        Some(first.join("codex"))
    );
}

#[test]
fn upgrade_command_matches_how_codex_was_installed() {
    let brew = Path::new("/opt/homebrew/Caskroom/codex/0.170.0/codex");
    assert_eq!(
        setup::codex_upgrade_command(Some(brew)),
        "brew upgrade --cask codex"
    );
    let mise = Path::new("/Users/eli/.local/share/mise/installs/codex/0.170.0/codex");
    assert_eq!(
        setup::codex_upgrade_command(Some(mise)),
        "mise use --global codex@latest"
    );
    let npm = Path::new("/usr/local/lib/node_modules/@openai/codex/bin/codex.js");
    assert_eq!(
        setup::codex_upgrade_command(Some(npm)),
        "npm install -g @openai/codex@latest"
    );
}

#[test]
fn an_old_codex_gets_the_desktop_wording_and_the_gate_floor() {
    let gate = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("desktop/gate.toml"))
        .unwrap();
    assert!(
        gate.contains(&format!("minimum_version = \"{}\"", setup::min_codex())),
        "the CLI floor must be the desktop [codex] floor"
    );
    assert_eq!(setup::codex_too_old("codex-cli 0.171.0"), None);
    assert_eq!(setup::codex_too_old("codex-cli 1.0.0"), None);
    assert_eq!(setup::codex_too_old("not a version"), None);
    let old = setup::codex_too_old("codex-cli 0.98.2").unwrap();
    assert_eq!(
        old,
        format!(
            "Codex 0.98.2 is older than the required {}.",
            setup::min_codex()
        )
    );

    let home = TempHome::new();
    let message = setup::codex_catalog_failure(
        "`codex debug models --bundled` exited with status 2",
        b"error: unrecognized subcommand 'models'",
        Some(Path::new("/opt/homebrew/Caskroom/codex/0.98.2/codex")),
        home.path(),
        Some("codex-cli 0.98.2"),
    );
    // OX Desktop matches this first line to offer its Update button.
    assert!(message.starts_with(&old), "{message}");
    assert!(message.contains("unrecognized subcommand"), "{message}");
    assert!(
        message.contains("\n  fix: brew upgrade --cask codex"),
        "{message}"
    );
}

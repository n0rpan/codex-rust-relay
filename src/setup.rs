//! Interactive and scriptable provider setup, plus offline `ox doctor`.
//!
//! `ox setup` and `ox provider add` edit `codeox.toml` with a narrow scope:
//!
//! * only `[providers.<key>]` and its `[[providers.<key>.models]]` tables are
//!   added; every existing byte, comment and top-level key is preserved;
//! * duplicate provider keys are refused instead of merged or overwritten;
//! * edits are serialized, validated with [`provider::CodeoxConfig`], and
//!   saved with a same-directory temporary file plus rename;
//! * the provider registry contains credential references. Guided setup can
//!   save a key in a separate private file using a hidden terminal prompt.
//!
//! `doctor` is offline: it reads `codeox.toml` and `config.toml`, checks the
//! environment for credential references, and makes no network requests. It
//! never opens `auth.json`.
//!
//! `wire_api = "chat_completions"` and `"anthropic_messages"` are written when
//! requested, and the whole candidate is validated by the provider module's
//! own parser, so `provider add` cannot write a configuration that startup
//! would reject. Anthropic version headers are emitted as
//! `anthropic_version = "..."` only when explicitly requested; otherwise the
//! field is omitted and the adapter's default applies.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use toml_edit::{Document, Item, TableLike};

use crate::provider::{
    CodeoxConfig, MAX_CONFIG_BYTES, PROVIDER_ID, RelayError, SavedKeys, alias_for,
    credential_env_error, looks_like_credential,
};

/// Narrow integration seam for `main.rs`.
///
/// Returns `None` when `args` do not name a setup or diagnostic command, so the
/// relay launch path is unchanged. `Some(result)` means the command ran (or
/// failed) and `main` should return that exit code instead of starting Codex.
pub fn dispatch(args: &[OsString], codex_home: &Path) -> Option<Result<u8, RelayError>> {
    let command = args.first()?.to_str()?;
    match command {
        "provider" => Some(provider_command(&args[1..], codex_home)),
        "doctor" => Some(doctor_command(&args[1..], codex_home)),
        _ => None,
    }
}

fn provider_command(args: &[OsString], codex_home: &Path) -> Result<u8, RelayError> {
    if args.first().is_some_and(|arg| arg == "add") {
        if args.len() == 1 && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            guided_provider(codex_home)?;
            return Ok(0);
        }
        return provider_add_command(&args[1..], codex_home, false);
    }

    let mut output = std::io::stdout().lock();
    match args.first().and_then(|arg| arg.to_str()) {
        Some("-h" | "--help") => {
            write_provider_usage(&mut output)?;
            Ok(0)
        }
        None => {
            write_provider_usage(&mut output)?;
            Ok(2)
        }
        Some(other) => {
            writeln!(output, "error: unknown provider command {other:?}")?;
            writeln!(output)?;
            write_provider_usage(&mut output)?;
            Ok(2)
        }
    }
}

pub(crate) fn run_setup(args: &[OsString], codex_home: &Path) -> Result<u8, RelayError> {
    provider_add_command(args, codex_home, true)
}

pub(crate) enum SetupChoice {
    Subscription(&'static str),
    Provider,
    Brave,
    Done,
}

pub(crate) fn choose_setup() -> Result<SetupChoice, RelayError> {
    let mut prompt = Prompt {
        input: &mut std::io::stdin().lock(),
        output: &mut std::io::stdout().lock(),
    };
    writeln!(
        prompt.output,
        "\nox setup\n\
         \x20 1. Sign in with ChatGPT   use your ChatGPT plan in Codex\n\
         \x20 2. Sign in with Claude    use your Claude plan in Claude Code\n\
         \x20 3. Add an API key         OpenRouter, Anthropic, OpenAI, or any compatible endpoint\n\
         \x20 4. Enable web search      Brave Search for both clients\n\
         \x20 5. Done"
    )?;
    prompt.ask_required_with("Choose 1–5", None, |choice| match choice {
        "1" => Ok(SetupChoice::Subscription("chatgpt")),
        "2" => Ok(SetupChoice::Subscription("claude")),
        "3" => Ok(SetupChoice::Provider),
        "4" => Ok(SetupChoice::Brave),
        "5" => Ok(SetupChoice::Done),
        _ => Err("enter a number from 1 to 5".to_owned()),
    })
}

struct ProviderPreset {
    id: &'static str,
    name: &'static str,
    url: Option<&'static str>,
    wire: WireKind,
    env: &'static str,
}

const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "opencode-go",
        name: "OpenCode Go",
        url: Some("https://opencode.ai/inference/go/openai/v1"),
        wire: WireKind::ChatCompletions,
        env: "OPENCODE_API_KEY",
    },
    ProviderPreset {
        id: "openrouter",
        name: "OpenRouter",
        url: Some("https://openrouter.ai/api/v1"),
        wire: WireKind::Responses,
        env: "OPENROUTER_API_KEY",
    },
    ProviderPreset {
        id: "anthropic",
        name: "Anthropic",
        url: Some("https://api.anthropic.com/v1"),
        wire: WireKind::AnthropicMessages,
        env: "ANTHROPIC_API_KEY",
    },
    ProviderPreset {
        id: "openai",
        name: "OpenAI",
        url: Some("https://api.openai.com/v1"),
        wire: WireKind::Responses,
        env: "OPENAI_API_KEY",
    },
    ProviderPreset {
        id: "alibaba",
        name: "Alibaba Cloud",
        url: None,
        wire: WireKind::ChatCompletions,
        env: "ALIBABA_API_KEY",
    },
];

/// Templates use the same provider IDs, wire formats and key references as terminal setup.
pub(crate) fn desktop_presets() -> Vec<(String, String)> {
    let mut templates: Vec<_> = PRESETS
        .iter()
        .map(|preset| {
            let url = preset.url.unwrap_or("https://YOUR-ENDPOINT/v1");
            let mut text = format!(
                "[providers.{}]\nbase_url = {}\nwire_api = {}\nenv_key = {}\ndiscovery = true\n",
                preset.id,
                toml_string(url),
                toml_string(preset.wire.as_str()),
                toml_string(preset.env)
            );
            if preset.id == "opencode-go" {
                text.push_str("models_url = \"https://opencode.ai/inference/go/v1/models\"\n");
            }
            if preset.id == "anthropic" {
                text.push_str("anthropic_version = \"2026-10-21\"\n");
            }
            (preset.name.into(), text)
        })
        .collect();
    templates.push(("Custom".into(),"[providers.custom]\nbase_url = \"http://127.0.0.1:8000/v1\"\nwire_api = \"chat_completions\"\nenv_key = \"CUSTOM_API_KEY\"\ndiscovery = false\n\n[[providers.custom.models]]\nid = \"my-model\"\n".into()));
    templates.push(("Brave Search".into(),"[search]\nenabled = true\nenv_key = \"BRAVE_SEARCH_API_KEY\"\nmax_results = 8\ntimeout_seconds = 30\n".into()));
    templates
}

/// Only seed routes whose credential name and endpoint identify a built-in service.
pub(crate) fn transferred_provider(name: &str, scope: &str) -> Option<(&'static str, String)> {
    let preset = PRESETS.iter().find(|preset| preset.env == name)?;
    let url = reqwest::Url::parse(scope).ok()?;
    let wire = match preset.url {
        Some(expected) if scope == expected => preset.wire,
        None if preset.id == "alibaba" && url.scheme() == "https" && url.port().is_none() => {
            let catalog = crate::provider::alibaba_chat_catalog_path(url.host_str()?)?;
            if url.as_str() != scope {
                return None;
            }
            if url.path() == catalog.strip_suffix("/models")? {
                WireKind::ChatCompletions
            } else if url.path() == "/apps/anthropic/v1" {
                WireKind::AnthropicMessages
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let section = render_provider_section(&ProviderForm {
        key: preset.id.into(),
        name: Some(preset.name.into()),
        base_url: scope.into(),
        wire,
        env_key: Some(preset.env.into()),
        models_url: (preset.id == "opencode-go")
            .then(|| "https://opencode.ai/inference/go/v1/models".into()),
        anthropic_version: (preset.id == "anthropic").then(|| "2026-10-21".into()),
        discovery: Some(true),
        models: if preset.id == "alibaba" {
            vec![ModelForm {
                id: "qwen3.8-flash".into(),
                ..ModelForm::default()
            }]
        } else {
            Vec::new()
        },
    });
    Some((preset.id, section))
}

pub(crate) fn guided_provider(home: &Path) -> Result<(), RelayError> {
    let mut prompt = Prompt {
        input: &mut std::io::stdin().lock(),
        output: &mut std::io::stdout().lock(),
    };
    writeln!(prompt.output, "\nWhich service issued your API key?")?;
    for (index, preset) in PRESETS.iter().enumerate() {
        writeln!(prompt.output, "  {}. {}", index + 1, preset.name)?;
    }
    let other = PRESETS.len() + 1;
    writeln!(prompt.output, "  {other}. Other or self-hosted")?;
    writeln!(prompt.output, "  0. Back")?;
    let choice = prompt.ask_required_with("Service number", None, |answer| {
        answer
            .parse::<usize>()
            .ok()
            .filter(|number| *number <= other)
            .ok_or_else(|| format!("choose a number from 0 to {other}"))
    })?;
    if choice == 0 {
        return Ok(());
    }

    let (id, name, url, wire, env) = if choice == other {
        let (url, wire) = prompt.ask_required_with(
            "Paste the API base URL or full inference endpoint",
            None,
            |raw| infer_connection(raw, WireKind::Responses),
        )?;
        let host = url.host_str().unwrap_or("local");
        let label = host.replace(|c: char| !c.is_ascii_alphanumeric(), "-");
        let port = url
            .port()
            .map(|port| format!("-{port}"))
            .unwrap_or_default();
        let id = format!("custom-{}{port}", &label[..label.len().min(46)]);
        let env = default_env_key(&id);
        writeln!(
            prompt.output,
            "Using {} for {host}. ox will look up models when you launch.",
            wire_label(wire)
        )?;
        (id, host.to_owned(), url, wire, env)
    } else {
        let preset = &PRESETS[choice - 1];
        let (url, wire) = if let Some(url) = preset.url {
            (reqwest::Url::parse(url)?, preset.wire)
        } else {
            prompt.ask_required_with(
                "Paste the Alibaba Cloud API URL from your account",
                None,
                |raw| {
                    let (url, wire) = infer_connection(raw, preset.wire)?;
                    if !url.host_str().is_some_and(|host| host.ends_with(".aliyuncs.com")) {
                        return Err("use your Alibaba Cloud endpoint ending in .aliyuncs.com, or choose Other for a custom gateway".to_owned());
                    }
                    Ok((url, wire))
                },
            )?
        };
        (
            preset.id.to_owned(),
            preset.name.to_owned(),
            url,
            wire,
            preset.env.to_owned(),
        )
    };

    let saved = save_guided_route(home, &mut prompt, id.clone(), &name, url, wire, env)?;
    if saved && choice == other {
        writeln!(
            prompt.output,
            "One endpoint works with both ox codex and ox claude. ox translates when the formats differ."
        )?;
        let mut configured = vec![wire];
        while configured.len() < 3
            && prompt.ask_bool("Add another API endpoint for this provider", false)?
        {
            let (url, wire) = prompt.ask_required_with(
                "Paste the full inference URL ending in /messages, /responses or /chat/completions",
                None,
                |raw| {
                    let (url, wire) = infer_connection(raw, WireKind::Responses)?;
                    if configured.contains(&wire) {
                        return Err("choose an endpoint for a different API format".to_owned());
                    }
                    Ok((url, wire))
                },
            )?;
            let route_id = custom_route_id(&id, wire);
            let env = default_env_key(&route_id);
            writeln!(
                prompt.output,
                "Adding {route_id} using {}.",
                wire_label(wire)
            )?;
            if !save_guided_route(home, &mut prompt, route_id, &name, url, wire, env)? {
                break;
            }
            configured.push(wire);
        }
    }
    Ok(())
}

/// Asks for the API key of each configured provider that has none, using the
/// hidden prompt. Returns true when at least one key was saved.
pub(crate) fn prompt_missing_keys(home: &Path) -> Result<bool, RelayError> {
    let path = CodeoxConfig::path(home);
    let config = CodeoxConfig::load(&path)?;
    let keys = SavedKeys::for_config(home, &config)?;
    let mut prompt = Prompt {
        input: &mut std::io::stdin().lock(),
        output: &mut std::io::stdout().lock(),
    };
    let mut saved_any = false;
    for vendor in &config.vendors {
        let Some(env) = vendor.env_key.as_deref() else {
            continue;
        };
        if credential_state(&|name| keys.get(name), env) == CredentialState::Present {
            continue;
        }
        writeln!(prompt.output, "{} has no API key yet.", vendor.name)?;
        let Some(key) = prompt.guided_key(&vendor.name, env)? else {
            continue;
        };
        let _lock = lock_config(home)?;
        let current = CodeoxConfig::load(&path)?;
        save_key(home, &current, env, vendor.base_url.as_str(), key)?;
        writeln!(
            prompt.output,
            "Saved the {} key on this device.",
            vendor.name
        )?;
        saved_any = true;
    }
    Ok(saved_any)
}

fn custom_route_id(id: &str, wire: WireKind) -> String {
    let suffix = match wire {
        WireKind::AnthropicMessages => "-messages",
        WireKind::ChatCompletions => "-chat",
        WireKind::Responses => "-responses",
    };
    format!("{}{suffix}", &id[..id.len().min(64 - suffix.len())])
}

fn save_guided_route(
    home: &Path,
    prompt: &mut Prompt<'_>,
    mut id: String,
    name: &str,
    url: reqwest::Url,
    wire: WireKind,
    mut env: String,
) -> Result<bool, RelayError> {
    let path = CodeoxConfig::path(home);
    let existing = read_existing_codeox(&path)?
        .as_deref()
        .map(parse_config_for_validation)
        .transpose()?;
    if id == "alibaba"
        && existing.as_ref().is_some_and(|config| {
            config
                .vendors
                .iter()
                .any(|vendor| vendor.key == id && vendor.wire.as_str() != wire.as_str())
        })
    {
        id = match wire {
            WireKind::AnthropicMessages => "alibaba-messages",
            _ => "alibaba-chat",
        }
        .to_owned();
        env = default_env_key(&id);
        writeln!(
            prompt.output,
            "Adding {id} alongside your existing Alibaba route."
        )?;
    } else if id.starts_with("custom-")
        && existing.as_ref().is_some_and(|config| {
            config
                .vendors
                .iter()
                .any(|vendor| vendor.key == id && vendor.wire.as_str() != wire.as_str())
        })
    {
        id = custom_route_id(&id, wire);
        env = default_env_key(&id);
    }
    let current = existing
        .as_ref()
        .and_then(|config| config.vendors.iter().find(|v| v.key == id));
    let env = if let Some(vendor) = current {
        if vendor.base_url != url || vendor.wire.as_str() != wire.as_str() {
            return Err(format!("provider {id:?} already uses a different connection. Check its existing settings in codeox.toml").into());
        }
        vendor.env_key.clone().ok_or(
            "this provider has no env_key. Set an env_key in codeox.toml before saving a key",
        )?
    } else {
        env
    };
    let key = prompt.guided_key(name, &env)?;
    if current.is_none() {
        let mut args: Vec<_> = [
            id.as_str(),
            "--name",
            name,
            "--base-url",
            url.as_str(),
            "--wire",
            wire.as_str(),
            "--env-key",
            &env,
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        if id == "anthropic" {
            args.extend(["--anthropic-version", "2026-10-21"].map(OsString::from));
        }
        if url.scheme() == "https"
            && url.host_str() == Some("opencode.ai")
            && url.port_or_known_default() == Some(443)
            && matches!(
                url.path(),
                "/inference/go/openai/v1" | "/inference/go/anthropic/v1"
            )
        {
            args.extend(
                ["--models-url", "https://opencode.ai/inference/go/v1/models"].map(OsString::from),
            );
        }
        if matches!(id.as_str(), "alibaba" | "alibaba-messages" | "alibaba-chat") {
            args.extend(["--model", "qwen3.8-flash"].map(OsString::from));
        }
        let mut report = Vec::new();
        let code = run_provider_add(
            &args,
            home,
            prompt.input,
            &mut report,
            false,
            &crate::provider::process_env,
        )?;
        if code != 0 {
            prompt.output.write_all(&report)?;
            return Ok(false);
        }
    }

    let _lock = lock_config(home)?;
    let config = CodeoxConfig::load(&path)?;
    if let Some(key) = key {
        if !config.vendors.iter().any(|vendor| {
            vendor.key == id
                && vendor.base_url == url
                && vendor.wire.as_str() == wire.as_str()
                && vendor.env_key.as_deref() == Some(env.as_str())
        }) {
            return Err(
                "provider settings changed during setup. Choose the service again to save its key"
                    .into(),
            );
        }
        save_key(home, &config, &env, url.as_str(), key)?;
        writeln!(prompt.output, "Saved the {name} key on this device.")?;
    }
    let keys = SavedKeys::for_config(home, &config)?;
    if credential_state(&|name| keys.get(name), &env) == CredentialState::Present {
        writeln!(
            prompt.output,
            "{name} is ready to try in both ox codex and ox claude."
        )?;
    } else {
        writeln!(
            prompt.output,
            "{name} needs a key. Choose it again to paste one, or load ${env}."
        )?;
    }
    Ok(true)
}

fn infer_connection(raw: &str, default_wire: WireKind) -> Result<(reqwest::Url, WireKind), String> {
    let raw = validate_base_url(raw)?;
    let mut url = reqwest::Url::parse(&raw).map_err(|_| "enter a valid API URL".to_owned())?;
    let path = url.path().trim_end_matches('/');
    let (base, wire) = if let Some(base) = path.strip_suffix("/chat/completions") {
        (base, WireKind::ChatCompletions)
    } else if let Some(base) = path.strip_suffix("/messages") {
        (base, WireKind::AnthropicMessages)
    } else if let Some(base) = path.strip_suffix("/responses") {
        (base, WireKind::Responses)
    } else if url.host_str() == Some("api.anthropic.com")
        || path.ends_with("/apps/anthropic")
        || path.ends_with("/apps/anthropic/v1")
        || path == "/inference/go/anthropic/v1" && url.host_str() == Some("opencode.ai")
    {
        (path, WireKind::AnthropicMessages)
    } else if path.ends_with("/compatible-mode/v1")
        || path == "/inference/go/openai/v1" && url.host_str() == Some("opencode.ai")
    {
        (path, WireKind::ChatCompletions)
    } else {
        (path, default_wire)
    };
    let base = if base.ends_with("/apps/anthropic") && wire == WireKind::AnthropicMessages {
        format!("{base}/v1")
    } else if base.is_empty() {
        "/v1".to_owned()
    } else {
        base.to_owned()
    };
    url.set_path(&base);
    Ok((url, wire))
}

fn wire_label(wire: WireKind) -> &'static str {
    match wire {
        WireKind::Responses => "OpenAI Responses",
        WireKind::ChatCompletions => "OpenAI Chat Completions",
        WireKind::AnthropicMessages => "Anthropic Messages",
    }
}

pub(crate) fn save_key(
    home: &Path,
    config: &CodeoxConfig,
    name: &str,
    scope: &str,
    value: String,
) -> Result<(), RelayError> {
    if !SavedKeys::scope_matches(config, name, scope) {
        return Err(format!(
            "${name} refers to more than one destination. Give each destination its own env_key in codeox.toml."
        ).into());
    }
    let mut keys = SavedKeys::load(home)?;
    let encoded = keys.set(name, scope, value)?;
    write_config_atomic(&SavedKeys::path(home), &encoded)
}

pub(crate) fn enable_brave(home: &Path) -> Result<(), RelayError> {
    let path = CodeoxConfig::path(home);
    let lock = lock_config(home)?;
    let Some(existing) = read_existing_codeox(&path)? else {
        println!(
            "Choose 3 to add an API provider first. Brave search runs through the API gateway."
        );
        return Ok(());
    };
    let config = parse_config_for_validation(&existing)?;
    let settings = if let Some(settings) = config.search {
        settings
    } else {
        let document = Document::parse(&existing).map_err(|error| error.message().to_owned())?;
        let candidate = if let Some(search) = document.get("search") {
            let span = search
                .get("enabled")
                .and_then(Item::span)
                .ok_or("could not find the disabled Brave setting in codeox.toml")?;
            let mut candidate = existing;
            candidate.replace_range(span, "true");
            candidate
        } else {
            append_section(&existing, "[search]\nenv_key = \"BRAVE_SEARCH_API_KEY\"\n")
        };
        let settings = parse_config_for_validation(&candidate)?
            .search
            .ok_or("could not enable Brave search in codeox.toml")?;
        write_config_atomic(&path, &candidate)?;
        settings
    };
    drop(lock);

    let mut prompt = Prompt {
        input: &mut std::io::stdin().lock(),
        output: &mut std::io::stdout().lock(),
    };
    let key = prompt.guided_key("Brave Search", &settings.env_key)?;
    let _lock = lock_config(home)?;
    let config = CodeoxConfig::load(&path)?;
    if let Some(key) = key {
        if config.search.as_ref().map(|search| search.env_key.as_str())
            != Some(settings.env_key.as_str())
        {
            return Err(
                "Brave settings changed during setup. Choose Brave again to save its key".into(),
            );
        }
        save_key(
            home,
            &config,
            &settings.env_key,
            SavedKeys::BRAVE_SCOPE,
            key,
        )?;
        writeln!(prompt.output, "Saved the Brave Search key on this device.")?;
    }
    let keys = SavedKeys::for_config(home, &config)?;
    if credential_state(&|name| keys.get(name), &settings.env_key) == CredentialState::Present {
        writeln!(
            prompt.output,
            "Brave web search is enabled for both API-backed clients."
        )?;
    } else {
        writeln!(
            prompt.output,
            "Brave search needs a key. Choose Brave again to paste one, or load ${}.",
            settings.env_key
        )?;
    }
    Ok(())
}

fn provider_add_command(
    args: &[OsString],
    codex_home: &Path,
    setup: bool,
) -> Result<u8, RelayError> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let interactive = stdin.is_terminal() && stdout.is_terminal();
    let mut output = stdout.lock();
    if setup && args.is_empty() {
        if !interactive {
            write_add_usage(&mut output)?;
            return Ok(0);
        }
        writeln!(output, "\nSet up an API provider for Codex or Claude Code.")?;
        writeln!(
            output,
            "The provider name becomes the first part of your model aliases, such as openrouter/model-id."
        )?;
        writeln!(
            output,
            "Keep the actual API key in your shell. Setup will ask for its variable name later.\n"
        )?;
    }
    run_provider_add(
        args,
        codex_home,
        &mut stdin.lock(),
        &mut output,
        interactive,
        &crate::provider::process_env,
    )
}

fn doctor_command(args: &[OsString], codex_home: &Path) -> Result<u8, RelayError> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    // A broken key file is reported by the doctor instead of ending it.
    let keys = match CodeoxConfig::load(&CodeoxConfig::path(codex_home)) {
        Ok(config) => SavedKeys::for_config(codex_home, &config).unwrap_or_default(),
        Err(_) => SavedKeys::default(),
    };
    let env = |name: &str| keys.get(name);
    let color = crate::ui::stdout_color();
    run_doctor_styled(args, codex_home, &mut output, &env, color)
}

// ---------------------------------------------------------------------------
// Wire kinds
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WireKind {
    Responses,
    ChatCompletions,
    AnthropicMessages,
}

impl WireKind {
    /// Parses the canonical value stored in `codeox.toml`.
    fn parse_config(raw: &str) -> Option<Self> {
        match raw.trim() {
            "" | "responses" => Some(Self::Responses),
            "chat_completions" => Some(Self::ChatCompletions),
            "anthropic_messages" => Some(Self::AnthropicMessages),
            _ => None,
        }
    }

    /// Parses a command-line value, accepting common aliases.
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "responses" | "response" => Some(Self::Responses),
            "chat" | "chat_completions" | "chat-completions" | "chatcompletions" => {
                Some(Self::ChatCompletions)
            }
            "anthropic" | "anthropic_messages" | "anthropic-messages" | "messages" => {
                Some(Self::AnthropicMessages)
            }
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }
}

fn wire_summary(wire: WireKind, anthropic_version: Option<&str>) -> String {
    match wire {
        WireKind::Responses => "responses".to_owned(),
        WireKind::ChatCompletions => "chat_completions".to_owned(),
        WireKind::AnthropicMessages => match anthropic_version {
            Some(version) => format!("anthropic_messages (anthropic_version: {version})"),
            None => "anthropic_messages (anthropic_version: adapter default)".to_owned(),
        },
    }
}

// ---------------------------------------------------------------------------
// Provider add: flags
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct ModelForm {
    id: String,
    alias: Option<String>,
    name: Option<String>,
    context_window: Option<i64>,
    input_modalities: Option<Vec<String>>,
    reasoning_efforts: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
struct ProviderForm {
    key: String,
    name: Option<String>,
    base_url: String,
    env_key: Option<String>,
    wire: WireKind,
    models_url: Option<String>,
    discovery: Option<bool>,
    anthropic_version: Option<String>,
    models: Vec<ModelForm>,
}

#[derive(Default)]
struct AddArgs {
    key: Option<String>,
    name: Option<String>,
    base_url: Option<String>,
    wire: Option<WireKind>,
    env_key: Option<String>,
    no_env_key: bool,
    discovery: Option<bool>,
    anthropic_version: Option<String>,
    models_url: Option<String>,
    models: Vec<ModelForm>,
    model_name: Option<String>,
    context_window: Option<i64>,
    alias: Option<String>,
    input_modalities: Option<Vec<String>>,
    reasoning_efforts: Option<Vec<String>>,
    help: bool,
}

fn parse_add_args(args: &[OsString]) -> Result<AddArgs, String> {
    let mut parsed = AddArgs::default();
    let mut iter = args.iter();
    let mut after_separator = false;

    while let Some(argument) = iter.next() {
        let argument = argument
            .to_str()
            .ok_or_else(|| "arguments must be valid UTF-8".to_owned())?;

        if after_separator {
            set_positional(&mut parsed, argument)?;
            continue;
        }
        if argument == "--" {
            after_separator = true;
            continue;
        }
        if argument == "-" || !argument.starts_with('-') {
            set_positional(&mut parsed, argument)?;
            continue;
        }

        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (argument, None),
        };
        let mut value = |flag: &str| -> Result<String, String> {
            if let Some(value) = inline.clone() {
                return Ok(value);
            }
            iter.next()
                .and_then(|value| value.to_str())
                .map(str::to_owned)
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        let no_value = |flag: &str| -> Result<(), String> {
            if inline.is_some() {
                Err(format!("{flag} does not take a value"))
            } else {
                Ok(())
            }
        };

        match flag {
            "-h" | "--help" => {
                no_value(flag)?;
                parsed.help = true;
            }
            "--provider-key" | "--provider_key" | "--id" => {
                let key = value(flag)?;
                if parsed.key.is_some() {
                    return Err("provider name was given more than once".to_owned());
                }
                parsed.key = Some(key);
            }
            "--name" => parsed.name = Some(value(flag)?),
            "--base-url" | "--base_url" => parsed.base_url = Some(value(flag)?),
            "--wire" | "--wire-api" | "--wire_api" => {
                let raw = value(flag)?;
                parsed.wire = Some(
                    WireKind::parse(&raw).ok_or_else(|| {
                        format!(
                            "unknown wire protocol {raw:?}; use responses, chat_completions or anthropic_messages"
                        )
                    })?,
                );
            }
            "--env-key" | "--env_key" | "--key-env" => parsed.env_key = Some(value(flag)?),
            "--no-env-key" | "--no_env_key" => {
                no_value(flag)?;
                parsed.no_env_key = true;
            }
            "--model" => parsed.models.push(ModelForm {
                id: value(flag)?,
                ..ModelForm::default()
            }),
            "--model-name" | "--model_name" => parsed.model_name = Some(value(flag)?),
            "--context-window" | "--context_window" => {
                let raw = value(flag)?;
                let limit = raw.parse::<i64>().map_err(|_| {
                    format!("--context-window needs a positive integer, got {raw:?}")
                })?;
                if limit <= 0 {
                    return Err("--context-window needs a positive integer".to_owned());
                }
                parsed.context_window = Some(limit);
            }
            "--alias" => parsed.alias = Some(value(flag)?),
            "--input-modalities" | "--input_modalities" => {
                parsed.input_modalities = Some(parse_list(&value(flag)?)?);
            }
            "--reasoning-efforts" | "--reasoning_efforts" => {
                parsed.reasoning_efforts = Some(parse_list(&value(flag)?)?);
            }
            "--discovery" => parsed.discovery = Some(parse_bool_flag(&value(flag)?)?),
            "--no-discovery" | "--no_discovery" => {
                no_value(flag)?;
                if parsed.discovery == Some(true) {
                    return Err("--discovery and --no-discovery cannot both be set".to_owned());
                }
                parsed.discovery = Some(false);
            }
            "--models-url" | "--models_url" => parsed.models_url = Some(value(flag)?),
            "--anthropic-version" | "--anthropic_version" => {
                parsed.anthropic_version = Some(value(flag)?);
            }
            // Never accept a credential value, even accidentally.
            "--api-key" | "--api_key" | "--key" | "--token" | "--secret" | "--password" => {
                return Err(
                    "API keys are accepted only at the hidden ox setup prompt. Use --env-key NAME to refer to an environment variable"
                        .to_owned(),
                );
            }
            other => return Err(format!("unknown option {other:?}")),
        }
    }
    Ok(parsed)
}

fn set_positional(parsed: &mut AddArgs, argument: &str) -> Result<(), String> {
    if parsed.key.is_some() {
        return Err(format!("unexpected extra argument {argument:?}"));
    }
    parsed.key = Some(argument.to_owned());
    Ok(())
}

fn parse_list(raw: &str) -> Result<Vec<String>, String> {
    let values = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Err("expected a comma-separated list, for example text,image".to_owned());
    }
    Ok(values)
}

fn parse_bool_flag(raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(format!("expected true or false, got {raw:?}")),
    }
}

// ---------------------------------------------------------------------------
// Provider add: validation helpers
// ---------------------------------------------------------------------------

fn validate_provider_key(key: &str) -> Result<String, String> {
    if looks_like_credential(key) {
        return Err(
            "that looks like a credential. Enter a provider name such as openrouter or anthropic"
                .to_owned(),
        );
    }
    if key.is_empty()
        || key.len() > 64
        || key.starts_with(['-', '_'])
        || !key
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'_' || ch == b'-')
    {
        return Err(
            "provider names use lowercase ASCII letters, digits, `_` or `-`, starting with a letter or digit"
                .to_owned(),
        );
    }
    Ok(key.to_owned())
}

fn validate_model_id(id: &str) -> Result<String, String> {
    if id.is_empty()
        || id.len() > 200
        || id.starts_with('-')
        || id
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || matches!(ch, '"' | '\\'))
    {
        return Err(
            "model ids must be non-empty and contain no whitespace, control characters, quotes or backslashes"
                .to_owned(),
        );
    }
    Ok(id.to_owned())
}

fn validate_env_key_name(name: &str) -> Result<String, String> {
    match credential_env_error(name) {
        Some(reason) => Err(reason.to_owned()),
        None => Ok(name.to_owned()),
    }
}

fn validate_anthropic_version(value: &str) -> Result<String, String> {
    // Match the provider module's accepted header shape (for example
    // "2023-06-01"); the adapter owns the default when the field is omitted.
    let bytes = value.as_bytes();
    let date_like = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit());
    if !date_like {
        return Err(
            "anthropic_version must be a date such as 2023-06-01; omit it to use the adapter default"
                .to_owned(),
        );
    }
    Ok(value.to_owned())
}

fn validate_base_url(value: &str) -> Result<String, String> {
    let probe = ProviderForm {
        key: "probe".to_owned(),
        name: None,
        base_url: value.to_owned(),
        env_key: None,
        wire: WireKind::Responses,
        models_url: None,
        discovery: None,
        anthropic_version: None,
        models: Vec::new(),
    };
    validate_provider(&probe).map_err(|error| error.to_string())?;
    Ok(value.to_owned())
}

fn default_env_key(key: &str) -> String {
    let mut name = String::new();
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() {
            name.push(ch.to_ascii_uppercase());
        } else {
            name.push('_');
        }
    }
    if name.is_empty() {
        name.push_str("PROVIDER");
    }
    if name.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name.push_str("_API_KEY");
    name
}

// ---------------------------------------------------------------------------
// Provider add: prompt
// ---------------------------------------------------------------------------

struct Prompt<'a> {
    input: &'a mut dyn BufRead,
    output: &'a mut dyn Write,
}

impl Prompt<'_> {
    fn guided_key(&mut self, service: &str, env: &str) -> Result<Option<String>, RelayError> {
        if credential_state(&crate::provider::process_env, env) == CredentialState::Present {
            writeln!(self.output, "Using your existing ${env}.")?;
            return Ok(None);
        }
        writeln!(
            self.output,
            "Paste your {service} key to save it locally. Press Enter to keep an existing saved key or use ${env} later."
        )?;
        let key = self.hidden_key()?;
        if key.is_empty() {
            Ok(None)
        } else {
            Ok(Some(key))
        }
    }

    #[cfg(unix)]
    fn hidden_key(&mut self) -> Result<String, RelayError> {
        use std::os::fd::AsRawFd;

        struct TerminalMode(i32, libc::termios);
        impl Drop for TerminalMode {
            fn drop(&mut self) {
                // SAFETY: the original settings were read from this terminal.
                unsafe { libc::tcsetattr(self.0, libc::TCSANOW, &self.1) };
            }
        }

        let fd = std::io::stdin().as_raw_fd();
        let mut original = std::mem::MaybeUninit::uninit();
        // SAFETY: tcgetattr writes a termios value on success.
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(
                "could not hide input on this terminal; set the API key variable in your shell"
                    .into(),
            );
        }
        // SAFETY: tcgetattr has initialized original.
        let original = unsafe { original.assume_init() };
        let mut hidden = original;
        hidden.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
        hidden.c_cc[libc::VMIN] = 1;
        hidden.c_cc[libc::VTIME] = 0;
        let restore = TerminalMode(fd, original);
        // SAFETY: the descriptor is stdin and hidden is an initialized termios.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) } != 0 {
            return Err(
                "could not hide input on this terminal; set the API key variable in your shell"
                    .into(),
            );
        }
        self.write_prompt("API key (hidden)", None)?;
        let mut key = String::new();
        let mut invalid = false;
        loop {
            let mut byte = [0];
            if self.input.read(&mut byte)? == 0 || matches!(byte[0], 3 | 4) {
                writeln!(self.output)?;
                return Err("API key entry cancelled".into());
            }
            match byte[0] {
                b'\r' | b'\n' => break,
                8 | 127 => {
                    key.pop();
                }
                value if value.is_ascii_graphic() && key.len() < 4096 => {
                    key.push(char::from(value))
                }
                _ => invalid = true,
            }
        }
        drop(restore);
        writeln!(self.output)?;
        if invalid {
            return Err("API keys must contain only printable ASCII with no spaces".into());
        }
        Ok(key)
    }

    #[cfg(not(unix))]
    fn hidden_key(&mut self) -> Result<String, RelayError> {
        Err("set the API key variable in your shell on this platform".into())
    }

    fn write_prompt(&mut self, label: &str, default: Option<&str>) -> Result<(), RelayError> {
        match default {
            Some(default) => write!(self.output, "{label} [{default}]: ")?,
            None => write!(self.output, "{label}: ")?,
        }
        self.output.flush()?;
        Ok(())
    }

    fn read_line(&mut self) -> Result<String, RelayError> {
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            return Err("interactive input ended before setup was complete".into());
        }
        Ok(line.trim_end_matches(['\n', '\r']).trim().to_owned())
    }

    fn ask_default(&mut self, label: &str, default: &str) -> Result<String, RelayError> {
        self.write_prompt(label, Some(default))?;
        let line = self.read_line()?;
        if line.is_empty() {
            Ok(default.to_owned())
        } else {
            Ok(line)
        }
    }

    fn ask_optional(
        &mut self,
        label: &str,
        default: Option<&str>,
    ) -> Result<Option<String>, RelayError> {
        self.write_prompt(label, default)?;
        let line = self.read_line()?;
        if line.is_empty() {
            Ok(default.map(str::to_owned))
        } else {
            Ok(Some(line))
        }
    }

    fn ask_required_with<T>(
        &mut self,
        label: &str,
        default: Option<&str>,
        mut validate: impl FnMut(&str) -> Result<T, String>,
    ) -> Result<T, RelayError> {
        loop {
            self.write_prompt(label, default)?;
            let line = self.read_line()?;
            let value = if line.is_empty() {
                default.unwrap_or("")
            } else {
                &line
            };
            if value.is_empty() {
                writeln!(self.output, "  error: a value is required")?;
                continue;
            }
            match validate(value) {
                Ok(value) => return Ok(value),
                Err(message) => writeln!(self.output, "  error: {message}")?,
            }
        }
    }

    fn ask_bool(&mut self, label: &str, default: bool) -> Result<bool, RelayError> {
        let suffix = if default { "Y/n" } else { "y/N" };
        loop {
            self.write_prompt(label, Some(suffix))?;
            let line = self.read_line()?;
            match line.to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" | "true" | "1" | "on" => return Ok(true),
                "n" | "no" | "false" | "0" | "off" => return Ok(false),
                _ => writeln!(self.output, "  error: please answer yes or no")?,
            }
        }
    }

    fn ask_wire(&mut self, default: WireKind) -> Result<WireKind, RelayError> {
        loop {
            let line = self.ask_default(
                "API format (responses, chat_completions, anthropic_messages)",
                default.as_str(),
            )?;
            match WireKind::parse(&line) {
                Some(wire) => return Ok(wire),
                None => writeln!(
                    self.output,
                    "  error: choose responses, chat_completions or anthropic_messages"
                )?,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Provider add: execution
// ---------------------------------------------------------------------------

pub(crate) fn run_provider_add(
    args: &[OsString],
    codex_home: &Path,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    interactive: bool,
    env: &crate::provider::Env<'_>,
) -> Result<u8, RelayError> {
    let mut parsed = match parse_add_args(args) {
        Ok(parsed) => parsed,
        Err(message) => return usage_error(output, &message),
    };
    if parsed.help {
        write_add_usage(output)?;
        return Ok(0);
    }

    // Apply flags that describe a single static model.
    let single_model_flags = parsed.model_name.is_some()
        || parsed.context_window.is_some()
        || parsed.alias.is_some()
        || parsed.input_modalities.is_some()
        || parsed.reasoning_efforts.is_some();
    if single_model_flags && parsed.models.len() != 1 {
        return usage_error(
            output,
            "--model-name, --context-window, --alias, --input-modalities and --reasoning-efforts need exactly one --model",
        );
    }
    if let Some(model) = parsed.models.first_mut() {
        model.name = parsed.model_name.take();
        model.context_window = parsed.context_window.take();
        model.alias = parsed.alias.take();
        model.input_modalities = parsed.input_modalities.take();
        model.reasoning_efforts = parsed.reasoning_efforts.take();
    }
    if parsed.no_env_key && parsed.env_key.is_some() {
        return usage_error(output, "--env-key and --no-env-key cannot both be set");
    }

    let interactive = interactive && (parsed.key.is_none() || parsed.base_url.is_none());
    let mut prompt = Prompt { input, output };

    let key = match parsed.key.take() {
        Some(key) => match validate_provider_key(&key) {
            Ok(key) => key,
            Err(message) => return usage_error(prompt.output, &message),
        },
        None if interactive => prompt.ask_required_with(
            "Provider name (e.g. openrouter, anthropic, openai)",
            None,
            validate_provider_key,
        )?,
        None => {
            return usage_error(
                prompt.output,
                "provider name is required (for example: ox provider add openrouter --base-url https://openrouter.ai/api/v1 --wire responses)",
            );
        }
    };

    let name = match parsed.name.take() {
        Some(name) => {
            if name.trim().is_empty() {
                return usage_error(prompt.output, "display name must not be empty");
            }
            Some(name)
        }
        None if interactive => {
            prompt.ask_optional(&format!("Display name (optional, Enter for {key:?})"), None)?
        }
        None => None,
    };

    let suggested_url = match key.as_str() {
        "openrouter" => Some("https://openrouter.ai/api/v1"),
        "anthropic" => Some("https://api.anthropic.com/v1"),
        "openai" => Some("https://api.openai.com/v1"),
        _ => None,
    };
    let base_url = match parsed.base_url.take() {
        Some(base_url) => base_url,
        None if interactive => {
            prompt.ask_required_with("Base URL", suggested_url, validate_base_url)?
        }
        None => {
            return usage_error(
                prompt.output,
                "--base-url is required when setup is not interactive (for example https://api.example.com/v1)",
            );
        }
    };

    let wire = match parsed.wire {
        Some(wire) => wire,
        None if interactive => {
            let default = if key == "anthropic" && suggested_url == Some(base_url.as_str()) {
                WireKind::AnthropicMessages
            } else {
                WireKind::Responses
            };
            prompt.ask_wire(default)?
        }
        None => WireKind::Responses,
    };

    let env_key = if parsed.no_env_key {
        None
    } else if let Some(name) = parsed.env_key.take() {
        match validate_env_key_name(&name) {
            Ok(name) => Some(name),
            Err(message) => return usage_error(prompt.output, &message),
        }
    } else if interactive {
        let default = default_env_key(&key);
        writeln!(
            prompt.output,
            "\nEnter an environment variable name. Keep the secret value in your shell. Press Enter for {default}."
        )?;
        loop {
            let value = prompt.ask_default("API key variable name", &default)?;
            match validate_env_key_name(&value) {
                Ok(value) => break Some(value),
                Err(message) => writeln!(prompt.output, "  error: {message}")?,
            }
        }
    } else {
        match validate_env_key_name(&default_env_key(&key)) {
            Ok(name) => Some(name),
            Err(message) => return usage_error(prompt.output, &message),
        }
    };

    let discovery = match parsed.discovery {
        Some(discovery) => discovery,
        None if interactive => {
            prompt.ask_bool("Discover models from the provider at launch", true)?
        }
        None => true,
    };

    let mut models = std::mem::take(&mut parsed.models);
    if !discovery && models.is_empty() {
        if interactive {
            while models.is_empty() {
                let model = prompt.ask_required_with(
                    "Static model id (required when discovery is off)",
                    None,
                    |raw| {
                        validate_model_id(raw).map(|id| ModelForm {
                            id,
                            ..ModelForm::default()
                        })
                    },
                )?;
                models.push(model);
            }
        } else {
            return usage_error(
                prompt.output,
                "--no-discovery requires at least one --model so the provider has a catalog",
            );
        }
    }
    if interactive && discovery && models.is_empty() {
        // Offer static entries, which also act as capability overrides.
        loop {
            let Some(id) =
                prompt.ask_optional("Optional model ID (Enter to rely on discovery)", None)?
            else {
                break;
            };
            match validate_model_id(&id) {
                Ok(id) => models.push(ModelForm {
                    id,
                    ..ModelForm::default()
                }),
                Err(message) => writeln!(prompt.output, "  error: {message}")?,
            }
        }
    }

    let mut seen_models = BTreeSet::new();
    for model in &models {
        if !seen_models.insert(model.id.clone()) {
            return usage_error(
                prompt.output,
                &format!("model {:?} was given more than once", model.id),
            );
        }
    }

    let anthropic_version = match parsed.anthropic_version.take() {
        Some(version) => {
            if wire != WireKind::AnthropicMessages {
                return usage_error(
                    prompt.output,
                    "--anthropic-version is only valid with --wire anthropic_messages",
                );
            }
            match validate_anthropic_version(&version) {
                Ok(version) => Some(version),
                Err(message) => return usage_error(prompt.output, &message),
            }
        }
        None if interactive && wire == WireKind::AnthropicMessages => {
            match prompt.ask_optional("Anthropic version (blank uses the adapter default)", None)? {
                Some(version) => match validate_anthropic_version(&version) {
                    Ok(version) => Some(version),
                    Err(message) => return usage_error(prompt.output, &message),
                },
                None => None,
            }
        }
        None => None,
    };

    let form = ProviderForm {
        key,
        name,
        base_url,
        env_key: env_key.clone(),
        wire,
        models_url: parsed.models_url.take(),
        discovery: if discovery { None } else { Some(false) },
        anthropic_version: anthropic_version.clone(),
        models,
    };

    // Reuse the provider module's URL, env-key, model and alias rules.
    if let Err(error) = validate_provider(&form) {
        return usage_error(prompt.output, &format!("invalid provider: {error}"));
    }

    let config_path = CodeoxConfig::path(codex_home);
    let config_lock = lock_config(codex_home)?;
    let existing = read_existing_codeox(&config_path)?.unwrap_or_default();
    if !existing.trim().is_empty() {
        let document = Document::parse(&existing).map_err(|error| {
            format!(
                "codeox.toml is not valid TOML ({}); fix it before adding a provider",
                error.message()
            )
        })?;
        if let Some(providers) = document.get("providers") {
            let Some(table) = providers.as_table() else {
                return usage_error(
                    prompt.output,
                    "codeox.toml defines `providers` as an inline value; convert it to [providers.<name>] tables before adding a provider",
                );
            };
            if table.contains_key(&form.key) {
                return usage_error(
                    prompt.output,
                    &format!(
                        "provider {:?} already exists in codeox.toml; edit that section instead of adding a duplicate",
                        form.key
                    ),
                );
            }
        }
    }

    let section = render_provider_section(&form);
    let candidate = append_section(&existing, &section);
    let config = match parse_config_for_validation(&candidate) {
        Ok(config) => config,
        Err(error) => return usage_error(prompt.output, &format!("invalid codeox.toml: {error}")),
    };
    if let Err(error) = check_alias_collisions(&config) {
        return usage_error(prompt.output, &format!("invalid codeox.toml: {error}"));
    }

    write_config_atomic(&config_path, &candidate)?;
    drop(config_lock);

    writeln!(
        prompt.output,
        "Added provider {:?} to {}.",
        form.key,
        config_path.display()
    )?;
    writeln!(
        prompt.output,
        "  wire: {}",
        wire_summary(form.wire, form.anthropic_version.as_deref())
    )?;
    match &form.env_key {
        Some(name) => {
            writeln!(
                prompt.output,
                "  credential: references ${name}; no key value was read or stored"
            )?;
            if credential_state(env, name) == CredentialState::Present {
                writeln!(prompt.output, "  ${name} is set in this environment")?;
            } else {
                writeln!(
                    prompt.output,
                    "  Load the vendor API key into ${name} in this shell before running ox doctor."
                )?;
            }
        }
        None => {
            writeln!(
                prompt.output,
                "  warning: no env_key was configured; requests to this provider will fail until one is added"
            )?;
        }
    }
    match (form.discovery, form.models.len()) {
        (Some(false), count) => writeln!(
            prompt.output,
            "  models: discovery off; {count} static model(s)"
        )?,
        (_, 0) => writeln!(
            prompt.output,
            "  models: discovery on; no static entries (catalog comes from the provider)"
        )?,
        (_, count) => writeln!(
            prompt.output,
            "  models: discovery on; {count} static model(s), also used as overrides"
        )?,
    }
    let mut first_alias = None;
    for model in &form.models {
        let alias = alias_for(&form.key, &model.id, model.alias.as_deref())?;
        writeln!(prompt.output, "  model alias: {alias}")?;
        first_alias.get_or_insert(alias);
    }
    if config.vendors.iter().any(|vendor| {
        vendor.key == form.key
            && vendor.base_url.scheme() == "https"
            && vendor.base_url.host_str() == Some("api.openai.com")
            && vendor.base_url.port_or_known_default() == Some(443)
    }) {
        writeln!(
            prompt.output,
            "  Codex with this API key requires Codex access in your OpenAI API organization."
        )?;
        writeln!(
            prompt.output,
            "  For ChatGPT plan usage, run `ox login chatgpt` and `ox codex --chatgpt`."
        )?;
    }
    if let Some(alias) = first_alias {
        let quoted = format!("'{}'", alias.replace('\'', "'\\''"));
        writeln!(
            prompt.output,
            "Next: run `ox doctor`, then choose a client:"
        )?;
        writeln!(prompt.output, "  ox codex --model {quoted}")?;
        writeln!(prompt.output, "  ox claude --model {quoted}")?;
    } else {
        writeln!(
            prompt.output,
            "Next: run `ox doctor`, then run `ox codex` or `ox claude` from your project."
        )?;
        writeln!(
            prompt.output,
            "Use /model to choose a discovered model, or pass --model '{}/<upstream-model-id>'.",
            form.key
        )?;
    }
    Ok(0)
}

fn usage_error(output: &mut dyn Write, message: &str) -> Result<u8, RelayError> {
    writeln!(output, "error: {message}")?;
    writeln!(output)?;
    writeln!(
        output,
        "Run `ox setup --help` for options and copyable examples."
    )?;
    Ok(2)
}

fn write_provider_usage(output: &mut dyn Write) -> Result<(), RelayError> {
    writeln!(output, "Usage:")?;
    writeln!(output, "  ox provider add [PROVIDER] [options]")?;
    writeln!(output)?;
    writeln!(output, "Run `ox provider add --help` for options.")?;
    Ok(())
}

pub(crate) fn print_setup_help() -> Result<u8, RelayError> {
    write_add_usage(&mut std::io::stdout().lock())?;
    Ok(0)
}

fn write_add_usage(output: &mut dyn Write) -> Result<(), RelayError> {
    writeln!(output, "Usage:")?;
    writeln!(output, "  ox setup [PROVIDER] [options]")?;
    writeln!(output, "  ox provider add [PROVIDER] [options]")?;
    writeln!(output)?;
    writeln!(
        output,
        "Run ox setup with no arguments to manage sign-ins, API providers, and Brave."
    )?;
    writeln!(
        output,
        "Supply the provider name and --base-url to add a provider without prompts."
    )?;
    writeln!(output)?;
    writeln!(
        output,
        "Adds one provider to $CODEX_HOME/codeox.toml, preserving the rest of the"
    )?;
    writeln!(
        output,
        "file byte for byte. Guided setup can save a key at its hidden prompt."
    )?;
    writeln!(
        output,
        "Scripts can use --env-key to name a variable ox reads at launch."
    )?;
    writeln!(output)?;
    writeln!(output, "Options:")?;
    writeln!(
        output,
        "  --id PROVIDER                Alternative to the positional provider name"
    )?;
    writeln!(
        output,
        "  --name NAME                  Display name (defaults to the provider name)"
    )?;
    writeln!(
        output,
        "  --base-url URL               Provider base URL (required unless prompted)"
    )?;
    writeln!(
        output,
        "  --wire WIRE                  responses (default), chat_completions or anthropic_messages"
    )?;
    writeln!(
        output,
        "  --env-key NAME               Environment variable holding the API key"
    )?;
    writeln!(
        output,
        "                               (e.g. OPENROUTER_API_KEY for openrouter)"
    )?;
    writeln!(
        output,
        "  --no-env-key                 Configure no credential reference (provider will be unavailable)"
    )?;
    writeln!(
        output,
        "  --model ID                   Add a static model (repeatable)"
    )?;
    writeln!(
        output,
        "  --model-name NAME            Display name for the single --model"
    )?;
    writeln!(
        output,
        "  --context-window TOKENS      Context window for the single --model"
    )?;
    writeln!(
        output,
        "  --alias ALIAS                Stable alias for the single --model"
    )?;
    writeln!(
        output,
        "  --input-modalities LIST      Comma list for the single --model (text,image)"
    )?;
    writeln!(
        output,
        "  --reasoning-efforts LIST     Comma list for the single --model"
    )?;
    writeln!(
        output,
        "  --discovery true|false       Override model discovery (default true)"
    )?;
    writeln!(
        output,
        "  --no-discovery               Shorthand for --discovery false"
    )?;
    writeln!(
        output,
        "  --models-url URL             Override the /models endpoint (same origin)"
    )?;
    writeln!(
        output,
        "  --anthropic-version VALUE    Anthropic version header value (anthropic_messages only)"
    )?;
    writeln!(output, "  -h, --help                   Show this help")?;
    writeln!(output)?;
    writeln!(output, "Examples:")?;
    writeln!(output, "  ox provider add openrouter \\")?;
    writeln!(
        output,
        "    --base-url https://openrouter.ai/api/v1 --wire responses \\"
    )?;
    writeln!(output, "    --env-key OPENROUTER_API_KEY")?;
    writeln!(output)?;
    writeln!(
        output,
        "  ox provider add local --base-url http://127.0.0.1:8080/v1 \\"
    )?;
    writeln!(
        output,
        "    --wire chat_completions --env-key LOCAL_API_KEY \\"
    )?;
    writeln!(
        output,
        "    --no-discovery --model local-coder --model-name \"Local Coder\" \\"
    )?;
    writeln!(output, "    --context-window 32768")?;
    writeln!(output)?;
    writeln!(
        output,
        "In a terminal, run `ox setup` or `ox provider add` to choose from presets."
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering and atomic storage
// ---------------------------------------------------------------------------

fn render_provider_section(form: &ProviderForm) -> String {
    let mut out = String::new();
    out.push_str(&format!("[providers.{}]\n", form.key));
    if let Some(name) = &form.name {
        out.push_str(&format!("name = {}\n", toml_string(name)));
    }
    out.push_str(&format!("base_url = {}\n", toml_string(&form.base_url)));
    if let Some(env_key) = &form.env_key {
        out.push_str(&format!("env_key = {}\n", toml_string(env_key)));
    }
    out.push_str(&format!("wire_api = {}\n", toml_string(form.wire.as_str())));
    if let Some(models_url) = &form.models_url {
        out.push_str(&format!("models_url = {}\n", toml_string(models_url)));
    }
    if form.discovery == Some(false) {
        out.push_str("discovery = false\n");
    }
    if let Some(version) = &form.anthropic_version {
        out.push_str(&format!("anthropic_version = {}\n", toml_string(version)));
    }
    for model in &form.models {
        out.push_str(&format!("\n[[providers.{}.models]]\n", form.key));
        out.push_str(&format!("id = {}\n", toml_string(&model.id)));
        if let Some(alias) = &model.alias {
            out.push_str(&format!("alias = {}\n", toml_string(alias)));
        }
        if let Some(name) = &model.name {
            out.push_str(&format!("name = {}\n", toml_string(name)));
        }
        if let Some(limit) = model.context_window {
            out.push_str(&format!("context_window = {limit}\n"));
        }
        if let Some(modalities) = &model.input_modalities {
            out.push_str(&format!(
                "input_modalities = {}\n",
                render_string_array(modalities)
            ));
        }
        if let Some(efforts) = &model.reasoning_efforts {
            out.push_str(&format!(
                "reasoning_efforts = {}\n",
                render_string_array(efforts)
            ));
        }
    }
    out
}

fn render_string_array(values: &[String]) -> String {
    let rendered = values
        .iter()
        .map(|value| toml_string(value))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{rendered}]")
}

fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04X}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

pub(crate) fn append_section(existing: &str, section: &str) -> String {
    let mut out = String::with_capacity(existing.len() + section.len() + 2);
    out.push_str(existing);
    if !existing.is_empty() {
        if !existing.ends_with('\n') {
            out.push('\n');
        }
        if !existing.ends_with("\n\n") {
            out.push('\n');
        }
    }
    out.push_str(section);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn read_existing_codeox(path: &Path) -> Result<Option<String>, RelayError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("could not inspect codeox.toml".into()),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(
                    "codeox.toml is a symbolic link; refusing to replace it; edit its target directly"
                        .into(),
                );
            }
            if metadata.len() > MAX_CONFIG_BYTES {
                return Err("codeox.toml is too large".into());
            }
            match fs::read_to_string(path) {
                Ok(text) => Ok(Some(text)),
                Err(_) => Err("could not read codeox.toml as UTF-8".into()),
            }
        }
    }
}

pub(crate) fn lock_config(codex_home: &Path) -> Result<File, RelayError> {
    let mut directories = fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(codex_home)?;
    // The config itself is replaced by rename. Keep the lock on a separate
    // inode so waiting writers always acquire the same lock.
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(codex_home.join(".codeox.toml.lock"))?;
    lock.lock()?;
    Ok(lock)
}

pub(crate) fn write_config_atomic(path: &Path, contents: &str) -> Result<(), RelayError> {
    let parent = path.parent().ok_or("codeox.toml has no parent directory")?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid codeox.toml path")?;
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|_| "could not generate a temporary file name")?;
    let temporary = parent.join(format!(".{file_name}.tmp-{}", hex::encode(random)));
    let existing_permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());

    let result = (|| -> Result<(), RelayError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        if let Some(permissions) = existing_permissions {
            fs::set_permissions(&temporary, permissions)?;
        }
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        {
            if let Ok(directory) = File::open(parent) {
                let _ = directory.sync_all();
            }
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| format!("could not write {file_name} atomically: {error}").into())
}

// ---------------------------------------------------------------------------
// Validation through the provider module
// ---------------------------------------------------------------------------

fn validate_provider(form: &ProviderForm) -> Result<(), RelayError> {
    let text = render_provider_section(form);
    parse_config_for_validation(&text).map(|_| ())
}

/// Parses a candidate `codeox.toml` through [`provider::CodeoxConfig`].
///
/// This is the same parser the relay uses at launch, so `provider add` cannot
/// write URL, credential-name, model, wire or cross-option combinations that
/// startup would reject. It is exposed to the integration tests so they can
/// assert the same contract.
pub(crate) fn parse_config_for_validation(text: &str) -> Result<CodeoxConfig, RelayError> {
    CodeoxConfig::load_from_str(text)
}

pub(crate) fn check_alias_collisions(config: &CodeoxConfig) -> Result<(), RelayError> {
    let mut aliases: BTreeMap<String, (String, String)> = BTreeMap::new();
    for vendor in &config.vendors {
        for model in &vendor.models {
            let alias = alias_for(&vendor.key, &model.id, model.alias.as_deref())?;
            if let Some((other_vendor, other_model)) = aliases.get(&alias) {
                // A duplicate id on the same provider is de-duplicated by the
                // catalog (first entry wins), so it is not a launch conflict.
                if other_vendor == &vendor.key && other_model == &model.id {
                    continue;
                }
                return Err(format!(
                    "model alias {alias:?} is used by both provider {other_vendor:?} and provider {:?}; give one entry an explicit `alias`",
                    vendor.key
                )
                .into());
            }
            aliases.insert(alias, (vendor.key.clone(), model.id.clone()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Doctor
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Pass,
    Info,
    Warning,
    Error,
}

impl Level {
    /// The mark printed before a diagnostic and its colour code.
    fn mark(self) -> (&'static str, &'static str) {
        match self {
            Self::Pass => ("✓", crate::ui::GREEN),
            Self::Info => ("·", crate::ui::DIM),
            Self::Warning => ("!", crate::ui::YELLOW),
            Self::Error => ("✗", crate::ui::RED),
        }
    }
}

struct Diagnostic {
    level: Level,
    scope: Option<String>,
    message: String,
}

struct DoctorReport {
    home: PathBuf,
    mode: String,
    diagnostics: Vec<Diagnostic>,
    provider_count: usize,
    /// The command that resolves the first error that has a known remedy.
    next: Option<String>,
    /// Stock clients found on PATH: (codex, claude).
    clients: (bool, bool),
    /// First configured provider key, used for the live-probe suggestion.
    first_provider: Option<String>,
}

impl DoctorReport {
    fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            mode: "unknown".to_owned(),
            diagnostics: Vec::new(),
            provider_count: 0,
            next: None,
            clients: (false, false),
            first_provider: None,
        }
    }

    fn pass(&mut self, scope: Option<String>, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            level: Level::Pass,
            scope,
            message: message.into(),
        });
    }

    /// Records the remedy for an error. The first suggestion wins, so the
    /// report ends with the command for the earliest problem.
    fn suggest(&mut self, command: &str) {
        if self.next.is_none() {
            self.next = Some(command.to_owned());
        }
    }

    fn info(&mut self, scope: Option<String>, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            level: Level::Info,
            scope,
            message: message.into(),
        });
    }

    fn warning(&mut self, scope: Option<String>, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            level: Level::Warning,
            scope,
            message: message.into(),
        });
    }

    fn error(&mut self, scope: Option<String>, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            level: Level::Error,
            scope,
            message: message.into(),
        });
    }

    fn count(&self, level: Level) -> usize {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.level == level)
            .count()
    }

    fn exit_code(&self, strict: bool) -> u8 {
        if self.count(Level::Error) > 0 || (strict && self.count(Level::Warning) > 0) {
            1
        } else {
            0
        }
    }

    fn render(&self, color: bool) -> String {
        use crate::ui::{BOLD, paint};

        let mut out = String::new();
        out.push_str(&paint("ox doctor (offline)", BOLD, color));
        out.push('\n');
        out.push_str(&format!("home: {}\n", self.home.display()));
        out.push_str(&format!("mode: {}\n", self.mode));
        out.push_str(&format!("providers: {}\n", self.provider_count));

        // Group by scope in first-seen order. Diagnostics without a scope are
        // about the setup as a whole and are shown first.
        let mut scopes: Vec<Option<&str>> = vec![None];
        for diagnostic in &self.diagnostics {
            let scope = diagnostic.scope.as_deref();
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
        for scope in scopes {
            let mut lines = self
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.scope.as_deref() == scope)
                .peekable();
            if lines.peek().is_none() {
                continue;
            }
            out.push('\n');
            out.push_str(&paint(scope.unwrap_or("setup"), BOLD, color));
            out.push('\n');
            for diagnostic in lines {
                let (mark, code) = diagnostic.level.mark();
                out.push_str(&format!(
                    "  {} {}\n",
                    paint(mark, code, color),
                    diagnostic.message
                ));
            }
        }
        out.push_str(&format!(
            "\nsummary: {} provider(s), {} error(s), {} warning(s)\n",
            self.provider_count,
            self.count(Level::Error),
            self.count(Level::Warning)
        ));
        out.push_str(&format!(
            "{} {}\n",
            paint("next:", BOLD, color),
            self.next_step()
        ));
        out
    }

    /// The single command to run next.
    fn next_step(&self) -> String {
        if self.count(Level::Error) > 0 {
            return match &self.next {
                Some(command) => command.clone(),
                None => format!(
                    "edit {} to resolve the ✗ lines, then run ox doctor",
                    self.home.join(crate::provider::CONFIG_FILE).display()
                ),
            };
        }
        match self.clients {
            (false, false) => format!(
                "{}   (installs Codex; ox needs Codex or Claude Code)",
                crate::ui::install_command("codex")
            ),
            (true, _) => self.launch_step("ox codex"),
            (false, true) => self.launch_step("ox claude"),
        }
    }

    fn launch_step(&self, command: &str) -> String {
        match &self.first_provider {
            Some(provider) => format!(
                "{command}   (or test one real request: ox doctor --live --provider {provider})"
            ),
            None => command.to_owned(),
        }
    }
}

/// Finds an executable named `name` in the directories of `path`.
pub(crate) fn find_on_path(name: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    find_all_on_path(name, path).into_iter().next()
}

/// Every executable named `name` in `path`, in lookup order. Entries that
/// resolve to the same file (a symlinked bin directory, say) appear once.
pub(crate) fn find_all_on_path(name: &str, path: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let Some(path) = path else {
        return Vec::new();
    };
    let mut seen = BTreeSet::new();
    std::env::split_paths(path)
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| directory.join(name))
        .filter(|candidate| candidate.is_file())
        .filter(|candidate| {
            seen.insert(
                candidate
                    .canonicalize()
                    .unwrap_or_else(|_| candidate.clone()),
            )
        })
        .collect()
}

/// The minimum stock Codex that ox supports. OX Desktop enforces the same
/// floor, so both read it from the desktop gate.
pub(crate) fn min_codex() -> &'static str {
    let mut section = "";
    for line in include_str!("../desktop/gate.toml").lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name;
        } else if section == "codex"
            && let Some(value) = line.strip_prefix("minimum_version")
        {
            return value
                .trim_start_matches([' ', '='])
                .trim()
                .trim_matches('"');
        }
    }
    panic!("desktop/gate.toml has no [codex] minimum_version")
}

/// The first `x.y.z` in `codex --version` output, such as `codex-cli 0.171.0`.
fn semver(text: &str) -> Option<(u64, u64, u64)> {
    text.split_whitespace().find_map(|word| {
        let mut parts = word.trim_start_matches('v').split(['.', '-', '+']);
        Some((
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        ))
    })
}

/// The desktop's wording for a Codex below the floor, so OX Desktop offers
/// its Update button for this failure too.
pub(crate) fn codex_too_old(version_output: &str) -> Option<String> {
    let (major, minor, patch) = semver(version_output)?;
    ((major, minor, patch) < semver(min_codex())?).then(|| {
        format!(
            "Codex {major}.{minor}.{patch} is older than the required {}.",
            min_codex()
        )
    })
}

/// How to upgrade the Codex at `path`. `brew install` leaves an installed
/// cask alone, so a Homebrew install needs `brew upgrade` instead.
pub(crate) fn codex_upgrade_command(path: Option<&Path>) -> &'static str {
    match path.and_then(codex_channel) {
        Some(channel) => channel.command,
        None if cfg!(target_os = "macos") => {
            "brew upgrade --cask codex, or npm install -g @openai/codex@latest if npm installed it"
        }
        None => "npm install -g @openai/codex@latest",
    }
}

/// The package manager that owns an installed Codex.
pub(crate) struct CodexChannel {
    pub(crate) tool: &'static str,
    pub(crate) args: &'static [&'static str],
    pub(crate) command: &'static str,
}

/// Every channel ships the same native Rust binary (npm wraps it in a small
/// Node launcher), so the right update is the one for the channel that owns
/// this install. Unknown layouts, such as a manual download, return None.
pub(crate) fn codex_channel(path: &Path) -> Option<CodexChannel> {
    let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let has = |name: &str| real.components().any(|c| c.as_os_str() == name);
    if has("mise") {
        Some(CodexChannel {
            tool: "mise",
            args: &["use", "--global", "codex@latest"],
            command: "mise use --global codex@latest",
        })
    } else if has("Caskroom") || has("Cellar") {
        Some(CodexChannel {
            tool: "brew",
            args: &["upgrade", "--cask", "codex"],
            command: "brew upgrade --cask codex",
        })
    } else if has("node_modules") {
        Some(CodexChannel {
            tool: "npm",
            args: &["install", "-g", "@openai/codex@latest"],
            command: "npm install -g @openai/codex@latest",
        })
    } else {
        None
    }
}

/// The update for this Codex as a runnable command line, when its channel is
/// known and that package manager is on `path`.
pub(crate) fn codex_update_argv(
    codex: &Path,
    path: Option<&std::ffi::OsStr>,
) -> Option<(PathBuf, &'static [&'static str], &'static str)> {
    let channel = codex_channel(codex)?;
    let tool = find_on_path(channel.tool, path)?;
    Some((tool, channel.args, channel.command))
}

/// The last few lines of a child's stderr, safe to print: control characters
/// are dropped and the text is capped, so a chatty failure stays readable.
pub(crate) fn stderr_tail(stderr: &[u8]) -> Option<String> {
    const LINES: usize = 5;
    const BYTES: usize = 500;
    let text = String::from_utf8_lossy(stderr);
    let lines = text
        .lines()
        .map(|line| {
            line.chars()
                .map(|c| if c == '\t' { ' ' } else { c })
                .filter(|c| !c.is_control())
                .collect::<String>()
        })
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let tail = lines[lines.len().saturating_sub(LINES)..].join("\n         ");
    if tail.is_empty() {
        return None;
    }
    if tail.len() <= BYTES {
        return Some(tail);
    }
    let mut start = tail.len() - BYTES;
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    Some(format!("…{}", &tail[start..]))
}

/// The launch error for `codex debug models --bundled`. Keeps the historical
/// first line and adds `why:`/`fix:` so an old Codex and a config that Codex
/// rejects can be told apart.
pub(crate) fn codex_catalog_failure(
    why: &str,
    stderr: &[u8],
    codex: Option<&Path>,
    codex_home: &Path,
    version_output: Option<&str>,
) -> String {
    if let Some(old) = version_output.and_then(codex_too_old) {
        let mut message =
            format!("{old}\n  why: could not read the bundled Codex model catalog; {why}");
        if let Some(codex) = codex {
            message.push_str(&format!("\n       codex: {}", codex.display()));
        }
        if let Some(tail) = stderr_tail(stderr) {
            message.push_str(&format!("\n       stderr: {tail}"));
        }
        message.push_str(&format!("\n  fix: {}", codex_upgrade_command(codex)));
        return message;
    }
    let mut message = format!("could not read the bundled Codex model catalog\n  why: {why}");
    if let Some(codex) = codex {
        message.push_str(&format!("\n       codex: {}", codex.display()));
    }
    if let Some(tail) = stderr_tail(stderr) {
        message.push_str(&format!("\n       stderr: {tail}"));
    }
    message.push_str(&format!(
        "\n  fix: run `codex --version`; ox needs Codex {} or later. Upgrade with: {}\n       If Codex reports a config error, check {}",
        min_codex(),
        codex_upgrade_command(codex),
        codex_home.join("config.toml").display()
    ));
    message
}

/// Runs the same `codex debug models --bundled` call a launch makes and
/// returns the failure message, if any.
pub(crate) fn probe_codex_catalog(codex: &Path, codex_home: &Path) -> Result<(), String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const TIMEOUT: Duration = Duration::from_secs(20);
    let failure = |why: &str, stderr: &[u8]| {
        codex_catalog_failure(
            why,
            stderr,
            Some(codex),
            codex_home,
            codex_version_blocking(codex).as_deref(),
        )
    };
    let mut child = Command::new(codex)
        .env("CODEX_HOME", codex_home)
        .env_remove("CODEX_SQLITE_HOME")
        .args(["debug", "models", "--bundled"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| failure(&format!("could not start codex ({error})"), b""))?;
    // Drain both pipes while waiting: the catalog is larger than a pipe buffer.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(failure(
                    &format!(
                        "`codex debug models --bundled` did not finish within {}s",
                        TIMEOUT.as_secs()
                    ),
                    b"",
                ));
            }
            Err(error) => return Err(failure(&format!("could not wait for codex ({error})"), b"")),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if !status.success() {
        return Err(failure(&exit_description(status), &stderr));
    }
    serde_json::from_slice::<crate::catalog::BundledModels>(&stdout)
        .map(|_| ())
        .map_err(|error| {
            failure(
                &format!("Codex printed a model catalog ox could not parse ({error})"),
                &stderr,
            )
        })
}

/// `codex --version` output, or None when it fails or takes over 5 seconds.
fn codex_version_blocking(codex: &Path) -> Option<String> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new(codex)
        .arg("--version")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut text).ok()?;
    Some(text.trim().to_owned())
}

/// "`codex debug models --bundled` exited with status 2", or the signal case.
pub(crate) fn exit_description(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("`codex debug models --bundled` exited with status {code}"),
        None => "`codex debug models --bundled` was stopped by a signal".to_owned(),
    }
}

/// The Codex home a launch reads its catalog with: the relay profile once ox
/// has created it, otherwise the native home it is copied from.
fn launch_codex_home(home: &Path) -> PathBuf {
    let relay = home.join("ox-clients").join("codex");
    if relay.is_dir() {
        relay
    } else {
        home.to_path_buf()
    }
}

fn doctor_codex_catalog(report: &mut DoctorReport, home: &Path) {
    let Some(codex) = find_on_path("codex", std::env::var_os("PATH").as_deref()) else {
        return;
    };
    let scope = Some("clients".to_owned());
    match probe_codex_catalog(&codex, &launch_codex_home(home)) {
        Ok(()) => report.pass(scope, "codex loads its bundled model catalog"),
        Err(message) => {
            if message.contains("older than the required") {
                report.suggest(codex_upgrade_command(Some(&codex)));
            }
            report.error(scope, message)
        }
    }
}

fn doctor_clients(report: &mut DoctorReport) {
    let path = std::env::var_os("PATH");
    let mut found = [false, false];
    for (index, client) in ["codex", "claude"].into_iter().enumerate() {
        let scope = Some("clients".to_owned());
        let all = find_all_on_path(client, path.as_deref());
        match all.first() {
            Some(location) => {
                found[index] = true;
                report.pass(
                    scope.clone(),
                    format!("{client} found at {}", location.display()),
                );
                if all.len() > 1 {
                    let list = all
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    report.warning(
                        scope,
                        format!("{client} is installed more than once on PATH ({list}); the first one runs"),
                    );
                }
            }
            None => report.info(
                scope,
                format!(
                    "{client} is not installed. Install {} with: {}",
                    crate::ui::client_name(client),
                    crate::ui::install_command(client)
                ),
            ),
        }
    }
    report.clients = (found[0], found[1]);
}

fn doctor_saved_keys(report: &mut DoctorReport, home: &Path) {
    if let Err(error) = SavedKeys::load(home) {
        let message = error.to_string();
        if message == "could not parse ox-api-keys.json" {
            report.suggest("ox doctor --fix");
        }
        report.error(Some("saved keys".to_owned()), message);
    }
}

fn doctor_report(home: &Path, env: &crate::provider::Env<'_>) -> DoctorReport {
    let mut report = DoctorReport::new(home);
    doctor_clients(&mut report);
    if report.clients.0 {
        doctor_codex_catalog(&mut report, home);
    }
    doctor_saved_keys(&mut report, home);
    let codeox_path = CodeoxConfig::path(home);
    if codeox_path.exists() {
        report.mode = "multi-vendor (codeox.toml)".to_owned();
        doctor_multi_vendor(&mut report, home, &codeox_path, env);
    } else {
        report.mode = "legacy (no codeox.toml; using config.toml)".to_owned();
        doctor_legacy(&mut report, home, env);
    }
    report
}

fn doctor_multi_vendor(
    report: &mut DoctorReport,
    home: &Path,
    config_path: &Path,
    env: &crate::provider::Env<'_>,
) {
    let text = match read_text_limited(config_path, "codeox.toml") {
        Ok(text) => text,
        Err(message) => {
            report.error(None, message);
            return;
        }
    };
    let document = match Document::parse(&text) {
        Ok(document) => document,
        Err(error) => {
            report.suggest("ox doctor --fix");
            report.error(
                None,
                format!("could not parse codeox.toml: {}", error.message()),
            );
            return;
        }
    };
    let Some(providers) = document.get("providers") else {
        report.suggest("ox doctor --fix");
        report.error(None, "codeox.toml needs a [providers.<name>] table");
        return;
    };
    let Some(table) = providers.as_table() else {
        report.error(
            None,
            "`providers` must be a table of provider configurations",
        );
        return;
    };
    if table.is_empty() {
        report.suggest("ox doctor --fix");
        report.error(None, "codeox.toml must configure at least one provider");
    }
    report.first_provider = table.iter().next().map(|(key, _)| key.to_owned());

    let mut diagnoses = Vec::new();
    for (key, item) in table.iter() {
        report.provider_count += 1;
        diagnoses.push(doctor_provider(report, key, item, env));
    }
    let vendor_keys = diagnoses
        .iter()
        .map(|diagnosis| diagnosis.key.clone())
        .collect::<BTreeSet<_>>();
    cross_provider_checks(report, &diagnoses);
    let config = parse_config_for_validation(&text);
    let search_configured = config.as_ref().is_ok_and(|config| config.search.is_some());
    let relay_home = home.join("ox-clients/codex");
    let codex_settings = if relay_home.join(".initialized").is_file() {
        &relay_home
    } else {
        home
    };
    doctor_codex_config(report, codex_settings, &vendor_keys, search_configured);

    let errors_before = report.count(Level::Error);
    match config {
        Ok(config) => {
            if let Some(search) = &config.search {
                report.info(
                    Some("search".to_owned()),
                    format!(
                        "Brave Search: up to {} results per query, {}s timeout",
                        search.max_results, search.timeout_seconds
                    ),
                );
                doctor_credential(report, "search", &search.env_key, env);
            }
            if report.count(Level::Error) == errors_before {
                report.pass(
                    None,
                    format!(
                        "config: codeox.toml parses and validates ({} provider(s))",
                        config.vendors.len()
                    ),
                );
            }
        }
        Err(error) => {
            let message = error.to_string();
            // Per-provider synthetic validation may already have reported the
            // same provider-module error; do not repeat it verbatim.
            if !report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.ends_with(&message))
            {
                report.error(None, format!("config: {message}"));
            }
        }
    }
}

struct ProviderDiagnosis {
    key: String,
    form: Option<ProviderForm>,
}

fn doctor_provider(
    report: &mut DoctorReport,
    key: &str,
    item: &Item,
    env: &crate::provider::Env<'_>,
) -> ProviderDiagnosis {
    let scope = format!("provider {key}");
    let Some(table) = item.as_table() else {
        report.error(Some(scope), "must be a [providers.<name>] table");
        return ProviderDiagnosis {
            key: key.to_owned(),
            form: None,
        };
    };

    let mut valid = true;
    let (name, name_ok) = extract_str(report, &scope, table, "name");
    valid &= name_ok;
    if name.as_deref().is_some_and(|name| name.trim().is_empty()) {
        report.error(Some(scope.clone()), "name must not be empty");
        valid = false;
    }

    let (base_url, base_ok) = extract_str(report, &scope, table, "base_url");
    valid &= base_ok;
    if base_url.is_none() && base_ok {
        report.error(Some(scope.clone()), "needs a base_url");
        valid = false;
    }

    let (env_key, env_ok) = extract_str(report, &scope, table, "env_key");
    valid &= env_ok;

    let (wire_raw, wire_ok) = extract_str(report, &scope, table, "wire_api");
    valid &= wire_ok;
    let wire = match wire_raw.as_deref() {
        None => Some(WireKind::Responses),
        Some(raw) => match WireKind::parse_config(raw) {
            Some(wire) => Some(wire),
            None => {
                report.error(
                    Some(scope.clone()),
                    format!(
                        "unknown wire_api {raw:?}; expected \"responses\", \"chat_completions\" or \"anthropic_messages\""
                    ),
                );
                valid = false;
                None
            }
        },
    };

    let (models_url, models_url_ok) = extract_str(report, &scope, table, "models_url");
    valid &= models_url_ok;

    let (discovery_raw, discovery_ok) = extract_bool(report, &scope, table, "discovery");
    valid &= discovery_ok;
    let discovery = discovery_raw.unwrap_or(true);

    let (anthropic_version, version_ok) = extract_str(report, &scope, table, "anthropic_version");
    valid &= version_ok;
    if let Some(version) = &anthropic_version
        && let Err(message) = validate_anthropic_version(version)
    {
        report.error(Some(scope.clone()), message);
        valid = false;
    }

    if let Some(wire) = wire {
        report.info(
            Some(scope.clone()),
            format!("wire: {}", wire_summary(wire, anthropic_version.as_deref())),
        );
    }

    let (models, models_ok) = extract_models(report, &scope, table);
    valid &= models_ok;

    report.info(
        Some(scope.clone()),
        format!(
            "models: discovery {}; {} static entr{}",
            if discovery { "on" } else { "off" },
            models.len(),
            if models.len() == 1 { "y" } else { "ies" }
        ),
    );
    for model in &models {
        if let Ok(alias) = alias_for(key, &model.id, model.alias.as_deref()) {
            report.info(Some(scope.clone()), format!("model alias: {alias}"));
        }
    }
    if !discovery && models.is_empty() {
        report.warning(
            Some(scope.clone()),
            "discovery is off and no static models are listed; /model will show no entries for this provider (prefix routing still works)",
        );
    }
    if discovery {
        if models.is_empty() {
            report.info(
                Some(scope.clone()),
                "no static models; the catalog depends on discovery at launch",
            );
        }
        report.info(
            Some(scope.clone()),
            "discovery is configured; doctor does not contact the provider (offline)",
        );
    }

    if env_ok {
        match &env_key {
            Some(name) => {
                doctor_credential(report, &scope, name, env);
                valid &= credential_env_error(name).is_none();
            }
            None => report.warning(
                Some(scope.clone()),
                "no env_key is configured; this provider has no credential reference and requests will fail",
            ),
        }
    }

    if valid && let Some(base_url) = base_url {
        let form = ProviderForm {
            key: key.to_owned(),
            name,
            base_url,
            env_key,
            wire: wire.unwrap_or(WireKind::Responses),
            models_url,
            discovery: discovery_raw,
            anthropic_version,
            models,
        };
        match validate_provider(&form) {
            Ok(()) => {
                report.pass(Some(scope), "validation: ok");
                return ProviderDiagnosis {
                    key: key.to_owned(),
                    form: Some(form),
                };
            }
            Err(error) => {
                report.error(Some(scope), format!("validation failed: {error}"));
            }
        }
    }

    ProviderDiagnosis {
        key: key.to_owned(),
        form: None,
    }
}

fn extract_str(
    report: &mut DoctorReport,
    scope: &str,
    table: &dyn TableLike,
    key: &str,
) -> (Option<String>, bool) {
    match opt_str(table, key) {
        Ok(value) => (value, true),
        Err(message) => {
            report.error(Some(scope.to_owned()), message);
            (None, false)
        }
    }
}

fn extract_bool(
    report: &mut DoctorReport,
    scope: &str,
    table: &dyn TableLike,
    key: &str,
) -> (Option<bool>, bool) {
    match table.get(key) {
        None => (None, true),
        Some(item) => match item.as_bool() {
            Some(value) => (Some(value), true),
            None => {
                report.error(Some(scope.to_owned()), format!("`{key}` must be a boolean"));
                (None, false)
            }
        },
    }
}

fn extract_models(
    report: &mut DoctorReport,
    scope: &str,
    table: &dyn TableLike,
) -> (Vec<ModelForm>, bool) {
    let Some(item) = table.get("models") else {
        return (Vec::new(), true);
    };
    let Some(tables) = item.as_array_of_tables() else {
        report.error(
            Some(scope.to_owned()),
            "models must be an array of [[providers.<name>.models]] tables",
        );
        return (Vec::new(), false);
    };

    let mut models = Vec::new();
    let mut valid = true;
    let mut seen_ids = BTreeSet::new();
    for (index, model_table) in tables.iter().enumerate() {
        let label = format!("{scope}, models[{index}]");
        let (id, id_ok) = extract_str(report, &label, model_table, "id");
        valid &= id_ok;
        let Some(id) = id else {
            if id_ok {
                report.error(Some(label.clone()), "needs an id");
            }
            valid = false;
            continue;
        };
        if !seen_ids.insert(id.clone()) {
            report.warning(
                Some(scope.to_owned()),
                format!("static model id {id:?} is listed more than once; the first entry wins"),
            );
        }

        let (alias, alias_ok) = extract_str(report, &label, model_table, "alias");
        valid &= alias_ok;
        let (name, name_ok) = extract_str(report, &label, model_table, "name");
        valid &= name_ok;
        if name.as_deref().is_some_and(|name| name.trim().is_empty()) {
            report.error(Some(label.clone()), "name must not be empty");
            valid = false;
        }

        let context_window = match model_table.get("context_window") {
            None => None,
            Some(item) => match item.as_integer() {
                Some(limit) if limit > 0 => Some(limit),
                Some(_) => {
                    report.error(Some(label.clone()), "context_window must be positive");
                    valid = false;
                    None
                }
                None => {
                    report.error(Some(label.clone()), "context_window must be an integer");
                    valid = false;
                    None
                }
            },
        };

        let (input_modalities, modalities_ok) =
            extract_strings(report, &label, model_table, "input_modalities");
        valid &= modalities_ok;
        let (reasoning_efforts, efforts_ok) =
            extract_strings(report, &label, model_table, "reasoning_efforts");
        valid &= efforts_ok;

        models.push(ModelForm {
            id,
            alias,
            name,
            context_window,
            input_modalities,
            reasoning_efforts,
        });
    }
    (models, valid)
}

fn extract_strings(
    report: &mut DoctorReport,
    scope: &str,
    table: &dyn TableLike,
    key: &str,
) -> (Option<Vec<String>>, bool) {
    match table.get(key) {
        None => (None, true),
        Some(item) => {
            let Some(array) = item.as_array() else {
                report.error(
                    Some(scope.to_owned()),
                    format!("`{key}` must be an array of strings"),
                );
                return (None, false);
            };
            let mut values = Vec::new();
            for value in array.iter() {
                let Some(value) = value.as_str() else {
                    report.error(
                        Some(scope.to_owned()),
                        format!("`{key}` must be an array of strings"),
                    );
                    return (None, false);
                };
                values.push(value.to_owned());
            }
            (Some(values), true)
        }
    }
}

fn cross_provider_checks(report: &mut DoctorReport, diagnoses: &[ProviderDiagnosis]) {
    let mut env_owners: BTreeMap<&str, Vec<(&str, reqwest::Url)>> = BTreeMap::new();
    for diagnosis in diagnoses {
        if diagnosis.key == PROVIDER_ID {
            report.warning(
                Some(format!("provider {}", diagnosis.key)),
                format!(
                    "provider key matches the launch-scoped provider ID {PROVIDER_ID:?}; its aliases will start with {PROVIDER_ID}/"
                ),
            );
        }
        if let Some(form) = &diagnosis.form
            && let Some(env_key) = form.env_key.as_deref()
            && let Ok(url) = reqwest::Url::parse(&form.base_url)
        {
            env_owners
                .entry(env_key)
                .or_default()
                .push((&form.key, url));
        }
    }
    for (env_key, owners) in env_owners {
        if owners
            .iter()
            .skip(1)
            .any(|(_, url)| url.origin() != owners[0].1.origin())
        {
            let quoted = owners
                .iter()
                .map(|(owner, _)| format!("{owner:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            report.warning(
                Some("providers".to_owned()),
                format!(
                    "providers {quoted} share env_key {env_key}; the same credential would be sent to multiple origins"
                ),
            );
        }
    }

    let mut aliases: BTreeMap<String, (String, String)> = BTreeMap::new();
    for diagnosis in diagnoses {
        let Some(form) = &diagnosis.form else {
            continue;
        };
        for model in &form.models {
            let Ok(alias) = alias_for(&form.key, &model.id, model.alias.as_deref()) else {
                continue;
            };
            if let Some((other_key, other_id)) = aliases.get(&alias) {
                if other_key != &form.key || other_id != &model.id {
                    report.error(
                        None,
                        format!(
                            "model alias {alias:?} is used by both provider {other_key:?} and provider {:?}; give one entry an explicit `alias`",
                            form.key
                        ),
                    );
                }
            } else {
                aliases.insert(alias, (form.key.clone(), model.id.clone()));
            }
        }
    }
}

const KEY_REMEDY: &str = "ox setup   (choose the same service again to paste its key)";

fn doctor_credential(
    report: &mut DoctorReport,
    scope: &str,
    name: &str,
    env: &crate::provider::Env<'_>,
) {
    if let Some(reason) = credential_env_error(name) {
        report.error(Some(scope.to_owned()), reason);
        return;
    }
    // The value is inspected only for presence/emptiness; it is never stored
    // or formatted into a diagnostic.
    match credential_state(env, name) {
        CredentialState::Present => report.pass(
            Some(scope.to_owned()),
            format!("credentials: {name} is available to ox (value hidden)"),
        ),
        CredentialState::Missing => {
            report.suggest(KEY_REMEDY);
            report.error(
                Some(scope.to_owned()),
                format!("credential reference {name} is not set; add the key in ox setup or export it before launching"),
            );
        }
        CredentialState::Empty => {
            report.suggest(KEY_REMEDY);
            report.error(
                Some(scope.to_owned()),
                format!("credential reference {name} is set but empty"),
            );
        }
        CredentialState::Whitespace => report.warning(
            Some(scope.to_owned()),
            format!("credential reference {name} contains whitespace; the value will be treated as missing"),
        ),
    }
}

fn doctor_codex_config(
    report: &mut DoctorReport,
    home: &Path,
    vendor_keys: &BTreeSet<String>,
    search_configured: bool,
) {
    let scope = Some("config.toml".to_owned());
    let path = home.join("config.toml");
    let text = match read_optional_text_limited(&path, "config.toml") {
        Ok(Some(text)) => text,
        Ok(None) => {
            report.info(
                scope,
                "not found; Codex uses its defaults and ox injects its provider at launch",
            );
            return;
        }
        Err(message) => {
            report.error(scope, message);
            return;
        }
    };
    let document = match Document::parse(text.as_str()) {
        Ok(document) => document,
        Err(error) => {
            report.error(
                scope,
                format!("could not parse config.toml: {}", error.message()),
            );
            return;
        }
    };
    if document.get("model_catalog_json").is_some() {
        report.warning(
            scope.clone(),
            "model_catalog_json is set; Codex ignores live model discovery (ox warns at launch)",
        );
    }
    if search_configured
        && let Some(policy) = crate::provider::configured_web_search_policy(&document, None)
        && matches!(policy.as_str(), "disabled" | "cached" | "indexed")
    {
        report.warning(
            scope.clone(),
            format!(
                "Codex's configured web_search = {policy:?} keeps Brave search off. For a live launch use `ox codex -c 'web_search=\"live\"' --model <alias>`."
            ),
        );
    }
    if document
        .get("model_providers")
        .and_then(|providers| providers.get(PROVIDER_ID))
        .is_some()
    {
        report.warning(
            scope.clone(),
            format!(
                "[model_providers.{PROVIDER_ID}] is defined here but ox overrides it for the relay launch"
            ),
        );
    }
    if let Some(selected) = document.get("model_provider").and_then(Item::as_str)
        && vendor_keys.contains(selected)
    {
        report.info(
            scope,
            format!(
                "model_provider = {selected:?} is replaced at launch by the relay's {PROVIDER_ID:?} provider; routing follows model aliases"
            ),
        );
    }
}

fn doctor_legacy(report: &mut DoctorReport, home: &Path, env: &crate::provider::Env<'_>) {
    let scope = Some("config.toml".to_owned());
    let path = home.join("config.toml");
    let text = match read_optional_text_limited(&path, "config.toml") {
        Ok(Some(text)) => text,
        Ok(None) => {
            report.mode = "not configured".to_owned();
            report.suggest("ox setup");
            report.error(
                None,
                "neither codeox.toml nor config.toml exists. Run `ox setup` to add a provider",
            );
            return;
        }
        Err(message) => {
            report.error(scope, message);
            return;
        }
    };
    let document = match Document::parse(&text) {
        Ok(document) => document,
        Err(error) => {
            report.error(
                scope,
                format!("could not parse config.toml: {}", error.message()),
            );
            return;
        }
    };
    let Some(id) = document.get("model_provider").and_then(Item::as_str) else {
        report.mode = "not configured".to_owned();
        report.suggest("ox setup");
        report.error(
            scope,
            "No ox provider configured. Run `ox setup` to get started.",
        );
        return;
    };

    let Some(provider) = document
        .get("model_providers")
        .and_then(|providers| providers.get(id))
        .and_then(Item::as_table)
    else {
        report.mode = "not configured".to_owned();
        report.suggest("ox setup");
        report.error(
            scope,
            format!("Codex provider {id:?} has no custom provider settings. Run `ox setup` to add a provider for ox."),
        );
        return;
    };
    report.info(scope.clone(), format!("selected provider: {id}"));
    if document.get("model_catalog_json").is_some() {
        report.error(
            scope.clone(),
            "model_catalog_json is set; legacy mode refuses to start. Remove it to enable live model discovery.",
        );
    }

    if let Some(name) = provider.get("name").and_then(Item::as_str) {
        report.info(scope.clone(), format!("name: {name}"));
    }
    if provider.get("base_url").and_then(Item::as_str).is_none() {
        report.error(scope.clone(), "the selected provider needs a base_url");
    }
    let wire = provider
        .get("wire_api")
        .and_then(Item::as_str)
        .unwrap_or("responses");
    if WireKind::parse_config(wire) != Some(WireKind::Responses) {
        report.error(
            scope.clone(),
            format!(
                "legacy mode supports wire_api = \"responses\" only; {wire:?} needs multi-vendor adapters and a codeox.toml provider"
            ),
        );
    }
    match provider.get("env_key").and_then(Item::as_str) {
        Some(name) => doctor_credential(report, "config.toml", name, env),
        None => report.error(
            scope.clone(),
            "set env_key to the name of the provider API key environment variable",
        ),
    }

    let errors_before = report.count(Level::Error);
    match crate::provider::Provider::load(home, env) {
        Ok(_) => report.pass(scope, "config.toml is valid; the legacy relay can start"),
        Err(error) => {
            if report.count(Level::Error) == errors_before {
                report.error(scope, error.to_string());
            }
        }
    }
}

fn credential_state(env: &crate::provider::Env<'_>, name: &str) -> CredentialState {
    match env(name) {
        None => CredentialState::Missing,
        Some(value) => {
            if value.is_empty() {
                CredentialState::Empty
            } else if value.chars().any(char::is_whitespace) {
                CredentialState::Whitespace
            } else {
                CredentialState::Present
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialState {
    Present,
    Missing,
    Empty,
    Whitespace,
}

// ---------------------------------------------------------------------------
// Doctor: shared file and TOML helpers
// ---------------------------------------------------------------------------

pub(crate) fn read_optional_text_limited(
    path: &Path,
    label: &str,
) -> Result<Option<String>, String> {
    match fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(format!("could not read {label}")),
        Ok(metadata) => {
            if metadata.len() > MAX_CONFIG_BYTES {
                return Err(format!("{label} is too large"));
            }
            match fs::read_to_string(path) {
                Ok(text) => Ok(Some(text)),
                Err(_) => Err(format!("could not read {label} as UTF-8")),
            }
        }
    }
}

fn read_text_limited(path: &Path, label: &str) -> Result<String, String> {
    match read_optional_text_limited(path, label)? {
        Some(text) => Ok(text),
        None => Err(format!("{label} disappeared while doctor was running")),
    }
}

fn opt_str(table: &dyn TableLike, key: &str) -> Result<Option<String>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => item
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| format!("`{key}` must be a string")),
    }
}

// ---------------------------------------------------------------------------
// Doctor command
// ---------------------------------------------------------------------------

/// Plain-text entry point used by `tests/setup.rs`.
#[allow(dead_code)]
pub(crate) fn run_doctor(
    args: &[OsString],
    codex_home: &Path,
    output: &mut dyn Write,
    env: &crate::provider::Env<'_>,
) -> Result<u8, RelayError> {
    run_doctor_styled(args, codex_home, output, env, false)
}

pub(crate) fn run_doctor_styled(
    args: &[OsString],
    codex_home: &Path,
    output: &mut dyn Write,
    env: &crate::provider::Env<'_>,
    color: bool,
) -> Result<u8, RelayError> {
    let mut strict = false;
    let mut fix = false;
    for argument in args {
        let argument = argument
            .to_str()
            .ok_or("doctor arguments must be valid UTF-8")?;
        match argument {
            "--strict" => strict = true,
            "--fix" => fix = true,
            "-h" | "--help" => {
                write_doctor_usage(output)?;
                return Ok(0);
            }
            other => {
                writeln!(output, "error: unknown doctor option {other:?}")?;
                writeln!(output)?;
                write_doctor_usage(output)?;
                return Ok(2);
            }
        }
    }
    if fix {
        let repairs = repair(codex_home)?;
        writeln!(
            output,
            "{}",
            crate::ui::paint("repairs", crate::ui::BOLD, color)
        )?;
        if repairs.is_empty() {
            writeln!(output, "  · nothing needed repair")?;
        }
        for line in &repairs {
            writeln!(
                output,
                "  {} fixed: {line}",
                crate::ui::paint("✓", crate::ui::GREEN, color)
            )?;
        }
        writeln!(output)?;
    }
    let report = doctor_report(codex_home, env);
    write!(output, "{}", report.render(color))?;
    Ok(report.exit_code(strict))
}

// ---------------------------------------------------------------------------
// Self-repair
// ---------------------------------------------------------------------------

/// Same file name as `picker::FAVORITES_FILE`; repeated here because this
/// module is also compiled into integration tests without the picker.
const FAVORITES_FILE_NAME: &str = "ox-favorites.json";
/// A temporary file older than this was left by an interrupted write.
const STALE_TEMPORARY_SECONDS: u64 = 600;

/// Repairs the local state ox can safely repair itself and returns one line
/// per repair. Unreadable files are renamed next to the original, never
/// deleted, so every repair is reversible. Running it again is a no-op.
pub(crate) fn repair(home: &Path) -> Result<Vec<String>, RelayError> {
    let mut fixed = Vec::new();
    if !home.is_dir() {
        return Ok(fixed);
    }
    let _lock = lock_config(home)?;

    let config_path = CodeoxConfig::path(home);
    if let Ok(Some(text)) = read_existing_codeox(&config_path) {
        let unusable = match Document::parse(&text) {
            Err(_) => true,
            Ok(document) => !document
                .get("providers")
                .and_then(Item::as_table)
                .is_some_and(|table| !table.is_empty()),
        };
        if unusable {
            let backup = quarantine(&config_path)?;
            fixed.push(format!(
                "codeox.toml could not be used, so it was moved to {backup}. Saved keys are kept; run ox setup to add your providers again"
            ));
        }
    }

    if SavedKeys::load(home)
        .is_err_and(|error| error.to_string() == "could not parse ox-api-keys.json")
    {
        let backup = quarantine(&SavedKeys::path(home))?;
        fixed.push(format!(
            "ox-api-keys.json could not be read, so it was moved to {backup}. Paste your keys again in ox setup"
        ));
    }

    let favorites = home.join(FAVORITES_FILE_NAME);
    if let Ok(Some(text)) = read_optional_text_limited(&favorites, FAVORITES_FILE_NAME)
        && serde_json::from_str::<Vec<String>>(&text).is_err()
        && !favorites.is_symlink()
    {
        let backup = quarantine(&favorites)?;
        fixed.push(format!(
            "ox-favorites.json could not be read, so it was moved to {backup}. Star your models again in the picker"
        ));
    }

    let mut removed = 0;
    for entry in fs::read_dir(home)?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let temporary = [
            ".codeox.toml.tmp-",
            ".ox-api-keys.json.tmp-",
            ".ox-favorites.json.tmp-",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix));
        let stale = entry
            .metadata()
            .ok()
            .filter(|metadata| metadata.is_file())
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age.as_secs() >= STALE_TEMPORARY_SECONDS);
        if temporary && stale && fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        fixed.push(format!(
            "removed {removed} temporary file(s) left by an interrupted save"
        ));
    }
    Ok(fixed)
}

/// Renames `path` to `<name>.broken-<unix seconds>` in the same directory and
/// returns the new file name. Never overwrites an existing backup.
fn quarantine(path: &Path) -> Result<String, RelayError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid file name")?;
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let mut backup = format!("{name}.broken-{seconds}");
    let mut attempt = 1;
    while path.with_file_name(&backup).symlink_metadata().is_ok() {
        attempt += 1;
        backup = format!("{name}.broken-{seconds}-{attempt}");
    }
    fs::rename(path, path.with_file_name(&backup))
        .map_err(|error| format!("could not move {name} aside: {error}"))?;
    Ok(backup)
}

fn write_doctor_usage(output: &mut dyn Write) -> Result<(), RelayError> {
    writeln!(output, "Usage:")?;
    writeln!(output, "  ox doctor [--fix] [--strict]")?;
    writeln!(
        output,
        "  Without API providers, shows recognized native client sign-ins."
    )?;
    writeln!(
        output,
        "  ox doctor --live --provider NAME [--model UPSTREAM_MODEL_ID]"
    )?;
    writeln!(output)?;
    writeln!(
        output,
        "Checks $CODEX_HOME/codeox.toml (or config.toml in legacy mode) offline,"
    )?;
    writeln!(
        output,
        "without sending inference requests. It reports each provider's wire, credential reference,"
    )?;
    writeln!(
        output,
        "model discovery configuration, and relevant launch/config conflicts."
    )?;
    writeln!(
        output,
        "Credential values and auth.json are never read into the report."
    )?;
    writeln!(
        output,
        "With --live, sends one streamed inference probe to the selected vendor."
    )?;
    writeln!(output)?;
    writeln!(
        output,
        "  --fix       repair what ox can repair itself, then run the check. Unreadable"
    )?;
    writeln!(
        output,
        "              files are moved to <name>.broken-<time>, never deleted"
    )?;
    writeln!(
        output,
        "  --strict    exit non-zero when there are warnings as well as errors"
    )?;
    writeln!(output, "  -h, --help  show this help")?;
    writeln!(output)?;
    writeln!(
        output,
        "Exit status: 0 healthy, 1 problems found, 2 usage error."
    )?;
    Ok(())
}

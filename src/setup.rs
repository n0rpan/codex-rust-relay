//! Interactive and scriptable `ox provider add`, plus offline `ox doctor`.
//!
//! The relay itself never writes to `$CODEX_HOME`. These two subcommands are the
//! only ox code that edits `codeox.toml`, and they deliberately keep the
//! edit narrow:
//!
//! * only `[providers.<key>]` and its `[[providers.<key>.models]]` tables are
//!   added; every existing byte, comment and top-level key is preserved;
//! * duplicate provider keys are refused instead of merged or overwritten;
//! * edits are serialized, validated with [`provider::CodeoxConfig`], and
//!   saved with a same-directory temporary file plus rename;
//! * only credential *references* (`env_key = "NAME"`) are stored. No flag or
//!   prompt accepts a credential value, and neither command ever prints one.
//!
//! `doctor` is offline: it reads `codeox.toml` and `config.toml`, checks the
//! environment for credential references, and makes no network requests. It
//! never opens `auth.json`.
//!
//! The other worker owns `provider.rs`, `catalog.rs`, the adapters and
//! `main.rs`. This module therefore depends only on the current public surface
//! of [`provider`] (`CodeoxConfig`, `alias_for`, the file-name/limits
//! constants and `RelayError`) and leaves dispatch wiring to `main.rs`:
//!
//! ```ignore
//! mod setup;
//! // ... after resolving codex_home:
//! let args: Vec<OsString> = args.collect();
//! if let Some(result) = setup::dispatch(&args, &codex_home) {
//!     return result;
//! }
//! // ... then pass the same args to the Codex child as before.
//! ```
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
    CodeoxConfig, MAX_CONFIG_BYTES, PROVIDER_ID, RelayError, TOKEN_ENV, alias_for,
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
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let env = |name: &str| crate::provider::process_env(name);

    match args.first().and_then(|arg| arg.to_str()) {
        Some("add") => run_provider_add(
            &args[1..],
            codex_home,
            &mut input,
            &mut output,
            interactive,
            &env,
        ),
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

fn doctor_command(args: &[OsString], codex_home: &Path) -> Result<u8, RelayError> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let env = |name: &str| crate::provider::process_env(name);
    run_doctor(args, codex_home, &mut output, &env)
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
                    return Err("provider key was given more than once".to_owned());
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
                    "ox stores credential references, not credential values; use --env-key NAME to name an environment variable"
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
    if key.is_empty()
        || key.len() > 64
        || key.starts_with(['-', '_'])
        || !key
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'_' || ch == b'-')
    {
        return Err(
            "provider keys use lowercase ASCII letters, digits, `_` or `-`, starting with a letter or digit"
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
    if name.is_empty() || name.len() > 128 {
        return Err(
            "pass the name of an environment variable (for example MY_PROVIDER_API_KEY), not a credential value"
                .to_owned(),
        );
    }
    let looks_like_secret = name.starts_with("sk-")
        || name.starts_with("sk_")
        || name.starts_with("xox")
        || name.starts_with("ghp_")
        || name.starts_with("AIza");
    let valid_name = name
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    if looks_like_secret || !valid_name {
        return Err(
            "that does not look like an environment variable name; pass a name such as MY_PROVIDER_API_KEY, never the key itself"
                .to_owned(),
        );
    }
    if name == TOKEN_ENV {
        return Err(format!(
            "{TOKEN_ENV} is reserved for the relay's launch token; choose another variable name"
        ));
    }
    Ok(name.to_owned())
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
        mut validate: impl FnMut(&str) -> Result<T, String>,
    ) -> Result<T, RelayError> {
        loop {
            self.write_prompt(label, None)?;
            let line = self.read_line()?;
            if line.is_empty() {
                writeln!(self.output, "  error: a value is required")?;
                continue;
            }
            match validate(&line) {
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
                "Wire protocol (responses, chat_completions, anthropic_messages)",
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

    let mut prompt = Prompt { input, output };

    let key = match parsed.key.take() {
        Some(key) => match validate_provider_key(&key) {
            Ok(key) => key,
            Err(message) => return usage_error(prompt.output, &message),
        },
        None if interactive => prompt.ask_required_with("Provider key", validate_provider_key)?,
        None => {
            return usage_error(
                prompt.output,
                "provider key is required (for example: ox provider add openrouter --base-url https://openrouter.ai/api/v1 --wire responses)",
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
            prompt.ask_optional(&format!("Display name (blank uses {key:?})"), None)?
        }
        None => None,
    };

    let base_url = match parsed.base_url.take() {
        Some(base_url) => base_url,
        None if interactive => prompt.ask_required_with("Base URL", validate_base_url)?,
        None => {
            return usage_error(
                prompt.output,
                "--base-url is required when setup is not interactive (for example https://api.example.com/v1)",
            );
        }
    };

    let wire = match parsed.wire {
        Some(wire) => wire,
        None if interactive => prompt.ask_wire(WireKind::Responses)?,
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
        loop {
            let value = prompt.ask_default(
                "Environment variable holding the API key (never the key itself)",
                &default,
            )?;
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
            let Some(id) = prompt.ask_optional(
                "Static model id (blank to rely on discovery; also useful as an override)",
                None,
            )?
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
    let candidate = append_provider(&existing, &section);
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
                    "  warning: ${name} is not set in this environment; export it before launching"
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
            "Next: run `ox doctor`, then launch with a model alias in the form {}/<upstream-model-id>.",
            form.key
        )?;
    }
    Ok(0)
}

fn usage_error(output: &mut dyn Write, message: &str) -> Result<u8, RelayError> {
    writeln!(output, "error: {message}")?;
    writeln!(output)?;
    write_add_usage(output)?;
    Ok(2)
}

fn write_provider_usage(output: &mut dyn Write) -> Result<(), RelayError> {
    writeln!(output, "Usage:")?;
    writeln!(output, "  ox provider add [KEY] [options]")?;
    writeln!(output)?;
    writeln!(output, "Run `ox provider add --help` for options.")?;
    Ok(())
}

fn write_add_usage(output: &mut dyn Write) -> Result<(), RelayError> {
    writeln!(output, "Usage:")?;
    writeln!(output, "  ox provider add [KEY] [options]")?;
    writeln!(output)?;
    writeln!(
        output,
        "Adds one provider to $CODEX_HOME/codeox.toml, preserving the rest of the"
    )?;
    writeln!(
        output,
        "file byte for byte. Credential values are never accepted or stored:"
    )?;
    writeln!(
        output,
        "--env-key names the environment variable ox reads at launch."
    )?;
    writeln!(output)?;
    writeln!(output, "Options:")?;
    writeln!(
        output,
        "  --provider-key KEY           Provider key (alternative to the positional KEY)"
    )?;
    writeln!(
        output,
        "  --name NAME                  Display name (default: KEY)"
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
        "                               (default: derived from KEY, e.g. KEY_API_KEY)"
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
        "With a terminal and no required flags, provider add prompts for the key,"
    )?;
    writeln!(
        output,
        "name, URL, wire, credential variable and discovery; it never prompts for"
    )?;
    writeln!(output, "a credential value.")?;
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

fn append_provider(existing: &str, section: &str) -> String {
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

fn lock_config(codex_home: &Path) -> Result<File, RelayError> {
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

fn write_config_atomic(path: &Path, contents: &str) -> Result<(), RelayError> {
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
    result.map_err(|error| format!("could not write codeox.toml atomically: {error}").into())
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

fn check_alias_collisions(config: &CodeoxConfig) -> Result<(), RelayError> {
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
    Info,
    Warning,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
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
}

impl DoctorReport {
    fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            mode: "unknown".to_owned(),
            diagnostics: Vec::new(),
            provider_count: 0,
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

    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("ox doctor (offline)\n");
        out.push_str(&format!("home: {}\n", self.home.display()));
        out.push_str(&format!("mode: {}\n", self.mode));
        out.push_str(&format!("providers: {}\n", self.provider_count));
        let mut current: Option<&str> = None;
        for diagnostic in &self.diagnostics {
            let scope = diagnostic.scope.as_deref();
            if scope != current {
                if let Some(scope) = scope {
                    out.push('\n');
                    out.push_str(scope);
                    out.push('\n');
                }
                current = scope;
            }
            out.push_str(&format!(
                "  {}: {}\n",
                diagnostic.level.as_str(),
                diagnostic.message
            ));
        }
        out.push_str(&format!(
            "\nsummary: {} provider(s), {} error(s), {} warning(s)\n",
            self.provider_count,
            self.count(Level::Error),
            self.count(Level::Warning)
        ));
        out
    }
}

fn doctor_report(home: &Path, env: &crate::provider::Env<'_>) -> DoctorReport {
    let mut report = DoctorReport::new(home);
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
            report.error(
                None,
                format!("could not parse codeox.toml: {}", error.message()),
            );
            return;
        }
    };
    let Some(providers) = document.get("providers") else {
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
        report.error(None, "codeox.toml must configure at least one provider");
    }

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
    doctor_codex_config(report, home, &vendor_keys, search_configured);

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
                report.info(
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
            Some(name) => doctor_credential(report, &scope, name, env),
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
                report.info(Some(scope), "validation: ok");
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

fn doctor_credential(
    report: &mut DoctorReport,
    scope: &str,
    name: &str,
    env: &crate::provider::Env<'_>,
) {
    // The value is inspected only for presence/emptiness; it is never stored
    // or formatted into a diagnostic.
    match credential_state(env, name) {
        CredentialState::Present => report.info(
            Some(scope.to_owned()),
            format!("credentials: {name} is set (value not shown)"),
        ),
        CredentialState::Missing => report.error(
            Some(scope.to_owned()),
            format!("credential reference {name} is not set in this environment; export it before launching"),
        ),
        CredentialState::Empty => report.error(
            Some(scope.to_owned()),
            format!("credential reference {name} is set but empty"),
        ),
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
            report.error(
                None,
                "neither codeox.toml nor config.toml exists; run `ox provider add` or configure Codex first",
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
    report.info(
        scope.clone(),
        "legacy relay mode: the provider selected by model_provider is relayed",
    );
    if document.get("model_catalog_json").is_some() {
        report.error(
            scope.clone(),
            "model_catalog_json is set; legacy mode refuses to start. Remove it to enable live model discovery.",
        );
    }

    let Some(id) = document.get("model_provider").and_then(Item::as_str) else {
        report.error(
            scope,
            "set model_provider to a provider ID using letters, numbers, _ or -",
        );
        return;
    };
    report.info(scope.clone(), format!("selected provider: {id}"));

    let Some(provider) = document
        .get("model_providers")
        .and_then(|providers| providers.get(id))
        .and_then(Item::as_table)
    else {
        report.error(
            scope,
            format!("the selected model_provider {id:?} is missing from model_providers"),
        );
        return;
    };

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
        Ok(_) => report.info(scope, "config.toml is valid; the legacy relay can start"),
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

fn read_optional_text_limited(path: &Path, label: &str) -> Result<Option<String>, String> {
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

pub(crate) fn run_doctor(
    args: &[OsString],
    codex_home: &Path,
    output: &mut dyn Write,
    env: &crate::provider::Env<'_>,
) -> Result<u8, RelayError> {
    let mut strict = false;
    for argument in args {
        let argument = argument
            .to_str()
            .ok_or("doctor arguments must be valid UTF-8")?;
        match argument {
            "--strict" => strict = true,
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
    let report = doctor_report(codex_home, env);
    write!(output, "{}", report.render())?;
    Ok(report.exit_code(strict))
}

fn write_doctor_usage(output: &mut dyn Write) -> Result<(), RelayError> {
    writeln!(output, "Usage:")?;
    writeln!(output, "  ox doctor [--strict]")?;
    writeln!(output)?;
    writeln!(
        output,
        "Checks $CODEX_HOME/codeox.toml (or config.toml in legacy mode) offline,"
    )?;
    writeln!(
        output,
        "without network access. It reports each provider's wire, credential reference,"
    )?;
    writeln!(
        output,
        "model discovery configuration, and relevant launch/config conflicts."
    )?;
    writeln!(
        output,
        "Credential values and auth.json are never read into the report."
    )?;
    writeln!(output)?;
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

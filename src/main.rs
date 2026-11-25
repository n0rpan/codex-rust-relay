//! Authenticated loopback gateway for stock Codex and Claude Code.
//!
//! Each launch selects a client protocol and routes model aliases through the
//! same provider registry and credential lookup. The relay lives for the
//! lifetime of the client process.

mod adapters;
mod catalog;
mod claude_frontend;
mod claude_gateway;
mod claude_request;
mod claude_response;
mod claude_search;
mod clients;
mod daemon;
mod desktop_launch;
mod desktop_settings;
mod diagnostics;
mod help;
mod host_lifetime;
mod native;
mod picker;
mod provider;
mod search;
mod settings_protocol;
mod setup;
mod sse;
mod standalone_search;
mod translate;
mod ui;
mod update;
mod upstream;
mod version;
mod workers;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::IsTerminal;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use catalog::{BundledModels, CatalogModel, ProviderModels};
use clients::ClientKind;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use provider::{
    CodeoxConfig, Env, PROVIDER_ID, RelayError, Resolution, Routes, SearchSettings, SecretKey,
    TOKEN_ENV,
};
use reqwest::Client;
use search::{BraveSearch, SearchConfig};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::process::Command;

type RelayBody = UnsyncBoxBody<Bytes, RelayError>;

/// Responses requests are buffered only so the `model` field can be rewritten.
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
/// Upstream model catalogs are parsed with a much smaller bound.
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const CATALOG_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an unused provider connection stays open for the next request.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
const MAX_ECHOED_MODEL_CHARS: usize = 120;

struct App {
    mode: Mode,
    client_kind: ClientKind,
    persistent: bool,
    client: Client,
    local_auth: HeaderValue,
    max_request_bytes: usize,
    /// Configured vendor credentials, used to scrub upstream error text.
    secrets: Arc<Vec<String>>,
    /// One pooled Brave client, present only when live search is opted in and
    /// its credential reference is usable.
    search: Option<BraveSearch>,
}

enum Mode {
    Legacy {
        provider: provider::Provider,
        bundled: BundledModels,
    },
    Codeox {
        vendors: Vec<VendorState>,
        routes: Routes,
        catalog: Value,
    },
}

struct VendorState {
    vendor: provider::Vendor,
    auth: Option<SecretKey>,
}

pub(crate) use version::VERSION;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match launch(std::env::args_os().skip(1)).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!(
                "{}",
                ui::render_error("ox", &error.to_string(), ui::stderr_color())
            );
            ExitCode::FAILURE
        }
    }
}

enum LaunchConfig {
    Codeox(CodeoxConfig),
    Legacy(provider::Provider),
}

impl LaunchConfig {
    fn env_refs(&self) -> BTreeSet<String> {
        match self {
            Self::Codeox(config) => config.env_refs(),
            Self::Legacy(provider) => [provider.env_key.clone()].into_iter().collect(),
        }
    }
}

async fn launch(args: impl Iterator<Item = OsString>) -> Result<u8, RelayError> {
    let args: Vec<OsString> = args.collect();
    if args.first().is_some_and(|a| a == "codex" || a == "claude") {
        host_lifetime::supervise()?;
    }
    if args
        .first()
        .is_some_and(|arg| arg == "--version" || arg == "-V")
    {
        println!("ox {VERSION}");
        return Ok(0);
    }
    if let Some(result) = help::dispatch(&args) {
        return result;
    }
    if args.first().is_some_and(|arg| arg == "host") {
        return desktop_launch::host(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "desktop") {
        return desktop_launch::run(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "update") {
        return update::run(&args[1..]).await;
    }
    let codex_home = std::env::var_os("CODEX_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .ok_or("set CODEX_HOME or HOME")?;
    if let Some(result) = native::dispatch(&args, &codex_home).await {
        return result;
    }
    match args.first().and_then(|arg| arg.to_str()) {
        Some("--ox-desktop-settings-v1") if args.len() == 1 => {
            return desktop_settings::run(&codex_home).await;
        }
        Some("setup") => return run_setup(&args[1..], &codex_home).await,
        Some("serve") => return daemon::run(&args[1..], &codex_home).await,
        Some("doctor") if args.iter().any(|arg| arg == "--live") => {
            return diagnostics::run(&args[1..], &codex_home).await;
        }
        Some("doctor")
            if args[1..].iter().all(|arg| arg == "--strict")
                && !CodeoxConfig::path(&codex_home).try_exists()?
                && matches!(
                    provider::Provider::load_if_configured(&codex_home, &provider::process_env),
                    Ok(None)
                )
                && native::report_saved_accounts(&codex_home) =>
        {
            return Ok(0);
        }
        _ => {}
    }
    if let Some(result) = setup::dispatch(&args, &codex_home) {
        return result;
    }
    if let Some(first) = args.first().and_then(|arg| arg.to_str())
        && let Some(suggestion) = mistyped_command(first)
    {
        return Err(format!(
            "unknown command {first:?}\n  fix: ox {suggestion}   (to send {first:?} to Codex instead: ox codex {first})"
        )
        .into());
    }
    let bare = args.is_empty();
    let (client_kind, mut args) = clients::select(args);
    let classifier = if matches!(client_kind, ClientKind::Claude) {
        clients::take_client_classifier(&mut args)?
    } else {
        None
    };
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    let config_path = CodeoxConfig::path(&codex_home);
    let config = if config_path.exists() {
        LaunchConfig::Codeox(load_config(&config_path)?)
    } else if matches!(client_kind, ClientKind::Codex)
        && let Some(provider) =
            provider::Provider::load_if_configured(&codex_home, &provider::process_env)?
    {
        LaunchConfig::Legacy(provider)
    } else {
        if classifier.is_some() {
            return Err("--client-classifier needs an ox API provider. Run ox setup first.".into());
        }
        if !args.is_empty() {
            return Err(NO_PROVIDER.into());
        }
        if let Some(code) = native::launch_if_signed_in(client_kind, &codex_home).await? {
            return Ok(code);
        }
        if bare
            && let Some(code) = native::launch_if_signed_in(ClientKind::Claude, &codex_home).await?
        {
            return Ok(code);
        }
        if !interactive {
            if bare {
                help::overview();
                return Ok(0);
            }
            return Err(NO_PROVIDER.into());
        }
        match first_run(client_kind, &codex_home).await? {
            Some(code) => return Ok(code),
            None => LaunchConfig::Codeox(load_config(&config_path)?),
        }
    };
    if interactive
        && args.is_empty()
        && matches!(&config, LaunchConfig::Codeox(_))
        && !api_ready(&codex_home)
        && !native::has_saved_account(client_kind, &codex_home)
        && let Err(error) = setup::prompt_missing_keys(&codex_home)
    {
        eprintln!("ox setup: {error}");
    }
    let saved = match &config {
        LaunchConfig::Codeox(config) => provider::SavedKeys::for_config(&codex_home, config)?,
        LaunchConfig::Legacy(_) => provider::SavedKeys::default(),
    };
    let env: &Env<'_> = &|name| saved.get(name);
    let mut env_refs = config.env_refs();
    env_refs.insert(daemon::KEY_ENV.to_owned());
    let client = upstream_client()?;

    let codex_profile = if matches!(client_kind, ClientKind::Codex) {
        let home = clients::relay_client_home(&codex_home, ClientKind::Codex)?;
        if matches!(&config, LaunchConfig::Codeox(_)) {
            warn_about_static_catalog(&home);
        }
        Some(home)
    } else {
        None
    };
    let (search, search_launch) = match (&config, &codex_profile) {
        (LaunchConfig::Codeox(config), Some(home)) => {
            resolve_search(config.search.as_ref(), env, &args, home)
        }
        (LaunchConfig::Codeox(config), None) => (
            config
                .search
                .as_ref()
                .and_then(|settings| build_search(settings, env)),
            SearchLaunch::Disabled,
        ),
        (LaunchConfig::Legacy(_), _) => (None, SearchLaunch::Disabled),
    };

    let bundled = async {
        match &codex_profile {
            Some(home) => bundled_catalog(home, &env_refs).await,
            None => Ok(BundledModels::default()),
        }
    };
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let base_url = format!("{origin}/v1");
    let token = random_token()?;
    let overrides = match &config {
        LaunchConfig::Codeox(config) => codeox_overrides(
            &base_url,
            config.vendors.iter().any(|v| v.wire.is_translated()),
            search_launch,
        ),
        LaunchConfig::Legacy(provider) => {
            let mut options = legacy_overrides(&provider.id, &base_url);
            if native::title_generation() {
                // Ephemeral naming jobs ignore all user config, including the selected provider.
                // Carry just the relay identity; inference credentials remain with OX.
                options.push(format!("model_provider={}", json!(provider.id)));
                options.push(format!(
                    "model_providers.{}.name={}",
                    provider.id,
                    json!(provider.name)
                ));
                options.push(format!(
                    "model_providers.{}.wire_api=\"responses\"",
                    provider.id
                ));
            }
            options
        }
    };

    let mut app = match config {
        LaunchConfig::Codeox(config) => {
            App::codeox_with(
                config,
                bundled,
                &token,
                client,
                MAX_REQUEST_BYTES,
                env,
                search,
            )
            .await?
        }
        LaunchConfig::Legacy(provider) => App::legacy(provider, bundled.await?, &token, client)?,
    };

    let classifier = classifier
        .as_deref()
        .map(|mode| app.client_classifier(mode))
        .transpose()?;
    if let Some(requested) = clients::explicit_model(&args) {
        app.check_launch_model(requested)?;
    }
    if interactive && args.is_empty() && matches!(&app.mode, Mode::Codeox { .. }) {
        let signed_in = classifier.is_none() && native::has_saved_account(client_kind, &codex_home);
        let models = app.launch_models();
        match picker::choose_model(client_kind, &models, signed_in, &codex_home)? {
            Some(picker::LaunchChoice::Model(alias)) => {
                args.extend([OsString::from("--model"), OsString::from(alias)]);
            }
            Some(picker::LaunchChoice::Subscription) => {
                let relay = workers::Relay::new(listener, app, token);
                return native::launch_subscription(client_kind, &codex_home, Some(relay)).await;
            }
            None => return Ok(0),
        }
    }
    if let Some(model) = clients::explicit_model(&args) {
        app.prewarm(model);
    }
    let favorites = picker::read_favorites(&codex_home).unwrap_or_default();
    if let Mode::Codeox { catalog, .. } = &mut app.mode {
        catalog::prioritize_favorites(catalog, &favorites);
    }
    app.client_kind = client_kind;
    // A session the OX session host runs on an API alias gets read_ox_thread,
    // so a handoff target can read its source. Workers stay subscription-only.
    let gateway = if host_lifetime::hosted() {
        match workers::Relay::load(&codex_home).await? {
            Some(relay) => {
                Some(relay.attach(&args, &codex_home, client_kind, workers::Tools::HandoffRead)?)
            }
            None => None,
        }
    } else {
        None
    };
    let (mcp, gateway) = gateway.map_or((vec![], None), |(mcp, g)| (mcp, Some(g)));
    let (mut command, _claude_settings) = match codex_profile {
        Some(home) => {
            let mut command = codex_relay_command(&home)?;
            for name in &env_refs {
                command.env_remove(name);
            }
            command.env(TOKEN_ENV, &token);
            for entry in &overrides {
                command.arg("-c").arg(entry);
            }
            command.args(&mcp);
            command.args(args);
            (command, None)
        }
        None => {
            let client_home = clients::relay_client_home(&codex_home, ClientKind::Claude)?;
            if let Some(classifier) = &classifier {
                match classifier {
                    Some(alias) => eprintln!(
                        "ox: Claude Code will send separate classifier requests to {alias}; API usage may be billed."
                    ),
                    None => eprintln!(
                        "ox: no configured Sonnet 5 or Sonnet 4.6 classifier found. Claude Code will use its own classifier selection."
                    ),
                }
            }
            let starred: Vec<_> = app
                .launch_models()
                .into_iter()
                .map(|(alias, _)| alias)
                .filter(|alias| favorites.iter().any(|favorite| favorite == alias))
                .collect();
            let (command, settings) = clients::claude_command(
                args,
                &origin,
                &token,
                &app.claude_aliases(),
                &env_refs,
                clients::ClaudeLaunchOptions {
                    starred: &starred,
                    config_dir: Some(&client_home),
                    client_classifier: classifier.as_ref().map(|alias| alias.as_deref()),
                },
                |model| app.claude_model_info(model),
            )?;
            let mut command = command;
            command.args(&mcp);
            (command, Some(settings))
        }
    };
    if let Some(gateway) = &gateway {
        let (name, value) = gateway.env();
        command.env(name, value);
    }
    command.kill_on_drop(true);
    exempt_loopback_proxy(&mut command);

    host_lifetime::isolate_stock(&mut command);
    let mut child = command.spawn().map_err(|error| {
        let client = command.as_std().get_program().to_string_lossy();
        client_launch_error(&client, error)
    })?;
    let gateway = async move {
        match gateway {
            Some(gateway) => gateway.serve().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        status = child.wait() => Ok(status?.code().unwrap_or(1) as u8),
        result = serve(listener, Arc::new(app)) => Err(result.unwrap_err().into()),
        error = gateway => Err(error),
    }
}

fn exempt_loopback_proxy(command: &mut Command) {
    for name in ["NO_PROXY", "no_proxy"] {
        let existing = std::env::var(name)
            .or_else(|_| {
                std::env::var(if name == "NO_PROXY" {
                    "no_proxy"
                } else {
                    "NO_PROXY"
                })
            })
            .unwrap_or_default();
        command.env(name, format!("{existing},127.0.0.1,localhost"));
    }
}

const NO_PROVIDER: &str = "no ox API provider configured. Run `ox setup` to add one, or `ox help subscriptions` for native sign-in";

const OX_COMMANDS: &[&str] = &[
    "setup", "doctor", "provider", "serve", "update", "login", "help", "codex", "claude",
];
/// Stock Codex subcommands that `ox <word>` forwards unchanged.
const CODEX_COMMANDS: &[&str] = &[
    "exec",
    "review",
    "logout",
    "mcp",
    "mcp-server",
    "app-server",
    "completion",
    "sandbox",
    "debug",
    "apply",
    "resume",
    "fork",
    "cloud",
    "features",
    "plugin",
    "marketplace",
    "app",
    "execpolicy",
];

/// Returns the ox command a mistyped first word most likely meant. Exact ox
/// commands, stock Codex subcommands, options and prompts return `None`.
fn mistyped_command(first: &str) -> Option<&'static str> {
    if first.is_empty()
        || !first.chars().all(|c| c.is_ascii_alphabetic())
        || OX_COMMANDS.contains(&first)
        || CODEX_COMMANDS.contains(&first)
    {
        return None;
    }
    ui::closest(first, OX_COMMANDS.iter().copied())
}

/// Loads `codeox.toml` for a launch and points at the doctor when the error
/// has no more specific remedy.
fn load_config(path: &Path) -> Result<CodeoxConfig, RelayError> {
    CodeoxConfig::load(path).map_err(|error| {
        let message = error.to_string();
        if ui::fix_hint(&message).is_some() {
            return error;
        }
        format!(
            "{message}\n  file: {}\n  fix: ox doctor   (it lists every problem in this file)",
            path.display()
        )
        .into()
    })
}

/// True when at least one configured API provider has a usable key.
fn api_ready(codex_home: &Path) -> bool {
    let Ok(config) = CodeoxConfig::load(&CodeoxConfig::path(codex_home)) else {
        return false;
    };
    let Ok(saved) = provider::SavedKeys::for_config(codex_home, &config) else {
        return false;
    };
    config
        .vendors
        .iter()
        .any(|vendor| vendor.api_key(&|name| saved.get(name)).is_some())
}

/// Guided setup for a launch with nothing configured. Returns `None` when an
/// API provider is ready and the launch should continue, or the exit code.
async fn first_run(client_kind: ClientKind, codex_home: &Path) -> Result<Option<u8>, RelayError> {
    use setup::SetupChoice;

    let binary = match client_kind {
        ClientKind::Codex => "codex",
        ClientKind::Claude => "claude",
    };
    let name = ui::client_name(binary);
    let installed = setup::find_on_path(binary, std::env::var_os("PATH").as_deref()).is_some();
    println!("Welcome to ox. Nothing is set up yet, so let's do that first.");
    println!("Choose 1, 2 or 3. ox starts {name} as soon as one of them is ready.");
    if !installed {
        println!(
            "Note: {name} is not installed yet. Install it with: {}",
            ui::install_command(binary)
        );
    }
    loop {
        let result = match setup::choose_setup()? {
            SetupChoice::Subscription(account) => {
                let client = if account == "chatgpt" {
                    ClientKind::Codex
                } else {
                    ClientKind::Claude
                };
                match native::setup_subscription(account, codex_home).await {
                    Ok(0) => {
                        if let Some(code) = native::launch_if_signed_in(client, codex_home).await? {
                            return Ok(Some(code));
                        }
                        Ok(())
                    }
                    Ok(_) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            SetupChoice::Provider => match setup::guided_provider(codex_home) {
                Ok(()) if api_ready(codex_home) && installed => {
                    println!("\nSetup is complete. Starting {name}.");
                    return Ok(None);
                }
                Ok(()) if api_ready(codex_home) => {
                    println!(
                        "\nSetup is complete. Install {name} with: {}\nThen run: ox {binary}",
                        ui::install_command(binary)
                    );
                    return Ok(Some(0));
                }
                other => other,
            },
            SetupChoice::Brave => setup::enable_brave(codex_home),
            SetupChoice::Done => {
                println!(
                    "Run `ox doctor` to check setup, then start `ox codex` or `ox claude` from your project."
                );
                return Ok(Some(0));
            }
        };
        if let Err(error) = result {
            eprintln!(
                "{}",
                ui::render_error("ox setup", &error.to_string(), ui::stderr_color())
            );
        }
    }
}

async fn run_setup(args: &[OsString], codex_home: &Path) -> Result<u8, RelayError> {
    use setup::SetupChoice;

    if !args.is_empty() || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return setup::run_setup(args, codex_home);
    }
    loop {
        let result = match setup::choose_setup()? {
            SetupChoice::Subscription(account) => native::setup_subscription(account, codex_home)
                .await
                .map(|_| ()),
            SetupChoice::Provider => setup::guided_provider(codex_home),
            SetupChoice::Brave => setup::enable_brave(codex_home),
            SetupChoice::Done => {
                println!(
                    "Run `ox doctor` to check setup, then start `ox codex` or `ox claude` from your project."
                );
                return Ok(0);
            }
        };
        if let Err(error) = result {
            eprintln!(
                "{}",
                ui::render_error("ox setup", &error.to_string(), ui::stderr_color())
            );
        }
    }
}

fn client_launch_error(client: &str, error: std::io::Error) -> RelayError {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "could not find `{client}` on PATH\n  why: ox starts the stock {} CLI and does not bundle it\n  fix: {}",
            ui::client_name(client),
            ui::install_command(client)
        )
        .into()
    } else {
        error.into()
    }
}

impl App {
    fn legacy(
        provider: provider::Provider,
        bundled: BundledModels,
        token: &str,
        client: Client,
    ) -> Result<Self, RelayError> {
        // Legacy keeps only the rendered header. Recover its token so an
        // upstream that echoes the credential in an error is still scrubbed.
        let secrets = provider
            .upstream_auth
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(|key| vec![key.to_owned()])
            .unwrap_or_default();
        Ok(Self {
            mode: Mode::Legacy { provider, bundled },
            client_kind: ClientKind::Codex,
            persistent: false,
            client,
            local_auth: provider::bearer(token)?,
            max_request_bytes: MAX_REQUEST_BYTES,
            secrets: Arc::new(secrets),
            search: None,
        })
    }

    async fn codeox(
        config: CodeoxConfig,
        bundled: BundledModels,
        token: &str,
        client: Client,
        max_request_bytes: usize,
        env: &Env<'_>,
        search: Option<BraveSearch>,
    ) -> Result<Self, RelayError> {
        Self::codeox_with(
            config,
            std::future::ready(Ok(bundled)),
            token,
            client,
            max_request_bytes,
            env,
            search,
        )
        .await
    }

    /// Like [`App::codeox`], but the bundled Codex catalog is still loading.
    /// It is awaited concurrently with provider model discovery so a launch
    /// waits for the slower of the two instead of their sum.
    async fn codeox_with(
        config: CodeoxConfig,
        bundled: impl Future<Output = Result<BundledModels, RelayError>>,
        token: &str,
        client: Client,
        max_request_bytes: usize,
        env: &Env<'_>,
        search: Option<BraveSearch>,
    ) -> Result<Self, RelayError> {
        let vendors = config.vendors;
        let mut secrets = Vec::new();
        let auths: Vec<_> = vendors
            .iter()
            .map(|vendor| {
                let auth = vendor.api_key(env);
                if let Some(key) = &auth {
                    secrets.push(key.expose().to_owned());
                } else {
                    eprintln!(
                        "ox: provider {:?}: {}. Run `ox doctor` to check setup.",
                        vendor.key,
                        vendor.unavailable_reason()
                    );
                }
                auth
            })
            .collect();

        let mut discovered: Vec<_> = std::iter::repeat_with(|| None)
            .take(vendors.len())
            .collect();
        let discovery = async {
            let client = &client;
            let requests: Vec<_> = vendors
                .iter()
                .zip(&auths)
                .enumerate()
                .filter_map(|(index, (vendor, auth))| {
                    let key = auth.as_ref().filter(|_| vendor.discovery)?;
                    Some(async move { (index, discover(client, vendor, key).await) })
                })
                .collect();
            let mut requests = futures_util::stream::iter(requests).buffer_unordered(4);

            let deadline = tokio::time::Instant::now() + CATALOG_TIMEOUT;
            loop {
                match tokio::time::timeout_at(deadline, requests.next()).await {
                    Ok(Some((index, Ok(models)))) => discovered[index] = Some(models),
                    Ok(Some((index, Err(reason)))) => eprintln!(
                        "ox: provider {:?}: model discovery failed: {reason}",
                        vendors[index].key
                    ),
                    Ok(None) => break,
                    Err(_) => {
                        eprintln!(
                            "ox: model discovery reached the startup deadline. Using configured models for remaining providers."
                        );
                        break;
                    }
                }
            }
            Ok::<(), RelayError>(())
        };
        let (bundled, ()) = tokio::try_join!(bundled, discovery)?;

        let models_by_vendor: Vec<_> = vendors
            .iter()
            .zip(discovered)
            .map(|(vendor, models)| {
                let specs = catalog::merge_models(models, &vendor.models);
                if specs.is_empty() {
                    eprintln!(
                        "ox: provider {:?}: no models are listed; requests for {:?} still route by prefix",
                        vendor.key, vendor.key
                    );
                }
                specs
            })
            .collect();

        let routes = Routes::build(&vendors, &models_by_vendor)?;
        let mut entries = Vec::new();
        for (index, specs) in models_by_vendor.iter().enumerate() {
            let vendor = &vendors[index];
            for spec in specs {
                let alias = match &spec.alias {
                    Some(alias) => alias.clone(),
                    None => provider::alias_for(&vendor.key, &spec.id, None)?,
                };
                entries.push(CatalogModel {
                    slug: alias,
                    display_name: spec.name.clone().unwrap_or_else(|| spec.id.clone()),
                    description: vendor.name.clone(),
                    context_window: spec
                        .context_window
                        .or_else(|| catalog::published_context_window(&vendor.base_url, &spec.id)),
                    input_modalities: spec.input_modalities.clone(),
                    reasoning_efforts: spec.reasoning_efforts.clone(),
                    apply_patch: vendor.wire.is_translated(),
                });
            }
        }
        let catalog = catalog::catalog_json(&entries, &bundled);
        let states = vendors
            .into_iter()
            .zip(auths)
            .map(|(vendor, auth)| VendorState { vendor, auth })
            .collect();
        Ok(Self {
            mode: Mode::Codeox {
                vendors: states,
                routes,
                catalog,
            },
            client_kind: ClientKind::Codex,
            persistent: false,
            client,
            local_auth: provider::bearer(token)?,
            max_request_bytes,
            secrets: Arc::new(secrets),
            search,
        })
    }

    fn visible_models(&self) -> impl Iterator<Item = &Value> {
        let entries = match &self.mode {
            Mode::Codeox { catalog, .. } => catalog["models"].as_array().map(Vec::as_slice),
            Mode::Legacy { .. } => None,
        };
        entries
            .unwrap_or_default()
            .iter()
            .filter(|entry| entry["visibility"] == "list")
    }

    /// The context window OX's catalog knows for each launchable alias.
    fn model_contexts(&self) -> Vec<(String, u64)> {
        let launchable: std::collections::BTreeSet<&str> = self
            .launch_models()
            .into_iter()
            .map(|(alias, _)| alias)
            .collect();
        self.visible_models()
            .filter_map(|entry| {
                let alias = entry["slug"].as_str()?;
                let window = entry["context_window"].as_u64()?;
                launchable
                    .contains(alias)
                    .then(|| (alias.to_owned(), window))
            })
            .collect()
    }

    fn launch_models(&self) -> Vec<(&str, &str)> {
        let Mode::Codeox {
            vendors, routes, ..
        } = &self.mode
        else {
            return Vec::new();
        };
        self.visible_models()
            .filter_map(|entry| {
                let alias = entry["slug"].as_str()?;
                let Resolution::Route(route) = routes.resolve(alias) else {
                    return None;
                };
                let vendor = &vendors[route.vendor];
                vendor.auth.as_ref()?;
                Some((alias, vendor.vendor.key.as_str()))
            })
            .collect()
    }

    /// Checks an explicit provider-qualified `--model` before the client
    /// starts. A prefix that matches no configured provider is an error with
    /// the closest provider. An ID missing from a provider's known list only
    /// earns a note, because prefix routing still reaches the provider.
    /// Opens the provider connection for the launch model while the client
    /// is still starting, so the first prompt does not wait for DNS, TCP and
    /// TLS. Best effort: failures are ignored and the first request connects
    /// as usual.
    fn prewarm(&self, model: &str) {
        let Some(url) = self.prewarm_target(model) else {
            return;
        };
        let client = self.client.clone();
        tokio::spawn(async move {
            // No credentials: only the connection matters, whatever the reply.
            let _ = client.head(url).timeout(CONNECT_TIMEOUT).send().await;
        });
    }

    /// The URL to connect to ahead of the first request, or `None` when a
    /// warm-up would not help: the provider has no key, is reached over plain
    /// HTTP (local servers connect instantly), or model discovery has already
    /// opened its connection.
    fn prewarm_target(&self, model: &str) -> Option<reqwest::Url> {
        let Mode::Codeox {
            vendors, routes, ..
        } = &self.mode
        else {
            return None;
        };
        let (_, state, _) = resolve_model(vendors, routes, model).ok()?;
        let vendor = &state.vendor;
        (vendor.base_url.scheme() == "https" && !vendor.discovery).then(|| vendor.base_url.clone())
    }

    fn check_launch_model(&self, requested: &str) -> Result<(), RelayError> {
        let Mode::Codeox {
            vendors, routes, ..
        } = &self.mode
        else {
            return Ok(());
        };
        let Some((prefix, rest)) = requested.split_once('/') else {
            return Ok(());
        };
        let keys: Vec<&str> = vendors
            .iter()
            .map(|state| state.vendor.key.as_str())
            .collect();
        if matches!(routes.resolve(requested), Resolution::Unknown) {
            if keys.contains(&prefix) {
                return Ok(());
            }
            let fix = match ui::closest(prefix, keys.iter().copied()) {
                Some(key) => format!("use --model {key}/{}", short(rest)),
                None => "ox doctor   (it lists your providers and model aliases)".to_owned(),
            };
            return Err(format!(
                "unknown provider {:?} in --model {:?}\n  why: codeox.toml has: {}\n  fix: {fix}",
                short(prefix),
                short(requested),
                keys.join(", ")
            )
            .into());
        }
        let listed: Vec<&str> = self
            .visible_models()
            .filter_map(|entry| entry["slug"].as_str())
            .filter(|slug| slug.split_once('/').is_some_and(|(key, _)| key == prefix))
            .collect();
        if !listed.is_empty()
            && !listed.contains(&requested)
            && let Some(suggestion) = ui::closest(requested, listed.iter().copied())
        {
            eprintln!(
                "ox: {} is not in the {prefix} model list. Closest match: {suggestion}. Continuing as requested.",
                short(requested)
            );
        }
        Ok(())
    }

    async fn handle(&self, request: Request<Incoming>) -> Response<RelayBody> {
        let bearer_matches = request
            .headers()
            .get(AUTHORIZATION)
            .is_some_and(|auth| auth.as_bytes().ct_eq(self.local_auth.as_bytes()).into());
        if matches!(self.client_kind, ClientKind::Claude)
            || self.persistent
                && matches!(
                    request.uri().path(),
                    "/v1/messages" | "/v1/messages/count_tokens" | "/v1/models" | "/api/hello"
                )
        {
            return self.handle_messages_client(request, bearer_matches).await;
        }
        if !bearer_matches {
            return error_response(StatusCode::UNAUTHORIZED, "Relay authentication failed");
        }

        match (request.method(), request.uri().path()) {
            (&Method::GET, "/v1/models") => self.models(request.into_parts().0.headers).await,
            (&Method::GET, "/v1/catalog") if self.persistent => {
                self.models(request.into_parts().0.headers).await
            }
            (&Method::POST, "/v1/responses" | "/v1/responses/compact") => {
                self.inference(request).await
            }
            (&Method::POST, standalone_search::ENDPOINT_PATH) => self.web_search(request).await,
            _ => error_response(StatusCode::NOT_FOUND, "Unknown relay endpoint"),
        }
    }

    /// Codex 0.171.0's standalone `web.run` tool posts here. The same loopback
    /// bearer token authenticates it, and only live Brave queries are served.
    async fn web_search(&self, request: Request<Incoming>) -> Response<RelayBody> {
        let Some(search) = &self.search else {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Live web search is not configured for this launch",
            );
        };
        let bytes =
            match read_bounded(request.into_body(), standalone_search::MAX_REQUEST_BYTES).await {
                Ok(bytes) => bytes,
                Err(response) => return response,
            };
        let reply = standalone_search::SearchEndpoint::new(search)
            .handle(&bytes)
            .await;
        if let Some(notice) = search.take_fallback_notice() {
            eprintln!("ox: {notice}");
        }
        let status =
            StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        bytes_response(status, Bytes::from(reply.body), "application/json")
    }

    async fn models(&self, headers: HeaderMap) -> Response<RelayBody> {
        match &self.mode {
            Mode::Codeox { catalog, .. } => json_response(StatusCode::OK, catalog.clone()),
            Mode::Legacy { provider, bundled } => {
                let upstream = self
                    .client
                    .get(provider.models_url.clone())
                    .headers(forward_headers(headers))
                    .header(AUTHORIZATION, provider.upstream_auth.clone())
                    .timeout(CATALOG_TIMEOUT)
                    .send()
                    .await;
                match upstream {
                    Ok(response) if response.status().is_success() => {
                        let catalog = read_upstream(response, MAX_CATALOG_BYTES)
                            .await
                            .ok()
                            .and_then(|bytes| serde_json::from_slice::<ProviderModels>(&bytes).ok())
                            .and_then(|models| models.legacy_catalog(&provider.name, bundled));
                        match catalog {
                            Some(catalog) => json_response(StatusCode::OK, catalog),
                            None => error_response(
                                StatusCode::BAD_GATEWAY,
                                "Invalid upstream model catalog",
                            ),
                        }
                    }
                    Ok(response) => stream_response(response),
                    Err(_) => {
                        error_response(StatusCode::BAD_GATEWAY, "Model catalog request failed")
                    }
                }
            }
        }
    }

    async fn inference<B>(&self, request: Request<B>) -> Response<RelayBody>
    where
        B: Body<Data = Bytes> + Send + Unpin + 'static,
        B::Error: Into<RelayError>,
    {
        let (parts, body) = request.into_parts();
        let path = parts
            .uri
            .path()
            .strip_prefix("/v1")
            .unwrap_or("")
            .to_owned();
        let query = if self.persistent {
            None
        } else {
            parts.uri.query().map(str::to_owned)
        };
        match &self.mode {
            Mode::Legacy { provider, .. } => {
                let upstream = self
                    .client
                    .post(provider.responses_url(&path, query.as_deref()))
                    .headers(forward_headers(parts.headers))
                    .header(AUTHORIZATION, provider.upstream_auth.clone())
                    .body(reqwest::Body::wrap_stream(body.into_data_stream()))
                    .send()
                    .await;
                match upstream {
                    Ok(response) => responses_response(response, &self.secrets).await,
                    Err(_) => error_response(StatusCode::BAD_GATEWAY, "Inference request failed"),
                }
            }
            Mode::Codeox {
                vendors, routes, ..
            } => {
                let bytes = match read_bounded(body, self.max_request_bytes).await {
                    Ok(bytes) => bytes,
                    Err(response) => return response,
                };
                let mut payload: Value = match serde_json::from_slice(&bytes) {
                    Ok(payload) => payload,
                    Err(_) => {
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            "Request body must be JSON",
                        );
                    }
                };
                drop(bytes);
                let Some(requested) = payload.get("model").and_then(Value::as_str) else {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "Request body is missing a model",
                    );
                };
                let requested = requested.to_owned();
                let (route, state, auth) = match resolve_model(vendors, routes, &requested) {
                    Ok(target) => target,
                    Err((status, message)) => return error_response(status, &message),
                };
                if translate::sanitize_replayed_history(&mut payload, route.wire) {
                    eprintln!(
                        "ox: sending this thread's history to provider {:?}",
                        state.vendor.key
                    );
                }

                if route.wire.is_translated() {
                    if path != "/responses" {
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            "translated providers support only POST /v1/responses; remote compaction is not available",
                        );
                    }
                    return upstream::translated(
                        &self.client,
                        upstream::Translation {
                            vendor: &state.vendor,
                            key: auth,
                            incoming: Some(&parts.headers),
                            upstream_model: &route.upstream_model,
                            requested_model: &requested,
                            secrets: Arc::clone(&self.secrets),
                        },
                        payload,
                    )
                    .await;
                }
                payload["model"] = json!(route.upstream_model);
                if path == "/responses" {
                    state.vendor.add_cache_hints(&mut payload);
                }
                let body = match serde_json::to_vec(&payload) {
                    Ok(body) => body,
                    Err(_) => {
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            "Request body could not be encoded",
                        );
                    }
                };
                drop(payload);
                let vendor_headers =
                    match state.vendor.inference_headers(auth, Some(&parts.headers)) {
                        Ok(headers) => headers,
                        Err(_) => {
                            return error_response(
                                StatusCode::BAD_GATEWAY,
                                "could not build the upstream authentication header",
                            );
                        }
                    };
                let mut headers = if self.persistent {
                    // A shared listener can sit behind a proxy with its own
                    // identity headers. Only the relay's credentials and
                    // request headers belong on the provider request.
                    HeaderMap::new()
                } else {
                    forward_headers(parts.headers)
                };
                // The body was re-encoded, so the client's framing headers no
                // longer describe it; hyper derives them from the new body.
                headers.remove(CONTENT_LENGTH);
                headers.remove("expect");
                if !headers.contains_key(CONTENT_TYPE) {
                    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                }
                headers.extend(vendor_headers);
                let upstream = self
                    .client
                    .post(state.vendor.responses_url(&path, query.as_deref()))
                    .headers(headers)
                    .body(body)
                    .send()
                    .await;
                match upstream {
                    Ok(response) => responses_response(response, &self.secrets).await,
                    Err(_) => error_response(StatusCode::BAD_GATEWAY, "Inference request failed"),
                }
            }
        }
    }
}

/// The one pooled client for every provider request.
///
/// A provider connection costs a DNS lookup, a TCP handshake and a TLS
/// handshake, so the client is tuned to pay that once per session:
/// - HTTP/2 where the provider offers it, so parallel requests (workers,
///   classifier and title requests) share one connection;
/// - idle connections are kept for five minutes, and TCP keep-alive stops
///   NAT gateways from dropping them while the user reads or types.
///
/// `OX_HTTP1=1` forces HTTP/1.1 for a provider or proxy with a broken HTTP/2
/// implementation.
fn upstream_client() -> Result<Client, reqwest::Error> {
    let builder = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(Duration::from_secs(300))
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE);
    if http1_only(std::env::var_os("OX_HTTP1").as_deref()) {
        builder.http1_only().build()
    } else {
        builder.build()
    }
}

fn http1_only(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}

fn resolve_model<'a>(
    vendors: &'a [VendorState],
    routes: &Routes,
    requested: &str,
) -> Result<(provider::Route, &'a VendorState, &'a SecretKey), (StatusCode, String)> {
    let route = match routes.resolve(requested) {
        Resolution::Route(route) => route,
        Resolution::Ambiguous(aliases) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "model {:?} is provided by multiple providers; use a provider-qualified alias such as {}",
                    short(requested),
                    aliases.join(", ")
                ),
            ));
        }
        Resolution::Unknown => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "unknown model {:?}; run /model to pick an entry from the catalog",
                    short(requested)
                ),
            ));
        }
    };
    let state = &vendors[route.vendor];
    let auth = state.auth.as_ref().ok_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            format!(
                "provider {:?} is unavailable; {}",
                state.vendor.key,
                state.vendor.unavailable_reason()
            ),
        )
    })?;
    Ok((route, state, auth))
}

async fn discover(
    client: &Client,
    vendor: &provider::Vendor,
    key: &SecretKey,
) -> Result<ProviderModels, String> {
    let headers = vendor
        .discovery_headers(key)
        .map_err(|_| "could not build discovery headers".to_owned())?;
    let response = client
        .get(vendor.models_url.clone())
        .headers(headers)
        .timeout(CATALOG_TIMEOUT)
        .send()
        .await
        .map_err(|_| "request failed".to_owned())?;
    let status = response.status();
    let bytes = read_upstream(response, MAX_CATALOG_BYTES).await?;
    if !status.is_success() {
        return Err(format!("upstream returned HTTP {}", status.as_u16()));
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid model catalog".to_owned())
}

async fn read_upstream(response: reqwest::Response, limit: usize) -> Result<Bytes, String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err("model catalog is too large".to_owned());
    }
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "model catalog request failed".to_owned())?;
        if buffer.len() + chunk.len() > limit {
            return Err("model catalog is too large".to_owned());
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buffer))
}

async fn read_bounded(
    body: impl Body<Data = Bytes> + Unpin,
    limit: usize,
) -> Result<Bytes, Response<RelayBody>> {
    if body
        .size_hint()
        .upper()
        .is_some_and(|upper| upper > limit as u64)
    {
        return Err(too_large(limit));
    }
    let mut collected = Vec::new();
    let mut body = body;
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(_) => {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "Could not read the request body",
                ));
            }
        };
        if let Ok(data) = frame.into_data() {
            if collected.len() + data.len() > limit {
                return Err(too_large(limit));
            }
            collected.extend_from_slice(&data);
        }
    }
    Ok(Bytes::from(collected))
}

fn too_large(limit: usize) -> Response<RelayBody> {
    error_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        &format!("Request body exceeds the {limit} byte relay limit"),
    )
}

async fn serve(listener: TcpListener, relay: Arc<App>) -> std::io::Result<()> {
    serve_requests(listener, move |request| {
        let relay = Arc::clone(&relay);
        async move { relay.handle(request).await }
    })
    .await
}

/// Accept one client connection with Nagle's algorithm turned off.
///
/// Streamed tokens are many small writes. With Nagle on, the kernel holds a
/// small write back until the client acknowledges the previous one, and a
/// client that is busy rendering delays that acknowledgement by up to 40 ms.
/// Failing to set the option is not fatal: the connection still works.
async fn accept_connection(listener: &TcpListener) -> std::io::Result<tokio::net::TcpStream> {
    let (socket, _) = listener.accept().await?;
    let _ = socket.set_nodelay(true);
    Ok(socket)
}

async fn serve_requests<F, Fut>(listener: TcpListener, handle: F) -> std::io::Result<()>
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Response<RelayBody>> + Send + 'static,
{
    loop {
        let socket = accept_connection(&listener).await?;
        let handle = handle.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let response = handle(request);
                async move { Ok::<_, std::convert::Infallible>(response.await) }
            });
            let _ = http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .serve_connection(TokioIo::new(socket), service)
                .await;
        });
    }
}

/// How this launch treats Codex's web tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchLaunch {
    /// No usable search: do not advertise anything, and keep translated vendors
    /// on their existing hosted-search-disabled default.
    Disabled,
    /// Usable live search: register the standalone provider flag and feature.
    Enabled {
        /// True when the user set no `web_search` policy. Opting into live Brave
        /// search then also opts Codex into its live policy.
        set_live: bool,
    },
    /// The user pinned `disabled`, `cached` or `indexed`; leave it untouched.
    PolicyPinned,
}

fn codeox_overrides(base_url: &str, translated: bool, search: SearchLaunch) -> Vec<String> {
    let prefix = format!("model_providers.{PROVIDER_ID}");
    let mut overrides = vec![
        format!("model_provider={}", json!(PROVIDER_ID)),
        format!("{prefix}.name={}", json!("Codeox")),
        format!("{prefix}.base_url={}", json!(base_url)),
        format!("{prefix}.wire_api={}", json!("responses")),
        format!(
            "{prefix}.model_catalog_url={}",
            json!(format!("{base_url}/models"))
        ),
        format!("{prefix}.env_key={}", json!(TOKEN_ENV)),
    ];
    match search {
        SearchLaunch::Enabled { set_live } => {
            // Codex 0.171.0 exposes the `web.run` namespace only when the
            // provider declares standalone support and the feature is on.
            overrides.push(format!(
                "{prefix}.supports_standalone_web_search={}",
                json!(true)
            ));
            overrides.push(format!("features.standalone_web_search={}", json!(true)));
            if set_live {
                overrides.push(format!("web_search={}", json!("live")));
            }
        }
        SearchLaunch::Disabled if translated => {
            // Hosted web search runs provider-side and cannot be honored by a
            // translated vendor; ordinary client tools and approvals are
            // untouched. With search configured above, the relay itself runs
            // the standalone tool instead.
            overrides.push(format!("web_search={}", json!("disabled")));
        }
        SearchLaunch::Disabled | SearchLaunch::PolicyPinned => {}
    }
    overrides
}

/// One pooled Brave client plus the launch decision for the web tool.
fn resolve_search(
    settings: Option<&SearchSettings>,
    env: &Env<'_>,
    args: &[OsString],
    codex_home: &Path,
) -> (Option<BraveSearch>, SearchLaunch) {
    let Some(settings) = settings else {
        return (None, SearchLaunch::Disabled);
    };
    let policy = explicit_web_search_policy(args, codex_home);
    if let Some(policy) = &policy
        && policy != "live"
    {
        eprintln!(
            "ox: search is configured, but Codex's web_search policy is pinned to {policy:?}; standalone web search stays off"
        );
        return (None, SearchLaunch::PolicyPinned);
    }
    let set_live = policy.is_none();
    match build_search(settings, env) {
        Some(search) => (Some(search), SearchLaunch::Enabled { set_live }),
        None => (None, SearchLaunch::Disabled),
    }
}

fn build_search(settings: &SearchSettings, env: &Env<'_>) -> Option<BraveSearch> {
    let Some(key) = env(&settings.env_key) else {
        eprintln!(
            "ox: [search] is configured but {} is not set; standalone web search is disabled",
            settings.env_key
        );
        return None;
    };
    let config = SearchConfig {
        max_results: settings.max_results,
        timeout: Duration::from_secs(settings.timeout_seconds),
        ..SearchConfig::default()
    };
    match BraveSearch::new(&key, config) {
        Ok(search) => Some(search),
        Err(error) => {
            eprintln!(
                "ox: [search] is configured but unusable ({error}); standalone web search is disabled"
            );
            None
        }
    }
}

/// The `web_search` value Codex will actually use, in precedence order:
/// command-line `-c` overrides, the selected profile, then top-level config.
/// `None` means the user never pinned a policy.
fn explicit_web_search_policy(args: &[OsString], codex_home: &Path) -> Option<String> {
    let mut cli_policy = None;
    let mut cli_profile = None;
    let mut index = 0;
    while index < args.len() {
        let Some(arg) = args[index].to_str() else {
            index += 1;
            continue;
        };
        if arg == "-c" || arg == "--config" {
            if let Some(value) = args.get(index + 1).and_then(|value| value.to_str()) {
                if let Some(policy) = config_assignment(value) {
                    cli_policy = Some(policy);
                }
                index += 1;
            }
        } else if let Some(value) = arg
            .strip_prefix("--config=")
            .or_else(|| arg.strip_prefix("-c").filter(|value| !value.is_empty()))
        {
            if let Some(policy) = config_assignment(value) {
                cli_policy = Some(policy);
            }
        } else if arg == "-p" || arg == "--profile" {
            if let Some(value) = args.get(index + 1).and_then(|value| value.to_str()) {
                cli_profile = Some(value.to_owned());
                index += 1;
            }
        } else if let Some(value) = arg
            .strip_prefix("--profile=")
            .or_else(|| arg.strip_prefix("-p").filter(|value| !value.is_empty()))
        {
            cli_profile = Some(value.to_owned());
        }
        index += 1;
    }

    cli_policy.or_else(|| {
        let raw = std::fs::read_to_string(codex_home.join("config.toml")).ok()?;
        let document = toml_edit::Document::parse(raw.as_str()).ok()?;
        provider::configured_web_search_policy(&document, cli_profile.as_deref())
    })
}

fn config_assignment(raw: &str) -> Option<String> {
    let (key, value) = raw.split_once('=')?;
    if key.trim() != "web_search" {
        return None;
    }
    Some(provider::normalise_web_search_policy(value))
}

fn legacy_overrides(provider_id: &str, base_url: &str) -> Vec<String> {
    let prefix = format!("model_providers.{provider_id}");
    vec![
        format!("{prefix}.base_url={}", json!(base_url)),
        format!(
            "{prefix}.model_catalog_url={}",
            json!(format!("{base_url}/models"))
        ),
        format!("{prefix}.env_key={}", json!(TOKEN_ENV)),
    ]
}

/// `model_catalog_json` in `config.toml` makes Codex ignore discovery, so
/// surface it instead of silently showing a stale model list.
fn warn_about_static_catalog(codex_home: &Path) {
    let Ok(raw) = std::fs::read_to_string(codex_home.join("config.toml")) else {
        return;
    };
    if let Ok(document) = toml_edit::Document::parse(&raw)
        && document.get("model_catalog_json").is_some()
    {
        eprintln!("ox: config.toml sets model_catalog_json, which may hide the ox model catalog");
    }
}

async fn bundled_catalog(
    codex_home: &Path,
    strip: &BTreeSet<String>,
) -> Result<BundledModels, RelayError> {
    let mut command = Command::new("codex");
    command
        .env("CODEX_HOME", codex_home)
        .env_remove("CODEX_SQLITE_HOME")
        .args(["debug", "models", "--bundled"]);
    for name in strip {
        command.env_remove(name);
    }
    let output = command
        .output()
        .await
        .map_err(|error| client_launch_error("codex", error))?;
    if !output.status.success() {
        return Err("could not read the bundled Codex model catalog".into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn codex_relay_command(home: &Path) -> Result<Command, RelayError> {
    let mut command = Command::new("codex");
    let state = home.to_str().ok_or("Codex profile path must be UTF-8")?;
    let logs = home.join("log");
    let logs = logs.to_str().ok_or("Codex log path must be UTF-8")?;
    // A copied config or an inherited env var can otherwise point Codex
    // back to the native databases and logs despite a separate CODEX_HOME.
    command
        .env("CODEX_HOME", home)
        .env_remove("CODEX_SQLITE_HOME")
        .arg("-c")
        .arg(format!("sqlite_home={}", json!(state)))
        .arg("-c")
        .arg(format!("log_dir={}", json!(logs)));
    Ok(command)
}

fn random_token() -> Result<String, RelayError> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    Ok(hex::encode(bytes))
}

fn forward_headers(mut headers: HeaderMap) -> HeaderMap {
    let connection_headers = headers
        .get_all("connection")
        .into_iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect::<Vec<_>>();
    for name in connection_headers {
        headers.remove(name);
    }
    for name in [
        "accept-encoding",
        "authorization",
        "connection",
        "host",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "x-opencode-session",
    ] {
        headers.remove(name);
    }
    headers
}

/// Stream successful native Responses requests and sanitize failures.
async fn responses_response(
    response: reqwest::Response,
    secrets: &[String],
) -> Response<RelayBody> {
    if response.status().is_redirection() {
        return error_response(
            StatusCode::BAD_GATEWAY,
            "provider returned an unexpected redirect",
        );
    }
    if upstream::is_http_failure(&response) {
        return upstream::http_failure(response, secrets).await;
    }
    stream_response(response)
}

fn stream_response(response: reqwest::Response) -> Response<RelayBody> {
    let status = response.status();
    let headers = forward_headers(response.headers().clone());
    let stream = response
        .bytes_stream()
        .map(|chunk| chunk.map(Frame::data).map_err(RelayError::from));
    let mut downstream = Response::new(StreamBody::new(stream).boxed_unsync());
    *downstream.status_mut() = status;
    *downstream.headers_mut() = headers;
    downstream
}

fn error_response(status: StatusCode, message: &str) -> Response<RelayBody> {
    json_response(status, json!({ "error": { "message": message } }))
}

fn json_response(status: StatusCode, value: Value) -> Response<RelayBody> {
    bytes_response(status, Bytes::from(value.to_string()), "application/json")
}

fn bytes_response(
    status: StatusCode,
    body: Bytes,
    content_type: &'static str,
) -> Response<RelayBody> {
    let mut response = Response::new(
        Full::new(body)
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

fn short(value: &str) -> String {
    if value.chars().count() <= MAX_ECHOED_MODEL_CHARS {
        return value.to_owned();
    }
    let mut truncated = value
        .chars()
        .take(MAX_ECHOED_MODEL_CHARS)
        .collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::LOCATION;
    use std::convert::Infallible;
    use std::sync::Mutex;
    use tokio::task::JoinHandle;

    const RELAY_TOKEN: &str = "relay-test-token";

    struct MockVendor {
        base: String,
        hits: Arc<Mutex<Vec<Hit>>>,
        task: JoinHandle<()>,
    }

    #[derive(Clone, Debug)]
    struct Hit {
        path: String,
        authorization: Option<String>,
        body: Option<Value>,
    }

    impl MockVendor {
        fn responses_hits(&self) -> Vec<Hit> {
            self.hits
                .lock()
                .unwrap()
                .iter()
                .filter(|hit| hit.path.starts_with("/v1/responses"))
                .cloned()
                .collect()
        }

        fn hits_len(&self) -> usize {
            self.hits.lock().unwrap().len()
        }
    }

    impl Drop for MockVendor {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    struct Behavior {
        models: Result<Value, u16>,
        catalog_barrier: Option<tokio::sync::Barrier>,
        redirect: Option<String>,
        /// A canned native Responses HTTP failure, with headers to prove that
        /// arbitrary vendor headers are not relayed.
        responses_error: Option<(u16, Value, Vec<(&'static str, &'static str)>)>,
    }

    fn models_body(ids: &[&str]) -> Value {
        json!({ "data": ids.iter().map(|id| json!({ "id": id, "name": id })).collect::<Vec<_>>() })
    }

    async fn spawn_mock(models: Result<Value, u16>, redirect: Option<String>) -> MockVendor {
        spawn_mock_behavior(Behavior {
            models,
            catalog_barrier: None,
            redirect,
            responses_error: None,
        })
        .await
    }

    async fn spawn_responses_error(
        status: u16,
        body: Value,
        headers: &[(&'static str, &'static str)],
    ) -> MockVendor {
        spawn_mock_behavior(Behavior {
            models: Ok(Value::Null),
            catalog_barrier: None,
            redirect: None,
            responses_error: Some((status, body, headers.to_vec())),
        })
        .await
    }

    async fn spawn_mock_behavior(behavior: Behavior) -> MockVendor {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let behavior = Arc::new(behavior);
        let shared_hits = Arc::clone(&hits);
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let hits = Arc::clone(&shared_hits);
                let behavior = Arc::clone(&behavior);
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let hits = Arc::clone(&hits);
                        let behavior = Arc::clone(&behavior);
                        async move {
                            Ok::<_, Infallible>(mock_response(&behavior, &hits, request).await)
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        MockVendor {
            base: format!("http://{addr}/v1"),
            hits,
            task,
        }
    }

    async fn mock_response(
        behavior: &Behavior,
        hits: &Mutex<Vec<Hit>>,
        request: Request<Incoming>,
    ) -> Response<RelayBody> {
        let (parts, body) = request.into_parts();
        let bytes = body
            .collect()
            .await
            .map(|collected| collected.to_bytes())
            .unwrap_or_default();
        let path = parts.uri.path().to_owned();
        let authorization = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        hits.lock().unwrap().push(Hit {
            path: path.clone(),
            authorization: authorization.clone(),
            body: serde_json::from_slice(&bytes).ok(),
        });
        if let Some(target) = &behavior.redirect {
            let mut response = json_response(StatusCode::FOUND, json!({}));
            response
                .headers_mut()
                .insert(LOCATION, HeaderValue::from_str(target).unwrap());
            return response;
        }
        if path == "/v1/models" {
            if let Some(barrier) = &behavior.catalog_barrier {
                barrier.wait().await;
            }
            return match &behavior.models {
                Ok(body) => json_response(StatusCode::OK, body.clone()),
                Err(status) => error_response(
                    StatusCode::from_u16(*status).unwrap(),
                    "upstream model catalog failed",
                ),
            };
        }
        if path.starts_with("/v1/responses") {
            if let Some((status, body, headers)) = &behavior.responses_error {
                let mut response =
                    json_response(StatusCode::from_u16(*status).unwrap(), body.clone());
                for (name, value) in headers {
                    response.headers_mut().insert(
                        HeaderName::from_bytes(name.as_bytes()).unwrap(),
                        HeaderValue::from_str(value).unwrap(),
                    );
                }
                return response;
            }
            let model = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|payload| payload.get("model").cloned())
                .unwrap_or(Value::Null);
            return json_response(
                StatusCode::OK,
                json!({ "model": model, "path": path, "authorization": authorization }),
            );
        }
        error_response(StatusCode::NOT_FOUND, "unexpected mock path")
    }

    pub(crate) struct RelayHandle {
        pub(crate) base: String,
        task: JoinHandle<()>,
    }

    impl Drop for RelayHandle {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[test]
    fn mistyped_commands_are_caught_but_codex_words_pass_through() {
        assert_eq!(mistyped_command("docter"), Some("doctor"));
        assert_eq!(mistyped_command("setpu"), Some("setup"));
        assert_eq!(mistyped_command("claud"), Some("claude"));
        assert_eq!(mistyped_command("updat"), Some("update"));
        for passthrough in [
            "doctor",
            "codex",
            "claude",
            "exec",
            "resume",
            "review",
            "cloud",
            "apply",
            "mcp",
            "debug",
            "fork",
            "plugin",
            "marketplace",
            "app",
            "execpolicy",
            "--model",
            "-c",
            "fix the failing test",
            "",
        ] {
            assert_eq!(mistyped_command(passthrough), None, "{passthrough}");
        }
    }

    #[tokio::test]
    async fn prewarm_targets_only_keyed_https_providers_without_discovery() {
        let config = r#"
[providers.static]
base_url = "https://static.example/v1"
env_key = "STATIC_KEY"
discovery = false

[[providers.static.models]]
id = "fast-coder"

[providers.local]
base_url = "http://127.0.0.1:9/v1"
env_key = "LOCAL_KEY"
discovery = false

[[providers.local.models]]
id = "fast-coder"

[providers.nokey]
base_url = "https://nokey.example/v1"
env_key = "MISSING_KEY"
discovery = false

[[providers.nokey.models]]
id = "fast-coder"
"#;
        let environment = |name: &str| match name {
            "STATIC_KEY" | "LOCAL_KEY" => Some("secret".to_owned()),
            _ => None,
        };
        let app = App::codeox(
            CodeoxConfig::load_from_str(config).unwrap(),
            bundled_fixture(),
            RELAY_TOKEN,
            test_client(),
            1 << 20,
            &environment,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            app.prewarm_target("static/fast-coder").unwrap().as_str(),
            "https://static.example/v1"
        );
        // Plain HTTP connects instantly, a provider without a key cannot be
        // used, and an unknown alias has no provider.
        assert!(app.prewarm_target("local/fast-coder").is_none());
        assert!(app.prewarm_target("nokey/fast-coder").is_none());
        assert!(app.prewarm_target("unknown/fast-coder").is_none());
    }

    #[test]
    fn ox_http1_forces_http1_only_when_set_to_a_true_value() {
        use std::ffi::OsStr;
        assert!(!http1_only(None));
        assert!(!http1_only(Some(OsStr::new(""))));
        assert!(!http1_only(Some(OsStr::new("0"))));
        assert!(http1_only(Some(OsStr::new("1"))));
        assert!(upstream_client().is_ok());
    }

    #[tokio::test]
    async fn launch_model_check_names_the_closest_provider() {
        let config = r#"
[providers.openrouter]
base_url = "https://openrouter.example/v1"
env_key = "OPENROUTER_KEY"
discovery = false

[[providers.openrouter.models]]
id = "fast-coder"
"#;
        let app = App::codeox(
            CodeoxConfig::load_from_str(config).unwrap(),
            bundled_fixture(),
            RELAY_TOKEN,
            test_client(),
            MAX_REQUEST_BYTES,
            &|_| None,
            None,
        )
        .await
        .unwrap();
        assert!(app.check_launch_model("openrouter/fast-coder").is_ok());
        assert!(app.check_launch_model("openrouter/not-listed").is_ok());
        assert!(app.check_launch_model("fast-coder").is_ok());
        assert!(app.check_launch_model("gpt-unknown").is_ok());
        let error = app
            .check_launch_model("openroutr/fast-coder")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown provider \"openroutr\""), "{error}");
        assert!(
            error.contains("fix: use --model openrouter/fast-coder"),
            "{error}"
        );
        let error = app
            .check_launch_model("zzzzzz/fast-coder")
            .unwrap_err()
            .to_string();
        assert!(error.contains("fix: ox doctor"), "{error}");
    }

    fn bundled_fixture() -> BundledModels {
        serde_json::from_value(json!({
            "models": [{"slug": "bundled-default", "visibility": "list"}]
        }))
        .unwrap()
    }

    pub(crate) fn test_client() -> Client {
        Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    pub(crate) async fn start_relay(
        config: &str,
        env: &[(&str, &str)],
        max_request_bytes: usize,
    ) -> RelayHandle {
        start_relay_with_search(config, env, max_request_bytes, None).await
    }

    /// Test-only entry point for injecting a loopback Brave client. Production
    /// code builds the one pooled client from `[search]` and its environment
    /// reference; `search.rs` itself only accepts a non-Brave origin in tests.
    pub(crate) async fn start_relay_with_search(
        config: &str,
        env: &[(&str, &str)],
        max_request_bytes: usize,
        search: Option<BraveSearch>,
    ) -> RelayHandle {
        start_relay_with_client(config, env, max_request_bytes, search, ClientKind::Codex).await
    }

    async fn start_relay_with_client(
        config: &str,
        env: &[(&str, &str)],
        max_request_bytes: usize,
        search: Option<BraveSearch>,
        client_kind: ClientKind,
    ) -> RelayHandle {
        let environment = |name: &str| {
            env.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        };
        let mut app = App::codeox(
            CodeoxConfig::load_from_str(config).unwrap(),
            bundled_fixture(),
            RELAY_TOKEN,
            test_client(),
            max_request_bytes,
            &environment,
            search,
        )
        .await
        .unwrap();
        app.client_kind = client_kind;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = serve(listener, Arc::new(app)).await;
        });
        RelayHandle {
            base: format!("http://{addr}/v1"),
            task,
        }
    }

    pub(crate) fn brave_at(base_url: &str) -> BraveSearch {
        BraveSearch::new(
            "test-brave-key",
            SearchConfig {
                base_url: base_url.to_owned(),
                ..SearchConfig::default()
            },
        )
        .unwrap()
    }

    fn auth_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {RELAY_TOKEN}")).unwrap(),
        );
        headers
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum BraveMode {
        /// LLM Context succeeds.
        LlmContext,
        /// LLM Context is not in the plan; the client falls back to web search.
        Fallback,
    }

    #[derive(Clone, Debug)]
    pub(crate) struct BraveHit {
        pub path: String,
        pub query: Vec<(String, String)>,
        pub token: Option<String>,
    }

    pub(crate) struct MockBrave {
        pub base: String,
        pub hits: Arc<Mutex<Vec<BraveHit>>>,
        task: JoinHandle<()>,
    }

    impl Drop for MockBrave {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn spawn_mock_brave(mode: BraveMode) -> MockBrave {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&hits);
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let hits = Arc::clone(&shared);
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let hits = Arc::clone(&hits);
                        async move {
                            let (parts, _) = request.into_parts();
                            let path = parts.uri.path().to_owned();
                            let query =
                                reqwest::Url::parse(&format!("http://brave.test{}", parts.uri))
                                    .map(|url| {
                                        url.query_pairs()
                                            .map(|(key, value)| {
                                                (key.into_owned(), value.into_owned())
                                            })
                                            .collect::<Vec<_>>()
                                    })
                                    .unwrap_or_default();
                            let token = parts
                                .headers
                                .get("x-subscription-token")
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_owned);
                            hits.lock().unwrap().push(BraveHit {
                                path: path.clone(),
                                query,
                                token,
                            });
                            let body =
                                if path == "/res/v1/llm/context" && mode == BraveMode::Fallback {
                                    return Ok::<_, Infallible>(json_response(
                                        StatusCode::BAD_REQUEST,
                                        json!({"error": {"code": "OPTION_NOT_IN_PLAN"}}),
                                    ));
                                } else {
                                    let fixture = if path == "/res/v1/llm/context" {
                                        include_str!("../tests/fixtures/brave/llm_context.json")
                                    } else {
                                        include_str!("../tests/fixtures/brave/web_search.json")
                                    };
                                    serde_json::from_str::<Value>(fixture).unwrap()
                                };
                            Ok::<_, Infallible>(json_response(StatusCode::OK, body))
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        MockBrave {
            base: format!("http://{addr}"),
            hits,
            task,
        }
    }

    pub(crate) fn brave_hits(mock: &MockBrave) -> Vec<BraveHit> {
        mock.hits.lock().unwrap().clone()
    }

    pub(crate) async fn start_brave(mode: BraveMode) -> MockBrave {
        spawn_mock_brave(mode).await
    }

    pub(crate) async fn get_models(relay: &RelayHandle) -> Value {
        let response = test_client()
            .get(format!("{}/models", relay.base))
            .headers(auth_headers())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }

    pub(crate) async fn post(relay: &RelayHandle, body: Value) -> reqwest::Response {
        test_client()
            .post(format!("{}/responses", relay.base))
            .headers(auth_headers())
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    fn listed_slugs(catalog: &Value) -> Vec<String> {
        catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|model| model["visibility"] == "list")
            .filter_map(|model| model["slug"].as_str().map(str::to_owned))
            .collect()
    }

    fn two_vendor_config(alpha: &str, beta: &str) -> String {
        format!(
            r#"
[providers.alpha]
name = "Alpha"
base_url = "{alpha}"
env_key = "ALPHA_KEY"

[providers.beta]
name = "Beta"
base_url = "{beta}"
env_key = "BETA_KEY"
"#
        )
    }

    #[tokio::test]
    async fn accepted_relay_connections_disable_nagle() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let socket = accept_connection(&listener).await.unwrap();
        assert!(socket.nodelay().unwrap());
    }

    fn one_vendor_config(base: &str) -> String {
        format!(
            r#"
[providers.alpha]
name = "Alpha"
base_url = "{base}"
env_key = "ALPHA_KEY"
"#
        )
    }

    #[tokio::test]
    async fn the_bundled_catalog_loads_while_providers_are_discovered() {
        let mock = spawn_mock(Ok(models_body(&["discovered"])), None).await;
        let environment = |name: &str| (name == "ALPHA_KEY").then(|| "alpha-secret".to_owned());
        let hits = Arc::clone(&mock.hits);
        // Resolves only after the discovery request reached the vendor, so a
        // launch that waited for the bundled catalog first would never finish.
        let bundled = async move {
            while hits.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Ok(bundled_fixture())
        };
        let app = tokio::time::timeout(
            Duration::from_secs(5),
            App::codeox_with(
                CodeoxConfig::load_from_str(&one_vendor_config(&mock.base)).unwrap(),
                bundled,
                RELAY_TOKEN,
                test_client(),
                1 << 20,
                &environment,
                None,
            ),
        )
        .await
        .expect("the bundled catalog and model discovery must run concurrently")
        .unwrap();
        let slugs: Vec<_> = app
            .visible_models()
            .filter_map(|entry| entry["slug"].as_str())
            .collect();
        assert!(slugs.contains(&"alpha/discovered"), "{slugs:?}");
    }

    #[tokio::test]
    async fn a_bundled_catalog_failure_does_not_wait_for_discovery() {
        // The barrier needs two waiters and only one request arrives, so
        // discovery never answers on its own.
        let mock = spawn_mock_behavior(Behavior {
            models: Ok(models_body(&["discovered"])),
            catalog_barrier: Some(tokio::sync::Barrier::new(2)),
            redirect: None,
            responses_error: None,
        })
        .await;
        let environment = |name: &str| (name == "ALPHA_KEY").then(|| "alpha-secret".to_owned());
        let bundled = async { Err::<BundledModels, RelayError>("bundled catalog failed".into()) };
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            App::codeox_with(
                CodeoxConfig::load_from_str(&one_vendor_config(&mock.base)).unwrap(),
                bundled,
                RELAY_TOKEN,
                test_client(),
                1 << 20,
                &environment,
                None,
            ),
        )
        .await
        .expect("a failed bundled catalog must end the launch at once");
        assert_eq!(
            result.err().map(|error| error.to_string()).as_deref(),
            Some("bundled catalog failed")
        );
    }

    #[tokio::test]
    async fn discovery_runs_for_multiple_vendors_concurrently() {
        let mock = spawn_mock_behavior(Behavior {
            models: Ok(models_body(&["discovered"])),
            catalog_barrier: Some(tokio::sync::Barrier::new(2)),
            redirect: None,
            responses_error: None,
        })
        .await;
        let config = two_vendor_config(&mock.base, &mock.base);
        let relay = tokio::time::timeout(
            Duration::from_secs(5),
            start_relay(
                &config,
                &[("ALPHA_KEY", "alpha-secret"), ("BETA_KEY", "beta-secret")],
                1 << 20,
            ),
        )
        .await
        .expect("both discovery requests must reach the vendor before either can finish");
        let slugs = listed_slugs(&get_models(&relay).await);
        assert!(slugs.contains(&"alpha/discovered".to_owned()), "{slugs:?}");
        assert!(slugs.contains(&"beta/discovered".to_owned()), "{slugs:?}");
    }

    #[tokio::test]
    async fn routes_aliases_per_vendor_without_collision() {
        let alpha = spawn_mock(Ok(models_body(&["shared-model"])), None).await;
        let beta = spawn_mock(Ok(models_body(&["shared-model"])), None).await;
        let relay = start_relay(
            &two_vendor_config(&alpha.base, &beta.base),
            &[("ALPHA_KEY", "alpha-secret"), ("BETA_KEY", "beta-secret")],
            1 << 20,
        )
        .await;

        let catalog = get_models(&relay).await;
        let slugs = listed_slugs(&catalog);
        assert!(
            slugs.contains(&"alpha/shared-model".to_owned()),
            "{slugs:?}"
        );
        assert!(slugs.contains(&"beta/shared-model".to_owned()), "{slugs:?}");
        assert!(!slugs.contains(&"shared-model".to_owned()), "{slugs:?}");
        assert_eq!(slugs.len(), 2);
        let responses_entry = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == "alpha/shared-model")
            .unwrap();
        assert!(
            responses_entry.get("apply_patch_tool_type").is_none(),
            "Responses vendors keep their previous tool surface"
        );
        let hidden = catalog["models"]
            .as_array()
            .unwrap()
            .iter().find(|model| model["slug"] == "bundled-default")
            .expect("bundled models are shadowed");
        assert_eq!(hidden["visibility"], "hide");
        assert_eq!(hidden["supported_in_api"], false);

        let response = post(&relay, json!({"model": "beta/shared-model", "input": "hi"})).await;
        assert_eq!(response.status(), 200);
        let echo: Value = response.json().await.unwrap();
        assert_eq!(echo["model"], "shared-model");
        assert_eq!(echo["path"], "/v1/responses");
        assert_eq!(echo["authorization"], "Bearer beta-secret");

        let beta_hits = beta.responses_hits();
        assert_eq!(beta_hits.len(), 1);
        assert_eq!(
            beta_hits[0].authorization.as_deref(),
            Some("Bearer beta-secret")
        );
        assert_eq!(
            beta_hits[0].body.as_ref().unwrap()["model"],
            "shared-model",
            "the upstream request carries the upstream id, not the alias"
        );
        assert!(
            alpha.responses_hits().is_empty(),
            "the other vendor must not see the request"
        );
        assert!(
            beta_hits
                .iter()
                .all(|hit| hit.authorization.as_deref() != Some("Bearer alpha-secret")),
            "vendor credentials never cross vendors"
        );
    }

    #[tokio::test]
    async fn vendor_without_credentials_is_isolated_and_reported() {
        let alpha = spawn_mock(Ok(models_body(&["shared-model"])), None).await;
        let beta = spawn_mock(Ok(models_body(&["never-used"])), None).await;
        let config = format!(
            r#"
[providers.alpha]
base_url = "{}"
env_key = "ALPHA_KEY"

[providers.beta]
base_url = "{}"
env_key = "BETA_KEY"

[[providers.beta.models]]
id = "beta-static"
name = "Beta Static"
context_window = 64000
"#,
            alpha.base, beta.base
        );
        let relay = start_relay(&config, &[("ALPHA_KEY", "alpha-secret")], 1 << 20).await;

        let catalog = get_models(&relay).await;
        let slugs = listed_slugs(&catalog);
        assert!(
            slugs.contains(&"alpha/shared-model".to_owned()),
            "{slugs:?}"
        );
        assert!(
            slugs.contains(&"beta/beta-static".to_owned()),
            "static models keep aliases available when credentials are missing: {slugs:?}"
        );
        let entry = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == "beta/beta-static")
            .unwrap();
        assert_eq!(entry["context_window"], 64000);

        let response = post(&relay, json!({"model": "beta/beta-static"})).await;
        assert_eq!(response.status(), 502);
        let error: Value = response.json().await.unwrap();
        let message = error["error"]["message"].as_str().unwrap();
        assert!(message.contains("unavailable"), "{message}");
        assert!(message.contains("BETA_KEY"), "{message}");
        assert!(!message.contains("beta-secret"), "{message}");
        assert!(beta.responses_hits().is_empty());
        assert_eq!(
            beta.hits_len(),
            0,
            "discovery is skipped without credentials"
        );

        let response = post(&relay, json!({"model": "alpha/shared-model"})).await;
        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn discovery_failure_does_not_hide_other_vendors() {
        let alpha = spawn_mock(Err(503), None).await;
        let beta = spawn_mock(Ok(models_body(&["beta-model"])), None).await;
        let config = format!(
            r#"
[providers.alpha]
base_url = "{}"
env_key = "ALPHA_KEY"

[[providers.alpha.models]]
id = "alpha-large"
name = "Alpha Large"
context_window = 128000

[providers.beta]
base_url = "{}"
env_key = "BETA_KEY"
"#,
            alpha.base, beta.base
        );
        let relay = start_relay(
            &config,
            &[("ALPHA_KEY", "alpha-secret"), ("BETA_KEY", "beta-secret")],
            1 << 20,
        )
        .await;

        let catalog = get_models(&relay).await;
        let slugs = listed_slugs(&catalog);
        assert!(slugs.contains(&"alpha/alpha-large".to_owned()), "{slugs:?}");
        assert!(slugs.contains(&"beta/beta-model".to_owned()), "{slugs:?}");
        let entry = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == "alpha/alpha-large")
            .unwrap();
        assert_eq!(entry["context_window"], 128000);

        let response = post(&relay, json!({"model": "alpha/alpha-large"})).await;
        assert_eq!(response.status(), 200);
        let echo: Value = response.json().await.unwrap();
        assert_eq!(echo["model"], "alpha-large");
        assert_eq!(echo["authorization"], "Bearer alpha-secret");
    }

    #[tokio::test]
    async fn prefixed_alias_routes_models_missing_from_discovery() {
        let alpha = spawn_mock(Ok(models_body(&["known-model"])), None).await;
        let beta = spawn_mock(Ok(Value::Null), None).await;
        let relay = start_relay(
            &two_vendor_config(&alpha.base, &beta.base),
            &[("ALPHA_KEY", "alpha-secret"), ("BETA_KEY", "beta-secret")],
            1 << 20,
        )
        .await;

        let response = post(&relay, json!({"model": "alpha/from-a-resumed-session"})).await;
        assert_eq!(response.status(), 200);
        let echo: Value = response.json().await.unwrap();
        assert_eq!(echo["model"], "from-a-resumed-session");
        assert_eq!(echo["authorization"], "Bearer alpha-secret");
    }

    #[tokio::test]
    async fn unknown_and_ambiguous_models_are_reported_safely() {
        let alpha = spawn_mock(Ok(models_body(&["shared-model"])), None).await;
        let beta = spawn_mock(Ok(models_body(&["shared-model"])), None).await;
        let relay = start_relay(
            &two_vendor_config(&alpha.base, &beta.base),
            &[("ALPHA_KEY", "alpha-secret"), ("BETA_KEY", "beta-secret")],
            1 << 20,
        )
        .await;

        let response = post(&relay, json!({"model": "shared-model"})).await;
        assert_eq!(response.status(), 400);
        let error: Value = response.json().await.unwrap();
        let message = error["error"]["message"].as_str().unwrap();
        assert!(message.contains("multiple providers"), "{message}");
        assert!(message.contains("alpha/shared-model"), "{message}");
        assert!(message.contains("beta/shared-model"), "{message}");

        let response = post(&relay, json!({"model": "nobody/nothing"})).await;
        assert_eq!(response.status(), 400);
        let error: Value = response.json().await.unwrap();
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown model")
        );

        let response = post(&relay, json!({"input": "no model"})).await;
        assert_eq!(response.status(), 400);
        assert!(alpha.responses_hits().is_empty() && beta.responses_hits().is_empty());
    }

    #[tokio::test]
    async fn relay_requires_its_loopback_token() {
        let alpha = spawn_mock(Ok(models_body(&["model"])), None).await;
        let relay = start_relay(
            &two_vendor_config(&alpha.base, &alpha.base),
            &[("ALPHA_KEY", "alpha-secret"), ("BETA_KEY", "beta-secret")],
            1 << 20,
        )
        .await;

        let response = test_client()
            .get(format!("{}/models", relay.base))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);

        let response = test_client()
            .get(format!("{}/models", relay.base))
            .header(AUTHORIZATION, "Bearer wrong-token")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);

        let response = test_client()
            .post(format!("{}/responses", relay.base))
            .json(&json!({"model": "alpha/model"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        assert!(alpha.responses_hits().is_empty());
    }

    #[tokio::test]
    async fn claude_launch_authenticates_and_serves_its_model_catalog() {
        let alpha = spawn_mock(Ok(models_body(&["fixture-model"])), None).await;
        let relay = start_relay_with_client(
            &two_vendor_config(&alpha.base, &alpha.base),
            &[("ALPHA_KEY", "alpha-secret"), ("BETA_KEY", "beta-secret")],
            1 << 20,
            None,
            ClientKind::Claude,
        )
        .await;
        let client = test_client();
        let url = format!("{}/models?limit=1000", relay.base);
        let denied = client
            .get(&url)
            .header("x-api-key", "wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        let catalog: Value = client
            .get(&url)
            .header("x-api-key", RELAY_TOKEN)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let models = catalog["data"].as_array().unwrap();
        assert!(
            models
                .iter()
                .any(|model| model["id"] == "alpha/fixture-model")
        );
        assert!(models.iter().all(|model| model["type"] == "model"));
        assert!(!models.iter().any(|model| model["id"] == "bundled-default"));
        assert_eq!(catalog["has_more"], false);

        let wrong_wire = client
            .post(format!("{}/responses", relay.base))
            .bearer_auth(RELAY_TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_wire.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oversized_requests_are_rejected_before_routing() {
        let alpha = spawn_mock(Ok(models_body(&["model"])), None).await;
        let relay = start_relay(
            &two_vendor_config(&alpha.base, &alpha.base),
            &[("ALPHA_KEY", "alpha-secret"), ("BETA_KEY", "beta-secret")],
            64,
        )
        .await;

        let body = json!({"model": "alpha/model", "input": "x".repeat(200)});
        let response = test_client()
            .post(format!("{}/responses", relay.base))
            .headers(auth_headers())
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 413);
        assert!(alpha.responses_hits().is_empty());
    }

    #[tokio::test]
    async fn non_json_bodies_are_rejected() {
        let alpha = spawn_mock(Ok(models_body(&["model"])), None).await;
        let relay = start_relay(
            &two_vendor_config(&alpha.base, &alpha.base),
            &[("ALPHA_KEY", "alpha-secret"), ("BETA_KEY", "beta-secret")],
            1 << 20,
        )
        .await;

        let response = test_client()
            .post(format!("{}/responses", relay.base))
            .headers(auth_headers())
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert!(alpha.responses_hits().is_empty());
    }

    #[tokio::test]
    async fn upstream_redirects_are_not_followed() {
        let trap = spawn_mock(Ok(models_body(&["trap"])), None).await;
        let alpha = spawn_mock(Ok(Value::Null), Some(trap.base.clone())).await;
        let relay = start_relay(
            &two_vendor_config(&alpha.base, &alpha.base),
            &[("ALPHA_KEY", "alpha-secret"), ("BETA_KEY", "beta-secret")],
            1 << 20,
        )
        .await;

        let response = post(&relay, json!({"model": "alpha/anything"})).await;
        assert_eq!(response.status(), 502);
        assert!(response.headers().get(LOCATION).is_none());
        assert_eq!(trap.hits_len(), 0, "redirects are never followed");
    }

    #[tokio::test]
    async fn responses_http_failures_do_not_relay_vendor_bodies_or_headers() {
        let key = "alpha_fixture_secret";
        let alpha = spawn_responses_error(
            401,
            json!({"error": {
                "message": format!("invalid key {key}"),
                "code": format!("invalid_{key}"),
                "type": "authentication_error"
            }}),
            &[
                ("x-api-key", key),
                ("authorization", key),
                ("x-vendor-trace", key),
            ],
        )
        .await;
        let beta = spawn_mock(Ok(models_body(&["beta-model"])), None).await;
        let relay = start_relay(
            &two_vendor_config(&alpha.base, &beta.base),
            &[("ALPHA_KEY", key), ("BETA_KEY", "beta-secret")],
            1 << 20,
        )
        .await;

        let response = post(&relay, json!({"model": "alpha/alpha-model"})).await;
        assert_eq!(response.status(), 401, "the upstream status is preserved");
        assert!(response.headers().get("x-api-key").is_none());
        assert!(response.headers().get("x-vendor-trace").is_none());
        assert!(response.headers().get(AUTHORIZATION).is_none());
        let text = response.text().await.unwrap();
        assert!(!text.contains(key), "{text}");
        let error: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(error["error"]["type"], "authentication_error");
        assert!(error["error"].get("code").is_none());
        let message = error["error"]["message"].as_str().unwrap();
        assert!(message.contains("401"), "{message}");
        assert!(message.contains("[redacted]"), "{message}");

        // The neighbouring vendor is unaffected: its credential still works
        // and a successful native Responses payload still streams through.
        let response = post(&relay, json!({"model": "beta/beta-model"})).await;
        assert_eq!(response.status(), 200);
        let echo: Value = response.json().await.unwrap();
        assert_eq!(echo["model"], "beta-model");
        assert_eq!(echo["authorization"], "Bearer beta-secret");
    }

    #[tokio::test]
    async fn oversized_responses_error_bodies_are_not_read_as_messages() {
        let huge = json!({"error": {"message": format!("{} alpha-secret", "x".repeat(70_000))}});
        let alpha = spawn_responses_error(500, huge, &[]).await;
        let relay = start_relay(
            &two_vendor_config(&alpha.base, &alpha.base),
            &[("ALPHA_KEY", "alpha-secret"), ("BETA_KEY", "beta-secret")],
            1 << 20,
        )
        .await;

        let response = post(&relay, json!({"model": "alpha/anything"})).await;
        assert_eq!(response.status(), 500);
        let text = response.text().await.unwrap();
        assert_eq!(
            text, r#"{"error":{"message":"provider returned HTTP 500"}}"#,
            "an oversized error body contributes no text at all"
        );
    }

    #[tokio::test]
    async fn responses_rate_limits_keep_status_and_retry_after() {
        let alpha = spawn_responses_error(
            429,
            json!({"error": {"message": "slow down alpha-secret", "code": "insufficient_quota"}}),
            &[("retry-after", "7"), ("x-ratelimit-key", "alpha-secret")],
        )
        .await;
        let relay = start_relay(
            &two_vendor_config(&alpha.base, &alpha.base),
            &[("ALPHA_KEY", "alpha-secret"), ("BETA_KEY", "beta-secret")],
            1 << 20,
        )
        .await;

        let response = post(&relay, json!({"model": "alpha/anything"})).await;
        assert_eq!(response.status(), 429);
        assert_eq!(response.headers().get("retry-after").unwrap(), "7");
        assert!(response.headers().get("x-ratelimit-key").is_none());
        let text = response.text().await.unwrap();
        assert!(!text.contains("alpha-secret"), "{text}");
        let error: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(error["error"]["code"], "insufficient_quota");
    }

    #[tokio::test]
    async fn legacy_mode_keeps_unprefixed_upstream_models() {
        let demo = spawn_mock(Ok(models_body(&["demo-coding"])), None).await;
        let home = TempHome::new();
        std::fs::write(
            home.0.join("config.toml"),
            format!(
                r#"
model = "demo-coding"
model_provider = "demo"

[model_providers.demo]
name = "Demo Provider"
base_url = "{}"
env_key = "DEMO_API_KEY"
wire_api = "responses"
"#,
                demo.base
            ),
        )
        .unwrap();
        let env = |name: &str| (name == "DEMO_API_KEY").then(|| "demo-secret".to_owned());
        let provider = provider::Provider::load(&home.0, &env).unwrap();
        let app = App::legacy(provider, bundled_fixture(), RELAY_TOKEN, test_client()).unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let _ = serve(listener, Arc::new(app)).await;
        });
        let relay = RelayHandle {
            base: format!("http://{addr}/v1"),
            task: tokio::spawn(async {}),
        };

        let catalog = get_models(&relay).await;
        assert_eq!(listed_slugs(&catalog), ["demo-coding"]);

        let response = post(&relay, json!({"model": "demo-coding"})).await;
        assert_eq!(response.status(), 200);
        let echo: Value = response.json().await.unwrap();
        assert_eq!(echo["model"], "demo-coding");
        assert_eq!(echo["authorization"], "Bearer demo-secret");
    }

    #[tokio::test]
    async fn legacy_mode_reduces_upstream_http_failures() {
        let demo = spawn_responses_error(
            401,
            json!({"error": {"message": "invalid key demo-secret"}}),
            &[("x-api-key", "demo-secret")],
        )
        .await;
        let home = TempHome::new();
        std::fs::write(
            home.0.join("config.toml"),
            format!(
                r#"
model = "demo-coding"
model_provider = "demo"

[model_providers.demo]
name = "Demo Provider"
base_url = "{}"
env_key = "DEMO_API_KEY"
wire_api = "responses"
"#,
                demo.base
            ),
        )
        .unwrap();
        let env = |name: &str| (name == "DEMO_API_KEY").then(|| "demo-secret".to_owned());
        let provider = provider::Provider::load(&home.0, &env).unwrap();
        let app = App::legacy(provider, bundled_fixture(), RELAY_TOKEN, test_client()).unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = serve(listener, Arc::new(app)).await;
        });
        let relay = RelayHandle {
            base: format!("http://{addr}/v1"),
            task: server,
        };

        let response = post(&relay, json!({"model": "demo-coding"})).await;
        assert_eq!(response.status(), 401);
        assert!(response.headers().get("x-api-key").is_none());
        let text = response.text().await.unwrap();
        assert!(!text.contains("demo-secret"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
    }

    pub(crate) struct TempHome(pub(crate) PathBuf);

    impl TempHome {
        pub(crate) fn new() -> Self {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).unwrap();
            let path = std::env::temp_dir().join(format!("codeox-main-{}", hex::encode(bytes)));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn short_truncates_echoed_model_names() {
        assert_eq!(short("alpha/model"), "alpha/model");
        let long = "a".repeat(500);
        let truncated = short(&long);
        assert!(truncated.chars().count() <= MAX_ECHOED_MODEL_CHARS + 1);
    }

    #[test]
    fn child_overrides_use_the_isolated_provider_id() {
        let overrides = codeox_overrides("http://127.0.0.1:1234/v1", false, SearchLaunch::Disabled);
        assert!(
            !overrides
                .iter()
                .any(|entry| entry.starts_with("web_search="))
        );
        let translated = codeox_overrides("http://127.0.0.1:1234/v1", true, SearchLaunch::Disabled);
        assert!(translated.contains(&"web_search=\"disabled\"".to_owned()));
        assert!(overrides.contains(&"model_provider=\"codeox\"".to_owned()));
        assert!(
            overrides.contains(
                &"model_providers.codeox.base_url=\"http://127.0.0.1:1234/v1\"".to_owned()
            )
        );
        assert!(overrides.contains(&"model_providers.codeox.env_key=\"CODEOX_TOKEN\"".to_owned()));
        assert!(overrides.contains(&"model_providers.codeox.wire_api=\"responses\"".to_owned()));
        let legacy = legacy_overrides("demo", "http://127.0.0.1:1234/v1");
        assert!(
            legacy
                .iter()
                .all(|entry| entry.starts_with("model_providers.demo."))
        );
        assert!(
            legacy
                .iter()
                .all(|entry| !entry.starts_with("model_provider="))
        );
    }

    #[test]
    fn search_overrides_are_added_only_for_an_opted_in_launch() {
        let enabled = codeox_overrides(
            "http://127.0.0.1:1234/v1",
            false,
            SearchLaunch::Enabled { set_live: true },
        );
        assert!(
            enabled
                .contains(&"model_providers.codeox.supports_standalone_web_search=true".to_owned())
        );
        assert!(enabled.contains(&"features.standalone_web_search=true".to_owned()));
        assert!(enabled.contains(&"web_search=\"live\"".to_owned()));

        let explicit_live = codeox_overrides(
            "http://127.0.0.1:1234/v1",
            false,
            SearchLaunch::Enabled { set_live: false },
        );
        assert!(explicit_live.contains(&"features.standalone_web_search=true".to_owned()));
        assert!(
            !explicit_live
                .iter()
                .any(|entry| entry.starts_with("web_search=")),
            "an explicit live policy is left alone"
        );

        let pinned = codeox_overrides("http://127.0.0.1:1234/v1", true, SearchLaunch::PolicyPinned);
        assert!(
            !pinned
                .iter()
                .any(|entry| entry.contains("standalone_web_search")),
            "a pinned policy never gains the standalone flags"
        );
        assert!(
            !pinned.iter().any(|entry| entry.starts_with("web_search=")),
            "a pinned policy is never rewritten"
        );
    }

    #[test]
    fn search_env_reference_is_stripped_from_both_codex_calls() {
        let config = CodeoxConfig::load_from_str(
            r#"
[providers.alpha]
base_url = "https://api.example.com/v1"
env_key = "ALPHA_KEY"

[[providers.alpha.models]]
id = "alpha-model"

[search]
env_key = "MY_BRAVE_KEY"
"#,
        )
        .unwrap();
        assert_eq!(
            config.env_refs(),
            ["ALPHA_KEY".to_owned(), "MY_BRAVE_KEY".to_owned()].into()
        );
    }

    #[test]
    fn explicit_web_search_policy_follows_cli_then_profile_then_top_level() {
        let home = TempHome::new();
        std::fs::write(
            home.0.join("config.toml"),
            r#"
profile = "work"
web_search = "cached"

[profiles.work]
web_search = "indexed"
"#,
        )
        .unwrap();
        assert_eq!(
            explicit_web_search_policy(&[], &home.0).as_deref(),
            Some("indexed")
        );
        assert_eq!(
            explicit_web_search_policy(
                &[
                    OsString::from("exec"),
                    OsString::from("-p"),
                    OsString::from("other")
                ],
                &home.0
            )
            .as_deref(),
            Some("cached"),
            "an unconfigured profile falls back to the top-level value"
        );
        assert_eq!(
            explicit_web_search_policy(
                &[
                    OsString::from("-c"),
                    OsString::from("web_search=\"disabled\"")
                ],
                &home.0
            )
            .as_deref(),
            Some("disabled")
        );
        std::fs::write(home.0.join("config.toml"), "").unwrap();
        assert_eq!(explicit_web_search_policy(&[], &home.0), None);
    }

    fn search_config(vendor: &str) -> String {
        format!(
            r#"
[providers.alpha]
name = "Alpha"
base_url = "{vendor}"
env_key = "ALPHA_KEY"
discovery = false

[[providers.alpha.models]]
id = "upstream-model"

[search]
env_key = "BRAVE_SEARCH_API_KEY"
"#
        )
    }

    fn search_body(query: &str) -> Value {
        json!({
            "id": "search-1",
            "model": "alpha/upstream-model",
            "input": [{"type": "message", "role": "user",
                       "content": [{"type": "input_text", "text": "SECRET conversation tail"}]}],
            "commands": {"search_query": [{"q": query}]},
            "settings": {"allowed_callers": ["direct"], "external_web_access": true},
            "max_output_tokens": 10000
        })
    }

    async fn post_search(relay: &RelayHandle, body: &[u8], authorized: bool) -> reqwest::Response {
        let mut request = test_client()
            .post(format!("{}/alpha/search", relay.base))
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_vec());
        if authorized {
            request = request.headers(auth_headers());
        }
        request.send().await.unwrap()
    }

    #[tokio::test]
    async fn standalone_search_requires_auth_a_body_cap_and_an_opt_in() {
        let vendor = spawn_mock(Ok(models_body(&["upstream-model"])), None).await;
        let brave = start_brave(BraveMode::LlmContext).await;
        let relay = start_relay_with_search(
            &search_config(&vendor.base),
            &[("ALPHA_KEY", "alpha-secret")],
            1 << 20,
            Some(brave_at(&brave.base)),
        )
        .await;

        let body = serde_json::to_vec(&search_body("rust book")).unwrap();
        let response = post_search(&relay, &body, false).await;
        assert_eq!(response.status(), 401);
        assert!(brave_hits(&brave).is_empty());

        let oversized = vec![b' '; standalone_search::MAX_REQUEST_BYTES + 1];
        let response = post_search(&relay, &oversized, true).await;
        assert_eq!(response.status(), 413);
        assert!(brave_hits(&brave).is_empty());

        let mut blocked = search_body("rust book");
        blocked["settings"]["external_web_access"] = json!(false);
        let response = post_search(&relay, &serde_json::to_vec(&blocked).unwrap(), true).await;
        assert_eq!(response.status(), 403);
        assert!(
            brave_hits(&brave).is_empty(),
            "a disabled policy never reaches Brave"
        );

        let no_search =
            start_relay(&search_config(&vendor.base), &[("ALPHA_KEY", "x")], 1 << 20).await;
        let response = post_search(&no_search, &body, true).await;
        assert_eq!(response.status(), 503);
    }

    #[tokio::test]
    async fn standalone_search_returns_codex_results_and_forwards_only_the_query() {
        let vendor = spawn_mock(Ok(models_body(&["upstream-model"])), None).await;
        let brave = start_brave(BraveMode::LlmContext).await;
        let relay = start_relay_with_search(
            &search_config(&vendor.base),
            &[("ALPHA_KEY", "alpha-secret")],
            1 << 20,
            Some(brave_at(&brave.base)),
        )
        .await;

        let response = post_search(
            &relay,
            &serde_json::to_vec(&search_body("rust book")).unwrap(),
            true,
        )
        .await;
        assert_eq!(response.status(), 200);
        let reply: Value = response.json().await.unwrap();
        assert!(reply["encrypted_output"].is_null());
        assert_eq!(reply["results"][0]["type"], "text_result");
        assert!(
            reply["results"][0]["ref_id"]
                .as_str()
                .unwrap()
                .starts_with(standalone_search::REF_ID_PREFIX)
        );
        assert_eq!(
            reply["results"][0]["url"],
            "https://www.rust-lang.org/learn"
        );

        let hits = brave_hits(&brave);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/res/v1/llm/context");
        assert_eq!(hits[0].token.as_deref(), Some("test-brave-key"));
        let pairs = &hits[0].query;
        assert!(pairs.contains(&("q".to_owned(), "rust book".to_owned())));
        assert!(
            pairs
                .iter()
                .all(|(_, value)| !value.contains("SECRET conversation tail")),
            "Codex's conversation tail never leaves the relay: {pairs:?}"
        );
    }

    #[tokio::test]
    async fn standalone_search_serves_web_fallback_when_llm_context_is_not_in_plan() {
        let vendor = spawn_mock(Ok(models_body(&["upstream-model"])), None).await;
        let brave = start_brave(BraveMode::Fallback).await;
        let relay = start_relay_with_search(
            &search_config(&vendor.base),
            &[("ALPHA_KEY", "alpha-secret")],
            1 << 20,
            Some(brave_at(&brave.base)),
        )
        .await;

        let response = post_search(
            &relay,
            &serde_json::to_vec(&search_body("rust")).unwrap(),
            true,
        )
        .await;
        assert_eq!(response.status(), 200);
        let reply: Value = response.json().await.unwrap();
        assert!(reply["output"].as_str().unwrap().contains("Rust"));
        let paths = brave_hits(&brave)
            .into_iter()
            .map(|hit| hit.path)
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            ["/res/v1/llm/context", "/res/v1/web/search"],
            "the entitlement fallback stays inside the same pooled client"
        );
    }
}

#[cfg(test)]
mod translated_tests {
    use super::tests::{
        BraveMode, TempHome, brave_at, brave_hits, get_models, post, start_brave, start_relay,
        start_relay_with_search, test_client,
    };
    use super::tests_support::*;
    use super::*;
    use std::collections::BTreeMap;
    use std::convert::Infallible;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::task::JoinHandle;

    #[derive(Clone, Debug)]
    struct WireHit {
        path: String,
        headers: BTreeMap<String, String>,
        body: Option<Value>,
    }

    #[derive(Clone)]
    enum Reply {
        Json(StatusCode, Value),
        Sse(String),
    }

    struct WireMock {
        base: String,
        hits: Arc<Mutex<Vec<WireHit>>>,
        task: JoinHandle<()>,
    }

    impl WireMock {
        fn hits(&self) -> Vec<WireHit> {
            self.hits.lock().unwrap().clone()
        }

        fn last(&self) -> WireHit {
            self.hits().pop().expect("a wire hit")
        }
    }

    impl Drop for WireMock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    type ReplyFn = Arc<dyn Fn(usize, &str) -> Reply + Send + Sync>;

    async fn wire_mock(reply: ReplyFn) -> WireMock {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let shared_hits = Arc::clone(&hits);
        let requests = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let hits = Arc::clone(&shared_hits);
                let reply = Arc::clone(&reply);
                let requests = Arc::clone(&requests);
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let hits = Arc::clone(&hits);
                        let reply = Arc::clone(&reply);
                        let requests = Arc::clone(&requests);
                        async move {
                            let request_number = requests.fetch_add(1, Ordering::SeqCst) + 1;
                            Ok::<_, Infallible>(
                                wire_response(&reply, &hits, request_number, request).await,
                            )
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        WireMock {
            base: format!("http://{addr}/v1"),
            hits,
            task,
        }
    }

    async fn wire_response(
        reply: &ReplyFn,
        hits: &Mutex<Vec<WireHit>>,
        request_number: usize,
        request: Request<Incoming>,
    ) -> Response<RelayBody> {
        let (parts, body) = request.into_parts();
        let bytes = body
            .collect()
            .await
            .map(|collected| collected.to_bytes())
            .unwrap_or_default();
        let path = parts.uri.path().to_owned();
        let headers = parts
            .headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        hits.lock().unwrap().push(WireHit {
            path: path.clone(),
            headers,
            body: serde_json::from_slice(&bytes).ok(),
        });
        match reply(request_number, &path) {
            Reply::Json(status, value) => json_response(status, value),
            Reply::Sse(text) => {
                let mut response = Response::new(
                    Full::new(Bytes::from(text))
                        .map_err(|never| match never {})
                        .boxed_unsync(),
                );
                response
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
                response
            }
        }
    }

    fn anthropic_sse(events: &[Value]) -> String {
        let mut text = String::new();
        for event in events {
            let kind = event["type"].as_str().unwrap_or("message");
            text.push_str(&format!("event: {kind}\ndata: {event}\n\n"));
        }
        text
    }

    fn chat_sse(chunks: &[Value]) -> String {
        let mut text = String::new();
        for chunk in chunks {
            text.push_str(&format!("data: {chunk}\n\n"));
        }
        text.push_str("data: [DONE]\n\n");
        text
    }

    fn downstream_events(text: &str) -> Vec<Value> {
        text.split("\n\n")
            .filter_map(|frame| {
                let data = frame.lines().find_map(|line| line.strip_prefix("data: "))?;
                serde_json::from_str(data).ok()
            })
            .collect()
    }

    fn translated_config(vendor: &str, wire: &str, extra: &str) -> String {
        format!(
            r#"
[providers.alpha]
name = "Alpha"
base_url = "{vendor}"
wire_api = "{wire}"
env_key = "ALPHA_KEY"
discovery = false
{extra}

[[providers.alpha.models]]
id = "upstream-model"
name = "Upstream Model"
context_window = 200000
"#
        )
    }

    fn request_base(model: &str) -> Value {
        json!({
            "model": model,
            "instructions": "You are a coding agent.",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
            ],
            "stream": true,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "cache",
            "reasoning": {"effort": "medium", "summary": "auto"},
            "parallel_tool_calls": false,
            "tool_choice": "auto"
        })
    }

    fn codex_tools() -> Value {
        json!([
            {"type": "function", "name": "exec_command", "description": "Run a command", "strict": false,
             "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}},
            {"type": "custom", "name": "apply_patch", "description": "Patch files",
             "format": {"type": "grammar", "syntax": "lark", "definition": "start: x"}},
            {"type": "namespace", "name": "multi_agent_v1", "description": "Agents",
             "tools": [
                 {"type": "function", "name": "spawn_agent", "description": "Spawn", "strict": false,
                  "parameters": {"type": "object", "properties": {"task": {"type": "string"}}}}
             ]},
            {"type": "web_search", "external_web_access": false}
        ])
    }

    fn anthropic_text_stream() -> String {
        anthropic_sse(&[
            json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "content": [], "model": "upstream-model", "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hello"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
            json!({"type": "message_stop"}),
        ])
    }

    #[tokio::test]
    async fn anthropic_text_uses_messages_endpoint_version_and_header_auth() {
        let mock = wire_mock(Arc::new(|_: usize, _: &str| {
            Reply::Sse(anthropic_text_stream())
        }))
        .await;
        let config = translated_config(&mock.base, "anthropic_messages", "");
        let relay = start_relay(&config, &[("ALPHA_KEY", "alpha-secret-key")], 1 << 20).await;

        let catalog = get_models(&relay).await;
        let listed = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == "alpha/upstream-model")
            .expect("the translated model is listed");
        assert_eq!(listed["apply_patch_tool_type"], "freeform");

        let mut request = request_base("alpha/upstream-model");
        request["max_output_tokens"] = json!(64);
        let response = post(&relay, request).await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/event-stream"
        );
        assert!(response.headers().get(AUTHORIZATION).is_none());
        assert!(response.headers().get("x-api-key").is_none());
        let text = response.text().await.unwrap();
        let events = downstream_events(&text);
        assert_eq!(events.last().unwrap()["type"], "response.completed");
        assert!(text.contains("hello"));

        let hit = mock.last();
        assert_eq!(hit.path, "/v1/messages");
        assert_eq!(hit.headers.get("x-api-key").unwrap(), "alpha-secret-key");
        assert_eq!(hit.headers.get("anthropic-version").unwrap(), "2023-06-01");
        assert!(!hit.headers.contains_key("authorization"));
        let body = hit.body.unwrap();
        assert_eq!(body["model"], "upstream-model");
        assert_eq!(body["max_tokens"], 64);
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"][0]["text"], "You are a coding agent.");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("store").is_none());
        assert!(
            body.get("output_config").is_none(),
            "thinking is off by default"
        );
    }

    #[tokio::test]
    async fn anthropic_future_version_drops_sampling_parameters() {
        let mock = wire_mock(Arc::new(|_: usize, _: &str| {
            Reply::Sse(anthropic_text_stream())
        }))
        .await;
        let config = translated_config(
            &mock.base,
            "anthropic_messages",
            "anthropic_version = \"2026-10-21\"\n",
        );
        let relay = start_relay(&config, &[("ALPHA_KEY", "alpha-secret-key")], 1 << 20).await;
        let mut request = request_base("alpha/upstream-model");
        request["temperature"] = json!(0.2);
        request["top_p"] = json!(0.9);
        let response = post(&relay, request).await;
        assert_eq!(response.status(), 200);
        let _ = response.text().await.unwrap();

        let hit = mock.last();
        assert_eq!(hit.headers.get("anthropic-version").unwrap(), "2026-10-21");
        let body = hit.body.unwrap();
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert_eq!(body["max_tokens"], 8192, "the relay supplies a default");
    }

    #[tokio::test]
    async fn anthropic_tool_loop_round_trips_namespaces_and_freeform_tools() {
        let mock = wire_mock(Arc::new(|count: usize, _: &str| {
            if count == 1 {
                Reply::Sse(anthropic_sse(&[
                    json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "content": [], "model": "upstream-model", "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "apply_patch", "input": {}}}),
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"input\":\"*** Begin Patch\"}"}}),
                    json!({"type": "content_block_stop", "index": 0}),
                    json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_2", "name": "multi_agent_v1__spawn_agent", "input": {"task": "x"}}}),
                    json!({"type": "content_block_stop", "index": 1}),
                    json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}}),
                    json!({"type": "message_stop"}),
                ]))
            } else {
                Reply::Sse(anthropic_text_stream())
            }
        }))
        .await;
        let config = translated_config(&mock.base, "anthropic_messages", "");
        let relay = start_relay(&config, &[("ALPHA_KEY", "alpha-secret-key")], 1 << 20).await;

        let mut request = request_base("alpha/upstream-model");
        request["tools"] = codex_tools();
        request["max_output_tokens"] = json!(1024);
        let response = post(&relay, request).await;
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        let events = downstream_events(&text);

        let done = events
            .iter()
            .filter(|event| event["type"] == "response.output_item.done")
            .map(|event| event["item"].clone())
            .collect::<Vec<_>>();
        assert_eq!(done[0]["type"], "custom_tool_call");
        assert_eq!(done[0]["name"], "apply_patch");
        assert_eq!(done[0]["input"], "*** Begin Patch");
        assert_eq!(done[1]["type"], "function_call");
        assert_eq!(done[1]["name"], "spawn_agent");
        assert_eq!(done[1]["namespace"], "multi_agent_v1");
        assert_eq!(events.last().unwrap()["type"], "response.completed");

        let first = mock.hits()[0].body.clone().unwrap();
        let names = first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["exec_command", "apply_patch", "multi_agent_v1__spawn_agent"]
        );
        assert_eq!(
            first["tools"][1]["input_schema"]["required"],
            json!(["input"])
        );

        // Replay the assistant items exactly as Codex would.
        let mut replay = request_base("alpha/upstream-model");
        replay["tools"] = codex_tools();
        replay["max_output_tokens"] = json!(1024);
        replay["input"] = json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            done[0].clone(),
            done[1].clone(),
            {"type": "custom_tool_call_output", "call_id": "toolu_1", "output": "patch applied"},
            {"type": "function_call_output", "call_id": "toolu_2", "output": "agent spawned"}
        ]);
        let response = post(&relay, replay).await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            downstream_events(&response.text().await.unwrap())
                .last()
                .unwrap()["type"],
            "response.completed"
        );

        let second = mock.hits()[1].body.clone().unwrap();
        let messages = second["messages"].as_array().unwrap();
        let tool_use = messages
            .iter()
            .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
            .filter(|block| block["type"] == "tool_use")
            .collect::<Vec<_>>();
        assert_eq!(tool_use[0]["name"], "apply_patch");
        assert_eq!(tool_use[0]["input"], json!({"input": "*** Begin Patch"}));
        assert_eq!(tool_use[1]["name"], "multi_agent_v1__spawn_agent");
        let results = messages
            .iter()
            .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
            .filter(|block| block["type"] == "tool_result")
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_use_id"], "toolu_1");
        assert_eq!(results[1]["tool_use_id"], "toolu_2");
    }

    #[tokio::test]
    async fn anthropic_signed_thinking_is_streamed_and_replayed() {
        let mock = wire_mock(Arc::new(|count: usize, _: &str| {
            if count == 1 {
                Reply::Sse(anthropic_sse(&[
                    json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "content": [], "model": "upstream-model", "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "think"}}),
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-1"}}),
                    json!({"type": "content_block_stop", "index": 0}),
                    json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "exec_command", "input": {"cmd": "ls"}}}),
                    json!({"type": "content_block_stop", "index": 1}),
                    json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}}),
                    json!({"type": "message_stop"}),
                ]))
            } else {
                Reply::Sse(anthropic_text_stream())
            }
        }))
        .await;
        let config = translated_config(
            &mock.base,
            "anthropic_messages",
            "anthropic_thinking = \"manual\"\nanthropic_thinking_budget_tokens = 1024\nmax_output_tokens = 4096\n",
        );
        let relay = start_relay(&config, &[("ALPHA_KEY", "alpha-secret-key")], 1 << 20).await;

        let mut request = request_base("alpha/upstream-model");
        request["tools"] = codex_tools();
        let response = post(&relay, request).await;
        assert_eq!(response.status(), 200);
        let events = downstream_events(&response.text().await.unwrap());
        let reasoning = events
            .iter()
            .find(|event| {
                event["type"] == "response.output_item.done" && event["item"]["type"] == "reasoning"
            })
            .expect("a reasoning item")
            .get("item")
            .unwrap()
            .clone();
        assert!(
            reasoning["encrypted_content"]
                .as_str()
                .unwrap()
                .starts_with(crate::adapters::anthropic::THINKING_ENVELOPE_PREFIX)
        );
        let call = events
            .iter()
            .find(|event| {
                event["type"] == "response.output_item.done"
                    && event["item"]["type"] == "function_call"
            })
            .expect("a function call item")
            .get("item")
            .unwrap()
            .clone();
        let first = mock.hits()[0].body.clone().unwrap();
        assert_eq!(
            first["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024})
        );
        assert_eq!(first["max_tokens"], 4096);

        let mut replay = request_base("alpha/upstream-model");
        replay["tools"] = codex_tools();
        replay["input"] = json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            reasoning,
            call,
            {"type": "function_call_output", "call_id": "toolu_1", "output": "out"}
        ]);
        let response = post(&relay, replay).await;
        assert_eq!(response.status(), 200);
        let _ = response.text().await.unwrap();
        let second = mock.hits()[1].body.clone().unwrap();
        let messages = second["messages"].as_array().unwrap();
        let thinking = messages
            .iter()
            .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
            .find(|block| block["type"] == "thinking")
            .expect("thinking was replayed");
        assert_eq!(thinking["thinking"], "think");
        assert_eq!(thinking["signature"], "sig-1");
    }

    #[tokio::test]
    async fn chat_completions_tool_loop_and_usage_option() {
        let mock = wire_mock(Arc::new(|count: usize, _: &str| {
            if count == 1 {
                Reply::Sse(chat_sse(&[
                    json!({"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "exec_command", "arguments": "{\"cmd\":"}}]}}]}),
                    json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"ls\"}"}}]}}]}),
                    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
                    json!({"choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}}),
                ]))
            } else {
                Reply::Sse(chat_sse(&[
                    json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": "done"}}]}),
                    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 9, "completion_tokens": 2, "total_tokens": 11}}),
                ]))
            }
        }))
        .await;
        let config = translated_config(&mock.base, "chat_completions", "");
        let relay = start_relay(&config, &[("ALPHA_KEY", "alpha-secret-key")], 1 << 20).await;

        let mut request = request_base("alpha/upstream-model");
        request["tools"] = codex_tools();
        request["max_output_tokens"] = json!(256);
        let response = post(&relay, request).await;
        assert_eq!(response.status(), 200);
        let events = downstream_events(&response.text().await.unwrap());
        let call = events
            .iter()
            .find(|event| {
                event["type"] == "response.output_item.done"
                    && event["item"]["type"] == "function_call"
            })
            .expect("a function call")
            .get("item")
            .unwrap()
            .clone();
        assert_eq!(call["name"], "exec_command");
        assert_eq!(call["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(events.last().unwrap()["type"], "response.completed");

        let first = mock.last().body.clone().unwrap();
        assert_eq!(first["max_tokens"], 256);
        assert_eq!(first["stream_options"], json!({"include_usage": true}));
        assert!(first.get("reasoning_effort").is_none());

        let mut replay = request_base("alpha/upstream-model");
        replay["tools"] = codex_tools();
        replay["max_output_tokens"] = json!(256);
        replay["input"] = json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            call,
            {"type": "function_call_output", "call_id": "call_1", "output": "listing"}
        ]);
        let response = post(&relay, replay).await;
        assert_eq!(response.status(), 200);
        let events = downstream_events(&response.text().await.unwrap());
        assert_eq!(events.last().unwrap()["type"], "response.completed");

        let second = mock.last().body.clone().unwrap();
        let messages = second["messages"].as_array().unwrap();
        let tool_message = messages
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("a tool message");
        assert_eq!(tool_message["tool_call_id"], "call_1");
        assert_eq!(tool_message["content"], "listing");
        let assistant = messages
            .iter()
            .find(|message| message["tool_calls"].is_array())
            .expect("assistant tool_calls");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["name"],
            "exec_command"
        );
    }

    #[tokio::test]
    async fn chat_translated_web_run_round_trips_through_the_namespace_shape() {
        let mock = wire_mock(Arc::new(|count: usize, _: &str| {
            if count == 1 {
                Reply::Sse(chat_sse(&[
                    json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_w", "type": "function", "function": {"name": "web__run", "arguments": "{\"search_query\":[{\"q\":\"rust\"}]}"}}]}}]}),
                    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
                ]))
            } else {
                Reply::Sse(chat_sse(&[
                    json!({"choices": [{"index": 0, "delta": {"content": "searched"}}]}),
                    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                ]))
            }
        }))
        .await;
        let config = translated_config(&mock.base, "chat_completions", "");
        let relay = start_relay(&config, &[("ALPHA_KEY", "alpha-secret-key")], 1 << 20).await;

        let mut request = request_base("alpha/upstream-model");
        let tools = json!([
            {"type": "namespace", "name": "web", "description": "Web tools",
             "tools": [{"type": "function", "name": "run", "description": "Search",
                        "strict": false, "parameters": {"type": "object",
                            "properties": {"search_query": {"type": "array"}}}}]}
        ]);
        request["tools"] = tools.clone();
        let response = post(&relay, request).await;
        assert_eq!(response.status(), 200);
        let events = downstream_events(&response.text().await.unwrap());
        let call = events
            .iter()
            .find(|event| {
                event["type"] == "response.output_item.done"
                    && event["item"]["type"] == "function_call"
            })
            .expect("a function call")
            .get("item")
            .unwrap()
            .clone();
        assert_eq!(call["namespace"], "web");
        assert_eq!(call["name"], "run");
        assert!(call["arguments"].as_str().unwrap().contains("search_query"));

        let first = mock.last().body.clone().unwrap();
        let names = first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["web__run"]);

        let mut replay = request_base("alpha/upstream-model");
        replay["tools"] = tools;
        replay["input"] = json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "search"}]},
            call,
            {"type": "function_call_output", "call_id": "call_w",
             "output": "Mock search output with turn0search0"}
        ]);
        let response = post(&relay, replay).await;
        assert_eq!(response.status(), 200);
        let _ = response.text().await.unwrap();

        let second = mock.last().body.clone().unwrap();
        let messages = second["messages"].as_array().unwrap();
        let assistant = messages
            .iter()
            .find(|message| message["tool_calls"].is_array())
            .expect("assistant tool_calls");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "web__run");
        let tool_message = messages
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("a tool message");
        assert!(
            tool_message["content"]
                .as_str()
                .unwrap()
                .contains("turn0search0")
        );
    }

    fn stock_search_relay_config(vendor: &str) -> String {
        format!(
            r#"
[providers.alpha]
name = "Alpha"
base_url = "{vendor}"
env_key = "ALPHA_KEY"
discovery = false

[[providers.alpha.models]]
id = "upstream-model"
name = "Upstream Model"
context_window = 200000

[search]
env_key = "BRAVE_SEARCH_API_KEY"
"#
        )
    }

    fn stock_web_call_stream() -> String {
        let item = json!({
            "id": "fc_w1", "type": "function_call", "status": "completed",
            "call_id": "call_w1", "name": "run", "namespace": "web",
            "arguments": "{\"search_query\":[{\"q\":\"rust book\"}]}"
        });
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        added["arguments"] = json!("");
        let events = [
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress", "output": []}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": added}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_w1",
                   "output_index": 0, "delta": item["arguments"]}),
            json!({"type": "response.function_call_arguments.done", "item_id": "fc_w1",
                   "output_index": 0, "arguments": item["arguments"]}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed",
                   "output": [item], "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}}),
        ];
        let mut text = String::new();
        for event in events {
            let kind = event["type"].as_str().unwrap();
            text.push_str(&format!("event: {kind}\ndata: {event}\n\n"));
        }
        text
    }

    fn stock_text_stream() -> String {
        let annotation = json!({"type": "url_citation", "url": "https://www.rust-lang.org/learn",
                                "title": "Learn Rust", "start_index": 0, "end_index": 4});
        let events = [
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"id": "msg_1", "type": "message", "role": "assistant",
                            "status": "in_progress", "content": []}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0,
                   "content_index": 0, "delta": "searched fixture"}),
            json!({"type": "response.output_text.annotation.added", "item_id": "msg_1",
                   "output_index": 0, "content_index": 0, "annotation_index": 0,
                   "annotation": annotation}),
            json!({"type": "response.output_item.done", "output_index": 0,
                   "item": {"id": "msg_1", "type": "message", "role": "assistant",
                            "status": "completed", "content": [{"type": "output_text",
                                "text": "searched fixture", "annotations": [annotation]}]}}),
            json!({"type": "response.completed", "response": {"id": "resp_2", "status": "completed",
                   "output": [], "usage": {"input_tokens": 20, "output_tokens": 3, "total_tokens": 23}}}),
        ];
        let mut text = String::new();
        for event in events {
            let kind = event["type"].as_str().unwrap();
            text.push_str(&format!("event: {kind}\ndata: {event}\n\n"));
        }
        text
    }

    /// Opt-in proof against the installed stock Codex CLI. The relay serves
    /// the model catalog and `/v1/alpha/search`; the vendor and Brave are both
    /// loopback mocks. Nothing here reaches a real model or Brave.
    #[tokio::test]
    #[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN to the stock Codex binary"]
    async fn stock_codex_emits_web_run_and_calls_the_alpha_search_endpoint() {
        let binary = match std::env::var_os("CODEOX_TEST_CODEX_BIN") {
            Some(path) => std::path::PathBuf::from(path),
            None => panic!("set CODEOX_TEST_CODEX_BIN to the stock Codex binary"),
        };
        let version = std::process::Command::new(&binary)
            .arg("--version")
            .output()
            .expect("run the stock Codex binary");
        assert!(version.status.success(), "stock Codex --version failed");
        println!(
            "Testing stock Codex: {}",
            String::from_utf8_lossy(&version.stdout).trim()
        );

        let responses = Arc::new(AtomicUsize::new(0));
        let reply_responses = Arc::clone(&responses);
        let mock = wire_mock(Arc::new(move |_: usize, _: &str| {
            if reply_responses.fetch_add(1, Ordering::SeqCst) == 0 {
                Reply::Sse(stock_web_call_stream())
            } else {
                Reply::Sse(stock_text_stream())
            }
        }))
        .await;
        let brave = start_brave(BraveMode::LlmContext).await;
        let relay = start_relay_with_search(
            &stock_search_relay_config(&mock.base),
            &[("ALPHA_KEY", "alpha-secret-key")],
            1 << 20,
            Some(brave_at(&brave.base)),
        )
        .await;

        let home = TempHome::new();
        let project = home.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            home.0.join("config.toml"),
            "model = \"alpha/upstream-model\"\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\nsuppress_unstable_features_warning = true\n",
        )
        .unwrap();

        let overrides =
            codeox_overrides(&relay.base, false, SearchLaunch::Enabled { set_live: true });
        let mut command = std::process::Command::new(&binary);
        for entry in &overrides {
            command.arg("-c").arg(entry);
        }
        command
            .args([
                "exec",
                "--strict-config",
                "--skip-git-repo-check",
                "-m",
                "alpha/upstream-model",
            ])
            .arg("Use the web tool to search for rust book, then reply.")
            .current_dir(&project)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home.0)
            .env("CODEX_HOME", &home.0)
            .env("CODEOX_TOKEN", RELAY_TOKEN)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = tokio::task::spawn_blocking(move || command.output())
            .await
            .unwrap()
            .expect("run stock Codex through the relay");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "stock Codex failed: {stdout}\n{stderr}"
        );

        let hits = mock.hits();
        assert_eq!(hits.len(), 2, "one tool call plus its continuation");
        let tools = hits[0].body.as_ref().unwrap()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["type"] == "namespace")
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>();
        assert!(
            tools.contains(&"web"),
            "stock Codex must emit the web.run namespace: {tools:?}"
        );
        let replay = hits[1].body.as_ref().unwrap()["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["type"].as_str().unwrap_or(""),
                    item["namespace"].as_str(),
                    item["name"].as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            replay.contains(&("function_call", Some("web"), Some("run"))),
            "the web.run call must round-trip in history: {replay:?}"
        );
        let continuation = hits[1].body.as_ref().unwrap()["input"].as_array().unwrap();
        let search_output = continuation
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .map(|item| item["output"].to_string())
            .unwrap_or_else(|| panic!("the search tool output: {continuation:?}"));
        assert!(
            search_output.contains("turn0search0"),
            "citations from the Brave result survive the roundtrip: {search_output}"
        );

        let brave = brave_hits(&brave);
        assert_eq!(brave.len(), 1);
        assert_eq!(brave[0].path, "/res/v1/llm/context");
        assert!(
            brave[0]
                .query
                .contains(&("q".to_owned(), "rust book".to_owned()))
        );
        assert_eq!(brave[0].token.as_deref(), Some("test-brave-key"));
    }

    #[tokio::test]
    async fn anthropic_translated_web_run_history_round_trips() {
        let mock = wire_mock(Arc::new(|_: usize, _: &str| {
            Reply::Sse(anthropic_text_stream())
        }))
        .await;
        let config = translated_config(&mock.base, "anthropic_messages", "");
        let relay = start_relay(&config, &[("ALPHA_KEY", "alpha-secret-key")], 1 << 20).await;

        let mut request = request_base("alpha/upstream-model");
        request["tools"] = json!([
            {"type": "namespace", "name": "web", "description": "Web tools",
             "tools": [{"type": "function", "name": "run", "description": "Search",
                        "strict": false, "parameters": {"type": "object",
                            "properties": {"search_query": {"type": "array"}}}}]}
        ]);
        request["input"] = json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "search"}]},
            {"type": "function_call", "namespace": "web", "name": "run",
             "call_id": "call_w", "arguments": "{\"search_query\":[{\"q\":\"rust\"}]}"},
            {"type": "function_call_output", "call_id": "call_w",
             "output": "Mock search output with turn0search0"}
        ]);
        let response = post(&relay, request).await;
        assert_eq!(response.status(), 200);
        let _ = response.text().await.unwrap();

        let body = mock.last().body.unwrap();
        let messages = body["messages"].as_array().unwrap();
        let tool_use = messages
            .iter()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .find(|block| block["type"] == "tool_use")
            .expect("the replayed web.run call");
        assert_eq!(tool_use["name"], "web__run");
        assert_eq!(tool_use["id"], "call_w");
        let tool_result = messages
            .iter()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .find(|block| block["type"] == "tool_result")
            .expect("the replayed search output");
        assert_eq!(tool_result["tool_use_id"], "call_w");
        assert!(tool_result["content"].to_string().contains("turn0search0"));
    }

    #[tokio::test]
    async fn responses_citations_stream_through_unchanged() {
        let citation = json!({"type": "url_citation", "url": "https://www.rust-lang.org/learn",
                              "title": "Learn Rust", "start_index": 0, "end_index": 5});
        let upstream = format!(
            "event: response.output_text.annotation.added\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
            json!({"type": "response.output_text.annotation.added", "item_id": "msg_1",
                   "output_index": 0, "content_index": 0, "annotation_index": 0,
                   "annotation": citation}),
            json!({"type": "response.completed", "response": {"id": "resp_1",
                   "object": "response", "status": "completed", "output": [],
                   "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}})
        );
        let mock = wire_mock(Arc::new(move |_: usize, _: &str| {
            Reply::Sse(upstream.clone())
        }))
        .await;
        let config = translated_config(&mock.base, "responses", "");
        let relay = start_relay(&config, &[("ALPHA_KEY", "alpha-secret-key")], 1 << 20).await;

        let response = post(&relay, request_base("alpha/upstream-model")).await;
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        let events = downstream_events(&text);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0]["annotation"]["type"], "url_citation",
            "native Responses citations pass through untouched"
        );
        assert_eq!(events[1]["type"], "response.completed");
    }

    #[tokio::test]
    async fn chat_custom_tool_calls_round_trip_through_function_arguments() {
        let mock = wire_mock(Arc::new(|count: usize, _: &str| {
            if count == 1 {
                Reply::Sse(chat_sse(&[
                    json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_p", "type": "function", "function": {"name": "apply_patch", "arguments": "{\"input\":\"*** Begin Patch\"}"}}]}}]}),
                    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
                ]))
            } else {
                Reply::Sse(chat_sse(&[
                    json!({"choices": [{"index": 0, "delta": {"content": "ok"}}]}),
                    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                ]))
            }
        }))
        .await;
        let config = translated_config(&mock.base, "chat_completions", "");
        let relay = start_relay(&config, &[("ALPHA_KEY", "alpha-secret-key")], 1 << 20).await;

        let mut request = request_base("alpha/upstream-model");
        request["tools"] = codex_tools();
        let response = post(&relay, request).await;
        let text = response.text().await.unwrap();
        let events = downstream_events(&text);
        let item = events
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .unwrap()
            .get("item")
            .unwrap()
            .clone();
        assert_eq!(item["type"], "custom_tool_call");
        assert_eq!(item["name"], "apply_patch");
        assert_eq!(item["input"], "*** Begin Patch");

        let mut replay = request_base("alpha/upstream-model");
        replay["tools"] = codex_tools();
        replay["input"] = json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            item,
            {"type": "custom_tool_call_output", "call_id": "call_p", "output": "ok"}
        ]);
        let response = post(&relay, replay).await;
        assert_eq!(response.status(), 200);
        let _ = response.text().await.unwrap();
        let second = mock.last().body.clone().unwrap();
        let assistant = second["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["tool_calls"].is_array())
            .expect("assistant tool_calls");
        let arguments: Value = serde_json::from_str(
            assistant["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(arguments, json!({"input": "*** Begin Patch"}));
    }

    #[tokio::test]
    async fn upstream_errors_are_redacted_and_headers_are_not_relayed() {
        let mock = wire_mock(Arc::new(|_: usize, _: &str| {
            Reply::Json(
                StatusCode::UNAUTHORIZED,
                json!({"error": {"type": "authentication_error", "message": "invalid key alpha-secret-key"}}),
            )
        }))
        .await;
        let config = translated_config(&mock.base, "anthropic_messages", "");
        let relay = start_relay(&config, &[("ALPHA_KEY", "alpha-secret-key")], 1 << 20).await;
        let response = post(&relay, request_base("alpha/upstream-model")).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(response.headers().get(AUTHORIZATION).is_none());
        assert!(response.headers().get("x-api-key").is_none());
        let text = response.text().await.unwrap();
        assert!(!text.contains("alpha-secret-key"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
        assert!(text.contains("401"), "{text}");
    }

    #[tokio::test]
    async fn truncated_streams_fail_instead_of_completing() {
        let anthropic = wire_mock(Arc::new(|_: usize, _: &str| {
            Reply::Sse(anthropic_sse(&[
                json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "content": [], "model": "upstream-model", "stop_reason": null, "usage": {"input_tokens": 1, "output_tokens": 0}}}),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}}),
            ]))
        }))
        .await;
        let relay = start_relay(
            &translated_config(&anthropic.base, "anthropic_messages", ""),
            &[("ALPHA_KEY", "alpha-secret-key")],
            1 << 20,
        )
        .await;
        let response = post(&relay, request_base("alpha/upstream-model")).await;
        let text = response.text().await.unwrap();
        let events = downstream_events(&text);
        assert_eq!(events.last().unwrap()["type"], "response.failed", "{text}");
        assert!(!text.contains("response.completed"));

        let chat = wire_mock(Arc::new(|_: usize, _: &str| {
            Reply::Sse(format!(
                "data: {}\n\n",
                json!({"choices": [{"index": 0, "delta": {"content": "partial"}}]})
            ))
        }))
        .await;
        let relay = start_relay(
            &translated_config(&chat.base, "chat_completions", ""),
            &[("ALPHA_KEY", "alpha-secret-key")],
            1 << 20,
        )
        .await;
        let response = post(&relay, request_base("alpha/upstream-model")).await;
        let text = response.text().await.unwrap();
        let events = downstream_events(&text);
        assert_eq!(events.last().unwrap()["type"], "response.failed", "{text}");
        assert!(!text.contains("response.completed"));
    }

    #[tokio::test]
    async fn chat_options_are_applied_from_vendor_config() {
        let mock = wire_mock(Arc::new(|_: usize, _: &str| {
            Reply::Sse(chat_sse(&[
                json!({"choices": [{"index": 0, "delta": {"content": "ok"}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ]))
        }))
        .await;
        let config = translated_config(
            &mock.base,
            "chat_completions",
            "max_tokens_field = \"max_completion_tokens\"\nchat_reasoning_effort = \"forward\"\ndeveloper_role = \"developer\"\ninclude_stream_usage = false\n",
        );
        let relay = start_relay(&config, &[("ALPHA_KEY", "alpha-secret-key")], 1 << 20).await;
        let mut request = request_base("alpha/upstream-model");
        request["max_output_tokens"] = json!(128);
        request["input"] = json!([
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "behave"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
        ]);
        let response = post(&relay, request).await;
        assert_eq!(response.status(), 200);
        let _ = response.text().await.unwrap();
        let body = mock.last().body.unwrap();
        assert_eq!(body["max_completion_tokens"], 128);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["reasoning_effort"], "medium");
        assert!(body.get("stream_options").is_none());
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "You are a coding agent.");
        let developer = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "developer")
            .expect("the developer message keeps its role");
        assert_eq!(developer["content"], "behave");
    }

    #[tokio::test]
    async fn compact_endpoint_is_rejected_for_translated_vendors() {
        let mock = wire_mock(Arc::new(|_: usize, _: &str| Reply::Sse(String::new()))).await;
        let relay = start_relay(
            &translated_config(&mock.base, "chat_completions", ""),
            &[("ALPHA_KEY", "alpha-secret-key")],
            1 << 20,
        )
        .await;
        let response = test_client()
            .post(format!("{}/responses/compact", relay.base))
            .headers(auth_headers())
            .json(&request_base("alpha/upstream-model"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(mock.hits().is_empty());
    }
}

/// Helpers shared with `translated_tests`; the module exists so the test helpers
/// stay reachable without widening the production surface.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    pub(crate) const RELAY_TOKEN: &str = "relay-test-token";

    pub(crate) fn auth_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {RELAY_TOKEN}")).unwrap(),
        );
        headers
    }
}

/// Loopback transport-failure tests: downstream disconnects and stalled vendor
/// reads. Lives in its own file to keep them out of the request-shape suites.
#[cfg(test)]
mod test_transport;

//! Launcher-level tests for the real `ox` binary.
//!
//! `provider add` and `doctor` are codeox subcommands, so they must be
//! dispatched before any client launch. Stub clients on `PATH` record the
//! arguments and environment without making upstream requests.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BRAVE_KEY: &str = "brave-test-key-must-not-leak";
const CLAUDE_DISCOVERY_ENV: &str = "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY";

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let path = std::env::temp_dir().join(format!("codeox-{name}-{}", hex::encode(bytes)));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_ox")
}

fn base_env(command: &mut Command, home: &Path) {
    command
        .env("CODEX_HOME", home)
        .env("HOME", home)
        .env_remove("BRAVE_SEARCH_API_KEY")
        .env_remove("ALPHA_KEY")
        .env_remove("CODEOX_TOKEN")
        .env_remove(CLAUDE_DISCOVERY_ENV);
}

fn write_fake_client(dir: &Path, client: &str) {
    let script = dir.join(client);
    fs::write(
        &script,
        r#"#!/bin/sh
if [ "$1" = "debug" ] && [ "$2" = "models" ]; then
  env > "$CAPTURE/bundled-env"
  printf '{"models":[]}'
  exit 0
fi
if [ "$3" = "login" ] && [ "$4" = "status" ]; then
  printf '%s\n' "$FAKE_CODEX_LOGIN_STATUS" >&2
  exit 0
fi
env > "$CAPTURE/run-env"
printf '%s\n' "$@" > "$CAPTURE/run-args"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
    }
}

fn write_codeox_config(home: &Path, search: bool) {
    let search = if search {
        "\n[search]\nenv_key = \"BRAVE_SEARCH_API_KEY\"\n"
    } else {
        ""
    };
    fs::write(
        home.join("codeox.toml"),
        format!(
            r#"[providers.alpha]
name = "Alpha"
base_url = "http://127.0.0.1:9/v1"
env_key = "ALPHA_KEY"
discovery = false

[[providers.alpha.models]]
id = "alpha-model"
{search}"#
        ),
    )
    .unwrap();
}

fn run_launcher(
    home: &Path,
    capture: &Path,
    fake_bin: &Path,
    env: &[(&str, &str)],
    args: &[&str],
) -> std::process::Output {
    let mut command = Command::new(binary());
    base_env(&mut command, home);
    command
        .env(
            "PATH",
            format!(
                "{}:{}",
                fake_bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("CAPTURE", capture);
    for (key, value) in env {
        command.env(key, value);
    }
    command.args(args);
    command.output().unwrap()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn version_is_available_without_a_home_or_stock_client() {
    let output = Command::new(binary())
        .env_clear()
        .arg("--version")
        .output()
        .expect("run ox --version");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        concat!("ox ", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn setup_creates_the_shared_home_without_a_client_installed() {
    let home = TempDir::new("launcher-subcommands");
    let config_home = home.0.join("new-home");
    // Empty PATH to verify setup works without either client installed.
    let empty_path = TempDir::new("empty-path");
    let mut add = Command::new(binary());
    base_env(&mut add, &config_home);
    let add = add
        .env("PATH", &empty_path.0)
        .args([
            "setup",
            "acme",
            "--base-url",
            "https://api.example.com/v1",
            "--no-discovery",
            "--model",
            "acme-model",
        ])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "setup failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let config = read(&config_home.join("codeox.toml"));
    assert!(config.contains("[providers.acme]"), "{config}");
    assert!(config.contains("id = \"acme-model\""), "{config}");
    assert!(
        !config.contains("acme_API_KEY") && !config.to_lowercase().contains("secret"),
        "provider add stores only a credential reference: {config}"
    );

    let mut doctor = Command::new(binary());
    base_env(&mut doctor, &config_home);
    let doctor = doctor
        .env("PATH", &empty_path.0)
        .env("ACME_API_KEY", "dummy-not-a-real-key")
        .arg("doctor")
        .output()
        .unwrap();
    assert!(
        doctor.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let report = String::from_utf8_lossy(&doctor.stdout);
    assert!(report.contains("acme"), "{report}");
    assert!(report.contains("ACME_API_KEY"), "{report}");
}

#[test]
fn first_run_and_help_work_with_only_native_codex_settings() {
    let home = TempDir::new("launcher-first-run");
    let empty_path = TempDir::new("launcher-no-clients");
    let native_config = home.0.join("config.toml");
    let original = "model_provider = \"openai\"\n";
    fs::write(&native_config, original).unwrap();

    let run = |args: &[&str]| {
        Command::new(binary())
            .env_clear()
            .env("HOME", &home.0)
            .env("CODEX_HOME", &home.0)
            .env("PATH", &empty_path.0)
            .args(args)
            .output()
            .unwrap()
    };
    let help = run(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Get started"));
    let bare = run(&[]);
    assert!(bare.status.success());
    assert_eq!(bare.stdout, help.stdout);

    for command in ["setup", "codex", "claude"] {
        let output = run(&[command, "--help"]);
        assert!(output.status.success(), "{command} --help");
        let usage = format!("ox {command}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(&usage));
    }
    let setup = run(&["setup"]);
    assert!(setup.status.success());
    assert!(String::from_utf8_lossy(&setup.stdout).contains("--env-key"));
    for (topic, text) in [
        ("setup", "--env-key"),
        ("desktop", "Model Providers & Gateways"),
        ("share", "tailscale serve --bg"),
        ("t3", "Claude Agent SDK"),
        ("subscriptions", "ox login chatgpt"),
    ] {
        let output = Command::new(binary())
            .env_clear()
            .args(["help", topic])
            .output()
            .unwrap();
        assert!(output.status.success(), "ox help {topic}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(text));
    }

    for command in ["codex", "claude"] {
        let missing_provider = run(&[command]);
        assert!(!missing_provider.status.success());
        assert!(String::from_utf8_lossy(&missing_provider.stderr).contains("Run `ox setup`"));
    }
    assert!(String::from_utf8_lossy(&run(&["doctor"]).stdout).contains("Run `ox setup`"));
    assert!(!home.0.join("codeox.toml").exists());
    assert_eq!(read(&native_config), original);
}

#[test]
fn native_sign_in_and_launch_use_stock_clients() {
    let home = TempDir::new("launcher-native-home");
    let capture = TempDir::new("launcher-native-capture");
    let fake_bin = TempDir::new("launcher-native-bin");
    write_fake_client(&fake_bin.0, "codex");
    write_fake_client(&fake_bin.0, "claude");
    let run = |args: &[&str], status: &str| {
        run_launcher(
            &home.0,
            &capture.0,
            &fake_bin.0,
            &[
                ("FAKE_CODEX_LOGIN_STATUS", status),
                ("OPENAI_API_KEY", "must-not-use-api-key"),
                ("OPENAI_BASE_URL", "https://fixture-openai.invalid/v1"),
                ("OPENAI_API_BASE", "https://fixture-openai.invalid/v1"),
                ("ANTHROPIC_BASE_URL", "https://fixture-anthropic.invalid"),
                ("ANTHROPIC_AUTH_TOKEN", "fixture-gateway-value"),
                ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ],
            args,
        )
    };

    let auth = home.0.join("auth.json");
    fs::write(&auth, r#"{"OPENAI_API_KEY":"fixture-api-key"}"#).unwrap();
    let blocked = run(&["codex", "--chatgpt"], "API key configured");
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("Run ox login chatgpt"));
    assert!(!capture.0.join("run-args").exists());

    fs::write(
        &auth,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fixture-token"}}"#,
    )
    .unwrap();
    let cases: &[(&[&str], &str)] = &[
        (
            &["login", "chatgpt", "--device-auth"],
            "-c\nmodel_provider=\"openai\"\nlogin\n--device-auth\n",
        ),
        (&["login", "claude"], "auth\nlogin\n--claudeai\n"),
        (
            &["codex", "--chatgpt", "resume"],
            "-c\nmodel_provider=\"openai\"\nresume\n",
        ),
        (&["claude", "--native", "--continue"], "--continue\n"),
    ];
    for &(args, forwarded) in cases {
        let output = run(args, "Logged in using ChatGPT");
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(read(&capture.0.join("run-args")), forwarded);
        let env = read(&capture.0.join("run-env"));
        if args == ["login", "claude"] {
            assert!(!env.contains("ANTHROPIC_BASE_URL="));
            assert!(!env.contains("ANTHROPIC_AUTH_TOKEN="));
            assert!(!env.contains("CLAUDE_CODE_USE_BEDROCK="));
        } else if args[0] == "codex" || args[1] == "chatgpt" {
            assert!(!env.contains("OPENAI_BASE_URL="));
            assert!(!env.contains("OPENAI_API_BASE="));
        }
    }
    assert!(!capture.0.join("bundled-env").exists());
    assert!(!home.0.join("codeox.toml").exists());
    let codex = run(&["codex", "--chatgpt"], "Logged in using ChatGPT");
    assert!(codex.status.success());
    assert!(!read(&capture.0.join("run-env")).contains("must-not-use-api-key"));

    fs::write(
        home.0.join("config.toml"),
        "cli_auth_credentials_store = \"keyring\"\n",
    )
    .unwrap();
    let blocked = run(&["codex", "--chatgpt"], "API key configured");
    assert!(!blocked.status.success());
    let codex = run(&["codex", "--chatgpt"], "Logged in using ChatGPT");
    assert!(codex.status.success());
}

#[test]
fn launcher_registers_search_only_when_opted_in_and_strips_the_key() {
    let home = TempDir::new("launcher-search-home");
    let capture = TempDir::new("launcher-search-capture");
    let fake_bin = TempDir::new("launcher-search-bin");
    write_fake_client(&fake_bin.0, "codex");
    write_codeox_config(&home.0, true);

    let output = run_launcher(
        &home.0,
        &capture.0,
        &fake_bin.0,
        &[
            ("BRAVE_SEARCH_API_KEY", BRAVE_KEY),
            ("ALPHA_KEY", "alpha-secret"),
        ],
        &["exec", "--json", "reviewed handoff summary"],
    );
    assert!(
        output.status.success(),
        "launcher failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let args = read(&capture.0.join("run-args"));
    assert!(args.contains("exec\n"), "{args}");
    assert!(args.contains("--json\n"), "{args}");
    assert!(args.contains("reviewed handoff summary\n"), "{args}");
    assert!(
        args.contains("model_providers.codeox.supports_standalone_web_search=true"),
        "{args}"
    );
    assert!(
        args.contains("features.standalone_web_search=true"),
        "{args}"
    );
    assert!(args.contains("web_search=\"live\""), "{args}");
    assert!(args.contains("model_provider=\"codeox\""), "{args}");

    for env_file in ["run-env", "bundled-env"] {
        let env = read(&capture.0.join(env_file));
        assert!(
            !env.contains(BRAVE_KEY),
            "{env_file} must not carry the Brave key: {env}"
        );
        assert!(
            !env.contains("alpha-secret"),
            "{env_file} must not carry a vendor key"
        );
        if env_file == "run-env" {
            assert!(env.contains("CODEOX_TOKEN="), "{env_file}");
        }
    }

    // Without the credential reference, nothing standalone is advertised.
    let output = run_launcher(
        &home.0,
        &capture.0,
        &fake_bin.0,
        &[("ALPHA_KEY", "alpha-secret")],
        &["exec", "hi"],
    );
    assert!(output.status.success());
    let args = read(&capture.0.join("run-args"));
    assert!(!args.contains("standalone_web_search"), "{args}");
    assert!(!args.contains("web_search="), "{args}");
}

#[test]
fn launcher_never_widens_a_pinned_web_search_policy() {
    let home = TempDir::new("launcher-policy-home");
    let capture = TempDir::new("launcher-policy-capture");
    let fake_bin = TempDir::new("launcher-policy-bin");
    write_fake_client(&fake_bin.0, "codex");
    write_codeox_config(&home.0, true);
    fs::write(home.0.join("config.toml"), "web_search = \"cached\"\n").unwrap();

    let output = run_launcher(
        &home.0,
        &capture.0,
        &fake_bin.0,
        &[("BRAVE_SEARCH_API_KEY", BRAVE_KEY)],
        &["exec", "hi"],
    );
    assert!(
        output.status.success(),
        "launcher failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let args = read(&capture.0.join("run-args"));
    assert!(!args.contains("standalone_web_search"), "{args}");
    assert!(
        !args.contains("web_search="),
        "a pinned policy must not be rewritten: {args}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pinned"), "{stderr}");
}

#[test]
fn claude_launcher_defaults_to_model_discovery_and_respects_opt_out() {
    let home = TempDir::new("launcher-claude-home");
    let capture = TempDir::new("launcher-claude-capture");
    let fake_bin = TempDir::new("launcher-claude-bin");
    write_fake_client(&fake_bin.0, "claude");
    write_codeox_config(&home.0, false);

    for (env, expected) in [
        ([("ALPHA_KEY", "alpha-secret")].as_slice(), "1"),
        (
            [("ALPHA_KEY", "alpha-secret"), (CLAUDE_DISCOVERY_ENV, "0")].as_slice(),
            "0",
        ),
    ] {
        let output = run_launcher(&home.0, &capture.0, &fake_bin.0, env, &["claude"]);
        assert!(
            output.status.success(),
            "launcher failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let env = read(&capture.0.join("run-env"));
        let entry = format!("{CLAUDE_DISCOVERY_ENV}={expected}");
        assert!(env.lines().any(|line| line == entry), "expected {entry}");
    }
}

#[test]
fn normal_codex_arguments_survive_unchanged() {
    let home = TempDir::new("launcher-args-home");
    let capture = TempDir::new("launcher-args-capture");
    let fake_bin = TempDir::new("launcher-args-bin");
    write_fake_client(&fake_bin.0, "codex");
    write_codeox_config(&home.0, false);

    let expected: Vec<OsString> = vec![
        "exec".into(),
        "--skip-git-repo-check".into(),
        "--model".into(),
        "alpha/alpha-model".into(),
        "keep this prompt".into(),
    ];
    let mut launcher = Command::new(binary());
    base_env(&mut launcher, &home.0);
    let output = launcher
        .env(
            "PATH",
            format!(
                "{}:{}",
                fake_bin.0.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("CAPTURE", &capture.0)
        .args(&expected)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "launcher failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let args = read(&capture.0.join("run-args"));
    let tail = args
        .lines()
        .skip_while(|line| *line != "exec")
        .collect::<Vec<_>>();
    assert_eq!(
        tail,
        expected
            .iter()
            .map(|arg| arg.to_str().unwrap())
            .collect::<Vec<_>>()
    );
}

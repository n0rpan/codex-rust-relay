//! The signed-in Claude subscription's model picker, for model search and an early
//! availability check. It comes from stock Claude Code's stream-JSON `initialize`
//! reply: no user message is sent, so nothing is inferred or billed. Results are
//! cached per account and runtime version and refreshed when a model is missing.
//! The worker's own process still checks its pin before the first turn.
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::claude_models::{self, PickerModel};

const FILE: &str = "claude-subscription-models.json";
const TTL_SECONDS: i64 = 6 * 60 * 60;
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_ID: &str = "ox-model-picker";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A non-identifying key for the saved account: the cache never holds its ID.
fn account_key(account: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, account.as_bytes());
    hex::encode(&digest.as_ref()[..12])
}

fn encode(account: &str, version: Option<&str>, at: i64, models: &[PickerModel]) -> Value {
    json!({
        "runtime": "claude",
        "account": account_key(account),
        "version": version,
        "fetched_at": at,
        "models": models.iter().map(PickerModel::to_json).collect::<Vec<_>>(),
    })
}

fn decode(cached: &Value, account: &str, at: i64) -> Option<Vec<PickerModel>> {
    let fresh = cached["fetched_at"]
        .as_i64()
        .is_some_and(|t| t <= at && at - t < TTL_SECONDS);
    (cached["runtime"] == "claude" && cached["account"] == account_key(account) && fresh)
        .then(|| claude_models::parse_picker(cached))
        .flatten()
        .filter(|models| !models.is_empty())
}

// Invalidate immediately when the installed CLI changes, without starting it on
// every cache hit. Following the symlink also catches versioned native installs.
fn installed_client() -> Option<Value> {
    let path = crate::setup::find_on_path("claude", std::env::var_os("PATH").as_deref())?
        .canonicalize()
        .ok()?;
    let meta = path.metadata().ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(json!([
        path,
        meta.len(),
        modified.as_secs(),
        modified.subsec_nanos()
    ]))
}

/// The cached picker for the saved account, if it is fresh.
pub(super) fn cached(home: &Path) -> Option<Vec<PickerModel>> {
    let account = crate::native::claude_account_id()?;
    let text = std::fs::read_to_string(home.join(FILE)).ok()?;
    let cache: Value = serde_json::from_str(&text).ok()?;
    if cache["installed_client"] != installed_client()? {
        return None;
    }
    decode(&cache, &account, now())
}

/// Remember a picker the account's own Claude Code reported.
pub(super) fn remember(home: &Path, version: Option<&str>, models: &[PickerModel]) {
    let Some(account) = crate::native::claude_account_id() else {
        return;
    };
    if models.is_empty() {
        return;
    }
    let Some(installed) = installed_client() else {
        return;
    };
    let mut cache = encode(&account, version, now(), models);
    cache["installed_client"] = installed;
    let _ = crate::setup::write_config_atomic(&home.join(FILE), &cache.to_string());
}

/// The account's picker: from the cache unless `refresh`, else from a fresh probe.
pub(super) async fn picker(home: &Path, refresh: bool) -> Result<Vec<PickerModel>, String> {
    if !refresh && let Some(models) = cached(home) {
        return Ok(models);
    }
    let (version, models) = probe(home).await?;
    remember(home, version.as_deref(), &models);
    Ok(models)
}

/// Start the stock client under the checked subscription overlay, ask only for its
/// initialization, and stop it. It never receives a user message.
async fn probe(home: &Path) -> Result<(Option<String>, Vec<PickerModel>), String> {
    let mut command = crate::native::claude_subscription_worker_command(home, false)
        .map_err(|e| e.to_string())?;
    command
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--strict-mcp-config",
        ])
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut process = super::jobs::WorkerProcess::spawn(command).map_err(|e| e.to_string())?;
    let mut input = process.child.stdin.take().expect("probe stdin is piped");
    let mut lines =
        BufReader::new(process.child.stdout.take().expect("probe stdout is piped")).lines();
    let request = claude_models::initialize_request(PROBE_ID).to_string() + "\n";
    input
        .write_all(request.as_bytes())
        .await
        .map_err(|_| "Claude Code closed before reporting its models.")?;
    input.flush().await.map_err(|e| e.to_string())?;
    // End of input: the probe sends nothing else, so no turn can follow, and stock
    // Claude Code answers its queued initialize before it exits.
    drop(input);
    let reply = tokio::time::timeout(PROBE_TIMEOUT, async {
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let r = &event["response"];
            if event["type"] == "control_response" && r["request_id"] == PROBE_ID {
                return if r["subtype"] == "success" {
                    Ok(r["response"].clone())
                } else {
                    Err("Claude Code refused to report its models.".to_owned())
                };
            }
        }
        Err("Claude Code exited before reporting its models.".to_owned())
    })
    .await
    .map_err(|_| "Claude Code did not report its models in time.".to_owned())??;
    picker_reply(&reply)
}

/// The picker of an `initialize` reply, refused unless it is the subscription's.
pub(super) fn picker_reply(reply: &Value) -> Result<(Option<String>, Vec<PickerModel>), String> {
    let account = &reply["account"];
    let subscription = account["apiProvider"] == "firstParty"
        && account["apiKeySource"].as_str().is_none_or(|s| s == "none")
        && account["tokenSource"].as_str().is_none_or(|s| s == "none")
        && account["subscriptionType"]
            .as_str()
            .is_some_and(|s| !s.is_empty());
    if !subscription {
        return Err(
            "Claude Code is not signed in to a Claude subscription here. Run ox login claude."
                .into(),
        );
    }
    let models =
        claude_models::parse_picker(reply).ok_or("Claude Code did not report its model picker.")?;
    Ok((
        reply["claude_code_version"].as_str().map(str::to_owned),
        models,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> Value {
        json!({
            "claude_code_version": "2.1.332",
            "account": {"apiProvider":"firstParty","subscriptionType":"Claude Max"},
            "models": [
                {"value":"default","resolvedModel":"claude-opus-5-5","displayName":"Default (recommended)"},
                {"value":"fable","resolvedModel":"claude-fable-5-2","displayName":"Fable 5.2"}
            ]
        })
    }

    /// Live and opt-in: `cargo test live_stock_picker -- --ignored`. Starts the
    /// installed Claude Code on the saved subscription, asks only for initialization
    /// and stops it. No user message is sent, so nothing is inferred or billed.
    #[tokio::test]
    #[ignore = "needs a signed-in stock Claude Code"]
    async fn live_stock_picker_reports_exact_models_without_a_turn() {
        let home = std::env::temp_dir().join(format!("ox-live-picker-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let models = picker(&home, true).await.expect("stock initialization");
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| claude_models::valid_model(m.exact())));
        for m in &models {
            eprintln!("{} -> {} ({})", m.value, m.exact(), m.display);
        }
        if crate::native::claude_account_id().is_some() {
            assert_eq!(cached(&home), Some(models));
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn only_a_subscription_initialization_yields_a_picker() {
        let (version, models) = picker_reply(&reply()).unwrap();
        assert_eq!(version.as_deref(), Some("2.1.332"));
        assert_eq!(models[1].exact(), "claude-fable-5-2");
        for account in [
            json!({"apiProvider":"firstParty","subscriptionType":"pro","apiKeySource":"ANTHROPIC_API_KEY"}),
            json!({"apiProvider":"firstParty","subscriptionType":"pro","tokenSource":"ANTHROPIC_AUTH_TOKEN"}),
            json!({"apiProvider":"bedrock","subscriptionType":"pro"}),
            json!({"apiProvider":"firstParty"}),
        ] {
            let mut r = reply();
            r["account"] = account;
            assert!(picker_reply(&r).is_err(), "{r}");
        }
    }

    #[test]
    fn the_cache_is_per_account_runtime_and_fresh_only() {
        let (_, models) = picker_reply(&reply()).unwrap();
        let saved = encode("account-a", Some("2.1.332"), 1_000, &models);
        assert!(
            !saved.to_string().contains("account-a"),
            "no raw account ID"
        );
        assert_eq!(decode(&saved, "account-a", 1_100), Some(models.clone()));
        assert_eq!(decode(&saved, "account-b", 1_100), None);
        assert_eq!(decode(&saved, "account-a", 1_000 + TTL_SECONDS), None);
        assert_eq!(
            decode(&saved, "account-a", 999),
            None,
            "a future stamp is stale"
        );
        let mut other = saved.clone();
        other["runtime"] = json!("codex");
        assert_eq!(decode(&other, "account-a", 1_100), None);
        assert_eq!(
            decode(&encode("account-a", None, 1_000, &[]), "account-a", 1_100),
            None
        );
    }
}

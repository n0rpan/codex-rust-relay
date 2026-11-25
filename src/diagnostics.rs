//! An explicit, bounded inference probe for the selected upstream model.

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::{Value, json};

use crate::provider::{CodeoxConfig, Env, MaxTokensField, RelayError, SavedKeys, Vendor, Wire};
use crate::sse::SseFramer;

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const RESPONSE_LIMIT: usize = 512 * 1024;
const MAX_TOKENS: u64 = 256;
const PROMPT: &str = "Reply with OK.";
const USAGE: &str = "Usage: ox doctor --live --provider NAME [--model UPSTREAM_MODEL_ID]";

pub(crate) async fn run(args: &[OsString], home: &Path) -> Result<u8, RelayError> {
    let mut args = args.iter();
    let mut provider = None;
    let mut model = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--live") => {}
            Some("--help" | "-h") => {
                println!("{USAGE}");
                println!("Sends one small streamed prompt using the configured vendor key.");
                return Ok(0);
            }
            Some("--provider") if provider.is_none() => {
                provider = Some(next_value(&mut args)?);
            }
            Some("--model") if model.is_none() => model = Some(next_value(&mut args)?),
            _ => return Err(USAGE.into()),
        }
    }
    let provider = provider.ok_or(USAGE)?;
    let config = CodeoxConfig::load(&CodeoxConfig::path(home))?;
    let vendor = config
        .vendors
        .iter()
        .find(|vendor| vendor.key == provider)
        .ok_or_else(|| unknown_provider(provider, &config))?;
    let model = model
        .or_else(|| vendor.models.first().map(|model| model.id.as_str()))
        .ok_or("pass --model with the vendor's upstream model ID")?;
    eprintln!(
        "ox doctor: probing {} via {} with one small API request",
        vendor.key,
        vendor.wire.as_str()
    );
    let saved = SavedKeys::for_config(home, &config)?;
    let (first_event, first_text) = probe(vendor, model, &|name| saved.get(name)).await?;
    println!("PASS: the provider accepted the key and model and streamed text");
    println!(
        "Upstream first event: {:.2}s. First visible text: {:.2}s.",
        first_event.as_secs_f64(),
        first_text.as_secs_f64()
    );
    Ok(0)
}

fn unknown_provider(requested: &str, config: &CodeoxConfig) -> String {
    let configured: Vec<&str> = config
        .vendors
        .iter()
        .map(|vendor| vendor.key.as_str())
        .collect();
    let fix = crate::ui::closest(requested, configured.iter().copied())
        .unwrap_or(configured.first().copied().unwrap_or("NAME"));
    format!(
        "provider {requested:?} is not configured\n  why: codeox.toml has: {}\n  fix: ox doctor --live --provider {fix}",
        configured.join(", ")
    )
}

fn next_value<'a>(args: &mut impl Iterator<Item = &'a OsString>) -> Result<&'a str, RelayError> {
    args.next()
        .and_then(|arg| arg.to_str())
        .filter(|value| !value.is_empty() && !value.starts_with('-'))
        .ok_or_else(|| USAGE.into())
}

async fn probe(
    vendor: &Vendor,
    model: &str,
    env: &Env<'_>,
) -> Result<(Duration, Duration), RelayError> {
    let key = vendor
        .api_key(env)
        .ok_or_else(|| vendor.unavailable_reason_with(env))?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(crate::CONNECT_TIMEOUT)
        .timeout(PROBE_TIMEOUT)
        .build()?;
    let (url, request) = match vendor.wire {
        Wire::Responses => (
            vendor.responses_url("/responses", None),
            json!({"model": model, "input": PROMPT, "stream": true, "store": false, "max_output_tokens": MAX_TOKENS}),
        ),
        Wire::AnthropicMessages => (
            vendor.endpoint(),
            json!({"model": model, "messages": [{"role": "user", "content": PROMPT}], "stream": true, "max_tokens": MAX_TOKENS}),
        ),
        Wire::ChatCompletions => {
            let mut request = json!({"model": model, "messages": [{"role": "user", "content": PROMPT}], "stream": true});
            let field = match vendor.chat.max_tokens_field {
                MaxTokensField::MaxTokens => "max_tokens",
                MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
            };
            request[field] = json!(MAX_TOKENS);
            (vendor.endpoint(), request)
        }
    };

    let start = Instant::now();
    let mut response = client
        .post(url)
        .headers(
            vendor
                .inference_headers(&key, None)
                .map_err(|_| "could not encode the configured provider credential")?,
        )
        .header(ACCEPT, "text/event-stream")
        .json(&request)
        .send()
        .await
        .map_err(|_| "could not reach the provider within 20 seconds")?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let hint = match status {
            401 | 403 => "check the key and its model permissions",
            404 => "check the base URL, API format, and upstream model ID",
            429 => "check provider quota and rate limits",
            _ => "check the provider's status and endpoint configuration",
        };
        return Err(format!("provider returned HTTP {status}. {hint}").into());
    }
    if !response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
    {
        return Err("provider returned a response without an event stream".into());
    }

    let mut framer = SseFramer::new(RESPONSE_LIMIT);
    let mut remaining = RESPONSE_LIMIT;
    let mut first_event = None;
    while let Some(bytes) = response
        .chunk()
        .await
        .map_err(|_| "provider stream stopped or exceeded the 20-second timeout")?
    {
        remaining = remaining
            .checked_sub(bytes.len())
            .ok_or("provider stream exceeded the live check limit")?;
        for event in framer.feed(&bytes)? {
            if event.data == "[DONE]" && vendor.wire == Wire::ChatCompletions {
                continue;
            }
            let elapsed = start.elapsed();
            let first_event = *first_event.get_or_insert(elapsed);
            let data: Value = serde_json::from_str(&event.data)
                .map_err(|_| "provider sent malformed JSON in its event stream")?;
            if data.get("error").is_some()
                || matches!(
                    data["type"].as_str(),
                    Some("error" | "response.failed" | "response.incomplete")
                )
            {
                return Err("provider reported an error in its event stream".into());
            }
            let text = match vendor.wire {
                Wire::Responses if data["type"] == "response.output_text.delta" => &data["delta"],
                Wire::AnthropicMessages if data["delta"]["type"] == "text_delta" => {
                    &data["delta"]["text"]
                }
                Wire::ChatCompletions => &data["choices"][0]["delta"]["content"],
                _ => continue,
            };
            if text.as_str().is_some_and(|text| !text.trim().is_empty()) {
                return Ok((first_event, elapsed));
            }
        }
    }
    Err("provider stream ended without any text".into())
}

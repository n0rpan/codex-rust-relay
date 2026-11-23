//! An explicit, bounded inference probe for the selected upstream model.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::{Value, json};

use crate::provider::{CodeoxConfig, MaxTokensField, RelayError, Vendor, Wire, process_env};
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
        .ok_or("provider is not configured in codeox.toml")?;
    let model = model
        .or_else(|| vendor.models.first().map(|model| model.id.as_str()))
        .ok_or("pass --model with the vendor's upstream model ID")?;
    eprintln!(
        "ox doctor: probing {} via {} with one small API request",
        vendor.key,
        vendor.wire.as_str()
    );
    probe(vendor, model).await?;
    println!("PASS: the provider accepted the key and model and streamed text");
    Ok(0)
}

fn next_value<'a>(args: &mut impl Iterator<Item = &'a OsString>) -> Result<&'a str, RelayError> {
    args.next()
        .and_then(|arg| arg.to_str())
        .filter(|value| !value.is_empty() && !value.starts_with('-'))
        .ok_or_else(|| USAGE.into())
}

async fn probe(vendor: &Vendor, model: &str) -> Result<(), RelayError> {
    let key = vendor
        .api_key(&process_env)
        .ok_or_else(|| vendor.unavailable_reason())?;
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
                return Ok(());
            }
        }
    }
    Err("provider stream ended without any text".into())
}

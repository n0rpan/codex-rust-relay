mod catalog;
mod provider;

use std::error::Error;
use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use catalog::{BundledModels, ProviderModels};
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use provider::Provider;
use reqwest::Client;
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::process::Command;

const TOKEN_ENV: &str = "CODEX_PROVIDER_RELAY_TOKEN";
type RelayError = Box<dyn Error + Send + Sync>;
type RelayBody = UnsyncBoxBody<Bytes, RelayError>;

struct Relay {
    provider: Provider,
    client: Client,
    bundled: BundledModels,
    local_auth: HeaderValue,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match launch(std::env::args_os().skip(1)).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("codex-provider-relay: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn launch(args: impl Iterator<Item = OsString>) -> Result<u8, RelayError> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .ok_or("set CODEX_HOME or HOME")?;
    let provider = Provider::load(&codex_home)?;
    let bundled = Command::new("codex")
        .env("CODEX_HOME", &codex_home)
        .env_remove(&provider.env_key)
        .args(["debug", "models", "--bundled"])
        .output()
        .await?;
    if !bundled.status.success() {
        return Err("could not read the bundled Codex model catalog".into());
    }
    let bundled = serde_json::from_slice(&bundled.stdout)?;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let base_url = format!("http://{}/v1", listener.local_addr()?);
    let mut random_bytes = [0; 32];
    getrandom::fill(&mut random_bytes).map_err(|error| error.to_string())?;
    let token = hex::encode(random_bytes);
    let provider_prefix = format!("model_providers.{}", provider.id);
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()?;

    let mut command = Command::new("codex");
    command
        .env_remove(&provider.env_key)
        .env("CODEX_HOME", codex_home)
        .env(TOKEN_ENV, &token)
        .arg("-c")
        .arg(format!("{provider_prefix}.base_url={}", json!(base_url)))
        .arg("-c")
        .arg(format!(
            "{provider_prefix}.model_catalog_url={}",
            json!(format!("{base_url}/models"))
        ))
        .arg("-c")
        .arg(format!("{provider_prefix}.env_key={}", json!(TOKEN_ENV)))
        .args(args)
        .kill_on_drop(true);
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

    let relay = Arc::new(Relay {
        provider,
        client,
        bundled,
        local_auth: bearer(&token)?,
    });
    let mut child = command.spawn()?;
    tokio::select! {
        status = child.wait() => Ok(status?.code().unwrap_or(1) as u8),
        result = serve(listener, relay) => Err(result.unwrap_err().into()),
    }
}

async fn serve(listener: TcpListener, relay: Arc<Relay>) -> std::io::Result<()> {
    loop {
        let (socket, _) = listener.accept().await?;
        let relay = Arc::clone(&relay);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let relay = Arc::clone(&relay);
                async move { Ok::<_, std::convert::Infallible>(relay.handle(request).await) }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(socket), service)
                .await;
        });
    }
}

impl Relay {
    async fn handle(&self, request: Request<Incoming>) -> Response<RelayBody> {
        let authorized = request
            .headers()
            .get(AUTHORIZATION)
            .is_some_and(|auth| auth.as_bytes().ct_eq(self.local_auth.as_bytes()).into());
        if !authorized {
            return error_response(StatusCode::UNAUTHORIZED, "Relay authentication failed");
        }

        match (request.method(), request.uri().path()) {
            (&Method::GET, "/v1/models") => self.models(request.into_parts().0.headers).await,
            (&Method::POST, "/v1/responses" | "/v1/responses/compact") => {
                self.inference(request).await
            }
            _ => error_response(StatusCode::NOT_FOUND, "Unknown relay endpoint"),
        }
    }

    async fn models(&self, headers: HeaderMap) -> Response<RelayBody> {
        let upstream = self
            .client
            .get(self.provider.models_url.clone())
            .headers(forward_headers(headers))
            .header(AUTHORIZATION, self.provider.upstream_auth.clone())
            .timeout(Duration::from_secs(20))
            .send()
            .await;
        match upstream {
            Ok(response) if response.status().is_success() => {
                if let Ok(models) = response.json::<ProviderModels>().await
                    && let Some(catalog) = models.codex_catalog(&self.provider.name, &self.bundled)
                {
                    return json_response(StatusCode::OK, catalog);
                }
                error_response(StatusCode::BAD_GATEWAY, "Invalid upstream model catalog")
            }
            Ok(response) => stream_response(response),
            Err(_) => error_response(StatusCode::BAD_GATEWAY, "Model catalog request failed"),
        }
    }

    async fn inference(&self, request: Request<Incoming>) -> Response<RelayBody> {
        let (parts, body) = request.into_parts();
        let path = parts.uri.path().strip_prefix("/v1").unwrap();
        let url = self.provider.responses_url(path, parts.uri.query());
        let upstream = self
            .client
            .post(url)
            .headers(forward_headers(parts.headers))
            .header(AUTHORIZATION, self.provider.upstream_auth.clone())
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send()
            .await;
        match upstream {
            Ok(response) => stream_response(response),
            Err(_) => error_response(StatusCode::BAD_GATEWAY, "Inference request failed"),
        }
    }
}

fn bearer(token: &str) -> Result<HeaderValue, RelayError> {
    Ok(HeaderValue::from_str(&format!("Bearer {token}"))?)
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
    ] {
        headers.remove(name);
    }
    headers
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

fn json_response(status: StatusCode, value: serde_json::Value) -> Response<RelayBody> {
    let mut response = Response::new(
        Full::new(Bytes::from(value.to_string()))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

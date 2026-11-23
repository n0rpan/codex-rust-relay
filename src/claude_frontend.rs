//! Claude Code's local Messages interface over the shared provider registry.

use std::sync::Arc;

use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::provider::Resolution;
use crate::{
    App, Mode, RelayBody, bytes_response, claude_gateway, claude_search, json_response,
    read_bounded, resolve_model, upstream,
};

impl App {
    pub(super) fn claude_aliases(&self) -> Vec<String> {
        let Mode::Codeox {
            vendors, routes, ..
        } = &self.mode
        else {
            return Vec::new();
        };
        let (mut configured, missing): (Vec<_>, Vec<_>) = self
            .visible_models()
            .filter_map(|entry| entry["slug"].as_str())
            .partition(|alias| {
                matches!(
                    routes.resolve(alias),
                    Resolution::Route(route) if vendors[route.vendor].auth.is_some()
                )
            });
        configured.extend(missing);
        configured.into_iter().map(str::to_owned).collect()
    }

    pub(super) fn client_classifier(
        &self,
        request: &str,
    ) -> Result<Option<String>, crate::provider::RelayError> {
        use crate::provider::Wire;
        let Mode::Codeox {
            vendors, routes, ..
        } = &self.mode
        else {
            return Ok(None);
        };
        if request != "auto" {
            if let Ok((route, _, _)) = resolve_model(vendors, routes, request)
                && route.wire == Wire::AnthropicMessages
            {
                return Ok(Some(request.to_owned()));
            }
            return Err("--client-classifier needs a model alias on an available Anthropic Messages provider".into());
        }
        let model_preference = ["claude-sonnet-5", "claude-sonnet-4-6"];
        Ok(self
            .visible_models()
            .filter_map(|entry| {
                let alias = entry["slug"].as_str()?;
                let (route, state, _) = resolve_model(vendors, routes, alias).ok()?;
                if !state.vendor.is_direct_anthropic() {
                    return None;
                }
                let priority = model_preference
                    .iter()
                    .position(|model| *model == route.upstream_model)?;
                Some((priority, alias))
            })
            .min_by_key(|(priority, _)| *priority)
            .map(|(_, alias)| alias.to_owned()))
    }

    pub(super) fn claude_model_info(&self, model: &str) -> Option<crate::clients::ClaudeModelInfo> {
        let Mode::Codeox {
            vendors, routes, ..
        } = &self.mode
        else {
            return None;
        };
        let (target, state, _) = resolve_model(vendors, routes, model).ok()?;
        let context_window = self.visible_models().find_map(|entry| {
            let window = entry["context_window"].as_u64()?;
            let Resolution::Route(route) = routes.resolve(entry["slug"].as_str()?) else {
                return None;
            };
            (route.vendor == target.vendor && route.upstream_model == target.upstream_model)
                .then_some(window)
        });
        Some(crate::clients::ClaudeModelInfo {
            can_request_server_review: state.vendor.is_direct_anthropic(),
            context_window,
        })
    }

    pub(super) async fn handle_messages_client(
        &self,
        request: Request<Incoming>,
        bearer_matches: bool,
    ) -> Response<RelayBody> {
        if request.method() == Method::HEAD && request.uri().path() == "/api/hello" {
            return bytes_response(StatusCode::NO_CONTENT, Bytes::new(), "text/plain");
        }
        let key_matches = request.headers().get("x-api-key").is_some_and(|key| {
            self.local_auth
                .as_bytes()
                .strip_prefix(b"Bearer ")
                .is_some_and(|token| key.as_bytes().ct_eq(token).into())
        });
        if !bearer_matches && !key_matches {
            return messages_error(StatusCode::UNAUTHORIZED, "Relay authentication failed");
        }
        match (request.method(), request.uri().path()) {
            (&Method::GET, "/v1/models") => {
                let data: Vec<_> = self
                    .visible_models()
                    .map(|entry| {
                        let mut model = json!({
                            "id": entry["slug"],
                            "display_name": entry["display_name"],
                            "description": entry["description"],
                            "type": "model",
                            "created_at": "1970-01-01T00:00:00Z",
                        });
                        if let Some(window) = entry["context_window"].as_u64() {
                            model["max_input_tokens"] = json!(window);
                        }
                        model
                    })
                    .collect();
                json_response(
                    StatusCode::OK,
                    json!({
                        "first_id": data.first().map(|entry| &entry["id"]),
                        "last_id": data.last().map(|entry| &entry["id"]),
                        "data": data,
                        "has_more": false,
                    }),
                )
            }
            (&Method::POST, "/v1/messages" | "/v1/messages/count_tokens") => {
                self.messages_inference(request).await
            }
            _ => messages_error(StatusCode::NOT_FOUND, "Unknown relay endpoint"),
        }
    }

    async fn messages_inference(&self, request: Request<Incoming>) -> Response<RelayBody> {
        let Mode::Codeox {
            vendors, routes, ..
        } = &self.mode
        else {
            return messages_error(
                StatusCode::BAD_REQUEST,
                "Claude Code needs a shared provider configuration",
            );
        };
        let (parts, body) = request.into_parts();
        let count_tokens = parts.uri.path() == "/v1/messages/count_tokens";
        let bytes = match read_bounded(body, self.max_request_bytes).await {
            Ok(bytes) => bytes,
            Err(response) => {
                return messages_error(response.status(), "Unable to read the Messages request");
            }
        };
        let payload: Value = match serde_json::from_slice(&bytes) {
            Ok(payload) => payload,
            Err(_) => return messages_error(StatusCode::BAD_REQUEST, "Request body must be JSON"),
        };
        drop(bytes);
        let Some(requested) = payload
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return messages_error(StatusCode::BAD_REQUEST, "Request body is missing a model");
        };
        let (route, state, auth) = match resolve_model(vendors, routes, &requested) {
            Ok(target) => target,
            Err((status, message)) => return messages_error(status, &message),
        };
        if !count_tokens
            && let Some(result) = claude_search::try_handle(&payload, self.search.as_ref()).await
        {
            let reply = claude_search::reply_or_error(result);
            let status =
                StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            return bytes_response(status, Bytes::from(reply.body), reply.content_type);
        }
        let context = upstream::Translation {
            vendor: &state.vendor,
            key: auth,
            incoming: Some(&parts.headers),
            upstream_model: &route.upstream_model,
            requested_model: &requested,
            secrets: Arc::clone(&self.secrets),
        };
        let beta = parts
            .uri
            .query()
            .is_some_and(|query| query.split('&').any(|item| item == "beta=true"));
        if count_tokens {
            claude_gateway::count_tokens(&self.client, context, payload, beta).await
        } else {
            claude_gateway::infer(&self.client, context, payload, beta).await
        }
    }
}

fn messages_error(status: StatusCode, message: &str) -> Response<RelayBody> {
    let kind = match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::NOT_FOUND => "not_found_error",
        StatusCode::PAYLOAD_TOO_LARGE => "request_too_large",
        StatusCode::BAD_REQUEST => "invalid_request_error",
        _ => "api_error",
    };
    json_response(
        status,
        json!({
            "type": "error",
            "error": { "type": kind, "message": message },
        }),
    )
}

//! Launch-scoped MCP tools for sending a task to an API model from a
//! subscription session. The stock client retains its own inference and tools.

mod jobs;

use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, ORIGIN};
use hyper::{Method, Request, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{OnceCell, Semaphore};

use crate::clients::ClientKind;
use crate::provider::{CodeoxConfig, RelayError, SavedKeys};
use crate::{App, Mode, RelayBody};

const TOKEN_ENV: &str = "OX_WORKER_SESSION";
const SERVER: &str = "ox_workers";
const MAX_CALL_BYTES: usize = 128 * 1024;
const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;
const MAX_ANSWER_BYTES: usize = 64 * 1024;
const PAGE_SIZE: usize = 40;
const WORKER_TIMEOUT: Duration = Duration::from_secs(90);
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];

pub(crate) struct Relay {
    listener: TcpListener,
    app: Option<App>,
    token: String,
}

impl Relay {
    pub(crate) fn new(listener: TcpListener, app: App, token: String) -> Self {
        Self {
            listener,
            app: Some(app),
            token,
        }
    }

    pub(crate) async fn load(home: &Path) -> Result<Option<Self>, RelayError> {
        let path = CodeoxConfig::path(home);
        if !path.try_exists()? {
            return Ok(None);
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        Ok(Some(Self {
            listener,
            app: None,
            token: crate::random_token()?,
        }))
    }

    pub(crate) async fn launch(
        self,
        mut command: Command,
        args: &[OsString],
        home: &Path,
        client: ClientKind,
    ) -> Result<u8, RelayError> {
        let origin = format!("http://{}", self.listener.local_addr()?);
        let url = format!("{origin}/mcp");
        command.env(TOKEN_ENV, &self.token);
        match client {
            ClientKind::Codex => {
                for (field, value) in [
                    ("url", json!(url)),
                    ("bearer_token_env_var", json!(TOKEN_ENV)),
                    ("tool_timeout_sec", json!(120)),
                ] {
                    command
                        .arg("-c")
                        .arg(format!("mcp_servers.{SERVER}.{field}={value}"));
                }
            }
            ClientKind::Claude => {
                let config = json!({
                    "mcpServers": {
                        (SERVER): {
                            "type": "http",
                            "url": url,
                            "headers": {"Authorization": format!("Bearer ${{{TOKEN_ENV}}}")},
                        }
                    }
                });
                command.arg(format!("--mcp-config={config}"));
            }
        }
        command.args(args);
        crate::exempt_loopback_proxy(&mut command);
        let service = Arc::new(Service {
            app: OnceCell::new_with(self.app.map(|app| Ok(prepare_app(app, home, &self.token)))),
            local_auth: crate::provider::bearer(&self.token)?,
            token: self.token,
            origin,
            home: home.to_owned(),
            cwd: project_directory(args, client)?,
            client,
            slots: Semaphore::new(4),
            jobs: Mutex::new(jobs::Jobs::default()),
        });
        eprintln!(
            "ox: API workers enabled. Ask for the ox model list or name an API model to delegate to."
        );
        tokio::select! {
            result = crate::native::launch(command) => result,
            result = crate::serve_requests(self.listener, move |request| {
                let service = Arc::clone(&service);
                async move { service.handle(request).await }
            }) => Err(result.unwrap_err().into()),
        }
    }
}

fn project_directory(args: &[OsString], client: ClientKind) -> Result<PathBuf, RelayError> {
    let cwd = std::env::current_dir()?;
    if !matches!(client, ClientKind::Codex) {
        return Ok(cwd);
    }
    let mut selected = None;
    let mut args = args.iter().take_while(|arg| *arg != "--");
    while let Some(arg) = args.next() {
        if arg == "-C" || arg == "--cd" {
            selected = args.next().map(PathBuf::from);
        } else if let Some(path) = arg.to_str().and_then(|arg| arg.strip_prefix("--cd=")) {
            selected = Some(PathBuf::from(path));
        }
    }
    match selected {
        Some(path) => Ok(cwd.join(path).canonicalize()?),
        None => Ok(cwd),
    }
}

struct Service {
    app: OnceCell<Result<App, String>>,
    local_auth: HeaderValue,
    token: String,
    origin: String,
    home: PathBuf,
    cwd: PathBuf,
    client: ClientKind,
    slots: Semaphore,
    jobs: Mutex<jobs::Jobs>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListModels {
    #[serde(default)]
    query: String,
    #[serde(default)]
    offset: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delegate {
    model: String,
    task: String,
}

impl Service {
    async fn app(&self) -> Result<&App, String> {
        self.app
            .get_or_init(|| async {
                let result = async {
                    let config = CodeoxConfig::load(&CodeoxConfig::path(&self.home))?;
                    let keys = SavedKeys::for_config(&self.home, &config)?;
                    App::codeox(
                        config,
                        Default::default(),
                        &self.token,
                        crate::upstream_client()?,
                        MAX_CALL_BYTES,
                        &|name| keys.get(name),
                        None,
                    )
                    .await
                }
                .await;
                result
                    .map(|app| prepare_app(app, &self.home, &self.token))
                    .map_err(|_| {
                        "ox could not load the API provider registry. Run ox doctor.".to_owned()
                    })
            })
            .await
            .as_ref()
            .map_err(Clone::clone)
    }

    async fn handle(&self, request: Request<Incoming>) -> Response<RelayBody> {
        if request.uri().path() != "/mcp" {
            return crate::error_response(StatusCode::NOT_FOUND, "Unknown worker endpoint");
        }
        if request
            .headers()
            .get_all(ORIGIN)
            .iter()
            .any(|origin| origin != self.origin.as_str())
        {
            return crate::error_response(StatusCode::FORBIDDEN, "Worker origin rejected");
        }
        if !request
            .headers()
            .get(AUTHORIZATION)
            .is_some_and(|auth| auth.as_bytes().ct_eq(self.local_auth.as_bytes()).into())
        {
            return crate::error_response(StatusCode::UNAUTHORIZED, "Worker authentication failed");
        }
        if request.method() != Method::POST {
            let mut response = crate::bytes_response(
                StatusCode::METHOD_NOT_ALLOWED,
                Bytes::new(),
                "application/json",
            );
            response
                .headers_mut()
                .insert(hyper::header::ALLOW, HeaderValue::from_static("POST"));
            return response;
        }
        if request
            .headers()
            .get("mcp-protocol-version")
            .is_some_and(|version| {
                !version
                    .to_str()
                    .is_ok_and(|version| PROTOCOL_VERSIONS.contains(&version))
            })
        {
            return crate::error_response(
                StatusCode::BAD_REQUEST,
                "Unsupported MCP protocol version",
            );
        }
        let bytes = match crate::read_bounded(request.into_body(), MAX_CALL_BYTES).await {
            Ok(bytes) => bytes,
            Err(response) => return response,
        };
        let Ok(call) = serde_json::from_slice::<Value>(&bytes) else {
            return rpc_error(Value::Null, -32700, "Invalid JSON");
        };
        let id = &call["id"];
        let Some(method) = call["method"].as_str().filter(|_| call["jsonrpc"] == "2.0") else {
            return rpc_error(id.clone(), -32600, "Invalid JSON-RPC request");
        };
        if call.get("id").is_none() {
            return crate::bytes_response(StatusCode::ACCEPTED, Bytes::new(), "application/json");
        }
        let params = &call["params"];
        let result = match method {
            "initialize" => initialize(params),
            "ping" => json!({}),
            "tools/list" => tools(),
            "tools/call" => match params["name"].as_str() {
                Some("list_ox_models") => match serde_json::from_value(
                    params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                ) {
                    Ok(filters) => tool_result(self.list_models(filters).await),
                    Err(_) => return rpc_error(id.clone(), -32602, "Invalid model list filters"),
                },
                Some("delegate_to_ox_model") => {
                    match serde_json::from_value(params["arguments"].clone()) {
                        Ok(task) => tool_result(self.delegate(task).await),
                        Err(_) => {
                            return rpc_error(
                                id.clone(),
                                -32602,
                                "Provide a model alias and a task",
                            );
                        }
                    }
                }
                Some("start_ox_worker") => {
                    match serde_json::from_value(params["arguments"].clone()) {
                        Ok(request) => tool_result(self.start_job(request).await),
                        Err(_) => {
                            return rpc_error(
                                id.clone(),
                                -32602,
                                "Specify a runtime, mode, and task",
                            );
                        }
                    }
                }
                Some(tool @ ("ox_worker_status" | "cancel_ox_worker")) => {
                    match serde_json::from_value(params["arguments"].clone()) {
                        Ok(job) => {
                            let mut jobs = self.jobs.lock().unwrap();
                            let result = if tool == "cancel_ox_worker" {
                                jobs.cancel(job)
                            } else {
                                jobs.status(job)
                            };
                            tool_result(result)
                        }
                        Err(_) => {
                            return rpc_error(id.clone(), -32602, "Provide the worker job ID");
                        }
                    }
                }
                _ => return rpc_error(id.clone(), -32602, "Unknown worker tool"),
            },
            _ => return rpc_error(id.clone(), -32601, "Method not found"),
        };
        crate::json_response(
            StatusCode::OK,
            json!({"jsonrpc": "2.0", "id": id, "result": result}),
        )
    }

    async fn start_job(&self, request: jobs::Start) -> Result<Value, String> {
        let app = self.app().await?;
        if let Some(model) = &request.model
            && let Mode::Codeox {
                vendors, routes, ..
            } = &app.mode
        {
            crate::resolve_model(vendors, routes, model)
                .map_err(|(_, error)| crate::upstream::redact(&error, &app.secrets).into_owned())?;
        }
        self.jobs
            .lock()
            .unwrap()
            .start(request, &self.home, &self.cwd, Arc::clone(&app.secrets))
    }

    async fn list_models(&self, filters: ListModels) -> Result<Value, String> {
        let query = filters.query.trim().to_lowercase();
        let mut total = 0;
        let mut models = Vec::new();
        for (alias, provider) in self.app().await?.launch_models() {
            if !alias.to_lowercase().contains(&query) && !provider.to_lowercase().contains(&query) {
                continue;
            }
            if total >= filters.offset && models.len() < PAGE_SIZE {
                models.push(json!({"model": alias, "provider": provider, "auth": "api_key"}));
            }
            total += 1;
        }
        let next = filters.offset.saturating_add(models.len());
        let subscriptions: Vec<_> = [
            (ClientKind::Codex, "codex", "openai", "ChatGPT subscription", false),
            (ClientKind::Claude, "claude", "anthropic", "Claude subscription", true),
        ]
        .into_iter()
        .filter_map(|(client, runtime, provider, name, worker)| {
            let account = crate::native::saved_subscription(client, &self.home)?;
            if !format!("{name} {runtime} {provider}").to_lowercase().contains(&query) {
                return None;
            }
            Some(json!({
                "name": name,
                "provider": provider,
                "auth": "subscription",
                "account": account,
                "runtime": runtime,
                "main_client": client == self.client,
                "worker_available": worker,
                "model_picker": "Use /model in the stock subscription client to see account model choices",
            }))
        })
        .collect();
        Ok(json!({
            "models": models,
            "subscriptions": subscriptions,
            "total": total,
            "next_offset": (next < total).then_some(next),
        }))
    }

    async fn delegate(&self, input: Delegate) -> Result<Value, String> {
        let model = input.model.trim();
        let task = input.task.trim();
        if model.is_empty() || model.len() > 256 || task.is_empty() || task.len() > 96 * 1024 {
            return Err(
                "Supply a model alias of up to 256 bytes and a task of up to 96 KiB.".into(),
            );
        }
        let _slot = self
            .slots
            .try_acquire()
            .map_err(|_| "All ox workers are busy. Wait for one to finish.")?;
        let app = self.app().await?;
        let task = crate::upstream::redact(task, &app.secrets);
        let payload = json!({
            "model": model,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": task}]}],
            "stream": true,
            "store": false,
            "max_output_tokens": 2000,
        });
        let request = Request::post("/v1/responses")
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(payload.to_string())))
            .expect("static worker request");
        let text = tokio::time::timeout(WORKER_TIMEOUT, async {
            let reply = app.inference(request).await;
            if !reply.status().is_success() {
                return Err(format!("ox model request returned HTTP {}. Check the alias with list_ox_models or run ox doctor.", reply.status().as_u16()));
            }
            worker_text(reply.into_body()).await
        })
        .await
        .map_err(|_| "The ox worker timed out.")??;
        let text = crate::upstream::redact(&text, &app.secrets);
        Ok(json!({"model": model, "answer": text}))
    }
}

fn prepare_app(mut app: App, home: &Path, token: &str) -> App {
    let favorites = crate::picker::read_favorites(home).unwrap_or_default();
    if let Mode::Codeox { catalog, .. } = &mut app.mode {
        crate::catalog::prioritize_favorites(catalog, &favorites);
    }
    Arc::make_mut(&mut app.secrets).push(token.to_owned());
    app
}

fn initialize(params: &Value) -> Value {
    let version = params["protocolVersion"]
        .as_str()
        .filter(|version| PROTOCOL_VERSIONS.contains(version))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "serverInfo": {"name": SERVER, "version": env!("CARGO_PKG_VERSION")},
        "capabilities": {"tools": {}},
        "instructions": "Use start_ox_worker for long coding tasks, Codex goals or Claude workflows, then check ox_worker_status. Use delegate_to_ox_model for one short text answer. Start jobs only at the user's request and only enable workspace writes when requested. The named provider bills the delegated work.",
    })
}

fn tools() -> Value {
    json!({
        "tools": [
            {
                "name": "list_ox_models",
                "description": "List available ox API models and detected native subscription routes, labelled by provider. Filter by model or provider. Use an API alias when delegating to an API model.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "offset": {"type": "integer", "minimum": 0}
                    },
                    "additionalProperties": false
                },
                "annotations": {"readOnlyHint": true, "openWorldHint": false},
            },
            {
                "name": "delegate_to_ox_model",
                "description": "Send an independent text task to the named API model. Include the necessary context in task. This sends that text to the chosen provider, which can bill the request. The worker has no files or shell access.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "model": {"type": "string", "description": "Exact ox model alias returned by list_ox_models."},
                        "task": {"type": "string", "description": "Task and only the context the chosen provider needs."}
                    },
                    "required": ["model", "task"],
                    "additionalProperties": false
                },
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": true},
            },
            {
                "name": "start_ox_worker",
                "description": "Start a background stock-client coding session in this project. Use mode goal for Codex Goal mode or workflow for a Claude Code workflow. Returns a job ID immediately. The worker can read project files. Request workspace_write only when the user asked the worker to edit this project.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "runtime": {"enum": ["codex", "claude"]},
                        "mode": {"enum": ["session", "goal", "workflow"]},
                        "model": {"type": "string", "description": "Exact ox API model alias. Leave out only for a Claude subscription worker."},
                        "task": {"type": "string", "description": "The job objective, desired validation, and stopping condition."},
                        "workspace_write": {"type": "boolean", "default": false},
                    },
                    "required": ["runtime", "mode", "task"],
                    "additionalProperties": false,
                },
                "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true},
            },
            {
                "name": "ox_worker_status",
                "description": "Get a worker's running or terminal status, native session ID, progress, and result. Jobs keep running between tool calls.",
                "inputSchema": {"type":"object","properties":{"job_id":{"type":"integer","minimum":1}},"required":["job_id"],"additionalProperties":false},
                "annotations": {"readOnlyHint":true,"openWorldHint":false},
            },
            {
                "name": "cancel_ox_worker",
                "description": "Stop a running worker in this ox launch. Work already written by the worker stays in the project.",
                "inputSchema": {"type":"object","properties":{"job_id":{"type":"integer","minimum":1}},"required":["job_id"],"additionalProperties":false},
                "annotations": {"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":false},
            },
        ]
    })
}

fn tool_result(result: Result<Value, String>) -> Value {
    match result {
        Ok(result) => json!({
            "content": [{"type": "text", "text": result.to_string()}],
            "structuredContent": result,
        }),
        Err(message) => json!({
            "isError": true,
            "content": [{"type":"text", "text": message}],
        }),
    }
}

fn rpc_error(id: Value, code: i32, message: &str) -> Response<RelayBody> {
    crate::json_response(
        StatusCode::OK,
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    )
}

async fn worker_text(mut body: RelayBody) -> Result<String, String> {
    let mut framer = crate::sse::SseFramer::default();
    let mut text = String::new();
    let mut bytes = 0;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| "The provider stream ended unexpectedly.")?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        bytes += data.len();
        if bytes > MAX_REPLY_BYTES {
            return Err("The provider response exceeded the worker limit.".into());
        }
        for event in framer.feed(&data)? {
            if event.data == "[DONE]" {
                continue;
            }
            let value: Value = serde_json::from_str(&event.data)
                .map_err(|_| "The provider sent an invalid stream event.")?;
            match value["type"].as_str().or(event.event.as_deref()) {
                Some("response.output_text.delta" | "response.refusal.delta") => {
                    if let Some(delta) = value["delta"].as_str() {
                        if text.len() + delta.len() > MAX_ANSWER_BYTES {
                            return Err("The provider answer exceeded the worker limit.".into());
                        }
                        text.push_str(delta);
                    }
                }
                Some("response.completed") if !text.is_empty() => return Ok(text),
                Some("response.completed") => {
                    let text = value["response"]["output"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .flat_map(|item| item["content"].as_array().into_iter().flatten())
                        .filter_map(|content| {
                            content["text"]
                                .as_str()
                                .or_else(|| content["refusal"].as_str())
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    return if text.len() > MAX_ANSWER_BYTES {
                        Err("The provider answer exceeded the worker limit.".into())
                    } else if text.is_empty() {
                        Err("The provider returned no text.".into())
                    } else {
                        Ok(text)
                    };
                }
                Some("response.failed" | "response.incomplete" | "error") => {
                    return Err("The provider did not complete the worker request.".into());
                }
                _ => {}
            }
        }
    }
    Err("The provider stream ended before completing the worker request.".into())
}

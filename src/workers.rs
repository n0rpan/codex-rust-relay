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
        if crate::native::title_generation() {
            return Ok(None);
        }
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
        let (mcp, gateway) = self.attach(args, home, client, Tools::All)?;
        command.env(TOKEN_ENV, &gateway.token);
        command.args(mcp);
        command.args(args);
        crate::exempt_loopback_proxy(&mut command);
        eprintln!(
            "ox: API workers enabled. Ask for the ox model list or name an API model to delegate to."
        );
        tokio::select! {
            result = crate::native::launch(command) => result,
            result = gateway.serve() => Err(result),
        }
    }

    /// Prepare this gateway for a client launch: the stock client's MCP
    /// arguments, and the gateway to serve while the client runs. The client
    /// also needs `TOKEN_ENV` set to [`Gateway::token`].
    pub(crate) fn attach(
        self,
        args: &[OsString],
        home: &Path,
        client: ClientKind,
        tools: Tools,
    ) -> Result<(Vec<OsString>, Gateway), RelayError> {
        let origin = format!("http://{}", self.listener.local_addr()?);
        let url = format!("{origin}/mcp");
        let mcp = match client {
            ClientKind::Codex => [
                ("url", json!(url)),
                ("bearer_token_env_var", json!(TOKEN_ENV)),
                ("tool_timeout_sec", json!(120)),
            ]
            .into_iter()
            .flat_map(|(field, value)| {
                [
                    OsString::from("-c"),
                    OsString::from(format!("mcp_servers.{SERVER}.{field}={value}")),
                ]
            })
            .collect(),
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
                vec![OsString::from(format!("--mcp-config={config}"))]
            }
        };
        let token = self.token.clone();
        let service = Arc::new(Service {
            app: OnceCell::new_with(self.app.map(|app| Ok(prepare_app(app, home, &self.token)))),
            local_auth: crate::provider::bearer(&self.token)?,
            token: self.token,
            origin,
            home: home.to_owned(),
            cwd: project_directory(args, client)?,
            session: session_hint(args, client),
            observed: Mutex::new(None),
            unbound: std::sync::atomic::AtomicBool::new(false),
            host_policy: CodeoxConfig::load(&CodeoxConfig::path(home))
                .map(|config| config.workers_host)
                .unwrap_or_default(),
            // Private to this launch. The host stores only a digest of it.
            launch: crate::random_token()?,
            client,
            slots: Semaphore::new(4),
            jobs: Mutex::new(jobs::Jobs::default()),
            tools,
        });
        Ok((
            mcp,
            Gateway {
                listener: self.listener,
                service,
                token,
            },
        ))
    }
}

/// Which tools a gateway offers. An API session the session host runs gets only
/// read_ox_thread, so a handoff target on an API alias can read its source.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tools {
    All,
    HandoffRead,
}

pub(crate) struct Gateway {
    listener: TcpListener,
    service: Arc<Service>,
    pub(crate) token: String,
}

impl Gateway {
    /// The environment variable and value the client needs for this gateway.
    pub(crate) fn env(&self) -> (&'static str, &str) {
        (TOKEN_ENV, &self.token)
    }
    pub(crate) async fn serve(self) -> RelayError {
        let service = self.service;
        crate::serve_requests(self.listener, move |request| {
            let service = Arc::clone(&service);
            async move { service.handle(request).await }
        })
        .await
        .unwrap_err()
        .into()
    }
}

fn valid_session(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The native session a terminal launch resumes, from its own arguments. The host
/// uses it only to let a resumed parent see the workers it started earlier.
fn session_hint(args: &[OsString], client: ClientKind) -> Option<String> {
    let args: Vec<&str> = args
        .iter()
        .take_while(|arg| *arg != "--")
        .filter_map(|a| a.to_str())
        .collect();
    let valid = |id: &&str| {
        !id.is_empty()
            && id.len() <= 128
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    match client {
        ClientKind::Claude => args
            .windows(2)
            .find_map(|w| {
                matches!(w[0], "--resume" | "-r" | "--session-id")
                    .then_some(w[1])
                    .filter(valid)
                    .map(str::to_owned)
            })
            .or_else(|| {
                args.iter().find_map(|a| {
                    a.strip_prefix("--resume=")
                        .or_else(|| a.strip_prefix("--session-id="))
                        .filter(valid)
                        .map(str::to_owned)
                })
            }),
        ClientKind::Codex => args
            .windows(2)
            .find(|w| w[0] == "resume" && valid(&w[1]))
            .map(|w| w[1].to_owned()),
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
    session: Option<String>,
    /// The native session first seen in a stock client's MCP request.
    observed: Mutex<Option<String>>,
    /// A host-owned worker was launched before this launch knew its session.
    unbound: std::sync::atomic::AtomicBool,
    host_policy: crate::provider::WorkersHost,
    launch: String,
    client: ClientKind,
    slots: Semaphore,
    jobs: Mutex<jobs::Jobs>,
    tools: Tools,
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
struct ReadThread {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    item: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
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
        // Stock Claude Code may name its session on outgoing requests. Treat it
        // only as an observation of this launch's own session.
        let header_session = request
            .headers()
            .get("x-claude-code-session-id")
            .and_then(|v| v.to_str().ok())
            .filter(|s| valid_session(s))
            .map(str::to_owned);
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
        // Stock Codex 0.171.0 and later put the calling thread in every tools/call
        // `_meta.threadId`. It identifies the parent exactly on a shared app-server.
        let thread = params["_meta"]["threadId"]
            .as_str()
            .filter(|s| valid_session(s))
            .map(str::to_owned)
            .or(header_session);
        if method == "tools/call" {
            self.observe(thread.as_deref()).await;
        }
        let thread = thread.as_deref();
        let result = match method {
            "initialize" => initialize(params),
            "ping" => json!({}),
            "tools/list" if self.tools == Tools::HandoffRead => {
                let mut all = tools();
                if let Some(list) = all["tools"].as_array_mut() {
                    list.retain(|t| t["name"] == "read_ox_thread");
                }
                all
            }
            "tools/list" => tools(),
            "tools/call"
                if self.tools == Tools::HandoffRead
                    && params["name"].as_str() != Some("read_ox_thread") =>
            {
                return rpc_error(id.clone(), -32602, "Unknown worker tool");
            }
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
                        Ok(request) => tool_result(self.start_job(request, thread).await),
                        Err(_) => {
                            return rpc_error(
                                id.clone(),
                                -32602,
                                "Specify a runtime, mode, and task",
                            );
                        }
                    }
                }
                Some("list_ox_workers") => tool_result(self.list_jobs(thread).await),
                Some("read_ox_thread") => {
                    match serde_json::from_value::<ReadThread>(
                        params
                            .get("arguments")
                            .cloned()
                            .unwrap_or_else(|| json!({})),
                    ) {
                        Ok(read) => tool_result(self.read_thread(read, thread).await),
                        Err(_) => {
                            return rpc_error(
                                id.clone(),
                                -32602,
                                "Use session, item, after and limit to read a handoff source",
                            );
                        }
                    }
                }
                Some("steer_ox_worker") => {
                    let result = async {
                        let request: jobs::Steer =
                            serde_json::from_value(params["arguments"].clone())
                                .map_err(|_| "Provide a worker job ID and message.".to_string())?;
                        let app = self.app().await.map_err(|e| e.to_string())?;
                        if !self.jobs.lock().unwrap().has(request.job_id) {
                            let text = crate::upstream::redact(&request.text, &app.secrets);
                            return self
                                .host_call(
                                    json!({"op":"steer","job_id":request.job_id,"text":text}),
                                    thread,
                                )
                                .await;
                        }
                        let received = self.jobs.lock().unwrap().steer(request, &app.secrets)?;
                        tokio::time::timeout(Duration::from_secs(30), received)
                            .await
                            .map_err(|_| "Timed out waiting for the worker's acknowledgement.")?
                            .map_err(
                                |_| "The worker finished before acknowledging the instruction.",
                            )?
                    }
                    .await;
                    tool_result(result)
                }
                Some(tool @ ("ox_worker_status" | "cancel_ox_worker")) => {
                    match serde_json::from_value(params["arguments"].clone()) {
                        Ok(job) => {
                            let job: jobs::JobId = job;
                            let local = self.jobs.lock().unwrap().has(job.job_id);
                            let result = if !local {
                                let op = if tool == "cancel_ox_worker" {
                                    "cancel"
                                } else {
                                    "status"
                                };
                                self.host_call(json!({"op":op,"job_id":job.job_id}), thread)
                                    .await
                            } else {
                                let mut jobs = self.jobs.lock().unwrap();
                                if tool == "cancel_ox_worker" {
                                    jobs.cancel(job)
                                } else {
                                    jobs.status(job)
                                }
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

    async fn start_job(&self, request: jobs::Start, thread: Option<&str>) -> Result<Value, String> {
        request.validate()?;
        let app = self.app().await?;
        if let Some(model) = &request.model
            && let Mode::Codeox {
                vendors, routes, ..
            } = &app.mode
        {
            crate::resolve_model(vendors, routes, model)
                .map_err(|(_, error)| crate::upstream::redact(&error, &app.secrets).into_owned())?;
        }
        // A running session host owns the worker, so it outlives this helper and
        // its approvals reach the human clients. Without one, run it here as before.
        if let Some(host) = crate::host_lifetime::HostLink::find(&self.cwd, self.host_policy)? {
            let start = request.host_start(&app.secrets);
            let caller = self.caller(thread);
            let unbound = caller["session"].is_null();
            let reply = host
                .send(
                    self.launch.clone(),
                    caller,
                    thread.map(str::to_owned),
                    Some(start),
                    None,
                )
                .await
                .map_err(|e| crate::upstream::redact(&e, &app.secrets).into_owned())?;
            if unbound {
                self.unbound
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            let mut reply = redact_value(reply, &app.secrets);
            reply["approvals"] = json!(
                "This worker is owned by the OX session host. Its approvals and questions go to OX Desktop and paired phones, not to you. If ox_worker_status reports waiting_for_operator, tell the user where to answer."
            );
            return Ok(reply);
        }
        self.jobs
            .lock()
            .unwrap()
            .start(request, &self.home, &self.cwd, Arc::clone(&app.secrets))
    }

    /// The session this launch names for a call. Codex reports the exact calling
    /// thread, which can be a subagent. A Claude launch has one session, fixed by
    /// its `--session-id` or `--resume` argument before the first turn. A later
    /// ID for the same Claude launch, such as after `/clear`, never replaces it;
    /// the launch token still covers that work while the launch lives.
    fn caller(&self, thread: Option<&str>) -> Value {
        let thread = thread.map(str::to_owned);
        let session = match self.client {
            ClientKind::Codex => thread.or_else(|| self.session.clone()),
            ClientKind::Claude => self.session.clone().or(thread),
        }
        .or_else(|| self.observed.lock().unwrap().clone());
        json!({
            "project": self.cwd,
            "runtime": match self.client {
                ClientKind::Codex => "codex",
                ClientKind::Claude => "claude",
            },
            "session": session,
            "launch": self.launch,
        })
    }

    /// Record this launch's native session the first time a request shows it,
    /// and bind it to host-owned workers that were launched before it was known.
    async fn observe(&self, thread: Option<&str>) {
        let Some(thread) = thread else { return };
        if self.session.is_some() {
            return;
        }
        {
            let mut observed = self.observed.lock().unwrap();
            if observed.is_some() {
                return;
            }
            *observed = Some(thread.to_owned());
        }
        if self
            .unbound
            .swap(false, std::sync::atomic::Ordering::AcqRel)
            && let Ok(Some(host)) =
                crate::host_lifetime::HostLink::find(&self.cwd, self.host_policy)
            && matches!(host, crate::host_lifetime::HostLink::External { .. })
        {
            let _ = host
                .send(
                    self.launch.clone(),
                    self.caller(Some(thread)),
                    Some(thread.to_owned()),
                    None,
                    Some(json!({"op":"bind","session":thread})),
                )
                .await;
        }
    }

    async fn host_call(&self, call: Value, thread: Option<&str>) -> Result<Value, String> {
        let app = self.app().await?;
        let host = crate::host_lifetime::HostLink::find(&self.cwd, self.host_policy)?
            .ok_or("Unknown worker job in this ox launch.")?;
        let reply = host
            .send(
                self.launch.clone(),
                self.caller(thread),
                thread.map(str::to_owned),
                None,
                Some(call),
            )
            .await
            .map_err(|e| crate::upstream::redact(&e, &app.secrets).into_owned())?;
        Ok(redact_value(reply, &app.secrets))
    }

    /// Read this session's handoff sources through the session host, which
    /// resolves the caller exactly as for every parent operation.
    async fn read_thread(&self, read: ReadThread, thread: Option<&str>) -> Result<Value, String> {
        if crate::host_lifetime::HostLink::find(&self.cwd, self.host_policy)?.is_none() {
            return Err("read_ox_thread needs the OX session host, which ran the handoff.".into());
        }
        if read.limit.is_some_and(|l| l == 0 || l > 100)
            || [&read.session, &read.item, &read.after]
                .into_iter()
                .flatten()
                .any(|s| s.is_empty() || s.len() > 256)
        {
            return Err("Use a limit of 1 to 100 and IDs of up to 256 bytes.".into());
        }
        self.host_call(
            json!({"op":"read_thread","session":read.session,"item":read.item,"after":read.after,"limit":read.limit}),
            thread,
        )
        .await
    }

    async fn list_jobs(&self, thread: Option<&str>) -> Result<Value, String> {
        let mut local = self.jobs.lock().unwrap().list();
        if crate::host_lifetime::HostLink::find(&self.cwd, self.host_policy)?.is_some() {
            let hosted = self.host_call(json!({"op":"list"}), thread).await?;
            if let (Some(jobs), Some(more)) =
                (local["jobs"].as_array_mut(), hosted["jobs"].as_array())
            {
                jobs.extend(more.iter().cloned());
            }
        }
        Ok(local)
    }

    async fn list_models(&self, filters: ListModels) -> Result<Value, String> {
        let query = filters.query.trim().to_lowercase();
        let mut total = 0;
        let mut models = Vec::new();
        // Every listed model can run. The user's starred models come first, in
        // star order, so agents see the preferred choices on the first page.
        let favorites = crate::picker::read_favorites(&self.home).unwrap_or_default();
        let rank = |alias: &str| favorites.iter().position(|f| f == alias);
        let app = self.app().await?;
        let contexts: std::collections::HashMap<String, u64> =
            app.model_contexts().into_iter().collect();
        let mut catalog = app.launch_models();
        catalog.sort_by_key(|(alias, _)| rank(alias).unwrap_or(usize::MAX));
        for (alias, provider) in catalog {
            if !alias.to_lowercase().contains(&query) && !provider.to_lowercase().contains(&query) {
                continue;
            }
            if total >= filters.offset && models.len() < PAGE_SIZE {
                models.push(json!({
                    "model": alias,
                    "provider": provider,
                    "auth": "api_key",
                    "starred": rank(alias).is_some(),
                    "context_window": contexts.get(alias),
                }));
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

fn redact_value(value: Value, secrets: &[String]) -> Value {
    match value {
        Value::String(text) => Value::String(crate::upstream::redact(&text, secrets).into_owned()),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| redact_value(v, secrets))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, redact_value(v, secrets)))
                .collect(),
        ),
        other => other,
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
        "serverInfo": {"name": SERVER, "version": crate::VERSION},
        "capabilities": {"tools": {}},
        "instructions": "Use start_ox_worker for long coding tasks, Codex goals or Claude workflows, then check ox_worker_status. Pick runtime and mode by the start_ox_worker mode notes. Workers inherit your permission mode only when the OX session host runs; with no host, inherit means read, so pass permission_mode write to let a worker edit. You can lower a worker's mode, never raise it. Check list_ox_models for provider, context_window and starred before choosing a model. Use delegate_to_ox_model for one short text answer. Start jobs only at the user's request. The named provider bills the delegated work. If this session continues another one (an OX handoff), read_ox_thread returns the source's omitted or cut messages by item ID.",
    })
}

fn tools() -> Value {
    json!({
        "tools": [
            {
                "name": "list_ox_models",
                "description": "List ox API models and detected subscription routes. Models: model, provider, auth (api_key), starred (the user's favourites, listed first), context_window (tokens, null when unknown). Subscriptions: runtime, account, main_client (this launch's client), worker_available (can run start_ox_worker without a model). query filters by alias, provider or runtime. Pages hold 40 rows: when next_offset is not null, pass it as offset for more. Use a model alias for delegate_to_ox_model and start_ox_worker.",
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
                "description": "Send one independent text request to the named API model, which the provider bills. Include the context it needs. Times out after 90 seconds; task up to 96 KiB. No files or shell: for multi-step work or files, use start_ox_worker.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "model": {"type": "string", "description": "Exact ox model alias returned by list_ox_models."},
                        "task": {"type": "string", "description": "Task and only the context the chosen provider needs. Up to 96 KiB."}
                    },
                    "required": ["model", "task"],
                    "additionalProperties": false
                },
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": true},
            },
            {
                "name": "start_ox_worker",
                "description": "Start a background coding job in this project and return its job ID at once. Choose by fit: a Codex goal for one long objective with a checkable stop condition; a Claude workflow for work Claude Code splits into steps with its Workflow tool and subagents; a session for ordinary coding on either runtime. Task up to 32 KiB. There is no time or count limit by default; set max_minutes per job, or OX_WORKER_MAX_MINUTES and OX_WORKER_MAX_PER_PARENT, to add one. A write worker starting while another writes in the same project returns a warning: consider a separate git worktree. Without the OX session host they stop when this launch exits. Inherit uses your mode only with the session host; without it, inherit is read. Lower with permission_mode, never raise it. classifier and yolo need the session host, which asks a person to accept a YOLO worker (status waiting_for_operator) unless YOLO was granted.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "runtime": {"enum": ["codex", "claude"]},
                        "mode": {"enum": ["session", "goal", "workflow"], "description": "session: background coding session on either runtime. goal: Codex only; task becomes the Codex goal objective, at most 4,000 characters. workflow: Claude Code only; the job fails if the Workflow tool is not used."},
                        "model": {"type": "string", "description": "Exact ox API model alias. Leave out only for a Claude subscription worker."},
                        "task": {"type": "string", "description": "The job objective, desired validation, and stopping condition."},
                        "permission_mode": {"enum": ["inherit", "read", "write", "classifier", "yolo"], "default": "inherit", "description": "read: no shell or writes. write: sandboxed shell plus workspace writes (the sandbox may block network; use classifier for broader access). classifier: Codex Auto-review / Claude Auto. yolo: Codex YOLO / Claude bypass. Above your mode is refused."},
                        "max_minutes": {"type": "integer", "minimum": 1, "maximum": 10080, "description": "Optional wall-clock limit for this worker in minutes. No limit by default (OX_WORKER_MAX_MINUTES sets one)."},
                        "workspace_write": {"type": "boolean", "description": "Deprecated: false means read, true means write. Use permission_mode."},
                    },
                    "required": ["runtime", "mode", "task"],
                    "additionalProperties": false,
                },
                "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true},
            },
            {
                "name":"read_ox_thread",
                "description":"Read the session this one was handed off from (and earlier sources in its chain), read-only and within this project. The handoff carried a budgeted selection of real messages; use this to read omitted or cut items in full. Pages by item ID: item returns one item, after continues after an item. Omit session for the nearest source.",
                "inputSchema":{"type":"object","properties":{
                    "session":{"type":"string","description":"A source session ID from the handoff note. Omit for the nearest source."},
                    "item":{"type":"string","description":"Return this item in full."},
                    "after":{"type":"string","description":"Start after this item ID. Use next_after from the previous page."},
                    "limit":{"type":"integer","minimum":1,"maximum":100,"default":20}
                },"additionalProperties":false},
                "annotations":{"readOnlyHint":true,"openWorldHint":false}
            },
            {
                "name":"list_ox_workers",
                "description":"List workers started by this session or OX launch and their current status. Workers owned by the OX session host keep running after this launch ends.",
                "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
                "annotations":{"readOnlyHint":true,"openWorldHint":false}
            },
            {
                "name":"steer_ox_worker",
                "description":"Deliver a follow-up instruction to a running worker which reports can_steer. It keeps its original model, workspace, and permissions.",
                "inputSchema":{"type":"object","properties":{"job_id":{"type":"integer","minimum":1},"text":{"type":"string"}},"required":["job_id","text"],"additionalProperties":false},
                "annotations":{"readOnlyHint":false,"destructiveHint":false,"openWorldHint":true}
            },
            {
                "name": "ox_worker_status",
                "description": "Get a worker's running or terminal status, native session ID, progress, and result. Jobs keep running between tool calls. Status waiting_for_operator means a person must answer an approval or question in OX Desktop or on a paired phone; tell the user where, using waiting_for_operator.message.",
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

#[cfg(test)]
mod tests {
    use super::*;
    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }
    /// Stock Codex's MCP client (rmcp) logs every 4xx other than 405 from a
    /// streamable-HTTP server as a fatal worker error. The gateway issues no
    /// `Mcp-Session-Id` and offers no SSE stream, so a GET or DELETE on its
    /// endpoint must be 405 with `Allow: POST`, never 404.
    #[tokio::test]
    async fn the_gateway_answers_stock_mcp_transport_requests_rmcp_accepts() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let token = "gateway-status-fixture".to_owned();
        let service = Arc::new(Service {
            app: OnceCell::new(),
            local_auth: crate::provider::bearer(&token).unwrap(),
            token: token.clone(),
            origin: origin.clone(),
            home: PathBuf::from("."),
            cwd: PathBuf::from("."),
            session: None,
            observed: Mutex::new(None),
            unbound: std::sync::atomic::AtomicBool::new(false),
            host_policy: Default::default(),
            launch: "launch".to_owned(),
            client: ClientKind::Codex,
            slots: Semaphore::new(1),
            jobs: Mutex::new(jobs::Jobs::default()),
            tools: Tools::All,
        });
        let server = tokio::spawn(crate::serve_requests(listener, move |request| {
            let service = Arc::clone(&service);
            async move { service.handle(request).await }
        }));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("{origin}/mcp");
        let send = |method: reqwest::Method| {
            client
                .request(method, &url)
                .bearer_auth(&token)
                .header("mcp-protocol-version", "2025-06-18")
        };

        let initialize = send(reqwest::Method::POST)
            .json(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                          "params": {"protocolVersion": "2025-06-18"}}))
            .send()
            .await
            .unwrap();
        assert_eq!(initialize.status(), StatusCode::OK);
        assert!(initialize.headers().get("mcp-session-id").is_none());

        let initialized = send(reqwest::Method::POST)
            .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .send()
            .await
            .unwrap();
        assert_eq!(initialized.status(), StatusCode::ACCEPTED);

        for (method, session) in [
            (reqwest::Method::GET, None),
            (reqwest::Method::DELETE, None),
            (reqwest::Method::GET, Some("unknown-session")),
            (reqwest::Method::DELETE, Some("unknown-session")),
        ] {
            let mut request = send(method.clone()).header("accept", "text/event-stream");
            if let Some(session) = session {
                request = request.header("mcp-session-id", session);
            }
            let reply = request.send().await.unwrap();
            assert_eq!(
                reply.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {session:?}"
            );
            assert_eq!(reply.headers()["allow"], "POST");
        }
        server.abort();
    }

    #[test]
    fn a_new_claude_launch_names_its_session_before_the_first_turn() {
        // The ID OX adds is the one the worker gateway records for the launch.
        let (rewritten, id) = crate::clients::claude_session_args(args(&["-p", "fix it"])).unwrap();
        let id = id.unwrap();
        assert_eq!(
            session_hint(&rewritten, ClientKind::Claude).as_deref(),
            Some(id.as_str())
        );
        // A later resume of that session names the same ID, so it reclaims the workers.
        let resumed = crate::clients::claude_session_args(args(&["--resume", &id])).unwrap();
        assert_eq!(resumed.1, None);
        assert_eq!(
            session_hint(&resumed.0, ClientKind::Claude).as_deref(),
            Some(id.as_str())
        );
    }

    #[test]
    fn worker_permission_modes_keep_workspace_write_compatible() {
        let start = |v: Value| serde_json::from_value::<jobs::Start>(v).unwrap();
        let base = json!({"runtime":"codex","mode":"session","model":"m","task":"t"});
        let with = |extra: Value| {
            let mut v = base.clone();
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            start(v)
        };
        // Inherit by default; the host resolves it against the parent's mode.
        let inherit = with(json!({}));
        assert_eq!(inherit.host_start(&[])["permission"], Value::Null);
        assert_eq!(inherit.host_start(&[])["write"], Value::Null);
        // Old callers keep working: false is read, true is write.
        let old = with(json!({"workspace_write": true}));
        assert_eq!(old.host_start(&[])["write"], true);
        assert_eq!(old.writes(), Ok(true));
        assert_eq!(with(json!({"workspace_write": false})).writes(), Ok(false));
        assert_eq!(
            inherit.writes(),
            Ok(false),
            "standalone inherit starts read"
        );
        // A lower named level, by level or native name.
        let lower = with(json!({"permission_mode": "read-only"}));
        assert_eq!(lower.host_start(&[])["permission"], "read");
        assert!(lower.validate().is_ok());
        assert_eq!(
            with(json!({"permission_mode": "accept_edits"})).writes(),
            Ok(true)
        );
        assert!(with(json!({"permission_mode": "root"})).validate().is_err());
        // Without a host nothing can ask the operator, so YOLO and classifiers are refused.
        assert!(with(json!({"permission_mode": "yolo"})).writes().is_err());
        assert!(with(json!({"permission_mode": "auto"})).writes().is_err());
        let tools = tools();
        let names: Vec<_> = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"read_ox_thread"));
        let start_tool = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "start_ox_worker")
            .unwrap();
        assert!(
            start_tool["description"]
                .as_str()
                .unwrap()
                .contains("Inherit uses your mode only with the session host")
        );
        assert!(start_tool["inputSchema"]["properties"]["workspace_write"].is_object());
    }

    #[test]
    fn only_plausible_native_session_ids_are_observed() {
        assert!(valid_session("019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b"));
        assert!(!valid_session(""));
        assert!(!valid_session("../etc"));
        assert!(!valid_session("a b"));
        assert!(!valid_session(&"a".repeat(129)));
    }

    #[test]
    fn with_workers_host_never_a_terminal_launch_keeps_workers_in_process() {
        // Without the host's private endpoint, `never` skips host discovery even
        // when a host socket is configured.
        if std::env::var_os("OX_HOST_LIFETIME_SOCKET").is_none() {
            let found = crate::host_lifetime::HostLink::find(
                std::path::Path::new("."),
                crate::provider::WorkersHost::Never,
            )
            .unwrap();
            assert!(found.is_none());
        }
    }

    #[test]
    fn a_resumed_terminal_launch_names_its_native_session() {
        let id = "0b8d6f2e-3c41-4f0a-9e57-1d2c3b4a5f60";
        assert_eq!(
            session_hint(&args(&["--resume", id]), ClientKind::Claude).as_deref(),
            Some(id)
        );
        assert_eq!(
            session_hint(&args(&[&format!("--session-id={id}")]), ClientKind::Claude).as_deref(),
            Some(id)
        );
        assert_eq!(
            session_hint(&args(&["resume", id]), ClientKind::Codex).as_deref(),
            Some(id)
        );
        assert_eq!(
            session_hint(&args(&["--", "--resume", id]), ClientKind::Claude),
            None
        );
        assert_eq!(
            session_hint(&args(&["--resume", "../x"]), ClientKind::Claude),
            None
        );
        assert_eq!(session_hint(&args(&[]), ClientKind::Codex), None);
    }
}

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::{
    sync::{mpsc, oneshot},
    task::AbortHandle,
};

use crate::provider::RelayError;

/// A worker limit from an environment variable. Unset or zero means no limit.
fn env_limit(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
}
const MAX_RETAINED: usize = 24;
const MAX_TEXT: usize = 24 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const CODEX_FLUSH_GRACE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(super) struct Jobs {
    entries: BTreeMap<u64, Job>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Start {
    runtime: Runtime,
    mode: Mode,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    checkout: Option<std::path::PathBuf>,
    task: String,
    /// Legacy: false means read, true means write. Capped by the parent's mode.
    #[serde(default)]
    workspace_write: Option<bool>,
    /// `inherit` (default) or a lower level: read, write, classifier or yolo.
    /// Never above the parent's own mode.
    #[serde(default)]
    permission_mode: Option<String>,
    /// Wall-clock limit in minutes. Falls back to OX_WORKER_MAX_MINUTES when set.
    #[serde(default)]
    max_minutes: Option<u32>,
}

/// The canonical level name of a permission mode, or None for inherit.
pub(super) fn permission_level(text: &str) -> Result<Option<&'static str>, String> {
    Ok(Some(
        match text
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' '], "_")
            .as_str()
        {
            "inherit" | "" => return Ok(None),
            "read" | "read_only" | "readonly" | "plan" | "default" => "read",
            "write" | "workspace" | "workspace_write" | "accept_edits" => "write",
            "classifier" | "auto_review" | "auto" => "classifier",
            "yolo" | "bypass" | "bypass_permissions" => "yolo",
            _ => {
                return Err(
                    "permission_mode must be inherit, read, write, classifier or yolo.".into(),
                );
            }
        },
    ))
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Runtime {
    Codex,
    Claude,
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Session,
    Goal,
    Workflow,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JobId {
    pub job_id: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Steer {
    pub job_id: u64,
    pub text: String,
}

struct Instruction {
    text: String,
    reply: oneshot::Sender<Result<Value, String>>,
}

struct Job {
    checkout: std::path::PathBuf,
    sender: mpsc::Sender<Instruction>,
    state: Arc<Mutex<State>>,
    task: AbortHandle,
    writes: bool,
}

struct State {
    runtime: Runtime,
    mode: Mode,
    can_steer: bool,
    messages: VecDeque<Value>,
    status: &'static str,
    native_session: Option<String>,
    progress: String,
    result: Option<String>,
    error: Option<String>,
    model: String,
    started: std::time::Instant,
    max_minutes: Option<u64>,
    warning: Option<String>,
}

impl State {
    fn record(&mut self, role: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.messages.len() == 32 {
            self.messages.pop_front();
        }
        self.messages
            .push_back(json!({"role":role,"text":&text[..text.floor_char_boundary(4096)]}));
    }

    fn summary(&self, job_id: u64) -> Value {
        json!({
            "job_id": job_id, "mode":self.mode, "runtime":self.runtime,
            "can_steer": self.can_steer && self.status == "running",
            "status": self.status,
            "model": self.model,
            "native_session": self.native_session,
            "progress": self.progress,
            "result": self.result,
            "error": self.error,
            "elapsed_seconds": self.started.elapsed().as_secs(),
            "max_minutes": self.max_minutes,
            "warning": self.warning,
        })
    }
    fn snapshot(&self, job_id: u64) -> Value {
        let mut result = self.summary(job_id);
        result["messages"] = json!(self.messages);
        result
    }
}

impl Jobs {
    pub(super) fn list(&self) -> Value {
        json!({"jobs":self.entries.iter().map(|(id,job)|job.state.lock().unwrap().summary(*id)).collect::<Vec<_>>()})
    }

    pub(super) fn steer(
        &self,
        request: Steer,
        secrets: &[String],
    ) -> Result<oneshot::Receiver<Result<Value, String>>, String> {
        let job = self
            .entries
            .get(&request.job_id)
            .ok_or("Unknown worker in this OX launch.")?;
        let state = job.state.lock().unwrap();
        if state.status != "running" || !state.can_steer {
            return Err("This worker is not accepting input.".into());
        }
        let text = request.text.trim();
        if text.is_empty() || text.len() > MAX_TEXT {
            return Err("Send a nonempty worker message of at most 24 KiB.".into());
        }
        let (reply, received) = oneshot::channel();
        job.sender
            .try_send(Instruction {
                text: clean(text, secrets),
                reply,
            })
            .map_err(|_| "Worker input is busy or closed.")?;
        Ok(received)
    }

    /// Whether the opt-in worker count limit is reached. No limit never is.
    fn slots_full(&self, cap: Option<u64>) -> bool {
        cap.is_some_and(|cap| {
            self.entries
                .values()
                .filter(|job| !job.task.is_finished())
                .count()
                >= cap as usize
        })
    }
    pub(super) fn start(
        &mut self,
        request: Start,
        home: &Path,
        cwd: &Path,
        secrets: Arc<Vec<String>>,
    ) -> Result<Value, String> {
        request.validate()?;
        request.writes()?;
        if self.slots_full(env_limit("OX_WORKER_MAX_PER_PARENT")) {
            return Err("All background worker slots are in use. Wait for one to finish or cancel one. The limit is OX_WORKER_MAX_PER_PARENT.".into());
        }
        let writes = request.writes()?;
        let cwd = crate::worker_checkout::resolve(cwd, request.checkout.as_deref(), false)?;
        let warning = self.write_overlap(writes, &cwd);
        let max_minutes = request
            .max_minutes
            .map(u64::from)
            .or_else(|| env_limit("OX_WORKER_MAX_MINUTES"));
        if self.entries.len() >= MAX_RETAINED
            && let Some(id) = self
                .entries
                .iter()
                .find(|(_, job)| job.task.is_finished())
                .map(|(id, _)| *id)
        {
            self.entries.remove(&id);
        }
        let command = request
            .command(home, &cwd)
            .map_err(|error| error.to_string())?;
        // Worker IDs appear in archived transcripts. Never recycle a small counter on reconnect.
        let id = loop {
            let candidate = getrandom::u64().map_err(|e| e.to_string())? & ((1u64 << 52) - 1);
            if candidate > 0 && !self.entries.contains_key(&candidate) {
                break candidate;
            }
        };
        let (sender, instructions) = mpsc::channel(4);
        let state = Arc::new(Mutex::new(State {
            status: "running",
            can_steer: false,
            runtime: request.runtime,
            mode: request.mode,
            messages: VecDeque::new(),
            model: request
                .model
                .clone()
                .unwrap_or_else(|| "Claude subscription".to_owned()),
            native_session: None,
            progress: request
                .claude_fallback_notice()
                .unwrap_or("Starting the stock client")
                .to_owned(),
            result: None,
            error: None,
            started: std::time::Instant::now(),
            max_minutes,
            warning,
        }));
        state
            .lock()
            .unwrap()
            .record("user", &clean(&request.task, &secrets));
        let initial = state.lock().unwrap().snapshot(id);
        let handle = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                let work = async {
                    match request.runtime {
                        Runtime::Codex => {
                            run_codex(command, &request, &state, &secrets, instructions).await
                        }
                        Runtime::Claude => {
                            run_claude(command, &request, &state, &secrets, instructions).await
                        }
                    }
                };
                let result = match max_minutes {
                    None => work.await,
                    Some(minutes) => {
                        match tokio::time::timeout(Duration::from_secs(minutes * 60), work).await {
                            Ok(result) => result,
                            Err(_) => Err(format!(
                                "Worker stopped after {minutes} minutes (time limit)."
                            )
                            .into()),
                        }
                    }
                };
                let mut state = state.lock().unwrap();
                if state.status == "cancelled" {
                    return;
                }
                match result {
                    Ok(status) => state.status = status,
                    Err(error) => {
                        state.status = "failed";
                        state.error = Some(clean(&error.to_string(), &secrets));
                    }
                }
            }
        });
        self.entries.insert(
            id,
            Job {
                checkout: cwd,
                sender,
                state,
                task: handle.abort_handle(),
                writes,
            },
        );
        Ok(initial)
    }

    /// A caution when this write worker starts beside another running write worker.
    fn write_overlap(&self, writes: bool, checkout: &Path) -> Option<String> {
        (writes
            && self
                .entries
                .values()
                .any(|job| job.writes && job.checkout == checkout && !job.task.is_finished()))
        .then(|| {
            "Another write worker is editing this project. Consider a separate git worktree."
                .to_owned()
        })
    }

    pub(super) fn has(&self, id: u64) -> bool {
        self.entries.contains_key(&id)
    }

    pub(super) fn status(&self, id: JobId) -> Result<Value, String> {
        self.entries
            .get(&id.job_id)
            .map(|job| job.state.lock().unwrap().snapshot(id.job_id))
            .ok_or_else(|| "Unknown worker job in this ox launch.".into())
    }

    pub(super) fn cancel(&mut self, id: JobId) -> Result<Value, String> {
        let job = self
            .entries
            .get(&id.job_id)
            .ok_or("Unknown worker job in this ox launch.")?;
        let mut state = job.state.lock().unwrap();
        if state.status == "running" {
            job.task.abort();
            state.status = "cancelled";
        }
        Ok(state.snapshot(id.job_id))
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        for job in self.entries.values() {
            job.task.abort();
        }
    }
}

impl Start {
    /// The launch the session host receives. Secrets are removed from the task first.
    pub(super) fn host_start(&self, secrets: &[String]) -> Value {
        let mut start = json!({
            "parent": "",
            "runtime": self.runtime,
            "model": self.model,
            "mode": self.mode,
            "task": crate::upstream::redact(&self.task, secrets),
            "write": self.workspace_write,
            "max_minutes": self.max_minutes,
            "permission": self.permission_mode.as_deref().map(|m| permission_level(m).ok().flatten().unwrap_or("inherit")),
        });
        if let Some(path) = &self.checkout {
            start["checkout"] = json!(path);
        }
        start
    }
    /// Without a session host the gateway cannot verify the parent's mode: it
    /// starts read workers by default and write workers on request, as before.
    pub(super) fn writes(&self) -> Result<bool, String> {
        match self
            .permission_mode
            .as_deref()
            .map(permission_level)
            .transpose()?
            .flatten()
        {
            None => Ok(self.workspace_write.unwrap_or(false)),
            Some("read") => Ok(false),
            Some("write") => Ok(true),
            Some(_) => Err("Classifier and YOLO workers need the OX session host, which asks the operator. Start OX Desktop or run `ox host install`.".into()),
        }
    }
    pub(super) fn validate(&self) -> Result<(), String> {
        if self.max_minutes.is_some_and(|m| m == 0 || m > 7 * 24 * 60) {
            return Err("max_minutes must be between 1 and 10080.".into());
        }
        if let Some(mode) = &self.permission_mode {
            permission_level(mode)?;
        }
        if self.task.trim().is_empty() || self.task.len() > 32 * 1024 {
            return Err("Provide a task of up to 32 KiB.".into());
        }
        if self.mode == Mode::Goal
            && (self.runtime != Runtime::Codex || self.task.chars().count() > 4000)
        {
            return Err(
                "Goal mode requires Codex and an objective of at most 4,000 characters.".into(),
            );
        }
        if self.mode == Mode::Workflow && self.runtime != Runtime::Claude {
            return Err("Workflow mode requires Claude Code.".into());
        }
        if self.runtime == Runtime::Codex && self.model.is_none() {
            return Err("Choose an ox API model for a Codex worker.".into());
        }
        if self.model.as_ref().is_some_and(|model| {
            model.trim().is_empty() || model.len() > 256 || model.starts_with('-')
        }) {
            return Err("Provide an ox model alias of up to 256 bytes.".into());
        }
        Ok(())
    }

    fn command(&self, home: &Path, cwd: &Path) -> Result<Command, RelayError> {
        // A no-host subscription worker carries its sandbox settings inside the
        // checked account overlay, so the contract's own --settings is dropped.
        let mut overlay = false;
        let mut command = if let Some(model) = &self.model {
            let mut command = Command::new(std::env::current_exe()?);
            let client = match self.runtime {
                Runtime::Codex => "codex",
                Runtime::Claude => "claude",
            };
            command.arg(client);
            if self.mode != Mode::Goal {
                command.args(["--model", model]);
            }
            command
        } else {
            if !crate::native::has_saved_account(crate::clients::ClientKind::Claude, home) {
                return Err(
                    "Run ox login claude on this host to use a Claude subscription worker.".into(),
                );
            }
            if crate::host_lifetime::hosted() {
                let mut wrapper = Command::new(std::env::current_exe()?);
                wrapper.args(["claude", "--subscription"]);
                wrapper
            } else {
                overlay = true;
                let sandbox = self.claude_contract()?.settings().is_some();
                crate::native::claude_subscription_worker_command(home, sandbox)?
            }
        };
        command
            .current_dir(cwd)
            .env("CODEX_HOME", home)
            .env_remove(super::TOKEN_ENV);
        // The worker helper registers under this gateway's lease, so the host retires it if the gateway dies.
        crate::host_lifetime::set_parent_lease(&mut command);
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        match (self.runtime, self.mode) {
            (Runtime::Codex, mode) => {
                if mode == Mode::Goal {
                    command.args(["--enable", "goals"]);
                }
                command
                    .args(["--no-daemon", "app-server", "--listen", "stdio://"])
                    .stdin(Stdio::piped());
            }
            (Runtime::Claude, _) => {
                let contract = self.claude_contract()?;
                command.args([
                    "--print",
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--permission-mode",
                    contract.permission_mode(),
                ]);
                // Stock Claude Code merges the sandbox settings with the account settings ox builds.
                let mut policy = contract.args();
                if overlay && let Some(at) = policy.iter().position(|a| a == "--settings") {
                    policy.drain(at..=at + 1);
                }
                command.args(policy);
                command.stdin(Stdio::piped());
            }
        }
        Ok(command)
    }

    /// The shared Claude worker contract for this request.
    fn claude_contract(&self) -> Result<crate::claude_worker_contract::Contract, RelayError> {
        Ok(crate::claude_worker_contract::Contract::detect(
            self.writes()?,
            self.mode == Mode::Workflow,
        ))
    }

    /// Warn when a Claude write worker's Bash commands can run unsandboxed.
    pub(super) fn claude_fallback_notice(&self) -> Option<&'static str> {
        (self.runtime == Runtime::Claude)
            .then(|| self.claude_contract().ok()?.fallback_notice())
            .flatten()
    }

    fn sandbox(&self) -> &'static str {
        if self.writes().unwrap_or(false) {
            "workspace-write"
        } else {
            "read-only"
        }
    }
}

// Kill the process group on cancellation so a relay and its stock child stop
// together. The group is created for this job and never comes from MCP input.
struct WorkerProcess {
    child: Child,
    group: Option<u32>,
}

impl WorkerProcess {
    fn spawn(mut command: Command) -> Result<Self, RelayError> {
        let child = command
            .spawn()
            .map_err(|error| format!("Could not start the installed worker client: {error}"))?;
        Ok(Self {
            group: child.id(),
            child,
        })
    }

    fn output(&mut self) -> Lines<BufReader<ChildStdout>> {
        BufReader::new(self.child.stdout.take().expect("worker stdout is piped")).lines()
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            unsafe { libc::kill(-(group as i32), libc::SIGKILL) };
        }
        let _ = self.child.start_kill();
    }
}

async fn run_claude(
    command: Command,
    request: &Start,
    state: &Mutex<State>,
    secrets: &[String],
    mut instructions: mpsc::Receiver<Instruction>,
) -> Result<&'static str, RelayError> {
    let mut process = WorkerProcess::spawn(command)?;
    let mut lines = process.output();
    let mut input = process.child.stdin.take().expect("worker stdin is piped");
    let user_message = |text: &str| json!({"type":"user","message":{"role":"user","content":text},"parent_tool_use_id":null,"session_id":""});
    let task = clean(request.task.trim(), secrets);
    let task = if request.mode == Mode::Workflow {
        format!(
            "Run this task as a Claude Code workflow using your Workflow tool. If the tool is unavailable, explain that and stop. Objective:\n{task}"
        )
    } else {
        task
    };
    write_rpc(&mut input, user_message(&task)).await?;
    let mut used_workflow = false;
    let mut pending = 1usize;
    // The error of the latest finished turn, cleared by a later successful turn.
    let mut turn_failure: Option<String> = None;
    while pending > 0 {
        let line = tokio::select! {
            Some(message) = instructions.recv() => {
                if message.reply.is_closed() { continue; }
                write_rpc(&mut input, user_message(&message.text)).await?;
                pending += 1;
                state.lock().unwrap().record("user", &message.text);
                let _ = message.reply.send(Ok(json!({"accepted":true})));
                continue;
            }
            line = lines.next_line() => line?,
        };
        let line = line.ok_or("Claude Code closed before returning a result.")?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let mut state = state.lock().unwrap();
        if let Some(id) = event["session_id"].as_str() {
            state.native_session = Some(clean(id, secrets));
        }
        if let Some(kind) = event["type"].as_str() {
            state.progress = clean(kind, secrets);
        }
        match event["type"].as_str() {
            Some("assistant") => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_use" {
                        used_workflow |= block["name"] == "Workflow";
                    }
                    if block["type"] == "text"
                        && let Some(text) = block["text"].as_str()
                    {
                        state.record("assistant", &clean(text, secrets));
                    }
                }
            }
            Some("result") => {
                // An error turn (overload, max turns, execution error) ends only that turn.
                // The process stays up so a session can be steered on.
                turn_failure = turn_error(&event, secrets);
                state.error = turn_failure.clone();
                if turn_failure.is_none()
                    && let Some(text) = event["result"].as_str()
                {
                    state.result = Some(clean(text, secrets));
                }
                pending -= 1;
            }
            _ => {}
        }
        state.can_steer = pending > 0;
    }
    drop(input);
    // The job ends with its last turn's error, whatever its mode. A session stays
    // steerable while turns are pending; once the last turn errors, nothing is left to steer.
    if let Some(error) = turn_failure {
        return Err(error.into());
    }
    if request.runtime == Runtime::Claude {
        request.claude_contract()?.check_workflow(used_workflow)?;
    }
    // Closing input lets the client flush its final session state before leaving the process group.
    if let Ok(status) = tokio::time::timeout(Duration::from_secs(5), process.child.wait()).await {
        let status = status?;
        process.group = None;
        if !status.success() {
            return Err("Claude Code exited with an error after its result.".into());
        }
    }
    Ok("completed")
}

async fn run_codex(
    command: Command,
    request: &Start,
    state: &Mutex<State>,
    secrets: &[String],
    mut instructions: mpsc::Receiver<Instruction>,
) -> Result<&'static str, RelayError> {
    let mut process = WorkerProcess::spawn(command)?;
    let mut lines = process.output();
    let mut input = process
        .child
        .stdin
        .take()
        .expect("app-server stdin is piped");
    rpc(
        &mut input,
        &mut lines,
        1,
        "initialize",
        json!({"clientInfo":{"name":"ox-worker","version":crate::VERSION}}),
    )
    .await?;
    write_rpc(&mut input, json!({"method":"initialized"})).await?;
    let session = rpc(
        &mut input,
        &mut lines,
        2,
        "thread/start",
        json!({"model":request.model,"sandbox":request.sandbox(),"approvalPolicy":"never"}),
    )
    .await?;
    let thread = session["thread"]["id"]
        .as_str()
        .ok_or("Codex returned no worker session ID.")?;
    state.lock().unwrap().native_session = Some(clean(thread, secrets));
    let task = clean(request.task.trim(), secrets);
    let (method, params) = if request.mode == Mode::Goal {
        (
            "thread/goal/set",
            json!({"threadId":thread,"objective":task,"status":"active"}),
        )
    } else {
        (
            "turn/start",
            json!({"threadId":thread,"input":[{"type":"text","text":task}]}),
        )
    };
    // Consume notifications and replies on the same loop, including ones arriving before the start reply.
    write_rpc(&mut input, json!({"id":3,"method":method,"params":params})).await?;
    let mut turn = None::<String>;
    let mut terminal = None;
    let mut seq = 3u64;
    let mut sending = BTreeMap::<u64, Instruction>::new();
    let mut awaiting_start = true;
    let start_timeout = tokio::time::sleep(RPC_TIMEOUT);
    tokio::pin!(start_timeout);
    let status = loop {
        let line = tokio::select! {
            _ = &mut start_timeout, if awaiting_start => return Err(format!("Codex did not answer {method}.").into()),
            Some(message) = instructions.recv() => {
                if message.reply.is_closed() { continue; }
                let Some(active) = turn.as_ref() else {
                    let _ = message.reply.send(Err("The worker turn has ended.".into()));
                    continue;
                };
                seq += 1;
                let params = json!({"threadId":thread, "expectedTurnId":active, "input":[{"type":"text", "text":message.text}]});
                write_rpc(&mut input, json!({"id":seq, "method":"turn/steer", "params":params})).await?;
                sending.insert(seq, message);
                continue;
            }
            line = lines.next_line() => line?,
        };
        let line = line.ok_or("Codex closed before the worker finished.")?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        deny_approval(&mut input, &event).await?;
        if event.get("method").is_none()
            && let Some(id) = event["id"].as_u64()
        {
            if id == 3 {
                if event.get("error").is_some() {
                    return Err(format!("Codex rejected {method}.").into());
                }
                awaiting_start = false;
            }
            if let Some(message) = sending.remove(&id) {
                let accepted = if event.get("result").is_some() {
                    state.lock().unwrap().record("user", &message.text);
                    Ok(json!({"accepted":true}))
                } else {
                    Err("Codex rejected the instruction for this worker turn.".into())
                };
                let _ = message.reply.send(accepted);
            }
            continue;
        }
        let p = &event["params"];
        if p["threadId"].as_str() != Some(thread) {
            continue;
        }
        let mut state = state.lock().unwrap();
        match event["method"].as_str() {
            Some("turn/started") => {
                turn = p["turn"]["id"].as_str().map(str::to_owned);
                state.can_steer = turn.is_some();
                state.progress = "Working".into();
            }
            Some("turn/completed") => {
                turn = None;
                state.can_steer = false;
                if p["turn"]["status"] == "failed" {
                    return Err("The Codex worker turn failed.".into());
                }
                if request.mode != Mode::Goal {
                    terminal = Some("completed");
                }
            }
            Some("thread/goal/updated") => {
                let status = p["goal"]["status"].as_str().unwrap_or("active");
                state.progress = format!("Goal {status}");
                terminal = match status {
                    "complete" => Some("completed"),
                    "blocked" | "paused" | "usageLimited" | "budgetLimited" => Some("stopped"),
                    _ => None,
                };
            }
            Some("item/completed") => {
                let item = &p["item"];
                if item["type"] == "agentMessage"
                    && let Some(text) = item["text"].as_str()
                {
                    let text = clean(text, secrets);
                    state.record("assistant", &text);
                    state.result = Some(text);
                } else if let Some(name) = item["type"]
                    .as_str()
                    .filter(|n| !matches!(*n, "reasoning" | "userMessage"))
                {
                    state.record("tool", &clean(name, secrets));
                }
            }
            _ => {}
        }
        if turn.is_none()
            && let Some(status) = terminal
        {
            break status;
        }
    };
    // Closing input lets the app-server flush its final session state before the
    // process group is killed on drop. Wait briefly, then let drop kill it.
    drop(input);
    let _ = tokio::time::timeout(CODEX_FLUSH_GRACE, process.child.wait()).await;
    Ok(status)
}

async fn rpc(
    input: &mut ChildStdin,
    lines: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, RelayError> {
    write_rpc(input, json!({"id":id,"method":method,"params":params})).await?;
    tokio::time::timeout(RPC_TIMEOUT, async {
        while let Some(line) = lines.next_line().await? {
            let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
            if event["id"] == id && event.get("method").is_none() {
                return event.get("result").cloned().ok_or_else(|| format!("Codex rejected {method}. Check the installed client's Goal mode support and settings.").into());
            }
            deny_approval(input, &event).await?;
        }
        Err(format!("Codex exited during {method}.").into())
    }).await.map_err(|_| format!("Codex did not answer {method}."))?
}

async fn deny_approval(input: &mut ChildStdin, event: &Value) -> Result<(), RelayError> {
    if event.get("method").is_some()
        && let Some(id) = event.get("id")
    {
        write_rpc(input, json!({"id":id,"error":{"code":-32000,"message":"Worker has no interactive approval channel"}})).await?;
    }
    Ok(())
}

async fn write_rpc(input: &mut ChildStdin, event: Value) -> Result<(), RelayError> {
    input.write_all(event.to_string().as_bytes()).await?;
    input.write_all(b"\n").await?;
    input.flush().await?;
    Ok(())
}

/// The cleaned error of a stream-json `result` event that ended its turn in failure.
fn turn_error(event: &Value, secrets: &[String]) -> Option<String> {
    if event["is_error"] != true {
        return None;
    }
    let text = |value: &Value| {
        value
            .as_str()
            .or_else(|| value["message"].as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let message = text(&event["result"])
        .or_else(|| {
            let errors: Vec<String> = event["errors"]
                .as_array()?
                .iter()
                .filter_map(text)
                .collect();
            (!errors.is_empty()).then(|| errors.join("; "))
        })
        .unwrap_or_else(|| "Claude Code reported a worker error.".to_owned());
    Some(clean(&message, secrets))
}

fn clean(text: &str, secrets: &[String]) -> String {
    let text = crate::upstream::redact(text, secrets);
    text[..text.floor_char_boundary(MAX_TEXT)].to_owned()
}

#[cfg(test)]
mod turn_error_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn successful_result_is_not_an_error() {
        let event = json!({"type":"result","is_error":false,"result":"done"});
        assert_eq!(turn_error(&event, &[]), None);
    }

    #[test]
    fn error_result_uses_its_text_and_redacts_secrets() {
        let event =
            json!({"type":"result","is_error":true,"result":"overloaded for key sk-test-123"});
        let secrets = vec!["sk-test-123".to_owned()];
        let error = turn_error(&event, &secrets).unwrap();
        assert!(error.starts_with("overloaded"));
        assert!(!error.contains("sk-test-123"));
    }

    #[test]
    fn error_result_falls_back_to_its_errors_then_a_default() {
        let event = json!({"type":"result","subtype":"error_during_execution","is_error":true,
            "errors":["tool crashed", {"message":"retry later"}]});
        assert_eq!(
            turn_error(&event, &[]).as_deref(),
            Some("tool crashed; retry later")
        );
        let bare = json!({"type":"result","is_error":true,"result":"  "});
        assert_eq!(
            turn_error(&bare, &[]).as_deref(),
            Some("Claude Code reported a worker error.")
        );
    }
}

#[cfg(test)]
mod worker_contract_tests {
    use super::*;
    use serde_json::json;

    fn start(value: Value) -> Start {
        serde_json::from_value(value).unwrap()
    }

    fn args_of(command: &Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let index = args.iter().position(|arg| arg == flag).expect(flag);
        &args[index + 1]
    }

    #[test]
    fn write_claude_workers_get_bash_with_or_without_a_sandbox() {
        let request = start(json!({
            "runtime": "claude", "mode": "session", "task": "fix it",
            "model": "fixture", "permission_mode": "write"
        }));
        let command = request.command(Path::new("/"), Path::new("/")).unwrap();
        let args = args_of(&command);
        assert_eq!(value_after(&args, "--permission-mode"), "acceptEdits");
        // The shell does not depend on the sandbox: Claude Code runs it sandboxed
        // where it can and otherwise unsandboxed, as in a terminal.
        assert_eq!(
            value_after(&args, "--tools"),
            "Read,Glob,Grep,Edit,Write,Bash,Workflow"
        );
        assert_eq!(value_after(&args, "--allowedTools"), "Read,Glob,Grep,Bash");
        let settings = value_after(&args, "--settings");
        assert_eq!(
            serde_json::from_str::<Value>(settings).unwrap(),
            crate::clients::claude_sandbox_document()
        );

        let read = start(json!({
            "runtime": "claude", "mode": "session", "task": "look",
            "model": "fixture", "permission_mode": "read"
        }));
        let args = args_of(&read.command(Path::new("/"), Path::new("/")).unwrap());
        assert_eq!(value_after(&args, "--tools"), "Read,Glob,Grep,Workflow");
        assert_eq!(
            value_after(&args, "--disallowedTools"),
            "Bash,Edit,Write,MultiEdit,NotebookEdit"
        );
        assert!(!args.iter().any(|arg| arg == "--settings"));
    }

    /// A fake Codex app-server that answers the RPCs, reports the turn as completed,
    /// and only writes its marker after stdin closes, as a client flushing state would.
    #[cfg(unix)]
    #[tokio::test]
    async fn codex_worker_gets_a_flush_window_after_turn_completed() {
        const SCRIPT: &str = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"method":"thread/start"'*) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-1"}}}' ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed"}}}' ;;
  esac
done
printf flushed > "$MARKER"
"#;
        let marker = std::env::temp_dir().join(format!("ox-codex-flush-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let mut command = Command::new("sh");
        command
            .args(["-c", SCRIPT])
            .env("MARKER", &marker)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        let request = start(json!({
            "runtime": "codex", "mode": "session", "task": "say hi", "model": "fixture"
        }));
        let state = Mutex::new(State {
            runtime: Runtime::Codex,
            mode: Mode::Session,
            can_steer: false,
            messages: VecDeque::new(),
            status: "running",
            native_session: None,
            progress: String::new(),
            result: None,
            error: None,
            model: "fixture".into(),
            started: std::time::Instant::now(),
            max_minutes: None,
            warning: None,
        });
        let (_instructions, receiver) = mpsc::channel::<Instruction>(1);
        let status = run_codex(command, &request, &state, &[], receiver)
            .await
            .expect("the scripted turn completes");
        assert_eq!(status, "completed");
        // The worker was not killed the moment turn/completed arrived.
        assert_eq!(
            std::fs::read_to_string(&marker).ok().as_deref(),
            Some("flushed")
        );
        let _ = std::fs::remove_file(&marker);
    }

    /// A session whose last turn errors ends failed with that error, not "completed".
    #[cfg(unix)]
    #[tokio::test]
    async fn session_whose_last_turn_errors_ends_failed() {
        const SCRIPT: &str = r#"
read -r _line
printf '%s\n' '{"type":"result","is_error":true,"result":"overloaded right now"}'
"#;
        let mut command = Command::new("sh");
        command
            .args(["-c", SCRIPT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        let request = start(json!({
            "runtime": "claude", "mode": "session", "task": "say hi", "model": "fixture"
        }));
        let state = Mutex::new(State {
            runtime: Runtime::Claude,
            mode: Mode::Session,
            can_steer: false,
            messages: VecDeque::new(),
            status: "running",
            native_session: None,
            progress: String::new(),
            result: None,
            error: None,
            model: "fixture".into(),
            started: std::time::Instant::now(),
            max_minutes: None,
            warning: None,
        });
        let (_instructions, receiver) = mpsc::channel::<Instruction>(1);
        let error = run_claude(command, &request, &state, &[], receiver)
            .await
            .expect_err("an errored last turn fails the session");
        assert!(error.to_string().contains("overloaded right now"));
    }

    fn pending_job(writes: bool) -> Job {
        let (sender, _) = mpsc::channel(1);
        let task = tokio::spawn(std::future::pending::<()>());
        Job {
            checkout: ".".into(),
            sender,
            state: Arc::new(Mutex::new(State {
                runtime: Runtime::Codex,
                mode: Mode::Session,
                can_steer: false,
                messages: VecDeque::new(),
                status: "running",
                native_session: None,
                progress: String::new(),
                result: None,
                error: None,
                model: String::new(),
                started: std::time::Instant::now(),
                max_minutes: None,
                warning: None,
            })),
            task: task.abort_handle(),
            writes,
        }
    }

    #[tokio::test]
    async fn an_opt_in_cap_limits_workers_and_no_cap_does_not() {
        let mut jobs = Jobs::default();
        for id in 1..=4 {
            jobs.entries.insert(id, pending_job(false));
        }
        assert!(!jobs.slots_full(None), "no limit by default");
        assert!(!jobs.slots_full(Some(5)));
        assert!(jobs.slots_full(Some(4)));
    }

    #[tokio::test]
    async fn a_second_write_worker_gets_a_warning_and_a_read_worker_does_not() {
        let mut jobs = Jobs::default();
        assert!(jobs.write_overlap(true, Path::new(".")).is_none());
        jobs.entries.insert(1, pending_job(true));
        assert!(
            jobs.write_overlap(true, Path::new("."))
                .unwrap()
                .contains("git worktree")
        );
        assert!(jobs.write_overlap(false, Path::new(".")).is_none());
    }

    #[test]
    fn max_minutes_is_bounded() {
        let ok = |m: u32| {
            start(json!({"runtime":"claude","mode":"session","task":"x","max_minutes":m}))
                .validate()
        };
        assert!(ok(30).is_ok());
        assert!(ok(0).is_err());
        assert!(ok(100_000).is_err());
        assert_eq!(
            start(json!({"runtime":"claude","mode":"session","task":"x","max_minutes":5}))
                .host_start(&[])["max_minutes"],
            5
        );
    }
}

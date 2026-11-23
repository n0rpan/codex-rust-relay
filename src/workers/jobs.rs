use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::AbortHandle;

use crate::provider::RelayError;

const MAX_RUNNING: usize = 4;
const MAX_RETAINED: usize = 24;
const MAX_TEXT: usize = 24 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(super) struct Jobs {
    next: u64,
    entries: BTreeMap<u64, Job>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Start {
    runtime: Runtime,
    mode: Mode,
    #[serde(default)]
    pub model: Option<String>,
    task: String,
    #[serde(default)]
    workspace_write: bool,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Runtime {
    Codex,
    Claude,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Session,
    Goal,
    Workflow,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JobId {
    job_id: u64,
}

struct Job {
    state: Arc<Mutex<State>>,
    task: AbortHandle,
}

struct State {
    status: &'static str,
    native_session: Option<String>,
    progress: String,
    result: Option<String>,
    error: Option<String>,
    model: String,
}

impl State {
    fn snapshot(&self, job_id: u64) -> Value {
        json!({
            "job_id": job_id,
            "status": self.status,
            "model": self.model,
            "native_session": self.native_session,
            "progress": self.progress,
            "result": self.result,
            "error": self.error,
        })
    }
}

impl Jobs {
    pub(super) fn start(
        &mut self,
        request: Start,
        home: &Path,
        cwd: &Path,
        secrets: Arc<Vec<String>>,
    ) -> Result<Value, String> {
        request.validate()?;
        if self
            .entries
            .values()
            .filter(|job| !job.task.is_finished())
            .count()
            >= MAX_RUNNING
        {
            return Err("All background worker slots are in use.".into());
        }
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
            .command(home, cwd, &secrets)
            .map_err(|error| error.to_string())?;
        self.next += 1;
        let id = self.next;
        let state = Arc::new(Mutex::new(State {
            status: "running",
            model: request
                .model
                .clone()
                .unwrap_or_else(|| "Claude subscription".to_owned()),
            native_session: None,
            progress: "Starting the stock client".to_owned(),
            result: None,
            error: None,
        }));
        let initial = state.lock().unwrap().snapshot(id);
        let handle = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                let result = match request.mode {
                    Mode::Goal => run_goal(command, &request, &state, &secrets).await,
                    Mode::Session | Mode::Workflow => {
                        run_session(command, request.runtime, request.mode, &state, &secrets).await
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
                state,
                task: handle.abort_handle(),
            },
        );
        Ok(initial)
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
    fn validate(&self) -> Result<(), String> {
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

    fn command(&self, home: &Path, cwd: &Path, secrets: &[String]) -> Result<Command, RelayError> {
        let task = crate::upstream::redact(self.task.trim(), secrets);
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
            crate::native::claude_account_command(home)?
        };
        command
            .current_dir(cwd)
            .env("CODEX_HOME", home)
            .env_remove(super::TOKEN_ENV);
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        match (self.runtime, self.mode) {
            (Runtime::Codex, Mode::Goal) => {
                command
                    .args([
                        "--enable",
                        "goals",
                        "--no-daemon",
                        "app-server",
                        "--listen",
                        "stdio://",
                    ])
                    .stdin(Stdio::piped());
            }
            (Runtime::Codex, _) => {
                command
                    .args([
                        "--ask-for-approval",
                        "never",
                        "--no-daemon",
                        "exec",
                        "--json",
                        "--skip-git-repo-check",
                        "--sandbox",
                        self.sandbox(),
                        "--",
                    ])
                    .arg(task.as_ref())
                    .stdin(Stdio::null());
            }
            (Runtime::Claude, mode) => {
                let task = if mode == Mode::Workflow {
                    format!(
                        "Run this task as a Claude Code workflow using your Workflow tool. If the tool is unavailable, explain that and stop. Objective:\n{task}"
                    )
                } else {
                    task.into_owned()
                };
                let tools = if self.workspace_write {
                    "Read,Glob,Grep,Edit,Write,Bash,Workflow"
                } else {
                    "Read,Glob,Grep,Workflow"
                };
                if !self.workspace_write {
                    command.args([
                        "--disallowedTools",
                        "Bash,Edit,Write,MultiEdit,NotebookEdit",
                    ]);
                }
                command
                    .args([
                        "--print",
                        "--output-format",
                        "stream-json",
                        "--verbose",
                        "--permission-mode",
                        if self.workspace_write {
                            "acceptEdits"
                        } else {
                            "dontAsk"
                        },
                        "--tools",
                        tools,
                        "--allowedTools",
                        if mode == Mode::Workflow {
                            "Read,Glob,Grep,Workflow"
                        } else {
                            "Read,Glob,Grep"
                        },
                        "--",
                    ])
                    .arg(task)
                    .stdin(Stdio::null());
            }
        }
        Ok(command)
    }

    fn sandbox(&self) -> &'static str {
        if self.workspace_write {
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
            .map_err(|_| "Could not start the installed worker client.")?;
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

async fn run_session(
    command: Command,
    runtime: Runtime,
    mode: Mode,
    state: &Mutex<State>,
    secrets: &[String],
) -> Result<&'static str, RelayError> {
    let mut process = WorkerProcess::spawn(command)?;
    let mut lines = process.output();
    let mut used_workflow = false;
    while let Some(line) = lines.next_line().await? {
        if let Ok(event) = serde_json::from_str::<Value>(&line) {
            if mode == Mode::Workflow && event["type"] == "assistant" {
                used_workflow |= event["message"]["content"]
                    .as_array()
                    .is_some_and(|blocks| {
                        blocks
                            .iter()
                            .any(|block| block["type"] == "tool_use" && block["name"] == "Workflow")
                    });
            }
            relay_progress(&event, runtime, state, secrets);
        }
    }
    let success = process.child.wait().await?.success();
    process.group = None;
    if !success {
        return Err(
            "The worker client exited with an error. Check its session or run ox doctor.".into(),
        );
    }
    if mode == Mode::Workflow && !used_workflow {
        return Err("Claude Code did not run the Workflow tool. Check the selected model, Claude Code version, and workflow permissions.".into());
    }
    Ok("completed")
}

fn relay_progress(event: &Value, runtime: Runtime, state: &Mutex<State>, secrets: &[String]) {
    let mut state = state.lock().unwrap();
    let (session, result, progress) = match runtime {
        Runtime::Codex => {
            let item = &event["item"];
            let text = (item["type"] == "agent_message")
                .then(|| item["text"].as_str())
                .flatten();
            (event["thread_id"].as_str(), text, event["type"].as_str())
        }
        Runtime::Claude => (
            event["session_id"].as_str(),
            event["result"].as_str(),
            event["type"].as_str(),
        ),
    };
    if let Some(id) = session {
        state.native_session = Some(clean(id, secrets));
    }
    if let Some(text) = result {
        state.result = Some(clean(text, secrets));
    }
    if let Some(kind) = progress {
        state.progress = clean(kind, secrets);
    }
}

async fn run_goal(
    command: Command,
    request: &Start,
    state: &Mutex<State>,
    secrets: &[String],
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
        json!({"clientInfo":{"name":"ox-worker","version": env!("CARGO_PKG_VERSION")}}),
    )
    .await?;
    write_rpc(&mut input, json!({"method":"initialized"})).await?;
    let session = rpc(
        &mut input,
        &mut lines,
        2,
        "thread/start",
        json!({
            "model": request.model,
            "sandbox": request.sandbox(),
            "approvalPolicy": "never",
        }),
    )
    .await?;
    let thread = session["thread"]["id"]
        .as_str()
        .ok_or("Codex returned no worker session ID.")?;
    state.lock().unwrap().native_session = Some(clean(thread, secrets));
    rpc(
        &mut input,
        &mut lines,
        3,
        "thread/goal/set",
        json!({
            "threadId": thread,
            "objective": crate::upstream::redact(request.task.trim(), secrets),
            "status": "active",
        }),
    )
    .await?;
    let mut terminal = None;
    let mut in_turn = false;
    while let Some(line) = lines.next_line().await? {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        deny_approval(&mut input, &event).await?;
        let params = &event["params"];
        match event["method"].as_str() {
            Some("turn/started") => in_turn = true,
            Some("turn/completed") => {
                in_turn = false;
                if params["turn"]["status"] == "failed" {
                    return Err(
                        "A Codex goal turn failed. Resume the worker session to see its error."
                            .into(),
                    );
                }
            }
            Some("thread/goal/updated") => {
                let status = params["goal"]["status"].as_str().unwrap_or("active");
                state.lock().unwrap().progress = format!("Goal {status}");
                terminal = match status {
                    "complete" => Some("completed"),
                    "blocked" | "paused" | "usageLimited" | "budgetLimited" => Some("stopped"),
                    _ => None,
                };
            }
            Some("item/completed") => {
                let item = &params["item"];
                if item["type"] == "agentMessage"
                    && let Some(text) = item["text"].as_str()
                {
                    state.lock().unwrap().result = Some(clean(text, secrets));
                }
            }
            _ => {}
        }
        if !in_turn && let Some(status) = terminal {
            return Ok(status);
        }
    }
    Err("Codex app-server exited before the goal stopped.".into())
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

fn clean(text: &str, secrets: &[String]) -> String {
    let text = crate::upstream::redact(text, secrets);
    text[..text.floor_char_boundary(MAX_TEXT)].to_owned()
}

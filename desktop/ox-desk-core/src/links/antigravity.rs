//! Google's Antigravity CLI speaks ACP JSON-RPC over newline-delimited stdio.
use crate::runtime::RequestId;
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

#[derive(Debug)]
pub enum Event {
    Reply {
        id: i64,
        result: Result<Value, String>,
    },
    Update(Value),
    Permission {
        id: RequestId,
        params: Value,
    },
    Closed(String),
}

pub struct Link {
    writes: mpsc::Sender<Outgoing>,
    close: mpsc::Sender<Closing>,
}

enum Outgoing {
    Frame(Value),
    CloseStdin,
}

enum Closing {
    OutputClosed,
    Failed(String),
    Dropped,
}

/// Resolve before changing cwd so a checkout cannot supply the executable via a relative PATH.
pub fn executable(path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("agy"))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

fn command(path: &OsStr, project: &Path, yolo: bool) -> io::Result<Command> {
    let program = executable(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Install the Google Antigravity CLI (agy) on this host",
        )
    })?;
    let mut command = Command::new(program);
    command.arg("acp").current_dir(project).env("PATH", path);
    // Antigravity's bypass switch applies to the entire process, independently of its ACP mode.
    if yolo {
        command.arg("--dangerously-skip-permissions");
    }
    Ok(command)
}

impl Link {
    pub fn spawn(
        path: &OsStr,
        project: &Path,
        yolo: bool,
        deliver: impl Fn(Event) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        Self::from_command(command(path, project, yolo)?, deliver)
    }

    fn from_command(
        mut command: Command,
        deliver: impl Fn(Event) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // ACP reserves stderr for logs. Keep raw provider diagnostics off the host wire.
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let mut output = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut input = BufWriter::new(child.stdin.take().expect("piped stdin"));
        let (writes, messages) = mpsc::channel::<Outgoing>();
        let (close, closing) = mpsc::channel();
        let deliver = Arc::new(deliver);

        let output_event = deliver.clone();
        let read_close = close.clone();
        let response_writer = writes.clone();
        thread::spawn(move || {
            let result = read_messages(&mut output, &response_writer, output_event.as_ref());
            let reason = result.map_or_else(
                |e| Closing::Failed(e.to_string()),
                |()| Closing::OutputClosed,
            );
            let _ = read_close.send(reason);
        });

        let write_close = close.clone();
        thread::spawn(move || {
            while let Ok(Outgoing::Frame(value)) = messages.recv() {
                if let Err(e) = write_message(&mut input, &value) {
                    let _ =
                        write_close.send(Closing::Failed(format!("Antigravity input closed: {e}")));
                    break;
                }
            }
        });

        thread::spawn(move || {
            let ended = closing.recv().unwrap_or(Closing::Dropped);
            let group = -(child.id() as i32);
            let grace = Duration::from_millis(250);
            if matches!(ended, Closing::Dropped) {
                // ACP agents can finalize their session when stdin closes.
                thread::sleep(grace);
            }
            // Keep the direct child unreaped through both signals, so the process group ID
            // cannot be recycled while tools and other children are being shut down.
            unsafe { libc::kill(group, libc::SIGTERM) };
            thread::sleep(grace);
            unsafe { libc::kill(group, libc::SIGKILL) };
            let status = child.wait();
            let reason = match ended {
                Closing::Dropped => return,
                Closing::Failed(reason) => reason,
                Closing::OutputClosed => match status {
                    Ok(status) if status.success() => "Antigravity closed its output".into(),
                    Ok(status) => format!("Antigravity closed its output ({status})"),
                    Err(e) => format!("Antigravity closed its output: {e}"),
                },
            };
            deliver(Event::Closed(reason));
        });

        Ok(Self { writes, close })
    }

    pub fn request(&self, id: i64, method: &str, params: Value) -> io::Result<()> {
        send(
            &self.writes,
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}),
        )
    }

    pub fn notify(&self, method: &str, params: Value) -> io::Result<()> {
        send(
            &self.writes,
            json!({"jsonrpc":"2.0", "method":method, "params":params}),
        )
    }

    pub fn reply(&self, id: &RequestId, result: Value) -> io::Result<()> {
        send(
            &self.writes,
            json!({"jsonrpc":"2.0", "id":id, "result":result}),
        )
    }

    pub fn unsupported(&self, id: &RequestId) -> io::Result<()> {
        send(&self.writes, method_not_found(json!(id)))
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.close.send(Closing::Dropped);
        let _ = self.writes.send(Outgoing::CloseStdin);
    }
}

fn send(writes: &mpsc::Sender<Outgoing>, value: Value) -> io::Result<()> {
    writes
        .send(Outgoing::Frame(value))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Antigravity writer closed"))
}

fn write_message(mut writer: impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn method_not_found(id: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32601, "message":"Method not found"}})
}

fn read_messages(
    mut output: impl BufRead,
    writes: &mpsc::Sender<Outgoing>,
    deliver: impl Fn(Event),
) -> io::Result<()> {
    const FRAME_LIMIT: u64 = 64 * 1024 * 1024;
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        let count = output
            .by_ref()
            .take(FRAME_LIMIT + 1)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            return Ok(());
        }
        if count as u64 > FRAME_LIMIT {
            return Err(io::Error::other(
                "Antigravity output exceeded the ACP frame limit",
            ));
        }
        let message = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::other(format!("Invalid Antigravity ACP JSON: {e}")))?;
        read_message(message, writes, &deliver)?;
    }
}

fn read_message(
    mut message: Value,
    writes: &mpsc::Sender<Outgoing>,
    deliver: impl Fn(Event),
) -> io::Result<()> {
    let invalid = || io::Error::other("Invalid Antigravity ACP message");
    let fields = message.as_object_mut().ok_or_else(invalid)?;
    if fields.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(invalid());
    }
    let id = fields.remove("id");
    if let Some(method) = fields.remove("method") {
        let params = fields.remove("params").unwrap_or(Value::Null);
        return match (method.as_str().ok_or_else(invalid)?, id) {
            ("session/update", None) => {
                deliver(Event::Update(params));
                Ok(())
            }
            ("session/request_permission", Some(id)) if !id.is_null() => {
                let id = serde_json::from_value(id).map_err(|_| invalid())?;
                deliver(Event::Permission { id, params });
                Ok(())
            }
            // A null ID cannot identify a permission prompt in OX. Cancel it on the wire.
            ("session/request_permission", Some(id)) => send(
                writes,
                json!({
                    "jsonrpc":"2.0", "id":id, "result":{"outcome":{"outcome":"cancelled"}}
                }),
            ),
            (_, Some(id)) => send(writes, method_not_found(id)),
            (_, None) => Ok(()),
        };
    }

    let result = match (fields.remove("result"), fields.remove("error")) {
        (Some(result), None) => Ok(result),
        (None, Some(error)) => {
            let code = error["code"].as_i64().ok_or_else(invalid)?;
            let message = error["message"].as_str().ok_or_else(invalid)?;
            Err(format!("Antigravity ACP error {code}: {message}"))
        }
        _ => return Err(invalid()),
    };
    // All OX requests use integer IDs. Uncorrelated protocol errors use a null ID.
    let Some(id) = id.and_then(|v| v.as_i64()) else {
        return Err(io::Error::other(
            result.err().unwrap_or_else(|| invalid().to_string()),
        ));
    };
    deliver(Event::Reply { id, result });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_subprocess_preserves_wire_order_and_request_ids() {
        let mut command = Command::new("python3");
        command.args([
            "-c",
            r#"
import json, os, sys
def receive():
    message = json.loads(sys.stdin.readline())
    assert message.pop('jsonrpc') == '2.0'
    return message
def send(**message):
    print(json.dumps(dict(jsonrpc='2.0', **message)), flush=True)

assert receive() == {'id': 42, 'method': 'initialize', 'params': {'protocolVersion': 1}}
send(id=42, result={'protocolVersion': 1})
send(id=11, method='terminal/create', params={})
assert receive() == {'id': 11, 'error': {'code': -32601, 'message': 'Method not found'}}
send(method='unused/notification', params={})
send(method='session/update', params={'sessionId': 's'})
send(id='ask:1', method='session/request_permission', params={'sessionId': 's'})
assert receive() == {'id': 'ask:1', 'result': {'outcome': {'outcome': 'selected', 'optionId': 'allow'}}}
assert receive() == {'method': 'session/cancel', 'params': {'sessionId': 's'}}
send(id=43, error={'code': -32000, 'message': 'Authentication required'})
print('Provider diagnostic never sent over the OX wire', file=sys.stderr)
os._exit(29)
"#,
        ]);
        let (sender, receiver) = mpsc::channel();
        let link = Link::from_command(command, move |event| sender.send(event).unwrap()).unwrap();
        let next = || receiver.recv_timeout(Duration::from_secs(5)).unwrap();

        link.request(42, "initialize", json!({"protocolVersion": 1}))
            .unwrap();
        assert!(
            matches!(next(), Event::Reply { id: 42, result: Ok(value) } if value["protocolVersion"] == 1)
        );
        assert!(matches!(next(), Event::Update(params) if params["sessionId"] == "s"));
        let Event::Permission { id, params } = next() else {
            panic!("expected a permission request")
        };
        assert_eq!(id, RequestId::Text("ask:1".into()));
        assert_eq!(params["sessionId"], "s");
        link.reply(
            &id,
            json!({"outcome":{"outcome":"selected","optionId":"allow"}}),
        )
        .unwrap();
        link.notify("session/cancel", json!({"sessionId":"s"}))
            .unwrap();
        assert!(
            matches!(next(), Event::Reply { id: 43, result: Err(error) } if error == "Antigravity ACP error -32000: Authentication required")
        );
        assert!(
            matches!(next(), Event::Closed(reason) if reason == "Antigravity closed its output (exit status: 29)")
        );
    }

    #[test]
    fn agy_uses_an_absolute_executable_and_only_bypasses_on_request() {
        let skipped = tempfile::tempdir().unwrap();
        let trusted = tempfile::tempdir().unwrap();
        std::fs::write(skipped.path().join("agy"), "no execute permission").unwrap();
        let binary = trusted.path().join("agy");
        std::fs::write(&binary, "executable placeholder").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([Path::new("."), skipped.path(), trusted.path()]).unwrap();
        let stock = command(&path, skipped.path(), false).unwrap();
        assert_eq!(stock.get_program(), binary);
        assert_eq!(stock.get_current_dir(), Some(skipped.path()));
        assert!(stock.get_args().eq(["acp"]));

        let bypass = command(&path, skipped.path(), true).unwrap();
        assert!(
            bypass
                .get_args()
                .eq(["acp", "--dangerously-skip-permissions"])
        );
    }
}

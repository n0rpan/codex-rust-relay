//! Private stream-json peer used by transport and packaged GUI tests.
use ox_desk_core::perf::monotonic_ns;
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{self, BufRead, BufWriter, Write},
    os::unix::net::UnixDatagram,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

type Out = Arc<Mutex<BufWriter<io::Stdout>>>;

fn send(out: &Out, frame: Value) -> io::Result<()> {
    let mut w = out.lock().unwrap();
    serde_json::to_writer(&mut *w, &frame)?;
    w.write_all(b"\n")?;
    w.flush()
}

fn trace(event: &str, id: &str) -> io::Result<()> {
    let Some(path) = env::var_os("OX_DESK_SYNTH_TRACE") else {
        return Ok(());
    };
    let mut bytes = serde_json::to_vec(&json!({"event":event,"id":id,"ns":monotonic_ns()}))?;
    bytes.push(b'\n');
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&bytes)
}

fn result(out: &Out, id: &str, subtype: &str) -> io::Result<()> {
    send(
        out,
        json!({"type":"result","session_id":id,"subtype":subtype,"is_error":false,"stop_reason":"end_turn"}),
    )
}

fn partial(out: &Out, id: &str, event: Value) -> io::Result<()> {
    send(
        out,
        json!({"type":"stream_event","session_id":id,"parent_tool_use_id":null,"event":event}),
    )
}

fn stream(out: &Out, id: &str, cancel: &AtomicBool) -> io::Result<()> {
    let message = format!("stream-{id}");
    partial(
        out,
        id,
        json!({"type":"message_start","message":{"id":message}}),
    )?;
    partial(
        out,
        id,
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
    )?;
    trace("claude-stream-start", id)?;
    let start = Instant::now();
    let paragraph =
        "A line of synthetic output with inline code and a formatted paragraph.\n\n".repeat(3);
    let chunk = &paragraph[..200];
    for i in 0..1000 {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        partial(
            out,
            id,
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":chunk}}),
        )?;
        if let Some(delay) =
            (start + Duration::from_millis(5 * (i + 1))).checked_duration_since(Instant::now())
        {
            thread::sleep(delay);
        }
    }
    send(
        out,
        json!({"type":"assistant","session_id":id, "parent_tool_use_id":null,
        "message":{"id":message,"role":"assistant","model":"demo","content":[{"type":"text","text":chunk.repeat(1000)}]}}),
    )?;
    result(out, id, "success")?;
    trace("claude-stream-end", id)
}

fn admin(out: &Out, id: &str, cancel: Arc<AtomicBool>) -> io::Result<()> {
    let Some(prefix) = env::var_os("OX_DESK_SYNTH_CONTROL") else {
        return Ok(());
    };
    let path = PathBuf::from(prefix).with_file_name(format!("c-{id}"));
    let socket = UnixDatagram::bind(path)?;
    let (out, id) = (out.clone(), id.to_owned());
    thread::spawn(move || -> io::Result<()> {
        let mut cmd = [0; 16];
        loop {
            let n = socket.recv(&mut cmd)?;
            if &cmd[..n] == b"stream" {
                stream(&out, &id, &cancel)?;
            }
        }
    });
    Ok(())
}

fn script(path: &Path, out: &Out) -> io::Result<()> {
    let mut stdin = io::stdin().lock().lines();
    let mut captured = Value::Null;
    for (i, line) in fs::read_to_string(path)?.lines().enumerate() {
        let step: Value = serde_json::from_str(line)?;
        if let Some(expected) = step["expect"].as_object() {
            let frame: Value = serde_json::from_str(
                &stdin
                    .next()
                    .ok_or_else(|| io::Error::other("Desktop closed early"))??,
            )?;
            for (pointer, value) in expected {
                if frame.pointer(pointer) != Some(value) {
                    return Err(io::Error::other(format!(
                        "Unexpected {pointer} at Claude step {}",
                        i + 1
                    )));
                }
            }
            captured = frame["request_id"].clone();
        } else if let Some(mut frame) = step.get("send").cloned() {
            if frame["cwd"] == "$cwd" {
                frame["cwd"] = json!(env::current_dir()?);
            }
            if frame["response"]["request_id"] == "$request" {
                frame["response"]["request_id"] = captured.clone();
            }
            send(out, frame)?;
        } else {
            return Err(io::Error::other("Expected a Claude script send or expect"));
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.iter().any(|s| s == "--version") {
        println!("2.1.331 (Claude Code)");
        return Ok(());
    }
    if args.iter().any(|s| s == "status") && args.iter().any(|s| s == "auth") {
        println!(
            "{}",
            json!({"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max"})
        );
        return Ok(());
    }
    let out: Out = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let path = env::current_dir()?.join("fake-claude.jsonl");
    if path.is_file() {
        return script(&path, &out);
    }
    let arg = |name: &str| {
        args.windows(2)
            .find(|p| p[0] == name)
            .map(|p| p[1].as_str())
    };
    let id = arg("--session-id")
        .or_else(|| arg("--resume"))
        .ok_or_else(|| io::Error::other("Missing fixture session ID"))?;
    let model = arg("--model").unwrap_or("claude-fixture");
    let native = arg("--model").is_none();
    // Look at what reached the child, including launch-scoped settings, without
    // logging any command line or environment value.
    let contains_canary = |s: &str| s.contains("ox_gate_secret_");
    let leaked = env::vars().any(|(_, v)| contains_canary(&v))
        || args.iter().any(|s| contains_canary(s))
        || arg("--settings")
            .and_then(|s| fs::read_to_string(s).ok())
            .is_some_and(|s| contains_canary(&s));
    trace(
        if leaked {
            "claude-credential-leak"
        } else {
            "claude-clean-env"
        },
        id,
    )?;
    let cancel = Arc::new(AtomicBool::new(false));
    admin(&out, id, cancel.clone())?;
    let mut initialized = false;
    for line in io::stdin().lock().lines() {
        let frame: Value = serde_json::from_str(&line?)?;
        if frame["type"] == "control_request" {
            let subtype = &frame["request"]["subtype"];
            if subtype == "interrupt" {
                trace("claude-stop-read", id)?;
                cancel.store(true, Ordering::Release);
            }
            let response = if subtype == "initialize" {
                let account = if native {
                    json!({"apiProvider":"firstParty","subscriptionType":"Claude Max"})
                } else {
                    json!({"apiProvider":"firstParty","tokenSource":"ANTHROPIC_AUTH_TOKEN"})
                };
                json!({"claude_code_version":"2.1.331","current_permission_mode":"default","account":account,
                    "models":[{"value":model,"displayName":model}],"commands":[],"capabilities":[]})
            } else {
                json!({})
            };
            send(
                &out,
                json!({"type":"control_response","response":{"subtype":"success","request_id":frame["request_id"],"response":response}}),
            )?;
            if subtype == "interrupt" {
                result(&out, id, "success")?;
            }
        } else if frame["type"] == "user" {
            if !initialized {
                initialized = true;
                send(
                    &out,
                    json!({"type":"system","subtype":"init","session_id":id,"cwd":env::current_dir()?,"model":model,
                    "permissionMode":"default","slash_commands":[],"tools":["Bash"]}),
                )?;
            }
            cancel.store(false, Ordering::Release);
            send(&out, frame)?;
            trace("claude-user", id)?;
        }
    }
    Ok(())
}

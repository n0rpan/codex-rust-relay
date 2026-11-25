//! Synthetic Codex and OX ports for a private display fixture.
use ox_desk_core::perf::monotonic_ns;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    env,
    fs::OpenOptions,
    io::{self, BufRead, BufWriter, Write},
    os::unix::net::UnixDatagram,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

type Pipe = Arc<Mutex<BufWriter<io::Stdout>>>;
fn write(pipe: &Pipe, value: Value) -> io::Result<()> {
    let mut out = pipe.lock().unwrap();
    serde_json::to_writer(&mut *out, &value)?;
    out.write_all(b"\n")?;
    out.flush()
}
fn event(pipe: &Pipe, method: &str, params: Value) -> io::Result<()> {
    write(pipe, json!({"method":method,"params":params}))
}
fn trace(kind: &str, id: &str) {
    if let Some(path) = env::var_os("OX_DESK_SYNTH_TRACE")
        && let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path)
    {
        let line = json!({"ns":monotonic_ns(),"event":kind,"id":id,"pid":std::process::id()})
            .to_string()
            + "\n";
        let _ = f.write_all(line.as_bytes());
    }
}
fn number(key: &str, default: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
fn row(id: &str, cwd: &std::path::Path, provider: &str, model: &str) -> Value {
    let n = id
        .trim_start_matches("desk-")
        .parse::<usize>()
        .unwrap_or_default();
    json!({"id":id,"cwd":cwd,"modelProvider":provider,"model":model,"preview":format!("Thread {n:04}"),"updatedAt":1_700_000_000-n as i64})
}
fn admin(pipe: Pipe, opened: Arc<Mutex<HashSet<String>>>) -> io::Result<()> {
    let Some(path) = env::var_os("OX_DESK_SYNTH_CONTROL") else {
        return Ok(());
    };
    let socket = UnixDatagram::bind(PathBuf::from(path))?;
    thread::spawn(move || {
        let mut input = [0; 64];
        while let Ok((n, _)) = socket.recv_from(&mut input) {
            let mut ids: Vec<_> = opened.lock().unwrap().iter().cloned().collect();
            ids.sort();
            let stream = match &input[..n] {
                b"stream" => true,
                b"busy" => false,
                _ => continue,
            };
            let turn = if stream { "stream" } else { "load" };
            let pipe = pipe.clone();
            thread::spawn(move || -> io::Result<()> {
                for id in &ids {
                    event(
                        &pipe,
                        "turn/started",
                        json!({"threadId":id,"turn":{"id":turn,"status":"inProgress"}}),
                    )?;
                }
                if !stream {
                    return Ok(());
                }
                trace("stream-start", &ids.len().to_string());
                let start = Instant::now();
                let piece =
                    "A line of generated output with inline code and a formatted paragraph.\n\n"
                        .repeat(3);
                for i in 0..1000 {
                    for id in &ids {
                        event(
                            &pipe,
                            "item/agentMessage/delta",
                            json!({"threadId":id,"turnId":turn,"itemId":"output","delta":&piece[..200]}),
                        )?;
                    }
                    if let Some(delay) = (start + Duration::from_millis(5 * (i + 1)))
                        .checked_duration_since(Instant::now())
                    {
                        thread::sleep(delay);
                    }
                }
                for id in &ids {
                    event(
                        &pipe,
                        "turn/completed",
                        json!({"threadId":id,"turn":{"id":turn,"status":"completed"}}),
                    )?;
                }
                trace("stream-end", "");
                Ok(())
            });
        }
    });
    Ok(())
}

pub fn run() -> io::Result<()> {
    // The desktop's header asks the helper for weather at startup. Answer nothing: running
    // as a synthetic server here would bind the control socket first and leave it behind,
    // so the real synthetic server would fail to bind it and the catalog would never load.
    if env::args().any(|s| s == "weather") {
        return Ok(());
    }
    if env::args().any(|s| s == "--ox-desktop-settings-v1") {
        let q: Value = serde_json::from_reader(io::stdin().lock())?;
        trace("helper", q["action"].as_str().unwrap_or_default());
        let result = match q["action"].as_str() {
            Some("legacy_workspace") => Value::Null,
            Some("codex_profiles") => json!({"subscription":null,"api":null}),
            Some("claude_profiles") => {
                let home = std::path::PathBuf::from(env::var_os("HOME").unwrap_or_default());
                json!({"subscription":home.join("absent-native"),"api":home.join("absent-api")})
            }
            Some("read" | "read_rows") => {
                let models: Vec<&str> = if env::var_os("OX_DESK_SYNTH_ROUTES").is_some() {
                    vec!["fixture-model"]
                } else {
                    vec![]
                };
                let mut result = json!({"version":1,"credentials":[],"models":models,"api_provider":"codeox","chatgpt_account":"saved_sign_in","claude_account":false,"presets":[]});
                if q["action"] == "read_rows" {
                    result["revisions"] =
                        json!({"registry":"","favorites":"","legacy":"","keys":""});
                    result["registry"] = json!({"providers":[],"search":{}});
                    result["favorites"] = json!([]);
                    result["issues"] = json!([]);
                    result["legacy"] = Value::Null;
                } else {
                    result["documents"] = json!([])
                }
                result
            }
            _ => json!([]),
        };
        println!("{result}");
        return Ok(());
    }
    let pipe = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let opened = Arc::new(Mutex::new(HashSet::new()));
    let api = env::args().any(|arg| arg == "--model");
    let (provider, model) = if api {
        ("codeox", "fixture-model")
    } else {
        ("openai", "synthetic")
    };
    if !api {
        admin(pipe.clone(), opened.clone())?;
    }
    let cwd = env::current_dir()?;
    let size = number("OX_DESK_SYNTH_THREADS", 30);
    let long = number("OX_DESK_SYNTH_ITEMS", 1);
    let mut new_id = size;
    trace("spawn", provider);
    for line in io::stdin().lock().lines() {
        let q: Value = serde_json::from_str(&line?)?;
        let p = &q["params"];
        let seq = &q["id"];
        let id = p["threadId"].as_str().unwrap_or_default();
        let row = |thread: &str| row(thread, &cwd, provider, model);
        let result = match q["method"].as_str() {
            Some("initialize") => json!({}),
            Some("initialized") => continue,
            Some("account/read") => json!({"account":{"type":"chatgpt"}}),
            Some("thread/list") => {
                let start = p["cursor"]
                    .as_str()
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or_default()
                    .min(size);
                let query = p["searchTerm"]
                    .as_str()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                trace("query", &query);
                if query == "hold" {
                    trace("list-held", "");
                    let all: Vec<_> = (0..size).map(|i| row(&format!("desk-{i:04}"))).collect();
                    let pipe = pipe.clone();
                    let seq = seq.clone();
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(450));
                        let _ = write(&pipe, json!({"id":seq,"result":{"data":all}}));
                    });
                    continue;
                }
                let limit = p["limit"].as_u64().unwrap_or(50) as usize;
                let mut next = start;
                let mut rows = Vec::new();
                while next < size && rows.len() < limit {
                    let item = row(&format!("desk-{next:04}"));
                    next += 1;
                    if query.is_empty()
                        || item["preview"]
                            .as_str()
                            .unwrap()
                            .to_ascii_lowercase()
                            .contains(&query)
                    {
                        rows.push(item);
                    }
                }
                json!({"data":rows,"nextCursor":(next<size).then(||next.to_string())})
            }
            Some("thread/read") => json!({"thread":row(id)}),
            Some("thread/resume") => {
                opened.lock().unwrap().insert(id.into());
                let n = if id == "desk-0000" { long } else { 1 };
                let items:Vec<_>=(0..n).map(|i|json!({"id":i.to_string(),"type":"agentMessage","text":format!("Response {i:04}")})).collect();
                let reply = json!({"thread":row(id),"modelProvider":provider,"model":model,"initialTurnsPage":{"data":[{"id":"history","status":"completed","items":items}]}});
                trace("history-reply", id);
                reply
            }
            Some("thread/start") => {
                let id = format!("desk-{new_id:04}");
                new_id += 1;
                trace("new-thread", &id);
                opened.lock().unwrap().insert(id.clone());
                json!({"thread":row(&id),"modelProvider":provider,"model":model})
            }
            Some("turn/start") => {
                trace("send", id);
                json!({"turn":{"id":"load","status":"inProgress"}})
            }
            Some("turn/interrupt") => {
                trace("stop-read", id);
                event(
                    &pipe,
                    "turn/completed",
                    json!({"threadId":id,"turn":{"id":"load","status":"interrupted"}}),
                )?;
                json!({})
            }
            _ => {
                write(
                    &pipe,
                    json!({"id":seq,"error":{"code":-32601,"message":"unsupported synthetic request"}}),
                )?;
                continue;
            }
        };
        write(&pipe, json!({"id":seq,"result":result}))?;
    }
    Ok(())
}

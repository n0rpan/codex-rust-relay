//! Opt-in check of OX context delivery through stock Claude Code binaries and a
//! local mock Messages API. Nothing reaches a real provider and no inference is
//! billed. Run with, for example:
//!
//! OX_CLAUDE_PROBE_BINS=/path/to/2.1.331/claude:/usr/lib/claude-code/claude \
//!   cargo test -p ox-desk-core --test claude_prompt_context_live -- --ignored
use ox_desk_core::{
    claude::Event as Frame,
    links::claude::{Billing, Event, Launch, Link, Session},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

const DESKTOP: &str = "This message was sent from OX Desktop.";
const PHONE: &str = "This message was sent from the OX phone interface.";
const MODEL: &str = "claude-sonnet-4-5";

type Log = Arc<Mutex<Vec<Value>>>;

/// Answers every Messages request with a short text reply. A request whose last
/// user message mentions SLOW waits first, so later sends queue behind it.
fn mock() -> (u16, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let requests = log.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let requests = requests.clone();
            thread::spawn(move || serve(stream, &requests));
        }
    });
    (port, log)
}
fn serve(stream: TcpStream, log: &Log) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut head = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
        if line == "\r\n" {
            break;
        }
        head.push_str(&line);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let mut out = stream;
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let respond = |out: &mut TcpStream, kind: &str, text: String| {
        let _ = write!(
            out,
            "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
            text.len()
        );
    };
    if !head.starts_with("POST /v1/messages") {
        return respond(&mut out, "application/json", "{}".into());
    }
    if head.starts_with("POST /v1/messages/count_tokens") {
        return respond(&mut out, "application/json", r#"{"input_tokens":1}"#.into());
    }
    log.lock().unwrap().push(body.clone());
    if body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .is_some_and(|m| m.to_string().contains("SLOW"))
    {
        thread::sleep(Duration::from_secs(3));
    }
    let id = format!("msg_{}", log.lock().unwrap().len());
    let message = json!({"id":id,"type":"message","role":"assistant","model":MODEL,"content":[],
        "stop_reason":null,"usage":{"input_tokens":1,"output_tokens":1}});
    if body["stream"] != true {
        let mut m = message;
        m["content"] = json!([{"type":"text","text":"mock-reply"}]);
        m["stop_reason"] = json!("end_turn");
        return respond(&mut out, "application/json", m.to_string());
    }
    let events = [
        json!({"type":"message_start","message":message}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"mock-reply"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ];
    let sse: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    respond(&mut out, "text/event-stream", sse);
}

/// Stands in for `ox`: drops OX's `claude` subcommand and starts stock Claude
/// against the mock with a throwaway profile and a placeholder token.
fn shim(root: &Path, claude: &str, port: u16) -> std::path::PathBuf {
    let config = root.join("config");
    fs::create_dir_all(&config).unwrap();
    let exe = root.join("ox");
    fs::write(
        &exe,
        format!(
            "#!/bin/sh\nshift\nfor v in $(env | sed -n 's/^\\(ANTHROPIC_[A-Z_]*\\|CLAUDE_[A-Z_]*\\)=.*/\\1/p'); do unset \"$v\"; done\n\
             export ANTHROPIC_BASE_URL=http://127.0.0.1:{port} ANTHROPIC_AUTH_TOKEN=mock-token \
             CLAUDE_CONFIG_DIR='{}' CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 DISABLE_AUTOUPDATER=1\n\
             exec '{claude}' \"$@\"\n",
            config.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
    exe
}

fn text(s: &str) -> Vec<Value> {
    vec![json!({"type":"text","text":s})]
}
/// The text of each user-role block in the last main-loop request.
fn conversation(log: &Log) -> Vec<String> {
    let log = log.lock().unwrap();
    let last = log.iter().rev().find(|b| b["tools"].is_array()).unwrap();
    last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| match &m["content"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b["text"].as_str().map(str::to_owned))
                .collect(),
            _ => vec![],
        })
        .collect()
}
fn context_before(blocks: &[String], user: &str) -> String {
    let at = blocks
        .iter()
        .position(|b| b.trim_end() == user)
        .unwrap_or_else(|| panic!("{user:?} missing from {blocks:#?}"));
    blocks[..at]
        .iter()
        .rev()
        .find(|b| b.contains("UserPromptSubmit hook additional context"))
        .cloned()
        .unwrap_or_default()
}

#[test]
#[ignore = "needs stock Claude Code binaries in OX_CLAUDE_PROBE_BINS"]
fn stock_claude_receives_stable_facts_and_each_submit_label() {
    let bins = std::env::var("OX_CLAUDE_PROBE_BINS").expect("OX_CLAUDE_PROBE_BINS");
    for claude in bins.split(':').filter(|s| !s.is_empty()) {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let (port, log) = mock();
        let (tx, rx) = mpsc::channel();
        let link = Link::spawn(
            Launch {
                ox: shim(root.path(), claude, port),
                path: std::env::var_os("PATH").unwrap(),
                project: project.clone(),
                billing: Billing::Api(MODEL.into()),
                session: Session::New("6f1c2b55-2f43-4d0e-9a51-3e7c1d0a9b21".into()),
                model: None,
                permission_mode: Some("default".into()),
                contract: None,
            },
            move |e| {
                let _ = tx.send(e);
            },
        )
        .unwrap();
        let results = |n: usize| {
            let until = Instant::now() + Duration::from_secs(60);
            let mut seen = 0;
            while seen < n {
                match rx.recv_timeout(until - Instant::now()) {
                    Ok(Event::Frame(Frame::Result(_))) => seen += 1,
                    Ok(Event::Closed { reason }) => panic!("{claude}: {reason}"),
                    Ok(Event::Frame(Frame::UnsupportedRequest { subtype, .. })) => {
                        panic!("{claude}: unanswered {subtype}")
                    }
                    Ok(_) => {}
                    Err(e) => panic!("{claude}: waiting for {n} results: {e}"),
                }
            }
        };
        let until = Instant::now() + Duration::from_secs(30);
        while link
            .send_user_with_context(
                "10000000-0000-4000-8000-000000000001",
                text("first SLOW"),
                DESKTOP,
            )
            .is_err()
        {
            assert!(Instant::now() < until, "{claude}: never initialized");
            thread::sleep(Duration::from_millis(50));
        }
        thread::sleep(Duration::from_millis(800));
        // Queued during a busy desktop turn, from the phone and then the desktop.
        link.send_user_with_context(
            "10000000-0000-4000-8000-000000000002",
            text("from phone"),
            PHONE,
        )
        .unwrap();
        link.send_user_with_context(
            "10000000-0000-4000-8000-000000000003",
            text("from desktop"),
            DESKTOP,
        )
        .unwrap();
        results(2);
        // A phone message queued behind a turn the desktop interrupts.
        link.send_user_with_context(
            "10000000-0000-4000-8000-000000000004",
            text("second SLOW"),
            DESKTOP,
        )
        .unwrap();
        thread::sleep(Duration::from_millis(800));
        link.send_user_with_context(
            "10000000-0000-4000-8000-000000000005",
            text("queued phone"),
            PHONE,
        )
        .unwrap();
        thread::sleep(Duration::from_millis(200));
        link.interrupt().unwrap();
        results(2);
        // A legacy send carries no label and must not inherit one.
        link.send_user("10000000-0000-4000-8000-000000000006", text("legacy"))
            .unwrap();
        results(1);

        let system = {
            let log = log.lock().unwrap();
            log.iter().rev().find(|b| b["tools"].is_array()).unwrap()["system"].to_string()
        };
        assert!(system.contains("This session is hosted by OX"), "{claude}");
        let blocks = conversation(&log);
        let c = |u| context_before(&blocks, u);
        assert!(c("first SLOW").contains(DESKTOP), "{claude}: {blocks:#?}");
        let merged = c("from phone");
        eprintln!("{claude}: queued context {merged:?}");
        if merged.contains("combines 2 messages") {
            assert!(merged.find(PHONE).unwrap() < merged.find(DESKTOP).unwrap());
        } else {
            // Claude ran the queued messages as separate turns.
            assert!(merged.contains(PHONE) && c("from desktop").contains(DESKTOP));
        }
        assert!(c("queued phone").contains(PHONE), "{claude}: {blocks:#?}");
        assert!(c("legacy").contains("could not determine"), "{claude}");
        // Every user text arrived exactly once and unchanged.
        for u in [
            "first SLOW",
            "from phone",
            "from desktop",
            "queued phone",
            "legacy",
        ] {
            assert_eq!(
                blocks.iter().filter(|b| b.trim_end() == u).count(),
                1,
                "{claude}: {u}: {blocks:#?}"
            );
        }
        drop(link);
    }
}

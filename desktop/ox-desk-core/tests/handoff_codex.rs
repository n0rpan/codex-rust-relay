//! A handoff into stock Codex: the provider's first request carries the selected
//! earlier messages in order, after Codex's own context, then the new request.
//! Runs the real desktop store against the installed Codex with a local
//! Responses provider. CI runs it for the floor and latest Codex.
use ox_desk_core::{
    handoff::{self, HandoffSource, Kind, SourceItem},
    host_protocol::Scope,
    links::codex::Link,
    modes::Decision,
    runtime::{Route, Runtime},
    store::{Connection, Profile, Store},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::Command,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

/// A minimal Responses provider that records each request body.
fn provider() -> (u16, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<Value>>> = Default::default();
    let record = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let record = record.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut stream = stream;
                loop {
                    let mut length = 0usize;
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((k, v)) = line.split_once(':')
                            && k.eq_ignore_ascii_case("content-length")
                        {
                            length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0; length];
                    if reader.read_exact(&mut body).is_err() {
                        return;
                    }
                    let n = {
                        let mut r = record.lock().unwrap();
                        r.push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                        r.len()
                    };
                    let item = json!({"type":"message","role":"assistant","id":format!("msg_{n}"),"status":"completed",
                        "content":[{"type":"output_text","text":format!("reply {n}"),"annotations":[]}]});
                    let events = [
                        json!({"type":"response.created","response":{"id":format!("resp_{n}")}}),
                        json!({"type":"response.output_item.done","output_index":0,"item":item}),
                        json!({"type":"response.completed","response":{"id":format!("resp_{n}"),"usage":{
                            "input_tokens":1,"output_tokens":1,"total_tokens":2,
                            "input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}}),
                    ];
                    let sse: String = events
                        .iter()
                        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
                        .collect();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                        sse.len()
                    );
                    if stream.write_all(head.as_bytes()).is_err()
                        || stream.write_all(sse.as_bytes()).is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    (port, seen)
}

/// User and assistant texts in model order, without Codex's own context.
fn conversation(request: &Value) -> Vec<(String, String)> {
    request["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| {
            i["type"] == "message" && matches!(i["role"].as_str(), Some("user" | "assistant"))
        })
        .map(|i| {
            let text = i["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ");
            (i["role"].as_str().unwrap().to_owned(), text)
        })
        .filter(|(_, text)| {
            !text.starts_with("<environment") && !text.starts_with("<user_instructions")
        })
        .collect()
}

#[test]
#[ignore = "CI runs against the floor and latest installed Codex binaries"]
fn a_codex_handoff_sends_carried_messages_in_order_before_the_request() {
    let bin = std::env::var_os("CODEOX_TEST_CODEX_BIN").expect("path to stock Codex binary");
    let (port, seen) = provider();
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = work.path().canonicalize().unwrap();
    let mut command = Command::new(bin);
    command
        .args([
            "-c",
            &format!(
                "model_providers.fx={{name=\"fx\",base_url=\"http://127.0.0.1:{port}/v1\",wire_api=\"responses\",env_key=\"FX_KEY\"}}"
            ),
            "-c",
            "model_provider=\"fx\"",
            "-c",
            "model=\"demo\"",
            "app-server",
            "--listen",
            "stdio://",
        ])
        .current_dir(&project)
        .env("CODEX_HOME", home.path())
        .env("FX_KEY", "fixture-only")
        .env("NO_PROXY", "127.0.0.1,localhost");
    for k in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
    ] {
        command.env_remove(k);
    }
    let (tx, rx) = mpsc::channel();
    let link = Link::from_command(command, move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    let mut store = Store::open(
        Profile {
            project: project.clone(),
            route: Route::codex_api("demo".into()),
            provider: "fx".into(),
        },
        link,
    )
    .unwrap();
    let pump = |store: &mut Store, until: &dyn Fn(&Store) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !until(store) {
            assert!(
                Instant::now() < deadline,
                "timed out; notice {:?}",
                store.notice()
            );
            if let Ok(e) = rx.recv_timeout(Duration::from_millis(200)) {
                store.apply(e);
            }
            let _ = store.drain(vec![], Instant::now());
        }
    };
    pump(&mut store, &|s| {
        s.state() == &Connection::Ready && s.can_send(None)
    });

    // A Claude source that ran a command, with an earlier exchange that is cut.
    let items = vec![
        SourceItem {
            id: "u1".into(),
            turn: "t1".into(),
            kind: Kind::User,
            text: "HANDOFF first request".into(),
        },
        SourceItem {
            id: "c1".into(),
            turn: "t1".into(),
            kind: Kind::Outcome,
            text: "Command `cargo test` exited 0".into(),
        },
        SourceItem {
            id: "a1".into(),
            turn: "t1".into(),
            kind: Kind::Assistant,
            text: "HANDOFF tests pass".into(),
        },
        SourceItem {
            id: "u2".into(),
            turn: "t2".into(),
            kind: Kind::User,
            text: "HANDOFF second request".into(),
        },
        SourceItem {
            id: "a2".into(),
            turn: "t2".into(),
            kind: Kind::Assistant,
            text: "HANDOFF partial answ".into(),
        },
    ];
    let selection = handoff::select(&items, "NEW request", Some(200_000)).unwrap();
    let source = HandoffSource {
        scope: Scope {
            project: project.clone(),
            route: Runtime::Claude.route(None),
        },
        session: "claude-session".into(),
        native: None,
        title: "Fix the build".into(),
        runtime: Runtime::Claude,
        route: Runtime::Claude.route(None),
        source_access: None,
        source_level: None,
        decision: Decision::Inherited,
        selection: selection.refs.clone(),
    };
    store
        .start_handoff(
            "NEW request".into(),
            handoff::codex_items(&source, &selection),
        )
        .unwrap();
    pump(&mut store, &|_| !seen.lock().unwrap().is_empty());
    let first = seen.lock().unwrap()[0].clone();
    let got = conversation(&first);
    let expect: Vec<(String, String)> = [
        ("user", "<ox-handoff>"),
        ("user", "HANDOFF first request"),
        ("assistant", "[Command `cargo test` exited 0]"),
        ("assistant", "HANDOFF tests pass"),
        ("user", "HANDOFF second request"),
        ("assistant", "HANDOFF partial answ"),
        ("user", "NEW request"),
    ]
    .map(|(r, t)| (r.to_owned(), t.to_owned()))
    .into();
    assert_eq!(got.len(), expect.len(), "{got:#?}");
    for ((role, text), (want_role, want)) in got.iter().zip(&expect) {
        assert_eq!(role, want_role, "{got:#?}");
        assert!(text.starts_with(want.as_str()), "{text:?} vs {want:?}");
    }
    assert!(
        got[0].1.contains("read_ox_thread"),
        "the note points at the read tool"
    );
    let opened = store.take_opened();
    assert!(opened.is_some(), "the store reports the new native session");
}

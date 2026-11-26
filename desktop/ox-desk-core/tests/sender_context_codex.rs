//! OX's sender statement reaches stock Codex as `additionalContext` on `turn/start` and
//! `turn/steer`, through compaction and a resume in a fresh app-server, while the user's
//! text and the visible thread history stay exactly as typed.
//!
//! Runs the real desktop store against the installed Codex with a local Responses
//! provider that records every model request. CI runs it for the floor and latest Codex:
//! `CODEOX_TEST_CODEX_BIN=/path/to/codex cargo test -p ox-desk-core --test
//! sender_context_codex -- --ignored`.
use ox_desk_core::{
    actions::Action,
    host_protocol::Scope,
    links::codex::Link,
    runtime::Route,
    store::{Connection, Profile, Store},
    surface::{Sender, Surface},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::Path,
    process::Command,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

/// A minimal Responses provider that records each request body. A request whose last
/// user message contains `SLOW` is answered after a pause, so a steer can arrive mid-turn.
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
                    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let slow = users(&request).last().is_some_and(|t| t.contains("SLOW"));
                    let n = {
                        let mut r = record.lock().unwrap();
                        r.push(request);
                        r.len()
                    };
                    if slow {
                        std::thread::sleep(Duration::from_secs(4));
                    }
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

fn texts(item: &Value) -> String {
    item["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ")
}
/// The user messages a request carries, in order, without Codex's own context.
fn users(request: &Value) -> Vec<String> {
    request["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["type"] == "message" && i["role"] == "user")
        .map(texts)
        .filter(|t| !t.starts_with('<'))
        .collect()
}
/// (role, text) of every input message that mentions `needle`.
fn mentions(request: &Value, needle: &str) -> Vec<(String, String)> {
    request["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["type"] == "message")
        .map(|i| (i["role"].as_str().unwrap_or("").to_owned(), texts(i)))
        .filter(|(_, t)| t.contains(needle))
        .collect()
}

const DESKTOP: &str = "sent from OX Desktop";
const PHONE: &str = "sent from the OX phone app";

struct Runtime {
    store: Store,
    rx: mpsc::Receiver<ox_desk_core::links::LinkEvent>,
}
impl Runtime {
    fn open(bin: &std::ffi::OsStr, port: u16, home: &Path, project: &Path) -> Self {
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
            .current_dir(project)
            .env("CODEX_HOME", home)
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
        let store = Store::open(
            Profile {
                project: project.to_owned(),
                route: Route::codex_api("demo".into()),
                provider: "fx".into(),
            },
            link,
        )
        .unwrap();
        let mut runtime = Self { store, rx };
        runtime.pump(&|s| s.state() == &Connection::Ready && s.can_send(None));
        runtime
    }
    fn pump(&mut self, until: &dyn Fn(&Store) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !until(&self.store) {
            assert!(
                Instant::now() < deadline,
                "timed out; notice {:?}",
                self.store.notice()
            );
            if let Ok(e) = self.rx.recv_timeout(Duration::from_millis(100)) {
                self.store.apply(e);
            }
            let _ = self.store.drain(vec![], Instant::now());
        }
    }
    fn send(&mut self, thread: Option<&str>, text: &str, surface: Surface) {
        self.store
            .dispatch_as(
                Action::SendText(thread.map(str::to_owned), text.into()),
                Sender::Person { surface },
            )
            .unwrap();
    }
    fn idle(&mut self, thread: &str) {
        self.pump(&|s| {
            s.transcript(thread)
                .is_some_and(|t| t.active_turn().is_none())
                && s.can_send(Some(thread))
        });
    }
}

/// The latest request whose last user message is `text`.
fn request_for(seen: &Mutex<Vec<Value>>, text: &str) -> Option<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .rev()
        .find(|r| users(r).last().map(String::as_str) == Some(text))
        .cloned()
}

#[test]
#[ignore = "CI runs against the floor and latest installed Codex binaries"]
fn codex_receives_each_senders_statement_through_steer_compaction_and_resume() {
    let bin = std::env::var_os("CODEOX_TEST_CODEX_BIN").expect("path to stock Codex binary");
    let (port, seen) = provider();
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = work.path().canonicalize().unwrap();
    let mut rt = Runtime::open(&bin, port, home.path(), &project);
    let wait_for = |rt: &mut Runtime, text: &str| {
        rt.pump(&|_| request_for(&seen, text).is_some());
        request_for(&seen, text).unwrap()
    };

    // 1. A new conversation from the desktop.
    rt.send(None, "first from desktop", Surface::Desktop);
    let first = wait_for(&mut rt, "first from desktop");
    let thread = rt.store.take_opened().expect("opened thread");
    let found = mentions(&first, DESKTOP);
    assert_eq!(found.len(), 1, "one statement: {found:#?}");
    assert_ne!(found[0].0, "user", "context is not user text: {found:#?}");
    assert!(mentions(&first, PHONE).is_empty());
    rt.idle(&thread);

    // 2. The phone continues the same conversation.
    rt.send(Some(&thread), "then from phone", Surface::Phone);
    let second = wait_for(&mut rt, "then from phone");
    assert_eq!(users(&second), ["first from desktop", "then from phone"]);
    let phone = mentions(&second, PHONE);
    assert_eq!(phone.len(), 1, "{phone:#?}");
    rt.idle(&thread);

    // 3. A steer from the desktop while a phone turn runs keeps each sender.
    rt.send(Some(&thread), "SLOW phone turn", Surface::Phone);
    rt.pump(&|s| {
        s.transcript(&thread)
            .is_some_and(|t| t.active_turn().is_some())
            && s.can_send(Some(&thread))
    });
    rt.send(Some(&thread), "steer from desktop", Surface::Desktop);
    let steered = wait_for(&mut rt, "steer from desktop");
    let all: Vec<_> = steered["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "message")
        .map(|i| (i["role"].as_str().unwrap().to_owned(), texts(i)))
        .collect();
    let at = |needle: &str| all.iter().rposition(|(_, t)| t.contains(needle)).unwrap();
    assert!(
        at(PHONE) < at("SLOW phone turn") && at("SLOW phone turn") < at(DESKTOP),
        "each statement precedes its own message: {all:#?}"
    );
    assert!(
        at(DESKTOP) < at("steer from desktop"),
        "the steer statement precedes the steer: {all:#?}"
    );
    rt.idle(&thread);

    // 4. Compaction, then a phone turn: the current statement is still delivered.
    let before = seen.lock().unwrap().len();
    rt.store
        .install_command_schema(ox_desk_core::slash::Capabilities {
            methods: Arc::new(["thread/compact/start".to_owned()].into()),
            ..Default::default()
        })
        .unwrap();
    rt.store
        .dispatch(Action::NativeCommand(
            Some(thread.clone()),
            "/compact".into(),
        ))
        .unwrap();
    rt.pump(&|_| seen.lock().unwrap().len() > before);
    rt.idle(&thread);
    rt.send(Some(&thread), "after compaction", Surface::Phone);
    let compacted = wait_for(&mut rt, "after compaction");
    println!(
        "after compaction: desktop {:?}, phone {:?}",
        mentions(&compacted, DESKTOP),
        mentions(&compacted, PHONE)
    );
    assert!(
        mentions(&compacted, PHONE)
            .last()
            .is_some_and(|(r, _)| r != "user"),
        "the current statement survives compaction"
    );
    rt.idle(&thread);

    // 5. A fresh app-server resumes the thread; the next send carries its own statement.
    drop(rt);
    let mut rt = Runtime::open(&bin, port, home.path(), &project);
    rt.pump(&|s| s.catalog_row(&thread).is_some());
    rt.store
        .dispatch(Action::SelectThread(thread.clone()))
        .unwrap();
    rt.pump(&|s| s.can_send(Some(&thread)));
    rt.send(Some(&thread), "after resume", Surface::Desktop);
    let resumed = wait_for(&mut rt, "after resume");
    let all: Vec<_> = resumed["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "message")
        .map(texts)
        .collect();
    let last_desktop = all.iter().rposition(|t| t.contains(DESKTOP)).unwrap();
    let last_phone = all.iter().rposition(|t| t.contains(PHONE)).unwrap_or(0);
    assert!(
        last_desktop > last_phone,
        "the resumed send's own statement is the latest: {all:#?}"
    );
    assert_eq!(users(&resumed).last().unwrap(), "after resume");
    rt.idle(&thread);

    // The visible history shows only what people typed.
    let scope = Scope {
        project: project.clone(),
        route: Route::codex_api("demo".into()),
    };
    let snapshot = rt.store.host_snapshot(&scope);
    for timeline in snapshot.transcripts.values() {
        for item in &timeline.items {
            assert!(
                !item.text.contains("OX Desktop") && !item.text.contains("OX phone app"),
                "visible history leaked a statement: {item:?}"
            );
        }
    }
}

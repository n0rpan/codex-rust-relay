//! Per-submit OX context on stock Claude Code, replayed from frames captured
//! from Claude 2.1.331 and 2.1.332 against a local mock provider.
use ox_desk_core::{
    claude::{
        Decoder, Event as Frame,
        catalog::{Origin, Profiles},
    },
    links::claude::{
        Billing, Event, Launch, Link, Session,
        context::{CALLBACK_ID, UNKNOWN_SENDER},
        minimum,
    },
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};
use tempfile::TempDir;

const SESSION: &str = "4b1f0c0e-6a8e-4c43-9d55-0d6a1d1f6c11";
const DESKTOP: &str = "This message was sent from OX Desktop.";
const PHONE: &str = "This message was sent from the OX phone interface.";

fn captures() -> Vec<Value> {
    include_str!("fixtures/claude-prompt-context.jsonl")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// A stdio stand-in for Claude Code: it records every input line and, after
/// the initialize request or the user message with uuid U, prints `after.init`
/// or `after.U` if present.
fn fixture(root: &Path) -> PathBuf {
    let account = json!({"apiProvider":"firstParty","subscriptionType":"Claude Max"});
    fs::write(root.join("after.init"),json!({"type":"control_response","response":{
        "subtype":"success","request_id":"ox-init","response":{"claude_code_version":minimum(),"account":account,"commands":[],"models":[]}
    }}).to_string()+"\n").unwrap();
    let exe = root.join("fixture");
    fs::write(
        &exe,
        r#"#!/bin/sh
while IFS= read -r row; do
  printf '%s\n' "$row" >> wire
  case "$row" in
    *'"request_id":"ox-init"'*) key=init ;;
    *'"type":"user"'*) key=$(printf '%s' "$row" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p') ;;
    *) key= ;;
  esac
  [ -n "$key" ] && [ -f "after.$key" ] && cat "after.$key"
done
"#,
    )
    .unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
    exe
}
fn after(root: &Path, uuid: &str, frames: &[&Value]) {
    let text: String = frames.iter().map(|f| format!("{f}\n")).collect();
    fs::write(root.join(format!("after.{uuid}")), text).unwrap();
}
fn spawn(root: &Path, worker: bool) -> (Link, Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let link = Link::spawn(
        Launch {
            ox: fixture(root),
            path: "/usr/bin:/bin".into(),
            project: root.into(),
            billing: Billing::Subscription,
            session: Session::New(SESSION.into()),
            model: None,
            permission_mode: "default".into(),
            contract: worker.then(|| {
                ox_desk_core::claude_worker_contract::Contract::with_support(false, false, false)
            }),
        },
        move |e| {
            let _ = tx.send(e);
        },
    )
    .unwrap();
    match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        Event::Frame(Frame::ControlReply(_)) => {}
        other => panic!("expected the initialize reply, got {other:?}"),
    }
    (link, rx)
}
fn wire(root: &Path, n: usize) -> Vec<Value> {
    wire_until(root, |rows| rows.len() >= n)
}
fn answered(root: &Path, n: usize) -> Vec<Value> {
    wire_until(root, |rows| {
        rows.iter()
            .filter(|r| r["type"] == "control_response")
            .count()
            >= n
    })
}
fn wire_until(root: &Path, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let rows: Vec<Value> = fs::read_to_string(root.join("wire"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        if done(&rows) {
            return rows;
        }
        assert!(Instant::now() < until, "timed out; wire: {rows:?}");
        std::thread::sleep(Duration::from_millis(15));
    }
}
fn answers(rows: &[Value]) -> Vec<(String, String)> {
    rows.iter()
        .filter(|r| r["type"] == "control_response")
        .map(|r| {
            let out = &r["response"]["response"]["hookSpecificOutput"];
            assert_eq!(r["response"]["subtype"], "success");
            assert_eq!(out["hookEventName"], "UserPromptSubmit");
            (
                r["response"]["request_id"].as_str().unwrap().to_owned(),
                out["additionalContext"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}
fn hook(id: &str, prompt: &str) -> Value {
    json!({"type":"control_request","request_id":id,"request":{"subtype":"hook_callback",
        "callback_id":CALLBACK_ID,"input":{"session_id":SESSION,"hook_event_name":"UserPromptSubmit",
        "prompt":prompt,"prompt_id":"p"}}})
}

#[test]
fn initialize_appends_stable_facts_and_registers_a_non_blocking_prompt_hook() {
    for worker in [false, true] {
        let temp = TempDir::new().unwrap();
        let (_link, _rx) = spawn(temp.path(), worker);
        let init = &wire(temp.path(), 1)[0]["request"];
        assert_eq!(init["subtype"], "initialize");
        assert_eq!(init["forwardSubagentText"], true);
        let facts = init["appendSystemPrompt"].as_str().unwrap();
        assert!(facts.contains("hosted by OX") && facts.contains("Claude subscription"));
        assert_eq!(facts.contains("OX worker"), worker, "{facts}");
        let hook = &init["hooks"]["UserPromptSubmit"][0];
        assert_eq!(hook["hookCallbackIds"], json!([CALLBACK_ID]));
        assert_eq!(hook["onFailure"], "continue");
        assert!(hook["timeout"].as_u64().unwrap() >= 10);
    }
}

/// Captured: "first SLOW" alone, then "from phone" and "from desktop" queued
/// during that busy turn and merged into one callback, then a two-block
/// message, then an image-only message.
#[test]
fn captured_stock_callbacks_keep_each_submit_label_and_the_exact_user_text() {
    for capture in captures() {
        let version = capture["version"].as_str().unwrap();
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let hooks: Vec<&Value> = capture["hooks"].as_array().unwrap().iter().collect();
        assert!(
            hooks
                .iter()
                .all(|h| h["request"]["subtype"] == "hook_callback")
        );
        let ids: Vec<&str> = hooks
            .iter()
            .map(|h| h["request_id"].as_str().unwrap())
            .collect();
        // The probe registered its own callback ID; everything else is as captured.
        let rewritten: Vec<Value> = hooks
            .iter()
            .map(|h| {
                let mut h = (*h).clone();
                h["request"]["callback_id"] = json!(CALLBACK_ID);
                h
            })
            .collect();
        let sent = capture["sent"].as_array().unwrap();
        let uuid = |i: usize| sent[i]["uuid"].as_str().unwrap();
        after(root, uuid(0), &[&rewritten[0]]);
        after(root, uuid(2), &[&rewritten[1]]);
        after(root, uuid(3), &[&rewritten[2]]);
        after(root, uuid(4), &[&rewritten[3]]);
        let (link, rx) = spawn(root, false);
        let labels = [DESKTOP, PHONE, DESKTOP, PHONE, DESKTOP];
        // As captured: the last two were sent after the preceding turn started.
        // (Had the image-only message been queued behind "line a / line b", Claude
        // could have merged it without changing the prompt, and the link would
        // correctly report the interface as unknown.)
        for (i, (s, label)) in sent.iter().zip(labels).enumerate() {
            if i >= 3 {
                answered(root, i - 1);
            }
            link.send_user_with_context(
                s["uuid"].as_str().unwrap(),
                s["content"].as_array().unwrap().clone(),
                label,
            )
            .unwrap();
        }
        let rows = answered(root, hooks.len());
        let got = answers(&rows);
        assert_eq!(got.len(), 4, "{version}: {rows:?}");
        assert_eq!(got[0], (ids[0].to_owned(), DESKTOP.to_owned()), "{version}");
        assert_eq!(got[1].0, ids[1]);
        assert_eq!(
            got[1].1,
            format!(
                "This turn combines 2 messages, in the order they were sent.\nMessage 1: {PHONE}\nMessage 2: {DESKTOP}"
            ),
            "{version}"
        );
        assert_eq!(got[2], (ids[2].to_owned(), PHONE.to_owned()), "{version}");
        assert_eq!(got[3], (ids[3].to_owned(), DESKTOP.to_owned()), "{version}");
        // User text reaches Claude exactly as submitted, with no context inside it.
        let users: Vec<&Value> = rows.iter().filter(|r| r["type"] == "user").collect();
        assert_eq!(users.len(), sent.len());
        for (u, s) in users.iter().zip(sent) {
            assert_eq!(u["uuid"], s["uuid"]);
            assert_eq!(u["message"]["content"], s["content"]);
            assert!(!u.to_string().contains("OX"), "{u}");
        }
        // Answered callbacks are not surfaced as unsupported requests.
        while let Ok(event) = rx.recv_timeout(Duration::from_millis(200)) {
            assert!(
                !matches!(event, Event::Frame(Frame::UnsupportedRequest { .. })),
                "{version}: {event:?}"
            );
        }
        // Stock replay echoes only the user's own content.
        for replay in capture["replays"].as_array().unwrap() {
            assert!(!replay.to_string().contains("OXCTX"), "{version}: {replay}");
        }
    }
}

#[test]
fn interrupts_slash_commands_and_legacy_sends_never_borrow_a_label() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    // "/cost" fires no callback; an interrupt keeps queued messages.
    after(root, "u1", &[&hook("h1", "first")]);
    after(root, "u3", &[&hook("h2", "after")]);
    after(root, "u4", &[&hook("h3", "plain")]);
    after(
        root,
        "u6",
        &[
            &hook("h4", "same"),
            &hook("h5", "same"),
            &hook("h6", "unknown text"),
        ],
    );
    let (link, _rx) = spawn(root, false);
    let text = |s: &str| vec![json!({"type":"text","text":s})];
    link.send_user_with_context("u1", text("first"), DESKTOP)
        .unwrap();
    link.send_user_with_context("u2", text("/cost"), PHONE)
        .unwrap();
    link.interrupt().unwrap();
    link.send_user_with_context("u3", text("after"), PHONE)
        .unwrap();
    link.send_user("u4", text("plain")).unwrap();
    link.send_user_with_context("u5", text("same"), DESKTOP)
        .unwrap();
    link.send_user_with_context("u6", text("same"), PHONE)
        .unwrap();
    let got = answers(&answered(root, 6));
    let expect = [
        ("h1", DESKTOP),
        ("h2", PHONE),
        ("h3", UNKNOWN_SENDER),
        // Two pending submits share the text and differ in label.
        ("h4", UNKNOWN_SENDER),
        ("h5", PHONE),
        ("h6", UNKNOWN_SENDER),
    ];
    assert_eq!(
        got,
        expect
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn foreign_hook_callbacks_still_reach_the_store() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let mut other = hook("other", "x");
    other["request"]["callback_id"] = json!("someone_else");
    after(root, "u1", &[&other]);
    let (link, rx) = spawn(root, false);
    link.send_user("u1", vec![json!({"type":"text","text":"x"})])
        .unwrap();
    match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        Event::Frame(Frame::UnsupportedRequest {
            request_id,
            subtype,
        }) => assert_eq!(
            (request_id.as_str(), subtype.as_str()),
            ("other", "hook_callback")
        ),
        other => panic!("{other:?}"),
    }
    assert!(answers(&wire(root, 2)).is_empty());
}

/// The context Claude stores as a transcript attachment never appears in OX history.
#[test]
fn captured_transcripts_show_user_text_without_ox_context() {
    for capture in captures() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().canonicalize().unwrap().join("project");
        fs::create_dir(&project).unwrap();
        let profiles = Profiles {
            subscription: tmp.path().join("native"),
            api: tmp.path().join("api"),
        };
        let key = project
            .to_string_lossy()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
        let dir = profiles.subscription.join("projects").join(key);
        fs::create_dir_all(&dir).unwrap();
        let rows: String = capture["transcript"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let mut r = r.clone();
                r["sessionId"] = json!(SESSION);
                r["cwd"] = json!(project);
                format!("{r}\n")
            })
            .collect();
        assert!(rows.contains("hook_additional_context") && rows.contains("OXCTX"));
        fs::write(dir.join(format!("{SESSION}.jsonl")), rows).unwrap();
        let session = profiles
            .session(&project, SESSION, Origin::Subscription)
            .unwrap();
        let mut frames = vec![];
        session
            .history(|v| {
                frames.push(v);
                Ok(())
            })
            .unwrap();
        let all = serde_json::to_string(&frames).unwrap();
        assert!(!all.contains("OXCTX"), "{all}");
        for text in ["first SLOW", "from phone", "from desktop", "line a"] {
            assert!(all.contains(text), "{text}: {all}");
        }
        let mut decoder = Decoder::default();
        for f in frames {
            decoder.decode_value(f).unwrap();
        }
    }
}

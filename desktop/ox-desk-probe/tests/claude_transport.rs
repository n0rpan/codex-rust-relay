use ox_desk_core::{
    claude::Event as Frame,
    links::claude::{Billing, Event, Launch, Link, Session},
};
use serde_json::json;
use std::{fs, path::PathBuf, sync::mpsc, time::Duration};

#[test]
fn scripted_claude_permission_uses_its_captured_session() {
    let project = std::env::temp_dir().join(format!("ox-scripted-claude-{}", std::process::id()));
    fs::create_dir(&project).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(project.clone());
    fs::write(
        project.join("fake-claude.jsonl"),
        include_str!("../fixtures/claude-approval.jsonl"),
    )
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let link = Link::spawn(
        Launch {
            ox: env!("CARGO_BIN_EXE_fake-claude").into(),
            path: "/usr/bin:/bin".into(),
            project,
            session: Session::New("00000000-0000-4000-8000-000000000001".into()),
            billing: Billing::Subscription,
            permission_mode: "default".into(),
            contract: None,
        },
        move |frame| {
            let _ = tx.send(frame);
        },
    )
    .unwrap();
    let next = || rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(next(), Event::Frame(Frame::ControlReply(_))));
    link.send_user(
        "00000000-0000-4000-8000-000000000002",
        vec![json!({"type":"text","text":"Write the marker"})],
    )
    .unwrap();
    assert!(matches!(next(), Event::Frame(Frame::Init(_))));
    let Event::Frame(Frame::Ask(ask)) = next() else {
        panic!("Expected Claude permission")
    };
    assert_eq!(ask.tool_use_id, "tool-1");
    link.respond(
        &ask.request_id,
        json!({"behavior":"deny","message":"Declined"}),
    )
    .unwrap();
    assert!(matches!(next(), Event::Frame(Frame::Result(_))));
}

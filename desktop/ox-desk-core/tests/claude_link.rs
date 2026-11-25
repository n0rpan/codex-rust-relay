//! Contract checks at the owned stdio boundary, before any model request is sent.
use ox_desk_core::{
    claude::Event as Frame,
    links::claude::{Billing, Event, Launch, Link, Session, minimum},
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::Duration,
};
use tempfile::TempDir;

const FIRST: &str = "dc12c72e-e6ad-4116-a16a-2dace464418b";
const SECOND: &str = "aa366e1d-0572-41ff-85ae-cdc2c4ce153d";

fn fixture(root: &Path, version: &str, account: Value) -> PathBuf {
    fs::write(root.join("initialize.response"),json!({"type":"control_response","response":{
        "subtype":"success","request_id":"ox-init","response":{"claude_code_version":version,"account":account,"commands":[],"models":[]}
    }}).to_string()+"\n").unwrap();
    let exe = root.join("fixture");
    fs::write(
        &exe,
        r#"#!/bin/sh
printf '%s\n' "$@" > args
IFS= read -r row || exit 1
printf '%s\n' "$row" > wire
cat initialize.response
while IFS= read -r row; do
  printf '%s\n' "$row" >> wire
done
"#,
    )
    .unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
    exe
}
fn spawn(
    project: &Path,
    ox: PathBuf,
    billing: Billing,
    session: Session,
    mode: &str,
) -> (Link, Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let link = Link::spawn(
        Launch {
            ox,
            path: "/usr/bin:/bin".into(),
            project: project.into(),
            billing,
            session,
            permission_mode: mode.into(),
        },
        move |e| {
            let _ = tx.send(e);
        },
    )
    .unwrap();
    (link, rx)
}
fn next(events: &Receiver<Event>) -> Event {
    events
        .recv_timeout(Duration::from_secs(5))
        .expect("Claude fixture event")
}
fn wait_lines(path: &Path, n: usize) -> Vec<Value> {
    let until = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let rows: Vec<_> = fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if rows.len() >= n {
            return rows;
        }
        assert!(
            std::time::Instant::now() < until,
            "expected {n} frames, got {}",
            rows.len()
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}
#[test]
fn stock_version_and_billing_are_checked_before_user_input() {
    for (version, billing, account, expected_login) in [
        (
            "2.1.330",
            Billing::Api("fixture/demo".into()),
            json!({"apiProvider":"firstParty","tokenSource":"ANTHROPIC_AUTH_TOKEN"}),
            false,
        ),
        (
            "2.1.331-beta.1",
            Billing::Subscription,
            json!({"apiProvider":"firstParty","subscriptionType":"Claude Max"}),
            false,
        ),
        (
            minimum(),
            Billing::Subscription,
            json!({"apiProvider":"firstParty","apiKeySource":"ANTHROPIC_API_KEY"}),
            true,
        ),
        (
            minimum(),
            Billing::Api("fixture/demo".into()),
            json!({"apiProvider":"firstParty","subscriptionType":"Claude Max"}),
            false,
        ),
    ] {
        let temp = TempDir::new().unwrap();
        let exe = fixture(temp.path(), version, account);
        let (link, rx) = spawn(
            temp.path(),
            exe,
            billing,
            Session::New(FIRST.into()),
            "default",
        );
        if expected_login {
            assert!(matches!(next(&rx), Event::SignInRequired));
        }
        assert!(matches!(next(&rx), Event::Closed { .. }));
        assert!(
            link.send_user(SECOND, vec![json!({"type":"text","text":"unsent"})])
                .is_err()
        );
        assert_eq!(wait_lines(&temp.path().join("wire"), 1).len(), 1);
    }
}
#[test]
fn each_session_owns_its_launch_and_control_frames() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let api = fixture(
        a.path(),
        minimum(),
        json!({"apiProvider":"firstParty","tokenSource":"ANTHROPIC_AUTH_TOKEN"}),
    );
    let subscription = fixture(
        b.path(),
        minimum(),
        json!({"apiProvider":"firstParty","subscriptionType":"Claude Max"}),
    );
    let (one, events) = spawn(
        a.path(),
        api,
        Billing::Api("fixture/demo".into()),
        Session::New(FIRST.into()),
        "default",
    );
    let (two, other_events) = spawn(
        b.path(),
        subscription,
        Billing::Subscription,
        Session::Fork {
            source: FIRST.into(),
            id: SECOND.into(),
        },
        "bypassPermissions",
    );
    for rx in [&events, &other_events] {
        assert!(matches!(next(rx), Event::Frame(Frame::ControlReply(_))));
    }
    let args = fs::read_to_string(a.path().join("args")).unwrap();
    assert!(args.contains("claude\n--model\nfixture/demo\n-p\n"));
    assert!(args.contains("--permission-mode\ndefault\n--allow-dangerously-skip-permissions\n"));
    let args = fs::read_to_string(b.path().join("args")).unwrap();
    assert!(args.contains("claude\n--subscription\n-p\n"));
    assert!(args.contains("--dangerously-skip-permissions\n"));
    assert!(args.contains(&format!(
        "--resume\n{FIRST}\n--fork-session\n--session-id\n{SECOND}\n"
    )));
    one.send_user(SECOND, vec![json!({"type":"text","text":"from A"})])
        .unwrap();
    let interrupt = one.interrupt().unwrap();
    one.respond("ask", json!({"behavior":"deny","message":"declined"}))
        .unwrap();
    assert!(one.set_model("another/billing").is_err());
    let mode = two.set_mode("plan").unwrap();
    let model = two.set_model("sonnet").unwrap();
    two.respond(
        "ask",
        json!({"behavior":"allow","updatedInput":{"command":"echo test"}}),
    )
    .unwrap();
    two.reject_request("unknown", "Unsupported request")
        .unwrap();
    let one_wire = wait_lines(&a.path().join("wire"), 4);
    assert_eq!(one_wire[1]["session_id"], FIRST);
    assert_eq!(one_wire[1]["uuid"], SECOND);
    assert_eq!(
        one_wire[2],
        json!({"type":"control_request","request_id":interrupt,"request":{"subtype":"interrupt"}})
    );
    assert_eq!(one_wire[3]["response"]["response"]["behavior"], "deny");
    let two_wire = wait_lines(&b.path().join("wire"), 5);
    assert_ne!(mode, model);
    assert_eq!(two_wire[1]["request_id"], mode);
    assert_eq!(two_wire[2]["request_id"], model);
    assert_eq!(
        two_wire[3]["response"]["response"]["updatedInput"]["command"],
        "echo test"
    );
    assert_eq!(two_wire[4]["response"]["subtype"], "error");
    drop(one);
    assert!(matches!(next(&events), Event::Closed { .. }));
    two.interrupt().unwrap();
    assert_eq!(
        wait_lines(&b.path().join("wire"), 6)[5]["request"]["subtype"],
        "interrupt"
    );
    drop(two);
    assert!(matches!(next(&other_events), Event::Closed { .. }));
}
#[test]
fn later_turns_may_report_a_moved_shell_directory_but_not_another_session() {
    // Claude Code repeats init each turn with the Bash tool's current directory,
    // so a thread that ran `cd sub` must stay open. A different session must not.
    let temp = TempDir::new().unwrap();
    let project = temp.path().canonicalize().unwrap();
    fs::create_dir(project.join("sub")).unwrap();
    let exe = fixture(
        &project,
        minimum(),
        json!({"apiProvider":"firstParty","subscriptionType":"Claude Max"}),
    );
    let init = |session: &str, cwd: &Path| {
        json!({"type":"system","subtype":"init","session_id":session,"uuid":"u",
            "model":"m","permissionMode":"default","slash_commands":[],"tools":[],
            "cwd":cwd})
        .to_string()
    };
    let response = fs::read_to_string(project.join("initialize.response")).unwrap();
    fs::write(
        project.join("initialize.response"),
        format!(
            "{response}{}\n{}\n{}\n",
            init(FIRST, &project),
            init(FIRST, &project.join("sub")),
            init(SECOND, &project.join("sub")),
        ),
    )
    .unwrap();
    let (_link, rx) = spawn(
        &project,
        exe,
        Billing::Subscription,
        Session::New(FIRST.into()),
        "default",
    );
    assert!(matches!(next(&rx), Event::Frame(Frame::ControlReply(_))));
    assert!(matches!(next(&rx), Event::Frame(Frame::Init(_))));
    assert!(matches!(next(&rx), Event::Frame(Frame::Init(_))));
    match next(&rx) {
        Event::Closed { reason } => {
            assert!(reason.contains("different project or session"), "{reason}")
        }
        other => panic!("expected the session mismatch to close the link, got {other:?}"),
    }
}

//! Contract checks at the owned stdio boundary, before any model request is sent.
use ox_desk_core::{
    actions::Action,
    claude::{Event as Frame, catalog::Profiles},
    links::LinkEvent,
    links::claude::{Billing, Event, Launch, Link, Session, minimum},
    runtime::{Access, Runtime},
    store::{
        Connection, Profile, Store,
        claude::{Launcher, SUBSCRIPTION_PROVIDER},
        routes_thread::Choice,
    },
};
use serde_json::{Value, json};
use std::{
    fs,
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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-link.sh")
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
            subscription_model: None,
            contract: None,
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
            // A key in the environment is not fixed by signing in: no sign-in prompt.
            json!({"apiProvider":"firstParty","apiKeySource":"ANTHROPIC_API_KEY"}),
            false,
        ),
        (
            minimum(),
            Billing::Subscription,
            json!({"apiProvider":"firstParty"}),
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

fn recording_store(
    project: &Path,
    exe: &Path,
    profiles: &Profiles,
) -> (Store, Receiver<LinkEvent>) {
    let (tx, rx) = mpsc::channel();
    let store = Store::open_claude(
        Profile {
            project: project.into(),
            route: Runtime::Claude.route(None),
            provider: SUBSCRIPTION_PROVIDER.into(),
        },
        Launcher {
            ox: exe.into(),
            path: "/usr/bin:/bin".into(),
            profiles: profiles.clone(),
            index: project.join("index"),
            aliases: vec![],
        },
        move |e| {
            let _ = tx.send(e);
        },
    )
    .unwrap();
    (store, rx)
}

/// Start and recover actual stores with a stock executable that records every invocation
/// and input. A saved Claude route opens at login, and history selection is a disk read.
#[test]
fn browsing_and_restoring_never_run_claude_and_each_first_input_is_delivered_once() {
    let root = TempDir::new().unwrap();
    let project = root.path().canonicalize().unwrap();
    let exe = fixture(
        &project,
        minimum(),
        json!({"apiProvider":"firstParty","subscriptionType":"Claude Max"}),
    );
    let profiles = Profiles {
        subscription: project.join("stock"),
        api: project.join("api"),
    };
    let journals = profiles.subscription.join("projects").join(
        project
            .to_string_lossy()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
    );
    fs::create_dir_all(&journals).unwrap();
    fs::write(journals.join(format!("{FIRST}.jsonl")), format!("{}\n", json!({"uuid":SECOND, "type":"user","cwd":project,"sessionId":FIRST,"entrypoint":"sdk-cli","message":{"role":"user","content":"Earlier message"}}))).unwrap();
    let open = || recording_store(&project, &exe, &profiles);
    let history = format!("claude:subscription:{FIRST}");
    let (mut store, events) = open();
    assert_eq!(store.state(), &Connection::Ready);
    assert!(store.can_send(None));
    store.dispatch(Action::NewDraft).unwrap();
    store.dispatch(Action::SetNextAccess(Access::Plan)).unwrap();
    store.select(&history).unwrap();
    let wait_history = |store: &mut Store, events: &Receiver<_>| {
        while !store.transcript(&history).is_some_and(|t| t.is_ready()) {
            store.apply(events.recv_timeout(Duration::from_secs(5)).unwrap());
        }
    };
    wait_history(&mut store, &events);
    let (mut replacement, events) = open();
    replacement.recover_from(&mut store);
    drop(store);
    wait_history(&mut replacement, &events);
    assert!(replacement.process_threads().is_empty());
    assert!(!project.join("launches").exists());
    replacement.dispatch(Action::NewDraft).unwrap();
    replacement
        .dispatch(Action::ModelOptions(None, Choice::Model("sonnet".into())))
        .unwrap();
    replacement
        .dispatch(Action::SendText(None, "First new turn".into()))
        .unwrap();
    let fresh = replacement.take_opened().unwrap();
    assert!(!replacement.can_send(Some(&fresh)));
    let wait_input = |store: &mut Store, events: &Receiver<_>, key: &str, expected: &str| {
        while !store
            .transcript(key)
            .is_some_and(|t| t.active_turn().is_some())
        {
            store.apply(events.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        let wire = wait_lines(&project.join("wire"), 2);
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[1]["message"]["content"][0]["text"], expected);
    };
    wait_input(&mut replacement, &events, &fresh, "First new turn");
    let first = fs::read_to_string(project.join("launches")).unwrap();
    assert_eq!(first.lines().count(), 1);
    assert!(
        first.contains("--session-id")
            && first.contains("--permission-mode plan")
            && first.contains("--subscription --model sonnet"),
        "{first}"
    );
    replacement
        .dispatch(Action::SetAccess(history.clone(), Access::Auto))
        .unwrap();
    replacement
        .dispatch(Action::ModelOptions(
            Some(history.clone()),
            Choice::Model("opus".into()),
        ))
        .unwrap();
    assert_eq!(fs::read_to_string(project.join("launches")).unwrap(), first);
    replacement
        .dispatch(Action::SendText(
            Some(history.clone()),
            "First resumed turn".into(),
        ))
        .unwrap();
    wait_input(&mut replacement, &events, &history, "First resumed turn");
    let launches = fs::read_to_string(project.join("launches")).unwrap();
    assert_eq!(launches.lines().count(), 2);
    let resumed = launches.lines().last().unwrap();
    assert!(
        resumed.contains(&format!("--resume {FIRST}"))
            && resumed.contains("--permission-mode auto")
            && resumed.contains("--model opus"),
        "{resumed}"
    );
}

#[test]
fn a_refused_first_turn_can_retry_its_reserved_id_after_sign_in() {
    let root = TempDir::new().unwrap();
    let project = root.path().canonicalize().unwrap();
    let exe = fixture(&project, minimum(), json!({"apiProvider":"firstParty"}));
    let profiles = Profiles {
        subscription: project.join("stock"),
        api: project.join("api"),
    };
    let (mut store, events) = recording_store(&project, &exe, &profiles);
    store.dispatch(Action::SetNextAccess(Access::Plan)).unwrap();
    store
        .dispatch(Action::ModelOptions(None, Choice::Model("sonnet".into())))
        .unwrap();
    store
        .dispatch(Action::SendText(None, "Keep my first turn".into()))
        .unwrap();
    let owner = store.take_opened().unwrap();
    while store.state() != &Connection::SignInRequired {
        store.apply(events.recv_timeout(Duration::from_secs(5)).unwrap());
    }
    assert_eq!(store.draft(Some(&owner)), "Keep my first turn");
    assert_eq!(wait_lines(&project.join("wire"), 1).len(), 1);
    fixture(
        &project,
        minimum(),
        json!({"apiProvider":"firstParty","subscriptionType":"Claude Max"}),
    );
    let (mut replacement, events) = recording_store(&project, &exe, &profiles);
    replacement.recover_from(&mut store);
    assert!(replacement.process_threads().is_empty());
    assert!(replacement.can_compose(Some(&owner)));
    replacement
        .dispatch(Action::SetNextAccess(Access::Auto))
        .unwrap();
    replacement
        .dispatch(Action::Submit(Some(owner.clone())))
        .unwrap();
    while !replacement
        .transcript(&owner)
        .is_some_and(|t| t.active_turn().is_some())
    {
        replacement.apply(events.recv_timeout(Duration::from_secs(5)).unwrap());
    }
    assert_eq!(replacement.next_access(), Some(Access::Auto));
    let launches = fs::read_to_string(project.join("launches")).unwrap();
    let calls = launches.lines().collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0], calls[1],
        "retry must preserve the original ID, model and mode"
    );
    assert_eq!(
        wait_lines(&project.join("wire"), 2)[1]["message"]["content"][0]["text"],
        "Keep my first turn"
    );
}

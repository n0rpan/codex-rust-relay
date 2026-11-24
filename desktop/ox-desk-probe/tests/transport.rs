use ox_desk_core::{
    codex::{
        frame::{Frame, RequestId},
        request::Call,
    },
    links::codex::{Launch, Link, LinkEvent},
};
use std::{env, path::PathBuf, sync::mpsc, time::Duration};

#[test]
fn urgent_request_and_server_ask_cross_a_pending_list() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (events, rx) = mpsc::channel();
    let link = Link::spawn(
        Launch {
            executable: PathBuf::from(env!("CARGO_BIN_EXE_fake-app-server")),
            arguments: vec![dir.join("fixtures/interrupt-listing.jsonl").into()],
            directory: dir,
            path: env::var_os("PATH").unwrap_or_default(),
        },
        move |event| {
            let _ = events.send(event);
        },
    )
    .unwrap();
    link.request(
        1,
        Call::List {
            project: "/synthetic".into(),
            query: String::new(),
            cursor: None,
        },
    )
    .unwrap();
    link.request(
        2,
        Call::Stop {
            thread: "agent".into(),
            turn: "turn".into(),
        },
    )
    .unwrap();
    let next = || {
        rx.recv_timeout(Duration::from_secs(3))
            .expect("protocol stalled")
    };
    assert!(matches!(
        next(),
        LinkEvent::Frame(Frame::Reply {
            id: RequestId::Number(2),
            result: Ok(_),
            ..
        })
    ));
    let LinkEvent::Frame(Frame::Unknown { id: Some(id), .. }) = next() else {
        panic!("missing ask");
    };
    link.reject(id, -32601, "Unsupported request").unwrap();
    assert!(matches!(
        next(),
        LinkEvent::Frame(Frame::Reply {
            id: RequestId::Number(1),
            result: Ok(_),
            ..
        })
    ));
    assert!(matches!(next(), LinkEvent::Closed { stderr_tail, .. } if stderr_tail.is_empty()));
}

#[test]
fn catalog_spans_routes_but_opening_and_approvals_stay_on_their_owner() {
    use ox_desk_core::store::{Profile, Route, Store};
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (events, inbox) = mpsc::channel();
    let link = Link::spawn(
        Launch {
            executable: PathBuf::from(env!("CARGO_BIN_EXE_fake-app-server")),
            arguments: vec![dir.join("fixtures/catalog-approval.jsonl").into()],
            directory: dir,
            path: env::var_os("PATH").unwrap_or_default(),
        },
        move |event| {
            let _ = events.send(event);
        },
    )
    .unwrap();
    let profile = Profile {
        project: "/synthetic".into(),
        route: Route::codex_api("fixture/demo".into()),
        provider: "codeox".into(),
    };
    let mut store = Store::open(profile, link).unwrap();
    let next = || inbox.recv_timeout(Duration::from_secs(4)).unwrap();
    store.apply(next()); // initialize
    store.apply(next()); // ask before list reply
    let id = store.asks()[0].id.clone();
    assert_eq!(store.asks()[0].thread.as_deref(), Some("background"));
    assert_eq!(store.asks()[0].decisions().len(), 2);
    assert!(store.answer(&id, 2).is_err());
    store.answer(&id, 0).unwrap();
    assert!(store.answer(&id, 0).is_err());
    store.apply(next());
    assert_eq!(store.threads().len(), 2);
    assert_eq!(store.threads()[0].title(), "This project");
    assert!(store.select("wrong-model").is_err());
    assert!(store.selected().is_none());
    assert!(store.has_more());
    store.list_more().unwrap();
    store.apply(next());
    assert!(!store.has_more());
    assert_eq!(store.threads().len(), 2);
    store.apply(next());
    let mine = store.catalog_row("mine").unwrap();
    assert_eq!(mine.status.as_ref().unwrap().label(), Some("Needs you"));
    assert!(store.transcript("mine").is_none());
    store.apply(next());
    let mine = store.catalog_row("mine").unwrap();
    assert!(mine.unread && mine.activity_order > 0);
    assert_eq!(mine.status.as_ref().unwrap().label(), Some("Error"));
    store.apply(next());
    assert!(store.catalog_row("mine").is_none());
    store.apply(next());
    assert_eq!(
        store.catalog_row("wrong-model").unwrap().title(),
        "Moved row"
    );
    assert!(store.next_deadline().is_none());
}

#[test]
fn simultaneous_resumes_keep_the_selected_thread_and_page_only_on_demand() {
    use ox_desk_core::{
        store::{Profile, Route, Store},
        transcript::Row,
    };
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (events, inbox) = mpsc::channel();
    let link = Link::spawn(
        Launch {
            executable: PathBuf::from(env!("CARGO_BIN_EXE_fake-app-server")),
            arguments: vec![dir.join("fixtures/overlapping-history.jsonl").into()],
            directory: dir,
            path: env::var_os("PATH").unwrap_or_default(),
        },
        move |event| {
            let _ = events.send(event);
        },
    )
    .unwrap();
    let profile = Profile {
        project: "/synthetic".into(),
        route: Route::codex_api("fixture/demo".into()),
        provider: "codeox".into(),
    };
    let mut store = Store::open(profile, link).unwrap();
    let apply =
        |store: &mut Store| store.apply(inbox.recv_timeout(Duration::from_secs(3)).unwrap());
    apply(&mut store);
    apply(&mut store);
    store.select("a").unwrap();
    assert!(store.transcript("a").unwrap().is_loading());
    store.select("b").unwrap();
    for _ in 0..4 {
        apply(&mut store);
    }
    assert_eq!(store.selected(), Some("b"));
    // Only the first background delta changes a sidebar badge. Further deltas
    // still reach A without repainting B or rebuilding the thread list.
    store.drain(vec![], std::time::Instant::now());
    let delta = || {
        LinkEvent::Frame(Frame::decode(br#"{"method":"item/agentMessage/delta","params":{"threadId":"a","turnId":"latest","itemId":"msg","delta":"!"}}"#, |_| None).unwrap())
    };
    let first = store.drain(vec![delta()], std::time::Instant::now());
    assert!(first.affects_view(Some("b")));
    let second = store.drain(vec![delta(), delta()], std::time::Instant::now());
    assert!(!second.affects_view(Some("b")));
    assert!(second.affects_view(Some("a")));
    assert!(store.transcript("a").unwrap().unread());
    assert_eq!(store.transcript("b").unwrap().len(), 1);
    store.select("a").unwrap();
    let a = store.transcript("a").unwrap();
    assert!(a.has_older());
    assert_eq!(a.len(), 3);
    assert!(matches!(a.row(0),Some(Row::Message(k)) if k.turn=="prev"));
    assert!(matches!(a.row(1),Some(Row::Tools(group)) if group.len()==2 && group[0].item=="tool1"));
    store.load_older("a").unwrap();
    apply(&mut store);
    let a = store.transcript("a").unwrap();
    assert_eq!((a.len(), a.prepended(), a.has_older()), (4, 1, false));
    assert!(matches!(a.row(0),Some(Row::Message(k)) if k.turn=="early"));
    store.select("stale").unwrap();
    apply(&mut store);
    assert!(
        store
            .transcript("stale")
            .unwrap()
            .error()
            .unwrap()
            .contains("different project")
    );
    assert!(store.next_deadline().is_none());
    store.select("b").unwrap();
    assert!(!store.transcript("b").unwrap().is_loading());
}

#[test]
fn forms_and_background_decisions_keep_their_own_request_id() {
    use ox_desk_core::store::{Profile, Route, Store};
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (tx, rx) = mpsc::channel();
    let link = Link::spawn(
        Launch {
            executable: PathBuf::from(env!("CARGO_BIN_EXE_fake-app-server")),
            arguments: vec![dir.join("fixtures/questions.jsonl").into()],
            directory: dir,
            path: env::var_os("PATH").unwrap_or_default(),
        },
        move |e| {
            let _ = tx.send(e);
        },
    )
    .unwrap();
    let mut store = Store::open(
        Profile {
            project: "/synthetic".into(),
            route: Route::CODEX_SUBSCRIPTION,
            provider: "openai".into(),
        },
        link,
    )
    .unwrap();
    let next = || {
        rx.recv_timeout(Duration::from_secs(3))
            .expect("fixture stalled")
    };
    for _ in 0..7 {
        store.apply(next());
    }
    assert_eq!(store.asks().len(), 1);
    let id = RequestId::Number(2);
    assert_eq!(store.asks()[0].id, id);
    assert!(
        store
            .answer_questions(&id, &["wrong".into(), "candidate".into()])
            .is_err()
    );
    store
        .answer_questions(&id, &["Linux".into(), "candidate".into()])
        .unwrap();
    store.apply(next());
    let ask = &store.asks()[0];
    assert_eq!(ask.thread.as_deref(), Some("b"));
    assert!(ask.detail.contains("pwd") && ask.detail.contains("/synthetic"));
    assert!(store.answer(&id, 0).is_err());
    store.answer(&RequestId::Text("2".into()), 0).unwrap();
    assert!(matches!(next(), LinkEvent::Closed{stderr_tail,..} if stderr_tail.is_empty()));
}

#[test]
fn two_turns_can_finish_out_of_order_while_a_failed_steer_returns_to_its_draft() {
    use ox_desk_core::store::{Profile, Route, Store};
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (tx, rx) = mpsc::channel();
    let link = Link::spawn(
        Launch {
            executable: PathBuf::from(env!("CARGO_BIN_EXE_fake-app-server")),
            arguments: vec![dir.join("fixtures/concurrent-turns.jsonl").into()],
            directory: dir,
            path: env::var_os("PATH").unwrap_or_default(),
        },
        move |e| {
            let _ = tx.send(e);
        },
    )
    .unwrap();
    let mut store = Store::open(
        Profile {
            project: "/synthetic".into(),
            route: Route::codex_api("fixture/demo".into()),
            provider: "codeox".into(),
        },
        link,
    )
    .unwrap();
    let apply = |store: &mut Store, n| {
        for _ in 0..n {
            store.apply(
                rx.recv_timeout(Duration::from_secs(3))
                    .expect("missing expected request"),
            );
        }
    };
    apply(&mut store, 2);
    store.edit(None, "one".into());
    store.submit(None).unwrap();
    apply(&mut store, 3);
    assert_eq!(store.selected(), Some("a"));
    assert_eq!(store.transcript("a").unwrap().active_turn(), Some("t1"));
    store.new_session();
    store.edit(None, "two".into());
    store.submit(None).unwrap();
    store.select("a").unwrap();
    store.edit(Some("a"), "follow one".into());
    store.submit(Some("a")).unwrap();
    store.edit(Some("a"), "another idea".into());
    apply(&mut store, 5);
    assert_eq!(store.selected(), Some("a"));
    assert!(store.transcript("b").unwrap().active_turn().is_none());
    assert_eq!(store.draft(Some("a")), "follow one\nanother idea");
    assert!(
        store
            .transcript("a")
            .unwrap()
            .error()
            .unwrap()
            .contains("no longer")
    );
    store.stop("a").unwrap();
    apply(&mut store, 2);
    assert!(store.transcript("a").unwrap().active_turn().is_none());
    store.new_session();
    store.edit(None, "wrong billing".into());
    store.submit(None).unwrap();
    apply(&mut store, 1);
    assert_eq!(store.draft(None), "wrong billing");
    assert!(store.transcript("x").is_none());
    assert!(store.notice().unwrap().contains("different billing"));
}

#[test]
fn delayed_catalog_reply_cannot_replace_a_newer_search() {
    use ox_desk_core::store::{Profile, Route, Store};
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (tx, rx) = mpsc::channel();
    let link = Link::spawn(
        Launch {
            executable: PathBuf::from(env!("CARGO_BIN_EXE_fake-app-server")),
            arguments: vec![dir.join("fixtures/search.jsonl").into()],
            directory: dir,
            path: env::var_os("PATH").unwrap_or_default(),
        },
        move |e| {
            let _ = tx.send(e);
        },
    )
    .unwrap();
    let mut store = Store::open(
        Profile {
            project: "/synthetic".into(),
            route: Route::codex_api("fixture/demo".into()),
            provider: "codeox".into(),
        },
        link,
    )
    .unwrap();
    let next = || rx.recv_timeout(Duration::from_secs(4)).unwrap();
    store.apply(next());
    store.search("linux".into()).unwrap();
    store.apply(next());
    store.apply(next());
    assert_eq!(store.threads()[0].id, "match");
    assert!(!store.has_more());
}

use ox_desk_core::{
    codex::frame::Frame,
    links::codex::{Launch, Link, LinkEvent},
    store::{Profile, Route, Store},
};
use serde_json::{Value, json};
use std::{
    env,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

fn event(method: &str, params: Value) -> LinkEvent {
    let bytes = serde_json::to_vec(&json!({"method":method,"params":params})).unwrap();
    LinkEvent::Frame(Frame::decode(&bytes, |_| None).unwrap())
}

#[test]
fn changes_and_plans_are_delivered_only_to_their_thread_and_turn() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (send, recv) = mpsc::channel();
    let link = Link::spawn(
        Launch {
            executable: env!("CARGO_BIN_EXE_fake-app-server").into(),
            arguments: vec![dir.join("fixtures/overlapping-history.jsonl").into()],
            directory: dir,
            path: env::var_os("PATH").unwrap_or_default(),
        },
        move |e| {
            let _ = send.send(e);
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
    let apply = |store: &mut Store| store.apply(recv.recv_timeout(Duration::from_secs(4)).unwrap());
    apply(&mut store);
    apply(&mut store);
    store.select("a").unwrap();
    store.select("b").unwrap();
    for _ in 0..4 {
        apply(&mut store);
    }
    store.drain(vec![], Instant::now());
    let changes=store.drain(vec![
        event("turn/plan/updated",json!({"threadId":"a","turnId":"latest","explanation":"Check edits","plan":[{"step":"Run tests","status":"inProgress"}]})),
        event("item/fileChange/patchUpdated",json!({"threadId":"a","turnId":"latest","itemId":"patch","changes":[{"path":"a.rs","kind":{"type":"update"},"diff":"@@ first"}]})),
        event("item/fileChange/patchUpdated",json!({"threadId":"a","turnId":"latest","itemId":"patch","changes":[{"path":"b.rs","kind":{"type":"update"},"diff":"@@ replacement"}]})),
    ],Instant::now());
    assert!(changes.affects_view(Some("a")));
    assert!(!changes.affects_view(Some("b")));
    assert_eq!(store.selected(), Some("b"));
    let review = store
        .transcript("a")
        .unwrap()
        .reviews
        .get("latest")
        .unwrap();
    let groups = review.file_groups();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].0, "b.rs");
    assert_eq!(review.explanation.as_deref(), Some("Check edits"));
    assert_eq!(review.steps[0].status, "inProgress");
    assert!(
        store
            .transcript("b")
            .unwrap()
            .reviews
            .turns()
            .all(|(_, r)| !r.has_content())
    );
    let aggregate = "diff --git a/c.rs b/c.rs\n--- a/c.rs\n+++ b/c.rs\n@@ -1 +1 @@\n-old\n+new\n";
    store.apply(event(
        "turn/diff/updated",
        json!({"threadId":"a","turnId":"latest","diff":aggregate}),
    ));
    store.apply(event(
        "turn/plan/updated",
        json!({"threadId":"a","turnId":"prev","plan":[{"step":"Older","status":"completed"}]}),
    ));
    let transcript = store.transcript("a").unwrap();
    assert_eq!(transcript.reviews.turns().next().unwrap().0, "latest");
    let review = transcript.reviews.get("latest").unwrap();
    assert_eq!(review.steps[0].text, "Run tests");
    assert_eq!(&*review.file_groups()[0].1[0].diff, aggregate);
}

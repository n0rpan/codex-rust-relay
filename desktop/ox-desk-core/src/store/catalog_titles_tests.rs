use super::super::{Connection, Profile, Route, Store, ThreadRow};
use crate::{
    codex::{
        activity::{Activity, Change},
        frame::{Frame, RequestId},
        history::{ReadThread, ThreadPage, TurnPage},
        items::ThreadItem,
        notice::Notification,
        reply::Reply,
    },
    links::codex::{Launch, Link, LinkEvent},
};
use serde_json::json;

fn store() -> Store {
    let project = std::env::temp_dir();
    let link = Link::spawn(
        Launch {
            executable: "/bin/cat".into(),
            arguments: vec![],
            directory: project.clone(),
            path: "/usr/bin:/bin".into(),
        },
        |_| {},
    )
    .unwrap();
    let mut store = Store::open(
        Profile {
            project,
            route: Route::CODEX_SUBSCRIPTION,
            provider: "openai".into(),
        },
        link,
    )
    .unwrap();
    store.state = Connection::Ready;
    store
}

fn row(name: Option<&str>, preview: &str) -> ThreadRow {
    serde_json::from_value(json!({"id":"thread","cwd":std::env::temp_dir(),"modelProvider":"openai","name":name,"preview":preview})).unwrap()
}

fn title(store: &Store) -> &str {
    store.catalog_row("thread").unwrap().title()
}

#[test]
fn list_and_restored_identity_can_arrive_in_either_order() {
    for restore_first in [true, false] {
        let mut s = store();
        s.list_more().unwrap();
        let list = s.seq;
        if restore_first {
            s.select_from_catalog(row(None, "")).unwrap();
        }
        let mut native = row(Some("Native name"), "Native preview");
        native.updated_at = 1_795_543_164;
        s.apply(LinkEvent::Frame(Frame::Reply {
            id: RequestId::Number(list),
            result: Ok(Reply::Threads(ThreadPage {
                data: vec![native],
                next_cursor: None,
            })),
        }));
        if !restore_first {
            s.select_from_catalog(row(None, "")).unwrap();
        }
        assert_eq!(title(&s), "Native name");
        assert_eq!(s.catalog_row("thread").unwrap().preview, "Native preview");
        assert_eq!(s.catalog_row("thread").unwrap().updated_at, 1_795_543_164);
    }
}

#[test]
fn verified_read_fills_placeholder_and_a_late_snapshot_cannot_undo_rename() {
    let mut s = store();
    s.select_from_catalog(row(None, "")).unwrap();
    s.checked_read(
        "thread",
        ReadThread {
            thread: Box::new(row(Some("Native name"), "Original message")),
        },
    );
    assert_eq!(title(&s), "Native name");
    s.notification(Notification::Renamed {
        thread: "thread".into(),
        name: Some("User rename".into()),
    })
    .unwrap();
    s.checked_read(
        "thread",
        ReadThread {
            thread: Box::new(row(Some("Older name"), "Older preview")),
        },
    );
    s.observed_row(row(None, ""));
    assert_eq!(title(&s), "User rename");
    assert_eq!(s.catalog_row("thread").unwrap().preview, "Original message");
}

#[test]
fn first_textual_input_survives_blank_image_and_followup_items_then_yields_to_stock() {
    let mut s = store();
    s.select_from_catalog(row(None, "")).unwrap();
    let notify = |s: &mut Store, content| {
        let item =
            ThreadItem::decode(&json!({"type":"userMessage","id":"i","content":content})).unwrap();
        s.catalog_activity(&Activity {
            thread: "thread".into(),
            turn: "t".into(),
            change: Change::Item {
                row: item,
                completed: true,
            },
        });
    };
    notify(
        &mut s,
        json!([{"type":"localImage","path":"/tmp/picture.png"},{"type":"text","text":" \n " }]),
    );
    assert_eq!(title(&s), "Untitled thread");
    let long = "ø".repeat(130);
    notify(
        &mut s,
        json!([{"type":"text","text":format!("  \n\n {long}\nsecond line")}]),
    );
    notify(&mut s, json!([{"type":"text","text":"Follow-up"}]));
    assert_eq!(title(&s), "ø".repeat(120));

    let page:TurnPage=serde_json::from_value(json!({"data":[
        {"id":"new","status":"completed","items":[{"type":"userMessage","id":"later","content":[{"type":"text","text":"Later turn"}]}]},
        {"id":"old","status":"completed","items":[{"type":"userMessage","id":"oldest","content":[{"type":"text","text":"Original historical question"}]}]}
    ]})).unwrap();
    s.history_page("thread", page);
    assert_eq!(title(&s), "Original historical question");
    s.sync_title(&row(None, "Stock preview"));
    notify(&mut s, json!([{"type":"text","text":"Another follow-up"}]));
    assert_eq!(title(&s), "Stock preview");
}

#[test]
fn meaningful_native_names_take_precedence_over_nickname_and_preview() {
    let mut r = row(Some("Review permissions"), "First prompt");
    r.agent_nickname = Some("Raven".into());
    assert_eq!(r.title(), "Review permissions");
    r.name = Some("  ".into());
    assert_eq!(r.title(), "Raven");
    r.agent_nickname = Some(" ".into());
    assert_eq!(r.title(), "First prompt");
}

#[test]
fn first_send_uses_the_first_nonempty_line_and_never_renames_an_image_only_draft() {
    for (text, expected) in [
        (
            " \n\n  Compare the two reports  \n# analysis",
            "Compare the two reports",
        ),
        ("", "Untitled thread"),
    ] {
        let mut s = store();
        s.edit(None, text.into());
        if text.is_empty() {
            assert!(s.submit(None).is_err());
            s.attach_image(
                None,
                crate::mentions::ImageInput::File("attachment.png".into()),
            );
        }
        s.submit(None).unwrap();
        let opened = serde_json::from_value(json!({
            "thread":{"id":"thread","modelProvider":"openai","cwd":std::env::temp_dir()},
            "modelProvider":"openai","model":"stock-model"
        }))
        .unwrap();
        s.apply(LinkEvent::Frame(Frame::Reply {
            id: RequestId::Number(s.seq),
            result: Ok(Reply::Opened(opened)),
        }));
        assert_eq!(title(&s), expected);
        if !text.is_empty() {
            // Reconnecting may load the newest page before the first turn.
            let recent: TurnPage = serde_json::from_value(json!({"nextCursor":"older", "data":[
                {"id":"new","status":"completed","items":[{"type":"userMessage","id":"later","content":[{"type":"text","text":"Later question"}]}]}
            ]})).unwrap();
            s.history_page("thread", recent);
            assert_eq!(title(&s), expected);
        }
    }
}

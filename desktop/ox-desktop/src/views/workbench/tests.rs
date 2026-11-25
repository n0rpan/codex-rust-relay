use super::{Connection, Instant, Route, open_store};
use ox_desk_core::{transcript::Row, transcript::item::Item};
use std::{sync::mpsc, time::Duration};

#[test]
#[ignore = "requires the stock Codex OX worker fixture"]
fn stock_ox_worker() {
    use ox_desk_core::{actions::Action, origin::Relation, runtime::Billing};
    // The parent starts a worker through its OX tools. The host owns the worker
    // as an ordinary session on the worker's own API route, stamped with its origin.
    let (tx, rx) = mpsc::channel();
    let dir = std::env::current_dir().unwrap();
    let mut store = open_store(dir.clone(), Route::CODEX_SUBSCRIPTION, move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(28);
    let mut sent = false;
    let mut parent = None;
    let mut allowed = false;
    loop {
        if let Ok(e) = rx.recv_timeout(Duration::from_millis(15)) {
            store.apply(e);
        }
        store.tick(Instant::now());
        assert!(Instant::now() < deadline, "notice={:?}", store.notice());
        if !sent && store.state() == &Connection::Ready {
            store.edit(None, "Start the OX worker fixture.".into());
            store.submit(None).unwrap();
            sent = true;
        }
        if parent.is_none() {
            parent = store.selected().map(str::to_owned);
        }
        if let Some(ask) = store.asks().first() {
            assert!(
                ask.detail.contains("start_ox_worker") && ask.detail.contains("fixture/demo"),
                "{}",
                ask.detail
            );
            let choice = ask
                .decisions()
                .iter()
                .position(|d| d.label == "Allow tool call")
                .unwrap();
            let id = ask.id.clone();
            store.answer(&id, choice).unwrap();
            allowed = true;
        }
        if allowed && store.asks().is_empty() {
            break;
        }
    }
    let parent = parent.unwrap();
    let route = Route::codex_api("fixture/demo".into());
    let (tx, rx) = mpsc::channel();
    let mut worker = open_store(dir, route.clone(), move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    let mut steered = false;
    let mut stopped = false;
    let id = loop {
        if let Ok(e) = rx.recv_timeout(Duration::from_millis(15)) {
            worker.apply(e);
        }
        assert!(Instant::now() < deadline + Duration::from_secs(20));
        let Some(id) = worker
            .threads()
            .iter()
            .map(|r| r.id.clone())
            .find(|id| worker.origin(id).is_some())
        else {
            continue;
        };
        let origin = worker.origin(&id).unwrap().clone();
        assert_eq!(origin.relation, Relation::Worker);
        assert_eq!(origin.parent.session.as_deref(), Some(parent.as_str()));
        assert_eq!(
            origin.parent.scope.as_ref().map(|s| &s.route),
            Some(&Route::CODEX_SUBSCRIPTION)
        );
        assert_eq!(origin.route.billing, Billing::Api("fixture/demo".into()));
        if worker.selected() != Some(id.as_str()) {
            worker.select(&id).unwrap();
            continue;
        }
        let running = worker
            .transcript(&id)
            .is_some_and(|t| t.active_turn().is_some());
        if running && !steered && worker.can_send(Some(&id)) {
            worker
                .dispatch(Action::SendText(
                    Some(id.clone()),
                    "Apply the OX worker clarification.".into(),
                ))
                .unwrap();
            steered = true;
        } else if running && steered && !stopped {
            worker.dispatch(Action::Stop(id.clone())).unwrap();
            stopped = true;
        } else if stopped && !running {
            break id;
        }
    };
    // The worker keeps its stamp after the subscriber reconnects.
    drop(worker);
    let (tx, rx) = mpsc::channel();
    let mut again = open_store(std::env::current_dir().unwrap(), route, move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    let until = Instant::now() + Duration::from_secs(8);
    while again.origin(&id).is_none() {
        again.apply(
            rx.recv_timeout(until.saturating_duration_since(Instant::now()))
                .unwrap(),
        );
    }
    assert_eq!(
        again.origin(&id).unwrap().parent.session.as_deref(),
        Some(parent.as_str())
    );
}
#[test]
#[ignore = "requires the stock Codex worker fixture"]
fn stock_subagent() {
    let (tx, rx) = mpsc::channel();
    let mut store = open_store(
        std::env::current_dir().unwrap(),
        Route::codex_api("fixture/demo".into()),
        move |event| {
            let _ = tx.send(event);
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(28);
    let mut sent = false;
    let mut parent = None;
    let mut child = None;
    let mut steered = false;
    let mut followed = false;
    let mut stopped = false;
    let mut parent_answered = false;
    let mut stop_complete = false;
    loop {
        let event=rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap_or_else(|e|panic!("Worker fixture {e}: parent={parent:?} child={child:?} steered={steered} followed={followed} stopped={stopped} root_answered={parent_answered}, catalog={:?} asks={:?}",store.threads().iter().map(|r|(&r.id,&r.parent_thread_id)).collect::<Vec<_>>(),store.asks().iter().map(|a|(&a.thread,&a.detail)).collect::<Vec<_>>()));
        store.apply(event);
        assert!(store.notice().is_none(), "{:?}", store.notice());
        if !sent && store.state() == &Connection::Ready {
            store.edit(None, "Create the desktop fixture worker".into());
            store.submit(None).unwrap();
            sent = true;
        }
        if parent.is_none() {
            parent = store.selected().map(str::to_owned);
        }
        let Some(root) = parent.as_deref() else {
            continue;
        };
        if child.is_none()
            && let Some(row) = store
                .threads()
                .iter()
                .find(|r| r.parent_thread_id.as_deref() == Some(root))
        {
            let id = row.id.clone();
            store.select(&id).unwrap();
            child = Some(id);
        }
        let Some(id) = child.as_deref() else { continue };
        let t = store.transcript(id).unwrap();
        assert!(t.error().is_none(), "{:?}", t.error());
        if !t.is_ready() {
            continue;
        }
        let approval = |token: &str, thread: &str| {
            store
                .asks()
                .iter()
                .find(|a| a.thread.as_deref() == Some(thread) && a.detail.contains(token))
                .map(|a| {
                    (
                        a.id.clone(),
                        a.decisions()
                            .iter()
                            .position(|b| b.label == "Allow once")
                            .unwrap(),
                    )
                })
        };
        if !steered
            && t.active_turn().is_some()
            && let Some((ask, choice)) = approval("child-probe", id)
        {
            store.edit(Some(id), "Apply the fixture clarification.".into());
            store.submit(Some(id)).unwrap();
            store.answer(&ask, choice).unwrap();
            steered = true;
        } else if steered && !followed && !store.sending(Some(id)) && t.active_turn().is_none() {
            let reply=(0..t.len()).any(|i|matches!(t.row(i),Some(Row::Message(k)) if matches!(&t.entry(k).unwrap().item,Item::AgentMessage{text,..} if text=="desk-worker-answer")));
            if reply {
                store.edit(Some(id), "Confirm the worker follow-up.".into());
                store.submit(Some(id)).unwrap();
                followed = true;
            }
        } else if followed && !stopped && approval("child-followup", id).is_some() {
            store.stop(id).unwrap();
            stopped = true;
        } else if stopped
            && !parent_answered
            && t.active_turn().is_none()
            && let Some((ask, choice)) = approval("parent-probe", root)
        {
            stop_complete = true;
            store.select(root).unwrap();
            store.answer(&ask, choice).unwrap();
            parent_answered = true;
        }
        if parent_answered && store.transcript(root).unwrap().active_turn().is_none() {
            break;
        }
    }
    assert!(steered && followed && stop_complete);
}

#[test]
#[ignore = "runs after a stock turn in the desktop loopback fixture"]
fn stock_reopen() {
    let route = match std::env::var("OX_DESKTOP_FIXTURE_MODE").as_deref() {
        Ok("native") => Route::CODEX_SUBSCRIPTION,
        Ok("allow") => Route::codex_api("fixture/demo".into()),
        other => panic!("Unexpected fixture route {other:?}"),
    };
    let (tx, rx) = mpsc::channel();
    let mut store = open_store(std::env::current_dir().unwrap(), route, move |event| {
        let _ = tx.send(event);
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        store.apply(
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("stock workbench timed out"),
        );
        assert!(store.notice().is_none(), "{:?}", store.notice());
        assert!(
            !matches!(
                store.state(),
                Connection::Closed(_) | Connection::SignInRequired
            ),
            "{:?}",
            store.state()
        );
        if let Some(id) = store.selected() {
            let thread = store.transcript(id).unwrap();
            assert!(thread.error().is_none(), "{:?}", thread.error());
            if !thread.is_loading() {
                let answer=(0..thread.len()).any(|ix| {
                    let Some(Row::Message(key))=thread.row(ix) else { return false; };
                    matches!(&thread.entry(key).unwrap().item,Item::AgentMessage{text,..} if text.contains("fixture complete"))
                });
                assert!(
                    answer,
                    "resumed thread did not contain the stock provider's reply"
                );
                break;
            }
        } else if let Some(row) = store.threads().first() {
            // Selecting before the route is ready queues the open; no notice appears.
            store.select(&row.id.clone()).unwrap();
        }
    }
    let api = store.profile().route.is_api();
    store.new_session();
    store.edit(None, "Exercise the workbench transport".into());
    store.submit(None).unwrap();
    let mut answered = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        store.apply(
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("stock turn timed out"),
        );
        assert!(store.notice().is_none(), "{:?}", store.notice());
        if let Some(ask) = store.asks().first() {
            assert!(
                api && ask.detail.contains("approved.txt"),
                "unexpected request: {}",
                ask.detail
            );
            let choice = ask
                .decisions()
                .iter()
                .position(|d| d.label == "Allow once")
                .unwrap();
            let id = ask.id.clone();
            store.answer(&id, choice).unwrap();
            answered = true;
        }
        if let Some(id) = store.selected() {
            let t = store.transcript(id).unwrap();
            assert!(t.error().is_none(), "{:?}", t.error());
            let answered_turn=(0..t.len()).any(|ix| {
                let Some(Row::Message(key))=t.row(ix) else {return false;};
                matches!(&t.entry(key).unwrap().item,Item::AgentMessage{text,..} if text.contains("fixture complete"))
            });
            if answered_turn && t.active_turn().is_none() && !store.sending(Some(id)) {
                break;
            }
        }
    }
    assert_eq!(api, answered);
    let id = store.selected().unwrap().to_owned();
    use ox_desk_core::runtime::Access;
    for preset in [
        Access::Yolo,
        Access::ReadOnly,
        Access::AutoReview,
        Access::Workspace,
    ] {
        store.set_access(&id, preset).unwrap();
        let deadline = Instant::now() + Duration::from_secs(6);
        while store.transcript(&id).unwrap().access() != Some(preset)
            || !store.can_change_access(&id)
        {
            store.apply(
                rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("missing applied permission settings"),
            );
        }
    }
    while !store.has_command_schema() || store.commands_loading() {
        store.apply(rx.recv_timeout(Duration::from_secs(5)).unwrap());
    }
    if !api {
        use ox_desk_core::store::routes_thread::Choice;
        let caps = store.command_caps(Some(&id));
        let (model, info) = caps
            .models
            .iter()
            .filter(|(_, m)| !m.efforts.is_empty())
            .max_by_key(|(key, _)| {
                (
                    caps.catalog
                        .get(*key)
                        .is_some_and(|tiers| !tiers.is_empty()),
                    *key == &caps.model,
                )
            })
            .unwrap();
        let effort = info
            .efforts
            .iter()
            .find(|e| e.reasoning_effort == "medium")
            .unwrap_or(&info.efforts[0])
            .reasoning_effort
            .clone();
        let tier = caps
            .catalog
            .get(model)
            .and_then(|ts| ts.first())
            .map(|t| t.id.clone());
        for choice in [
            Choice::Model(model.clone()),
            Choice::Effort(effort.clone()),
            Choice::Speed(tier.clone()),
        ] {
            store.choose(Some(&id), choice).unwrap();
            while store.changing_options(&id) {
                store.apply(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            }
            assert!(store.transcript(&id).unwrap().error().is_none());
        }
        let applied = store.options(Some(&id));
        assert_eq!(
            (&applied.model, &applied.effort, &applied.tier),
            (model, &Some(effort), &tier)
        );
        std::fs::write(std::env::current_dir().unwrap().join("expected-model-settings.json"),serde_json::to_vec(&serde_json::json!({"model":applied.model,"effort":applied.effort,"tier":applied.tier})).unwrap()).unwrap();
        store.edit(Some(&id), "verify-picked-settings".into());
        store.submit(Some(&id)).unwrap();
        let mut began = false;
        loop {
            store.apply(rx.recv_timeout(Duration::from_secs(15)).unwrap());
            let t = store.transcript(&id).unwrap();
            assert!(t.error().is_none(), "{:?}", t.error());
            began |= t.active_turn().is_some();
            if began && t.active_turn().is_none() {
                break;
            }
        }
        assert!(
            std::env::current_dir()
                .unwrap()
                .join("model-settings-received")
                .exists()
        );
    }
    let mut commands = vec![("/speed default", "Session speed set to default.")];
    if !api {
        commands.extend([
            (
                "/goal Exercise workbench commands",
                "Goal (active): Exercise workbench commands",
            ),
            ("/goal", "Goal (active): Exercise workbench commands"),
            ("/goal pause", "Goal (paused): Exercise workbench commands"),
            ("/goal resume", "Goal (active): Exercise workbench commands"),
            ("/goal clear", "Goal cleared."),
        ]);
    }
    commands.insert(
        0,
        (
            "/rename Workbench command fixture",
            "Session renamed to Workbench command fixture.",
        ),
    );
    for (command, expected) in commands {
        let deadline = Instant::now() + Duration::from_secs(5);
        store.edit(Some(&id), command.into());
        let effect = store
            .dispatch(ox_desk_core::actions::Action::ExecuteCommand(Some(
                id.clone(),
            )))
            .unwrap();
        assert!(
            matches!(effect, ox_desk_core::actions::Effect::Applied),
            "{command} did not run natively"
        );
        while store.sending(Some(&id)) {
            store.apply(
                rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap(),
            );
        }
        assert_eq!(
            store.command_notice(),
            Some(expected),
            "{command}: {:?}",
            store.transcript(&id).unwrap().error()
        );
    }
}

#[test]
#[ignore = "requires stock Codex, the ox binary and the loopback provider fixture"]
fn stock_session() {
    use ox_desk_core::{links::LinkEvent, store::Store, transcript::Transcript};
    let Ok(mode) = std::env::var("OX_DESKTOP_FIXTURE_MODE") else {
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/stock_session.py");
        assert!(
            std::process::Command::new("python3")
                .arg(script)
                .arg(std::env::current_exe().unwrap())
                .status()
                .unwrap()
                .success()
        );
        return;
    };
    let project = std::env::current_dir().unwrap();
    let route = match mode.as_str() {
        "native" | "wrong-auth" => Route::CODEX_SUBSCRIPTION,
        "legacy" => Route::codex_api("demo".into()),
        "allow" | "deny" | "stop" => Route::codex_api("fixture/demo".into()),
        _ => panic!("Unexpected stock session fixture {mode}"),
    };
    let connect = || {
        let (tx, rx) = mpsc::channel();
        let store = open_store(project.clone(), route.clone(), move |event| {
            let _ = tx.send(event);
        })
        .unwrap();
        (store, rx)
    };
    fn advance(store: &mut Store, rx: &mpsc::Receiver<LinkEvent>, deadline: Instant) {
        let event = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|err| {
                panic!("Stock Codex timed out: {err}, state {:?}", store.state())
            });
        store.apply(event);
        assert!(
            !matches!(store.state(), Connection::Closed(_)),
            "{:?}",
            store.state()
        );
        assert!(store.notice().is_none(), "{:?}", store.notice());
        if let Some(thread) = store.selected().and_then(|id| store.transcript(id)) {
            assert!(thread.error().is_none(), "{:?}", thread.error());
        }
    }
    fn answers(t: &Transcript) -> usize {
        (0..t.len()).filter(|ix| matches!(t.row(*ix),Some(Row::Message(k))
            if matches!(&t.entry(k).unwrap().item,Item::AgentMessage{text,..} if text=="fixture complete"))).count()
    }

    let deadline = Instant::now() + Duration::from_secs(35);
    let (mut store, rx) = connect();
    while store.state() == &Connection::Connecting {
        advance(&mut store, &rx, deadline);
    }
    if mode == "wrong-auth" {
        assert_eq!(store.state(), &Connection::SignInRequired);
        assert!(store.submit(None).is_err());
        return;
    }
    assert_eq!(store.state(), &Connection::Ready);
    if mode == "native" {
        store.read_limits().unwrap();
        while store.usage.loading {
            advance(&mut store, &rx, deadline);
        }
        assert!(store.usage.error.is_none(), "{:?}", store.usage.error);
        assert_eq!(
            store
                .usage
                .closest_limit(chrono::Utc::now().timestamp())
                .unwrap()
                .1
                .used_percent,
            92
        );
    }
    let prompt = format!("desktop {mode} fixture");
    store.edit(None, prompt.clone());
    store.submit(None).unwrap();
    let mut handled = false;
    let id = loop {
        advance(&mut store, &rx, deadline);
        let Some(id) = store.selected().map(str::to_owned) else {
            continue;
        };
        if !handled && let Some(ask) = store.asks().first() {
            assert_eq!(ask.thread.as_deref(), Some(id.as_str()));
            assert!(ask.detail.contains("echo accepted"));
            if mode == "stop" {
                store.stop(&id).unwrap();
            } else {
                let label = if mode == "deny" {
                    "Decline"
                } else {
                    "Allow once"
                };
                let choice = ask
                    .decisions()
                    .iter()
                    .position(|d| d.label == label)
                    .or_else(|| {
                        (mode == "deny")
                            .then(|| {
                                ask.decisions()
                                    .iter()
                                    .position(|d| d.label == "Cancel turn")
                            })
                            .flatten()
                    })
                    .expect("Stock Codex must offer a matching approval decision");
                store.answer(&ask.id.clone(), choice).unwrap();
            }
            handled = true;
        }
        let t = store.transcript(&id).unwrap();
        let done = if matches!(mode.as_str(), "stop" | "deny") {
            handled && store.asks().is_empty()
        } else {
            answers(t) == 1
        };
        if done && t.active_turn().is_none() && !store.sending(Some(&id)) {
            let command_completed = (0..t.len()).any(|ix| matches!(t.row(ix), Some(Row::Tools(keys))
                if keys.iter().any(|k| matches!(&t.entry(k).unwrap().item, Item::CommandExecution{status,command,..}
                    if status=="completed" && command.contains("echo accepted")))));
            assert_eq!(command_completed, mode == "allow");
            break id;
        }
    };
    assert_eq!(handled, matches!(mode.as_str(), "allow" | "deny" | "stop"));
    if mode == "native" {
        while store.transcript(&id).unwrap().usage.is_none() {
            advance(&mut store, &rx, deadline);
        }
        let usage = store.transcript(&id).unwrap().usage.as_ref().unwrap();
        assert_eq!(usage.last.total_tokens, 2);
        assert!(usage.total.total_tokens >= usage.last.total_tokens);
        assert!(usage.context_percent().is_some_and(|p| p > 0.));
    }
    let marker = project.join("approved.txt");
    if mode == "allow" {
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "accepted\n");
    } else {
        assert!(!marker.exists());
    }
    if matches!(mode.as_str(), "deny" | "stop") {
        return;
    }
    // The desktop layout persists only identity. Reproduce a cold selection while
    // stock thread/list is still in flight, using the actual app-server's metadata.
    let mut saved_identity = store.catalog_row(&id).unwrap().clone();
    if mode == "allow" {
        assert_eq!(saved_identity.title(), prompt);
    }
    saved_identity.name = None;
    saved_identity.preview.clear();
    drop(store);
    while !matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        LinkEvent::Closed { .. }
    ) {}

    let (mut store, rx) = connect();
    if mode == "allow" {
        while store.state() == &Connection::Connecting {
            advance(&mut store, &rx, deadline);
        }
        store.select_from_catalog(saved_identity).unwrap();
    } else {
        while !store.threads().iter().any(|row| row.id == id) {
            advance(&mut store, &rx, deadline);
        }
        store.select(&id).unwrap();
    }
    while !store.transcript(&id).unwrap().is_ready() {
        advance(&mut store, &rx, deadline);
    }
    if mode == "allow" {
        assert_eq!(store.catalog_row(&id).unwrap().title(), prompt);
    }
    let t = store.transcript(&id).unwrap();
    assert_eq!(answers(t), 1);
    assert!((0..t.len()).any(|ix|matches!(t.row(ix),Some(Row::Message(k))
        if matches!(&t.entry(k).unwrap().item,Item::UserMessage{content} if content.iter().any(|c|c.text.as_ref()==Some(&prompt))))));
    if mode == "legacy" {
        assert!(
            store
                .select("00000000-0000-0000-0000-000000000000")
                .is_err()
        );
        assert_eq!(store.selected(), Some(id.as_str()));
    }
    store.edit(Some(&id), "Continue the fixture.".into());
    store.submit(Some(&id)).unwrap();
    loop {
        advance(&mut store, &rx, deadline);
        let t = store.transcript(&id).unwrap();
        if answers(t) == 2 && t.active_turn().is_none() && !store.sending(Some(&id)) {
            break;
        }
    }
    use ox_desk_core::actions::Action;
    store.dispatch(Action::ForkThread(id.clone())).unwrap();
    while store.selected() == Some(&id)
        || !store
            .selected()
            .is_some_and(|fork| store.can_manage_thread(fork))
    {
        advance(&mut store, &rx, deadline);
    }
    let fork = store.selected().unwrap().to_owned();
    assert_eq!(answers(store.transcript(&fork).unwrap()), 2);
    store
        .dispatch(Action::RenameThread(fork.clone(), "Forked session".into()))
        .unwrap();
    while !store.can_manage_thread(&fork)
        || store.catalog_row(&fork).unwrap().name.as_deref() != Some("Forked session")
    {
        advance(&mut store, &rx, deadline);
    }
    assert_eq!(store.catalog_row(&fork).unwrap().title(), "Forked session");
    store.dispatch(Action::ArchiveThread(fork.clone())).unwrap();
    while store.catalog_row(&fork).is_some() {
        advance(&mut store, &rx, deadline);
    }
    assert!(store.selected().is_none());
    store.select(&id).unwrap();
    assert!(store.transcript(&id).unwrap().is_ready());
}

mod model_switch_fixture {
    use super::*;
    use ox_desk_core::{links::LinkEvent, store::Store};
    pub(super) fn connect(alias: &str) -> (Store, mpsc::Receiver<LinkEvent>) {
        let (tx, rx) = mpsc::channel();
        let store = open_store(
            std::env::current_dir().unwrap(),
            Route::codex_api(alias.into()),
            move |e| {
                let _ = tx.send(e);
            },
        )
        .unwrap();
        (store, rx)
    }
    #[track_caller]
    pub(super) fn until(
        store: &mut Store,
        rx: &mpsc::Receiver<LinkEvent>,
        condition: impl Fn(&Store) -> bool,
    ) {
        let caller = std::panic::Location::caller();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !condition(store) {
            let e = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|_| {
                    panic!(
                        "Timed out at {caller}: {:?} {:?} {:?}",
                        store.notice(),
                        store.state(),
                        store.threads()
                    )
                });
            store.apply(e);
            assert!(store.notice().is_none(), "{:?}", store.notice());
            if let Some(t) = store.selected().and_then(|id| store.transcript(id)) {
                assert!(t.error().is_none(), "{:?}", t.error());
            }
        }
    }
}
#[test]
#[ignore = "requires the stock loopback OX model switch fixture"]
fn stock_model_switch() {
    use model_switch_fixture::{connect, until};
    use ox_desk_core::{links::LinkEvent, store::routes_thread::Choice};
    let (mut store, rx) = connect("fixture/demo");
    until(&mut store, &rx, |s| s.state() == &Connection::Ready);
    until(&mut store, &rx, |s| {
        s.has_command_schema() && !s.commands_loading()
    });
    let caps = store.command_caps(None);
    assert!(
        caps.models.contains_key("fixture/alternate"),
        "Advertised {:?}",
        caps.models.keys().collect::<Vec<_>>()
    );
    store.edit(None, "model-switch-first".into());
    store.submit(None).unwrap();
    until(&mut store, &rx, |s| {
        s.selected()
            .and_then(|id| s.transcript(id))
            .is_some_and(|t| t.active_turn().is_none() && (0..t.len()).any(|i|matches!(t.row(i),Some(Row::Message(k)) if matches!(&t.entry(k).unwrap().item,Item::AgentMessage{text,..} if text=="fixture complete"))))
    });
    let id = store.selected().unwrap().to_owned();
    assert!(
        store
            .choose(Some(&id), Choice::Model("other/demo".into()))
            .is_err()
    );
    store
        .choose(Some(&id), Choice::Model("fixture/alternate".into()))
        .unwrap();
    until(&mut store, &rx, |s| !s.changing_options(&id));
    assert_eq!(store.options(Some(&id)).model, "fixture/alternate");
    store.edit(Some(&id), "model-switch-second".into());
    store.submit(Some(&id)).unwrap();
    until(&mut store, &rx, |s| {
        let t = s.transcript(&id).unwrap();
        t.active_turn().is_none() && (0..t.len()).filter(|&i|matches!(t.row(i),Some(Row::Message(k)) if matches!(&t.entry(k).unwrap().item,Item::AgentMessage{text,..} if text=="fixture complete"))).count()==2
    });
    assert_eq!(store.selected(), Some(id.as_str()));
    drop(store);
    while !matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        LinkEvent::Closed { .. }
    ) {}
    // The host still owns the live runtime after its first desktop disconnects.
    let (mut warm, events) = connect("fixture/demo");
    until(&mut warm, &events, |s| s.catalog_row(&id).is_some());
    warm.select(&id).unwrap();
    until(&mut warm, &events, |s| {
        s.transcript(&id).is_some_and(|t| t.is_ready())
    });
    assert_eq!(
        warm.thread_route(Some(&id)),
        Route::codex_api("fixture/alternate".into())
    );
    std::fs::write("model-switch-thread", id).unwrap();
}

#[test]
#[ignore = "runs after the isolated host restarts following stock_model_switch"]
fn stock_model_cold_routes() {
    use model_switch_fixture::{connect, until};
    let id = std::fs::read_to_string("model-switch-thread").unwrap();
    for wrong in ["fixture/demo", "other/demo"] {
        let (mut other, events) = connect(wrong);
        // The host lists the remembered thread before the route is ready, without its model.
        until(&mut other, &events, |s| {
            s.state() == &Connection::Ready && s.catalog_row(&id).is_some_and(|r| r.model.is_some())
        });
        assert_eq!(
            other.catalog_row(&id).unwrap().model.as_deref(),
            Some("fixture/alternate")
        );
        assert!(other.select(&id).is_err());
        assert!(other.selected().is_none());
        if wrong == "other/demo" {
            other.edit(None, "model-switch-foreign".into());
            other.submit(None).unwrap();
            until(&mut other, &events, |s| {
                s.selected().and_then(|owner|s.transcript(owner)).is_some_and(|t|
                    t.active_turn().is_none() && (0..t.len()).any(|i|matches!(t.row(i),Some(Row::Message(k)) if matches!(&t.entry(k).unwrap().item,Item::AgentMessage{text,..} if text=="fixture complete"))))
            });
            assert_ne!(other.selected(), Some(id.as_str()));
            assert!(
                std::env::current_dir()
                    .unwrap()
                    .join("other-provider-received")
                    .exists()
            );
        }
    }
    let (mut cold, rx) = connect("fixture/alternate");
    until(&mut cold, &rx, |s| s.catalog_row(&id).is_some());
    cold.select(&id).unwrap();
    until(&mut cold, &rx, |s| s.transcript(&id).unwrap().is_ready());
    assert_eq!(cold.options(Some(&id)).model, "fixture/alternate");
}

#[test]
#[ignore = "launch with the isolated stock Claude provider from claude_stock_session.py"]
fn claude_workbench_session() {
    use ox_desk_core::{
        actions::Action,
        links::LinkEvent,
        runtime::{Access, Runtime},
        store::{Connection, Store},
    };
    use std::{
        path::Path,
        sync::mpsc,
        time::{Duration, Instant},
    };

    struct Pane {
        store: Store,
        events: mpsc::Receiver<LinkEvent>,
    }
    impl Pane {
        fn open(dir: &Path, alias: Option<&str>) -> Self {
            let (tx, events) = mpsc::channel();
            let store = super::open_store(
                dir.into(),
                Runtime::Claude.route(alias.map(str::to_owned)),
                move |e| {
                    let _ = tx.send(e);
                },
            )
            .unwrap();
            Self { store, events }
        }
        fn wait(&mut self, what: &str, pred: impl Fn(&Store) -> bool) {
            let limit = Instant::now() + Duration::from_secs(45);
            while !pred(&self.store) {
                let event=self.events.recv_timeout(limit.saturating_duration_since(Instant::now())).unwrap_or_else(|e|panic!("{what}: {e}, state {:?}, notice {:?}, selected {:?}, thread error {:?}",self.store.state(),self.store.notice(),self.store.selected(),self.store.selected().and_then(|id|self.store.transcript(id)).and_then(|t|t.error())));
                self.store.apply(event);
                assert!(
                    !matches!(self.store.state(), Connection::Closed(_)),
                    "{what}: {:?}",
                    self.store.state()
                );
            }
        }
        fn act(&mut self, action: Action) {
            self.store.dispatch(action).unwrap();
        }
        fn send(&mut self, text: &str) {
            let id = self.store.selected().map(str::to_owned);
            self.act(Action::DraftEdit(id.clone(), text.into()));
            let creating = id.is_none();
            self.act(Action::Submit(id));
            if creating {
                self.wait("host assigned the Claude session", |s| {
                    s.selected().is_some()
                });
            }
        }
        fn idle(&mut self, id: &str) {
            self.wait("completed turn", |s| {
                s.transcript(id)
                    .is_some_and(|t| t.is_ready() && t.active_turn().is_none())
            });
        }
        fn decide(&mut self, label: &str) {
            let ask = &self.store.asks()[0];
            let choice = ask
                .decisions()
                .iter()
                .position(|d| d.label == label)
                .expect(label);
            let id = ask.id.clone();
            self.act(Action::AnswerAsk(id, choice));
        }
    }
    let mode = std::env::var("OX_CLAUDE_WORKBENCH_CASE").expect("isolated fixture");
    let root = std::env::current_dir().unwrap();
    let api = (mode == "api").then_some("fixture/demo");
    let mut a = Pane::open(&root, api);
    if mode == "signin" {
        a.wait("native sign-in", |s| {
            s.state() == &Connection::SignInRequired
        });
        assert!(!a.store.can_send(None));
        return;
    }
    a.wait("ready", |s| s.state() == &Connection::Ready);
    assert!(!a.store.command_caps(None).supports_goal() && !a.store.command_caps(None).speed);
    if mode == "subscription" {
        a.send("Native OX desktop subscription turn");
        let id = a.store.selected().unwrap().to_owned();
        a.idle(&id);
        assert!(a.store.transcript(&id).unwrap().len() >= 2);
        return;
    }
    let beta = root.with_file_name("WorkbenchBeta");
    let project_settings = beta.join(".claude");
    std::fs::create_dir_all(&project_settings).unwrap();
    std::fs::write(
        project_settings.join("settings.json"),
        r#"{"permissions":{"disableAutoMode":"disable"}}"#,
    )
    .unwrap();
    let mut b = Pane::open(&beta, api);
    b.wait("second project", |s| s.state() == &Connection::Ready);
    a.send("APPROVAL from workbench Alpha");
    b.send("APPROVAL from workbench Beta");
    a.wait("Alpha approval", |s| !s.asks().is_empty());
    b.wait("Beta approval", |s| !s.asks().is_empty());
    let id = a.store.selected().unwrap().to_owned();
    let beta_id = b.store.selected().unwrap().to_owned();
    assert_eq!(a.store.asks()[0].thread.as_deref(), Some(id.as_str()));
    a.send("Steering arrived from the workbench.");
    a.decide("Allow once");
    a.idle(&id);
    assert!(root.join("approved.txt").exists());
    assert!(!beta.join("approved.txt").exists());
    b.decide("Deny");
    b.idle(&beta_id);
    assert!(!beta.join("approved.txt").exists());
    b.send("APPROVAL interrupt from workbench");
    b.wait("interruptible approval", |s| !s.asks().is_empty());
    b.act(Action::Stop(beta_id.clone()));
    b.idle(&beta_id);
    b.wait("interrupt resolves approval", |s| s.asks().is_empty());

    for preset in [
        Access::BypassPermissions,
        Access::Auto,
        Access::AcceptEdits,
        Access::Default,
    ] {
        a.act(Action::SetAccess(id.clone(), preset));
        a.wait("confirmed Claude permission mode", |s| {
            s.transcript(&id).and_then(|t| t.access()) == Some(preset)
        });
    }
    for initial in [Access::Default, Access::BypassPermissions] {
        b.act(Action::SetAccess(beta_id.clone(), initial));
        b.wait("prior Claude permission mode", |s| {
            s.applied_access(Some(&beta_id)) == Some(initial) && s.can_send(Some(&beta_id))
        });
        b.store.dismiss_notice();
        b.act(Action::SetAccess(beta_id.clone(), Access::Auto));
        b.wait("stock rejects disabled Auto mode", |s| {
            s.notice().is_some_and(|n| n.contains("auto mode disabled"))
        });
        assert_eq!(b.store.applied_access(Some(&beta_id)), Some(initial));
    }
    a.send("QUESTION-FORM for this session");
    a.wait("native question form", |s| {
        s.asks().iter().any(|a| !a.questions.is_empty())
    });
    let ask = &a.store.asks()[0];
    assert!(ask.questions[0].multi_select);
    let ask_id = ask.id.clone();
    a.act(Action::AnswerQuestions(ask_id, vec!["Apple, Pear".into()]));
    a.idle(&id);
    let mut other = Pane::open(&root, Some("other/demo"));
    other.wait("second billing alias", |s| s.state() == &Connection::Ready);
    let original = other
        .store
        .threads()
        .iter()
        .find(|r| r.id == id)
        .unwrap()
        .clone();
    assert!(
        other
            .store
            .dispatch(Action::OpenCatalogRow(Box::new(original)))
            .is_err()
    );
    other.send("ONLY THE SECOND ALIAS");
    let other_id = other.store.selected().unwrap().to_owned();
    other.idle(&other_id);
    drop(other);
    // Reopen through the same public catalog and selection path as an app restart.
    drop(a);
    let mut a = Pane::open(&root, api);
    a.wait("reconnected", |s| s.state() == &Connection::Ready);
    a.act(Action::SetNextAccess(Access::BypassPermissions));
    a.wait("Claude confirmed YOLO for the empty draft", |s| {
        s.applied_access(None) == Some(Access::BypassPermissions)
    });
    let row = a
        .store
        .threads()
        .iter()
        .find(|r| r.id == id)
        .expect("saved Claude session");
    assert_eq!(row.model.as_deref(), api);
    a.act(Action::OpenCatalogRow(Box::new(row.clone())));
    a.wait("stored history", |s| {
        s.transcript(&id)
            .is_some_and(|t| t.is_ready() && t.len() >= 2)
    });
    assert_eq!(a.store.applied_access(Some(&id)), Some(Access::Default));
    assert_eq!(
        a.store.applied_access(None),
        Some(Access::BypassPermissions)
    );
    a.act(Action::ForkThread(id.clone()));
    a.wait("fork identity", |s| {
        s.selected().is_some_and(|child| child != id)
    });
    let fork = a.store.selected().unwrap().to_owned();
    assert_ne!(fork, id);
    a.wait("fork ready", |s| {
        s.transcript(&fork)
            .is_some_and(|t| t.is_ready() && t.len() >= 2)
    });
    a.send("SUBAGENT from forked session");
    a.wait("nested Claude worker", |s| {
        s.threads()
            .iter()
            .any(|r| r.parent_thread_id.as_ref() == Some(&fork))
    });
    a.idle(&fork);
    let child = a
        .store
        .threads()
        .iter()
        .find(|r| r.parent_thread_id.as_ref() == Some(&fork))
        .unwrap()
        .clone();
    a.act(Action::OpenCatalogRow(Box::new(child.clone())));
    a.wait("child transcript", |s| {
        s.transcript(&child.id).is_some_and(|t| !t.is_empty())
    });
    assert_eq!(a.store.selected(), Some(child.id.as_str()));
    a.act(Action::SelectThread(fork.clone()));
    a.wait("child task complete", |s| {
        s.transcript(&child.id)
            .is_some_and(|t| t.active_turn().is_none())
    });
    a.act(Action::DraftEdit(Some(fork.clone()), "/terminal".into()));
    a.act(Action::ExecuteCommand(Some(fork.clone())));
    let limit = Instant::now() + Duration::from_secs(10);
    let handoff = loop {
        if let Some(h) = a.store.take_handoff() {
            break h;
        }
        a.store.apply(
            a.events
                .recv_timeout(limit.saturating_duration_since(Instant::now()))
                .expect("stock process exited before terminal handoff"),
        );
    };
    assert_eq!(handoff.thread.as_deref(), Some(fork.as_str()));
    assert_eq!(a.store.terminal_session_id(&fork).unwrap().len(), 36);
    a.act(Action::ResumeHere(fork.clone()));
    a.wait("return from terminal", |s| {
        s.transcript(&fork)
            .is_some_and(|t| t.is_ready() && !t.is_empty())
    });
    a.act(Action::NewDraft);
    assert_eq!(
        a.store.applied_access(None),
        Some(Access::BypassPermissions)
    );
    a.send("Use the prepared draft");
    let consumed = a.store.selected().unwrap().to_owned();
    a.idle(&consumed);
    a.act(Action::NewDraft);
    a.wait("the next draft starts with its own permission mode", |s| {
        s.applied_access(None) == Some(Access::Default)
    });
}

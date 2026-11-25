use super::*;
use serde_json::json;
use std::os::unix::fs::symlink;

const ID: &str = "01234567-abcd-4567-abcd-0123456789ab";

#[test]
fn roots_are_read_only_and_duplicates_require_an_explicit_origin() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().canonicalize().unwrap().join("p.a_b");
    let other = project.with_file_name("p-a-b");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&other).unwrap();
    let profiles = Profiles {
        subscription: tmp.path().join("native"),
        api: tmp.path().join("api"),
    };
    assert!(profiles.list(&project).unwrap().is_empty());
    assert!(!profiles.subscription.exists());
    let key = project
        .to_string_lossy()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    for profile in [&profiles.subscription, &profiles.api] {
        let dir = profile.join("projects").join(&key);
        fs::create_dir_all(&dir).unwrap();
        let row = json!({"type":"user","cwd":project,"sessionId":ID,"message":{"role":"user","content":"First prompt"}});
        let path = dir.join(format!("{ID}.jsonl"));
        fs::write(&path,format!("{row}\n{{\"type\":\"ai-title\",\"sessionId\":\"{ID}\",\"aiTitle\":\"Stock-generated title\"}}\n{{\"type\":\"custom-title\",\"sessionId\":\"{ID}\",\"customTitle\":\"Named thread\"}}\n{{\"partial\":")).unwrap();
        symlink(path, dir.join("00000000-0000-4000-8000-000000000000.jsonl")).unwrap();
    }
    assert!(profiles.list(&other).unwrap().is_empty());
    let rows = profiles.list(&project).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].origin, Origin::Ambiguous);
    assert!(rows[0].history(|_| panic!()).is_err());
    let one = profiles
        .session(&project, ID, Origin::Subscription)
        .unwrap();
    assert_eq!(one.title, "Named thread");
    assert_eq!(one.native_title.as_deref(), Some("Named thread"));
    assert_eq!(one.preview, "First prompt");
    let mut frames = vec![];
    one.history(|v| {
        frames.push(v);
        Ok(())
    })
    .unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["session_id"], ID);
    assert!(
        profiles
            .session(&project, "../escape", Origin::Api)
            .is_err()
    );
}

#[test]
fn concurrent_windows_cannot_rebind_a_session() {
    let root = tempfile::tempdir().unwrap();
    let bindings = Bindings::new(root.path().join("desktop/claude-routes.json"));
    let project = root.path();
    std::thread::scope(|scope| {
        let workers: Vec<_> = ["vendor/a", "vendor/b"]
            .into_iter()
            .map(|alias| {
                let bindings = bindings.clone();
                scope.spawn(move || bindings.bind(project, project, ID, alias))
            })
            .collect();
        let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results.into_iter().find_map(Result::err).unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
    });
    let alias = bindings.alias(project, project, ID).unwrap().unwrap();
    bindings.bind(project, project, ID, &alias).unwrap();
    let different = project.join("other");
    fs::create_dir(&different).unwrap();
    assert_eq!(bindings.alias(project, &different, ID).unwrap(), None);
}

#[test]
fn unicode_project_route_and_nested_history_match_the_stock_record() {
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let project = root.path().canonicalize().unwrap().join("with-😀");
    fs::create_dir(&project).unwrap();
    let profiles = Profiles {
        subscription: root.path().join("native"),
        api: root.path().join("relay"),
    };
    let prefix = project
        .parent()
        .unwrap()
        .to_string_lossy()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    let directory = profiles
        .api
        .join("projects")
        .join(format!("{prefix}-with---"));
    let children = directory.join(ID).join("subagents");
    fs::create_dir_all(&children).unwrap();
    let user = json!({"type":"user","sessionId":ID,"cwd":project,"message":{"role":"user","content":"Ask a child"}});
    let assistant = json!({"type":"assistant","sessionId":ID,"cwd":project,"requestedModel":"acme/code","message":{"role":"assistant","model":"code","content":[{"type":"tool_use","name":"Agent","id":"call-child","input":{}}]}});
    let result = json!({"type":"user","sessionId":ID,"cwd":project,"toolUseResult":{"agentId":"a0","description":"Review"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-child","content":"Launched."}]}});
    fs::write(
        directory.join(format!("{ID}.jsonl")),
        format!("{user}\n{assistant}\n{result}\n"),
    )
    .unwrap();
    let child = json!({"type":"assistant","agentId":"a0","isSidechain":true,"sessionId":ID,"cwd":project,"message":{"role":"assistant","content":[{"type":"text","text":"Child answer"}]}});
    fs::write(children.join("agent-a0.jsonl"), child.to_string()).unwrap();
    let session = profiles.session(&project, ID, Origin::Api).unwrap();
    assert_eq!(session.model.as_deref(), Some("acme/code"));
    let mut frames = vec![];
    session
        .history(|row| {
            frames.push(row);
            Ok(())
        })
        .unwrap();
    assert_eq!(frames[2]["tool_use_result"]["agentId"], "a0");
    assert_eq!(frames[3]["subtype"], "task_started");
    assert_eq!(frames[4]["parent_tool_use_id"], "call-child");
    assert_eq!(frames[4]["session_id"], ID);
}

#[test]
fn first_launch_binding_uses_the_same_identity_before_and_after_relay_creation() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().canonicalize().unwrap();
    let profile = project.join("not-created/claude");
    let bindings = Bindings::new(project.join("private/routes.json"));
    bindings.bind(&project, &profile, ID, "test/alias").unwrap();
    assert!(!profile.exists());
    fs::create_dir_all(&profile).unwrap();
    assert_eq!(
        bindings.alias(&project, &profile, ID).unwrap().as_deref(),
        Some("test/alias")
    );
}

#[test]
fn billing_evidence_is_from_the_origin_even_when_the_tui_switched_models() {
    let project = Path::new("/project");
    let mut header = Header {
        id: ID,
        cwd: None,
        first: None,
        title: None,
        ai_title: None,
        saw_assistant: false,
        model: None,
    };
    let sample = |model| json!({"type":"assistant","sessionId":ID,"cwd":project,"requestedModel":model,"message":{"model":"provider-code"}});
    header.observe(sample("alpha/code"));
    header.observe(sample("beta/code"));
    assert_eq!(header.model.as_deref(), Some("alpha/code"));
}

#[test]
fn global_catalog_keeps_both_origins_and_validates_each_unopened_project() {
    let root = tempfile::tempdir().unwrap();
    let profiles = Profiles {
        subscription: root.path().join("subscription"),
        api: root.path().join("api"),
    };
    let projects = [root.path().join("First"), root.path().join("Other")];
    for p in &projects {
        fs::create_dir(p).unwrap();
    }
    for p in &projects {
        for home in [&profiles.subscription, &profiles.api] {
            let dir = home.join("projects").join(project_key(p));
            fs::create_dir_all(&dir).unwrap();
            let row = json!({"type":"user","cwd":p,"sessionId":ID,"message":{"role":"user","content":"Original question"}});
            fs::write(dir.join(format!("{ID}.jsonl")), format!("{row}\n")).unwrap();
            fs::write(
                dir.join("66666666-aaaa-4444-bbbb-000000000000.jsonl"),
                b"{invalid",
            )
            .unwrap();
            let wrong = dir.join("77777777-aaaa-4444-bbbb-000000000000.jsonl");
            let other = json!({"type":"user","cwd":"/unrelated","sessionId":"77777777-aaaa-4444-bbbb-000000000000","message":{"role":"user","content":"wrong cwd"}});
            fs::write(wrong, format!("{other}\n")).unwrap();
        }
    }
    let found = profiles.all().unwrap();
    assert_eq!(found.len(), 4);
    for p in &projects {
        for origin in [Origin::Api, Origin::Subscription] {
            assert!(
                found
                    .iter()
                    .any(|s| &s.project == p && s.origin == origin && s.id == ID)
            );
        }
    }
}

#[test]
fn the_project_is_the_first_rows_cwd_so_a_cd_or_a_subagent_directory_is_not_a_change() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().canonicalize().unwrap().join("home");
    let elsewhere = root.path().canonicalize().unwrap().join("worktree");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    let profiles = Profiles {
        subscription: root.path().join("native"),
        api: root.path().join("relay"),
    };
    let key = project
        .to_string_lossy()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    let directory = profiles.subscription.join("projects").join(key);
    let children = directory.join(ID).join("subagents");
    fs::create_dir_all(&children).unwrap();
    let row = |kind: &str, cwd: &Path, id: &str, text: &str| json!({"type":kind,"cwd":cwd,"sessionId":id,"message":{"role":kind,"content":text}});
    let journal = [
        row("user", &project, ID, "start here"),
        row("assistant", &project, ID, "ok"),
        // The session ran `cd` into another directory: later rows follow it.
        row("user", &elsewhere, ID, "now there"),
        row("assistant", &elsewhere, ID, "done"),
        // Another session's row in the same file is ignored whatever its cwd.
        row("user", Path::new("/unrelated"), "77777777-aaaa-4444-bbbb-000000000000", "x"),
        json!({"type":"user","cwd":elsewhere,"sessionId":ID,"toolUseResult":{"agentId":"a1","description":"Look"},
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"ok"}]}}),
    ]
    .map(|r| r.to_string())
    .join("\n");
    fs::write(directory.join(format!("{ID}.jsonl")), journal + "\n").unwrap();
    // Subagents start in their own directory.
    let child = json!({"type":"assistant","agentId":"a1","isSidechain":true,"sessionId":ID,"cwd":elsewhere,
        "message":{"role":"assistant","content":[{"type":"text","text":"found"}]}});
    fs::write(children.join("agent-a1.jsonl"), child.to_string()).unwrap();

    let session = profiles
        .session(&project, ID, Origin::Subscription)
        .unwrap();
    assert_eq!(session.project, project);
    assert_eq!(profiles.list(&project).unwrap().len(), 1);
    let mut rows = vec![];
    session
        .history(|r| {
            rows.push(r);
            Ok(())
        })
        .unwrap();
    // Four main rows, the tool result, the task marker and the child's row.
    assert_eq!(rows.len(), 7);
    assert!(rows.iter().any(|r| r["agent_id"] == "a1"));

    // The safety intent stands: a journal whose first row is in another project is rejected.
    let other = directory.join("99999999-aaaa-4444-bbbb-000000000000.jsonl");
    fs::write(
        &other,
        row(
            "user",
            &elsewhere,
            "99999999-aaaa-4444-bbbb-000000000000",
            "hi",
        )
        .to_string(),
    )
    .unwrap();
    assert!(
        profiles
            .session(
                &project,
                "99999999-aaaa-4444-bbbb-000000000000",
                Origin::Subscription
            )
            .is_err()
    );
}

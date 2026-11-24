use ox_desk_core::codex::names::{Method, Notice, ServerMethod};
use serde_json::Value;
use std::{collections::HashSet, fs, path::Path, process::Command};

fn check_methods(dir: &Path, file: &str, names: impl Iterator<Item = &'static str>) {
    let schema: Value = serde_json::from_slice(&fs::read(dir.join(file)).unwrap()).unwrap();
    let installed: HashSet<_> = schema["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| case["properties"]["method"]["enum"][0].as_str().unwrap())
        .collect();
    for name in names {
        assert!(installed.contains(name), "{file} is missing {name}");
    }
}

#[test]
#[ignore = "CI runs against the floor and latest installed Codex binaries"]
fn installed_protocol() {
    let bin = std::env::var_os("CODEOX_TEST_CODEX_BIN").expect("path to stock Codex binary");
    let schema = tempfile::tempdir().unwrap();
    let result = Command::new(bin)
        .args([
            "app-server",
            "generate-json-schema",
            "--experimental",
            "--out",
        ])
        .arg(schema.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    check_methods(
        schema.path(),
        "ClientRequest.json",
        Method::ALL.iter().map(|x| x.as_str()),
    );
    check_methods(
        schema.path(),
        "ClientNotification.json",
        ["initialized"].into_iter(),
    );
    check_methods(
        schema.path(),
        "ServerRequest.json",
        ServerMethod::ALL.iter().map(|x| x.as_str()),
    );
    check_methods(
        schema.path(),
        "ServerNotification.json",
        Notice::ALL.iter().map(|x| x.as_str()),
    );

    for file in ["v2/TurnStartParams.json", "v2/TurnSteerParams.json"] {
        let params: Value =
            serde_json::from_slice(&fs::read(schema.path().join(file)).unwrap()).unwrap();
        let variants = params["definitions"]["UserInput"]["oneOf"]
            .as_array()
            .unwrap();
        for (kind, field) in [("localImage", "path"), ("image", "url")] {
            assert!(
                variants
                    .iter()
                    .any(|v| v["properties"]["type"]["enum"][0] == kind
                        && (v["properties"].get(field).is_some()
                            || v["anyOf"].as_array().is_some_and(|arms| arms
                                .iter()
                                .any(|arm| arm["properties"].get(field).is_some())))),
                "{file} lacks {kind}.{field}"
            );
        }
    }
    let search: Value = serde_json::from_slice(
        &fs::read(schema.path().join("FuzzyFileSearchParams.json")).unwrap(),
    )
    .unwrap();
    for field in ["query", "roots", "cancellationToken"] {
        assert!(search["properties"].get(field).is_some());
    }
    for (ty, fields) in [
        (
            "ThreadStartParams",
            "cwd model modelProvider approvalPolicy sandbox",
        ),
        (
            "ThreadResumeParams",
            "threadId cwd model modelProvider excludeTurns initialTurnsPage",
        ),
        (
            "ThreadListParams",
            "cwd limit cursor searchTerm modelProviders sourceKinds ancestorThreadId sortKey useStateDbOnly",
        ),
        (
            "ThreadTurnsListParams",
            "threadId itemsView limit cursor sortDirection",
        ),
        ("ThreadTurnsListResponse", "nextCursor data"),
        (
            "ThreadSettingsUpdateParams",
            "threadId approvalPolicy approvalsReviewer sandboxPolicy effort model serviceTier",
        ),
        ("TurnStartParams", "threadId input effort serviceTier"),
        ("TurnSteerParams", "threadId input expectedTurnId"),
        ("TurnInterruptParams", "threadId turnId"),
    ] {
        let path = schema.path().join("v2").join(format!("{ty}.json"));
        let schema: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        for name in fields.split_whitespace() {
            assert!(
                schema["properties"].get(name).is_some(),
                "missing {ty}.{name}"
            );
        }
    }
}

#[test]
fn action_targets_reference_stock_methods() {
    use ox_desk_core::actions::{ACTION_TARGETS, Target};
    let mut seen = HashSet::new();
    for (name, target) in ACTION_TARGETS {
        assert!(seen.insert(name), "duplicate desktop action {name}");
        if let Target::Runtime(operation) = target {
            let methods = ox_desk_core::codex::action_methods(*operation);
            assert!(!methods.is_empty());
            for method in methods {
                assert_eq!(Method::parse(method.as_str()), Some(*method));
            }
        }
    }
}

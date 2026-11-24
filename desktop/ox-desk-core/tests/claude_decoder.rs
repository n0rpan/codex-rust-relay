//! The JSONL fixtures are projected from stock Claude Code 2.1.331 through OX against
//! a disposable loopback Messages provider. They preserve event order and tool results.
use ox_desk_core::{
    claude::{Decoder, DeltaTarget, Event},
    runtime::RequestId,
    transcript::item::Item,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
const TOOLS: &str = include_str!("fixtures/claude-tools.jsonl");
const AGENT: &str = include_str!("fixtures/claude-agent.jsonl");
fn replay(stream: &str) -> Vec<Event> {
    let mut decoder = Decoder::default();
    stream
        .lines()
        .flat_map(|s| decoder.decode(s.as_bytes()).unwrap())
        .collect()
}

#[test]
fn streamed_blocks_replace_the_matching_snapshot_and_complete_actual_tool_results() {
    let events = replay(TOOLS);
    assert!(
        matches!(&events[0],Event::Init(i) if i.model=="fixture/demo" && i.permission_mode=="default")
    );
    let mut partial = BTreeMap::<String, String>::new();
    let mut snapshots = BTreeMap::<String, String>::new();
    for event in &events {
        match event {
            Event::Delta {
                id,
                text,
                target: DeltaTarget::Text | DeltaTarget::Thinking,
                ..
            } => partial.entry(id.clone()).or_default().push_str(text),
            Event::Item {
                id,
                item: Item::Reasoning { summary },
                completed: true,
                ..
            } => {
                assert!(snapshots.insert(id.clone(), summary.join("")).is_none());
            }
            Event::Item {
                id,
                item: Item::AgentMessage { text, .. },
                completed: true,
                ..
            } => {
                assert!(snapshots.insert(id.clone(), text.clone()).is_none());
            }
            Event::Item {
                id,
                item: Item::FileChange { changes, .. },
                completed,
                ..
            } => {
                assert!(changes[0].path.ends_with("sample.txt"));
                let expected = match (id.as_str(), completed) {
                    ("toolu_writer", true) => "@@ -0,0 +1,2 @@\n+hello\n+there\n",
                    ("toolu_edit", true) => "@@ -1,2 +1,2 @@\n hello\n-there\n+world\n",
                    _ => "",
                };
                assert_eq!(&*changes[0].diff, expected);
            }
            _ => {}
        }
    }
    assert_eq!(partial, snapshots);
    assert!(partial.values().any(|v| v == "Fixture analysis."));
    assert!(partial.values().any(|v| v == "I will write."));
    assert!(events.iter().any(|e|matches!(e,Event::Item{item:Item::CommandExecution{aggregated_output:Some(s),exit_code:None,status,..},completed:true,..} if s=="stdout\nstderr" && status=="completed")));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e,Event::Result(r) if !r.is_error))
            .count(),
        4
    );
}

#[test]
fn approved_input_and_multiselect_answers_match_the_native_tool_result() {
    let events = replay(TOOLS);
    let mut asks = events
        .iter()
        .filter_map(|e| if let Event::Ask(p) = e { Some(p) } else { None });
    let write = asks.next().unwrap();
    assert_eq!(write.allow()["updatedInput"], *write.input);
    assert_eq!(write.deny("No"), json!({"behavior":"deny","message":"No"}));
    let form = asks.find(|p| p.tool == "AskUserQuestion").unwrap();
    let sheet = form.as_ask(RequestId::Text("proc/request".into()), "display".into());
    assert_eq!(sheet.decisions().len(), 1);
    assert_eq!(sheet.decisions()[0].label, "Deny");
    let q = &sheet.questions[0];
    assert!(q.multi_select && q.is_other);
    assert_eq!(q.id, "Select features?");
    assert!(form.answer(&BTreeMap::new()).is_err());
    let answers = BTreeMap::from([(q.id.clone(), "One, Two".to_string())]);
    let raw: Value = TOOLS
        .lines()
        .filter_map(|s| serde_json::from_str(s).ok())
        .find(|r: &Value| r["tool_use_result"]["answers"].is_object())
        .unwrap();
    assert_eq!(
        form.answer(&answers).unwrap()["updatedInput"],
        raw["tool_use_result"]
    );
}

#[test]
fn child_asks_and_results_belong_to_the_spawning_agent() {
    let mut d = Decoder::default();
    let mut nested = false;
    let mut statuses = vec![];
    let mut parent = String::new();
    for line in AGENT.lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        for e in d.decode_value(row.clone()).unwrap() {
            match e {
                Event::Item {
                    id,
                    item:
                        Item::AgentOperation {
                            status,
                            receiver_thread_ids,
                            ..
                        },
                    ..
                } => {
                    assert_eq!(receiver_thread_ids, vec![id.as_str()]);
                    parent = id;
                    statuses.push(status);
                }
                Event::Item {
                    scope,
                    item: Item::AgentMessage { .. },
                    ..
                } if scope.parent_tool_use_id.as_deref() == Some(parent.as_str()) => nested = true,
                _ => {}
            }
        }
        if row["subtype"] == "task_started" {
            let ask = json!({"type":"control_request","request_id":"r","request":{"subtype":"can_use_tool","tool_name":"Read","tool_use_id":"child-call","agent_id":row["task_id"],"input":{"file_path":"a"}}});
            assert!(
                matches!(d.decode_value(ask).unwrap().as_slice(),[Event::Ask(p)] if p.parent_tool_use_id.as_deref()==Some(parent.as_str()))
            );
        }
    }
    assert!(nested);
    assert_eq!(statuses, ["inProgress", "inProgress", "completed"]);
}

#[test]
fn approved_forms_require_distinct_questions_and_preserve_original_arguments() {
    let mut d = Decoder::default();
    let mut request: Value = TOOLS
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .find(|r: &Value| r["request"]["tool_name"] == "AskUserQuestion")
        .unwrap();
    let mut second = request["request"]["input"]["questions"][0].clone();
    second["question"] = json!("Another?");
    second["multiSelect"] = json!(false);
    request["request"]["input"]["questions"]
        .as_array_mut()
        .unwrap()
        .push(second);
    let events = d.decode_value(request.clone()).unwrap();
    let [Event::Ask(form)] = events.as_slice() else {
        panic!()
    };
    assert!(!form.questions[1].multi_select);
    let answers = BTreeMap::from([
        ("Another?".into(), "Custom text".into()),
        ("Select features?".into(), "Two".into()),
    ]);
    let result = form.answer(&answers).unwrap();
    assert_eq!(
        result["updatedInput"]["questions"],
        request["request"]["input"]["questions"]
    );
    assert_eq!(result["updatedInput"]["answers"], json!(answers));
    request["request"]["input"]["questions"][1]["question"] = json!("Select features?");
    assert!(matches!(
        d.decode_value(request).unwrap().as_slice(),
        [Event::UnsupportedRequest { .. }]
    ));
}

#[test]
fn controls_and_forward_compatible_frames_never_grant_unsupported_requests() {
    let mut d = Decoder::default();
    for subtype in ["elicitation", "future_request", "can_use_tool"] {
        let r =
            json!({"type":"control_request","request_id":"opaque","request":{"subtype":subtype}});
        assert!(
            matches!(d.decode_value(r).unwrap().as_slice(),[Event::UnsupportedRequest{request_id,..}] if request_id=="opaque")
        );
    }
    assert!(
        matches!(d.decode_value(json!({"type":"control_cancel_request","request_id":"r"})).unwrap().as_slice(),[Event::CancelAsk(id)] if id=="r")
    );
    let init = json!({"type":"control_response","response":{"subtype":"success","request_id":"init","response":{"commands":[{"name":"skill","description":"test","argumentHint":""},{"name":"built-in","builtin":true}],"models":[{"value":"default","displayName":"Default","resolvedModel":"demo","supportedEffortLevels":["low"]}],"account":{"tokenSource":"none","apiKeySource":"ANTHROPIC_API_KEY"},"claude_code_version":"2.1.331","current_permission_mode":"default","capabilities":["ui_surface_v1"]}}});
    let ev = d.decode_value(init).unwrap();
    let Event::ControlReply(reply) = &ev[0] else {
        panic!()
    };
    let caps = reply.capabilities().unwrap();
    assert!(!caps.commands[0].builtin && caps.commands[1].builtin);
    assert_eq!(caps.models[0].supported_effort_levels, ["low"]);
    assert_eq!(
        caps.account.api_key_source.as_deref(),
        Some("ANTHROPIC_API_KEY")
    );
    assert!(d.decode(br#"{"type":"keep_alive"}"#).unwrap().is_empty());
    assert!(
        d.decode(b"private fixture value")
            .unwrap_err()
            .to_string()
            .find("private")
            .is_none()
    );
}

#[test]
fn history_blocks_deferred_tools_and_failed_edits_keep_native_semantics() {
    let mut d = Decoder::default();
    let row = json!({"type":"assistant","uuid":"journal-1","session_id":"s","message":{"id":"msg","content":[
        {"type":"tool_use","id":"t1","name":"TodoWrite","input":{"todos":[{"content":"Build","status":"in_progress"}]}},
        {"type":"tool_use","id":"t2","name":"mcp__git__status","input":{}},
        {"type":"tool_use","id":"t3","name":"WebSearch","input":{"query":"docs"}},
        {"type":"tool_use","id":"t4","name":"Edit","input":{"file_path":"unchanged.rs"}},
        {"type":"text","text":"Done"}]}});
    let e = d.decode_value(row.clone()).unwrap();
    assert!(matches!(&e[0],Event::Item{item:Item::Plan{text},..} if text=="[>] Build"));
    assert!(
        matches!(&e[1],Event::Item{item:Item::McpToolCall{server,tool,..},..} if server=="git" && tool=="status")
    );
    assert!(matches!(&e[2],Event::Item{item:Item::WebSearch{query},..} if query=="docs"));
    assert!(matches!(&e[4],Event::Item{id,..} if id=="msg:4"));
    assert!(d.decode_value(row).unwrap().is_empty());
    let r = json!({"type":"user","uuid":"jr-2","session_id":"s","message":{"content":[{"type":"tool_result","tool_use_id":"t4","is_error":true,"content":"Denied"}]}});
    assert!(
        matches!(d.decode_value(r).unwrap().as_slice(),[Event::Item{item:Item::FileChange{status,changes},output:Some(output),..}] if status=="failed" && changes[0].diff.is_empty() && output=="Denied")
    );
}

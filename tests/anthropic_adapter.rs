//! Offline tests for the Anthropic Messages adapter.
//!
//! The module is included by relative path so this test target does not depend
//! on the relay binary wiring `mod adapters` (another worker owns `main.rs`).

#[path = "../src/adapters/anthropic.rs"]
mod anthropic;

use anthropic::{ResponseTranslator, THINKING_ENVELOPE_PREFIX, translate_request};
use serde_json::{Value, json};

fn push(translator: &mut ResponseTranslator, event: &str, data: Value) -> Vec<Value> {
    translator
        .push_event(event, &data.to_string())
        .unwrap_or_else(|error| panic!("event {event} failed to translate: {error}"))
}

fn types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["type"].as_str().expect("event type"))
        .collect()
}

fn find<'a>(events: &'a [Value], kind: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["type"].as_str() == Some(kind))
        .unwrap_or_else(|| panic!("missing event {kind}; got {:?}", types(events)))
}

fn message_start(model: &str) -> Value {
    json!({
        "type": "message_start",
        "message": {
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": model,
            "stop_reason": null,
            "stop_sequence": null,
            "usage": {
                "input_tokens": 10,
                "cache_creation_input_tokens": 2,
                "cache_read_input_tokens": 4,
                "output_tokens": 1
            }
        }
    })
}

fn assert_sequences_increase(events: &[Value]) {
    let mut previous = None;
    for event in events {
        let sequence = event["sequence_number"]
            .as_u64()
            .unwrap_or_else(|| panic!("event without sequence_number: {event}"));
        if let Some(previous) = previous {
            assert!(sequence > previous, "sequence numbers must increase");
        }
        previous = Some(sequence);
    }
}

// ---------------------------------------------------------------------------
// Request translation
// ---------------------------------------------------------------------------

#[test]
fn translates_a_codex_request_shape() {
    let request = json!({
        "model": "codex-side-name",
        "instructions": "You are a coding agent.",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]}
        ],
        "tools": [
            {
                "type": "function",
                "name": "shell",
                "description": "Run a shell command",
                "parameters": {
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"],
                    "additionalProperties": false
                },
                "strict": false
            }
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": "medium", "summary": "auto"},
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "cache-key",
        "text": {"verbosity": "low"},
        "max_output_tokens": 1024,
        "temperature": 0.2,
        "top_p": 0.9
    });

    let translated = translate_request(&request, "claude-upstream").expect("request translates");
    assert_eq!(translated["model"], "claude-upstream");
    assert_eq!(translated["max_tokens"], 1024);
    assert_eq!(translated["stream"], true);
    assert_eq!(translated["system"][0]["text"], "You are a coding agent.");
    assert_eq!(translated["temperature"], 0.2);
    assert_eq!(translated["top_p"], 0.9);
    assert_eq!(translated["output_config"], json!({"effort": "medium"}));
    assert_eq!(translated["messages"][0]["role"], "user");
    assert_eq!(
        without_cache_markers(&translated["messages"][0]["content"][0]),
        json!({"type": "text", "text": "list files"})
    );
    assert_eq!(translated["tools"][0]["name"], "shell");
    assert_eq!(
        translated["tools"][0]["input_schema"],
        json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
            "additionalProperties": false
        })
    );
    assert_eq!(translated["tools"][0]["strict"], false);
    assert_eq!(translated["tool_choice"], json!({"type": "auto"}));
    assert!(translated.get("store").is_none());
    assert!(translated.get("include").is_none());
    assert!(translated.get("parallel_tool_calls").is_none());
}

#[test]
fn preserves_instruction_order_and_text() {
    let request = json!({
        "instructions": "base instructions",
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "developer rules"}]},
            {"type": "message", "role": "user", "content": "hello"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "first"}]},
            {"type": "message", "role": "user", "content": "again"}
        ],
        "max_output_tokens": 64,
        "stream": true
    });
    let translated = translate_request(&request, "claude-upstream").unwrap();
    assert_eq!(
        translated["system"][0]["text"],
        "base instructions\n\ndeveloper rules"
    );
    assert_eq!(
        without_cache_markers(&translated["messages"]),
        json!([
            {"role": "user", "content": [{"type": "text", "text": "hello"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "first"}]},
            {"role": "user", "content": [{"type": "text", "text": "again"}]}
        ])
    );
}

#[test]
fn keeps_mid_conversation_developer_messages_in_order() {
    let request = json!({
        "input": [
            {"type": "message", "role": "developer", "content": "base instructions"},
            {"type": "message", "role": "user", "content": "hello"},
            {"type": "message", "role": "developer", "content": "config update"},
            {"type": "message", "role": "user", "content": "again"}
        ],
        "max_output_tokens": 64,
        "stream": true
    });
    let translated = translate_request(&request, "claude-upstream").unwrap();
    assert_eq!(translated["system"][0]["text"], "base instructions");
    assert_eq!(
        without_cache_markers(&translated["messages"]),
        json!([
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": "config update"},
                    {"type": "text", "text": "again"}
                ]
            }
        ])
    );
}

#[test]
fn defaults_max_tokens_when_codex_omits_it() {
    // Current Codex sends no max_output_tokens, but Anthropic requires max_tokens.
    let request = json!({
        "input": "hi",
        "stream": true
    });
    let translated = translate_request(&request, "claude-upstream").unwrap();
    assert_eq!(translated["max_tokens"], anthropic::DEFAULT_MAX_TOKENS);

    let mut invalid = request.clone();
    invalid["max_output_tokens"] = json!(0);
    assert!(translate_request(&invalid, "claude-upstream").is_err());
}

#[test]
fn ignores_configuration_updates_and_accepts_stream_options() {
    let request = json!({
        "input": [
            {"type": "message", "role": "developer", "content": "base"},
            {"role": "user", "content": "hi"},
            {"type": "configuration_update", "reasoning": {"effort": "high"}},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "next"}
        ],
        "stream": true,
        "stream_options": {"reasoning_summary_delivery": "sequential_cutoff"}
    });
    let translated = translate_request(&request, "claude-upstream").unwrap();
    assert_eq!(
        translated["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
            {"role": "user", "content": [{"type": "text", "text": "next", "cache_control": {"type": "ephemeral"}}]}
        ])
    );
}

#[test]
fn rejects_namespaced_function_calls() {
    let request = json!({
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "function_call", "call_id": "call_1", "name": "shell", "namespace": "ns", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": ""}
        ],
        "stream": true
    });
    let error = translate_request(&request, "claude-upstream").unwrap_err();
    assert!(error.contains("namespace"), "{error}");
}

#[test]
fn maps_tool_choice_variants() {
    let base = json!({
        "input": "hi",
        "max_output_tokens": 64,
        "stream": true,
        "tools": [
            {"type": "function", "name": "shell", "parameters": {"type": "object"}},
            {"type": "function", "name": "apply_patch"}
        ]
    });

    let mut required = base.clone();
    required["tool_choice"] = json!("required");
    assert_eq!(
        translate_request(&required, "m").unwrap()["tool_choice"],
        json!({"type": "any"})
    );

    let mut forced = base.clone();
    forced["tool_choice"] = json!({"type": "function", "name": "apply_patch"});
    assert_eq!(
        translate_request(&forced, "m").unwrap()["tool_choice"],
        json!({"type": "tool", "name": "apply_patch"})
    );

    let mut unknown = base.clone();
    unknown["tool_choice"] = json!({"type": "function", "name": "missing"});
    assert!(translate_request(&unknown, "m").is_err());

    let mut allowed = base.clone();
    allowed["tool_choice"] = json!({"type": "allowed_tools", "tools": []});
    assert!(translate_request(&allowed, "m").is_err());

    // "none" without tools is accepted and omitted.
    let no_tools = json!({
        "input": "hi",
        "max_output_tokens": 64,
        "stream": true,
        "tool_choice": "none"
    });
    assert!(
        translate_request(&no_tools, "m")
            .unwrap()
            .get("tool_choice")
            .is_none()
    );
}

#[test]
fn maps_reasoning_effort_and_disables_thinking_for_none() {
    let mut request = json!({"input": "hi", "max_output_tokens": 64, "stream": true});
    request["reasoning"] = json!({"effort": "minimal"});
    assert_eq!(
        translate_request(&request, "m").unwrap()["output_config"],
        json!({"effort": "low"})
    );

    request["reasoning"] = json!({"effort": "none"});
    let translated = translate_request(&request, "m").unwrap();
    assert_eq!(translated["thinking"], json!({"type": "disabled"}));
    assert!(translated.get("output_config").is_none());

    request["reasoning"] = json!({"effort": "turbo"});
    assert!(translate_request(&request, "m").is_err());
}

#[test]
fn rejects_server_side_state() {
    let base = json!({"input": "hi", "max_output_tokens": 64, "stream": true});
    for field in [
        "previous_response_id",
        "conversation",
        "background",
        "prompt",
        "store",
    ] {
        let mut request = base.clone();
        request[field] = if field == "store" {
            json!(true)
        } else {
            json!("value")
        };
        let error = translate_request(&request, "m").unwrap_err();
        assert!(error.contains(field), "{field}: {error}");
    }

    let mut referenced = base.clone();
    referenced["input"] = json!([{"type": "item_reference", "id": "msg_1"}]);
    assert!(translate_request(&referenced, "m").is_err());

    let mut non_streaming = base.clone();
    non_streaming["stream"] = json!(false);
    assert!(translate_request(&non_streaming, "m").is_err());

    let mut missing_stream = base.clone();
    missing_stream.as_object_mut().unwrap().remove("stream");
    assert!(translate_request(&missing_stream, "m").is_err());

    let mut priority = base.clone();
    priority["service_tier"] = json!("priority");
    assert!(translate_request(&priority, "m").is_err());
}

#[test]
fn rejects_hosted_tools_and_custom_tools() {
    let request = json!({
        "input": "hi",
        "max_output_tokens": 64,
        "stream": true,
        "tools": [{"type": "web_search"}]
    });
    let error = translate_request(&request, "m").unwrap_err();
    assert!(error.contains("web_search"), "{error}");

    let custom = json!({
        "input": "hi",
        "max_output_tokens": 64,
        "stream": true,
        "tools": [{"type": "custom", "name": "freeform"}]
    });
    assert!(translate_request(&custom, "m").is_err());

    let hosted_output = json!({
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "web_search_call", "id": "ws_1", "status": "completed"}
        ],
        "max_output_tokens": 64,
        "stream": true
    });
    assert!(translate_request(&hosted_output, "m").is_err());

    let custom_call = json!({
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "custom_tool_call", "call_id": "c1", "name": "freeform", "input": "x"}
        ],
        "max_output_tokens": 64,
        "stream": true
    });
    assert!(translate_request(&custom_call, "m").is_err());
}

#[test]
fn wraps_unparseable_historical_arguments_instead_of_failing_the_thread() {
    // A truncated or non-object call from an earlier turn must not make every
    // later request fail. Its raw text is replayed inside a valid tool_use input.
    for raw in ["[1,2]", "{\"command\": \"ls", "not json"] {
        let input = json!({
            "input": [
                {"role": "user", "content": "hi"},
                {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": raw},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
                {"role": "user", "content": "again"}
            ],
            "max_output_tokens": 64,
            "stream": true
        });
        let body = translate_request(&input, "m").unwrap_or_else(|error| panic!("{raw}: {error}"));
        let tool_use = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .find(|block| block["type"] == "tool_use")
            .expect("the historical call is replayed");
        assert_eq!(tool_use["id"], "call_1");
        assert_eq!(tool_use["input"], json!({"_raw": raw}));
    }
}

#[test]
fn rejects_unsupported_content_and_bad_arguments() {
    let image = json!({
        "input": [{
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
            ]
        }],
        "max_output_tokens": 64,
        "stream": true
    });
    assert!(translate_request(&image, "m").is_ok());
    let file = json!({
        "input": [{"type": "message", "role": "user",
            "content": [{"type": "input_file", "file_id": "f"}]}],
        "max_output_tokens": 64,
        "stream": true
    });
    assert!(translate_request(&file, "m").is_err());

    let bad_name = json!({
        "input": "hi",
        "max_output_tokens": 64,
        "stream": true,
        "tools": [{"type": "function", "name": "bad.name"}]
    });
    assert!(translate_request(&bad_name, "m").is_err());

    let structured = json!({
        "input": "hi",
        "max_output_tokens": 64,
        "stream": true,
        "text": {"format": {"type": "json_schema", "name": "x", "schema": {}}}
    });
    assert!(translate_request(&structured, "m").is_err());
}

#[test]
fn rejects_missing_tool_output_and_duplicate_tool_names() {
    let missing = json!({
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{}"}
        ],
        "max_output_tokens": 64,
        "stream": true
    });
    let error = translate_request(&missing, "m").unwrap_err();
    assert!(error.contains("function_call_output"), "{error}");

    let duplicate = json!({
        "input": "hi",
        "max_output_tokens": 64,
        "stream": true,
        "tools": [
            {"type": "function", "name": "shell"},
            {"type": "function", "name": "shell"}
        ]
    });
    assert!(translate_request(&duplicate, "m").is_err());
}

#[test]
fn translates_stop_sequences() {
    let mut request = json!({"input": "hi", "max_output_tokens": 64, "stream": true});
    request["stop"] = json!("STOP");
    assert_eq!(
        translate_request(&request, "m").unwrap()["stop_sequences"],
        json!(["STOP"])
    );
    request["stop"] = json!(["a", "b"]);
    assert_eq!(
        translate_request(&request, "m").unwrap()["stop_sequences"],
        json!(["a", "b"])
    );
}

// ---------------------------------------------------------------------------
// Streaming translation
// ---------------------------------------------------------------------------

#[test]
fn streams_text_with_complete_responses_events() {
    let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
    let mut events = Vec::new();
    events.extend(push(
        &mut translator,
        "message_start",
        message_start("claude-x"),
    ));
    events.extend(push(
        &mut translator,
        "content_block_start",
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    ));
    events.extend(push(&mut translator, "ping", json!({"type": "ping"})));
    events.extend(push(
        &mut translator,
        "content_block_delta",
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hel"}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_delta",
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_stop",
        json!({"type": "content_block_stop", "index": 0}),
    ));
    events.extend(push(
        &mut translator,
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 15}
        }),
    ));
    events.extend(push(
        &mut translator,
        "message_stop",
        json!({"type": "message_stop"}),
    ));

    assert_eq!(
        types(&events),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_sequences_increase(&events);
    assert_eq!(translator.response_id(), Some("resp_msg_test"));
    assert!(translator.is_terminal());

    let completed = find(&events, "response.completed");
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(completed["response"]["model"], "claude-x");
    assert_eq!(
        completed["response"]["output"][0]["content"][0]["text"],
        "Hello"
    );
    assert_eq!(completed["response"]["usage"]["input_tokens"], 16);
    assert_eq!(
        completed["response"]["usage"]["input_tokens_details"]["cached_tokens"],
        4
    );
    assert_eq!(completed["response"]["usage"]["output_tokens"], 15);
    assert_eq!(completed["response"]["usage"]["total_tokens"], 31);
    assert!(translator.finish().unwrap().is_empty());
}

#[test]
fn streams_tool_calls_with_stable_ids_and_argument_deltas() {
    let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
    let mut events = Vec::new();
    events.extend(push(
        &mut translator,
        "message_start",
        message_start("claude-x"),
    ));
    events.extend(push(
        &mut translator,
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {}}
        }),
    ));
    events.extend(push(
        &mut translator,
        "content_block_delta",
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"command\":"}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_delta",
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "\"ls -la\"}"}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_stop",
        json!({"type": "content_block_stop", "index": 0}),
    ));
    events.extend(push(
        &mut translator,
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use", "stop_sequence": null},
            "usage": {"output_tokens": 30}
        }),
    ));
    events.extend(push(
        &mut translator,
        "message_stop",
        json!({"type": "message_stop"}),
    ));

    let added = find(&events, "response.output_item.added");
    assert_eq!(added["item"]["id"], "fc_toolu_1");
    assert_eq!(added["item"]["call_id"], "toolu_1");
    assert_eq!(added["item"]["name"], "shell");
    assert_eq!(added["item"]["arguments"], "");

    let deltas: Vec<&str> = events
        .iter()
        .filter(|event| event["type"] == "response.function_call_arguments.delta")
        .map(|event| event["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, vec!["{\"command\":", "\"ls -la\"}"]);

    let done = find(&events, "response.function_call_arguments.done");
    assert_eq!(done["arguments"], "{\"command\":\"ls -la\"}");

    let completed = find(&events, "response.completed");
    assert_eq!(completed["response"]["status"], "completed");
    let item = &completed["response"]["output"][0];
    assert_eq!(item["type"], "function_call");
    assert_eq!(item["call_id"], "toolu_1");
    assert_eq!(item["name"], "shell");
    assert_eq!(item["arguments"], "{\"command\":\"ls -la\"}");
}

#[test]
fn maps_max_tokens_to_incomplete() {
    let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
    push(&mut translator, "message_start", message_start("claude-x"));
    push(
        &mut translator,
        "content_block_start",
        json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
    );
    push(
        &mut translator,
        "content_block_delta",
        json!({"index": 0, "delta": {"type": "text_delta", "text": "partial"}}),
    );
    push(&mut translator, "content_block_stop", json!({"index": 0}));
    push(
        &mut translator,
        "message_delta",
        json!({"delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 8}}),
    );
    let events = push(
        &mut translator,
        "message_stop",
        json!({"type": "message_stop"}),
    );

    // Items are closed in output order at message_stop, then the terminal event.
    assert_eq!(
        types(&events),
        vec!["response.output_item.done", "response.incomplete"]
    );
    let terminal = events.last().unwrap();
    assert_eq!(terminal["response"]["status"], "incomplete");
    assert_eq!(
        terminal["response"]["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );
    assert_eq!(
        terminal["response"]["output"][0]["content"][0]["text"],
        "partial"
    );
}

fn tool_use_stream(partial_json: &str, stop_reason: &str) -> Vec<Result<Vec<Value>, String>> {
    let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
    let mut results = Vec::new();
    let steps = [
        ("message_start", message_start("claude-x")),
        (
            "content_block_start",
            json!({
                "index": 0,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "apply_patch", "input": {}}
            }),
        ),
        (
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": partial_json}}),
        ),
        ("content_block_stop", json!({"index": 0})),
        (
            "message_delta",
            json!({"delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 8192}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ];
    for (event, data) in steps {
        results.push(translator.push_event(event, &data.to_string()));
    }
    results
}

#[test]
fn truncated_tool_use_at_max_tokens_is_incomplete_not_an_error() {
    // Output cut off mid-JSON inside a tool_use block at max_tokens.
    let results = tool_use_stream("{\"patch\": \"*** Begin Patch\\n+line", "max_tokens");
    for result in &results {
        assert!(
            result.is_ok(),
            "truncated tool must not fail the stream: {result:?}"
        );
    }
    let events = results.last().unwrap().as_ref().unwrap();
    // Nothing completed, so the terminal event is the only one at message_stop.
    assert_eq!(types(events), vec!["response.incomplete"]);
    let terminal = events.last().unwrap();
    assert_eq!(terminal["response"]["status"], "incomplete");
    assert_eq!(
        terminal["response"]["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );
    // The truncated function_call is dropped, not completed.
    assert_eq!(terminal["response"]["output"], json!([]));
    assert!(
        events
            .iter()
            .all(|event| event["item"]["type"] != "function_call"),
        "truncated call must not be emitted as a function_call item"
    );
}

#[test]
fn invalid_tool_json_with_tool_use_stop_still_errors() {
    let results = tool_use_stream("{\"command\": ls", "tool_use");
    let error = results
        .last()
        .unwrap()
        .as_ref()
        .expect_err("invalid tool JSON must fail the stream");
    assert!(error.contains("apply_patch"), "{error}");
    assert!(error.contains("invalid JSON arguments"), "{error}");
}

#[test]
fn maps_refusal_to_completed_not_retryable_incomplete() {
    let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
    push(&mut translator, "message_start", message_start("claude-x"));
    push(
        &mut translator,
        "content_block_start",
        json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
    );
    push(
        &mut translator,
        "content_block_delta",
        json!({"index": 0, "delta": {"type": "text_delta", "text": "I cannot help with that."}}),
    );
    push(&mut translator, "content_block_stop", json!({"index": 0}));
    push(
        &mut translator,
        "message_delta",
        json!({"delta": {"stop_reason": "refusal"}}),
    );
    let events = push(
        &mut translator,
        "message_stop",
        json!({"type": "message_stop"}),
    );
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "response.completed");
    assert_eq!(terminal["response"]["status"], "completed");
    assert_eq!(
        terminal["response"]["output"][0]["content"][0]["text"],
        "I cannot help with that."
    );
}

#[test]
fn maps_rate_limit_errors_to_terminal_failure() {
    let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
    push(&mut translator, "message_start", message_start("claude-x"));
    let events = push(
        &mut translator,
        "error",
        json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "slow down"}
        }),
    );
    assert_eq!(types(&events), vec!["response.failed"]);
    assert_eq!(events[0]["response"]["status"], "failed");
    assert_eq!(events[0]["response"]["error"]["code"], "rate_limit_error");
    assert_eq!(events[0]["response"]["error"]["message"], "slow down");
    assert_eq!(events[0]["response"]["output"], json!([]));

    // Late upstream events must not emit a second terminal event.
    assert!(
        push(
            &mut translator,
            "message_stop",
            json!({"type": "message_stop"})
        )
        .is_empty()
    );
    assert!(translator.finish().unwrap().is_empty());
}

#[test]
fn interruption_fails_without_completing_partial_tool_calls() {
    let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
    push(&mut translator, "message_start", message_start("claude-x"));
    push(
        &mut translator,
        "content_block_start",
        json!({
            "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {}}
        }),
    );
    push(
        &mut translator,
        "content_block_delta",
        json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"command\":"}}),
    );

    let events = translator.finish().expect("finish emits a terminal event");
    assert_eq!(types(&events), vec!["response.failed"]);
    assert_eq!(events[0]["response"]["error"]["code"], "stream_interrupted");
    assert_eq!(events[0]["response"]["output"], json!([]));
    assert!(translator.is_terminal());
    assert!(translator.finish().unwrap().is_empty());
    assert!(
        push(
            &mut translator,
            "message_stop",
            json!({"type": "message_stop"})
        )
        .is_empty()
    );
}

#[test]
fn rejects_unknown_content_blocks_and_bad_stream_order() {
    let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
    push(&mut translator, "message_start", message_start("claude-x"));
    let error = translator
        .push_event(
            "content_block_start",
            &json!({
                "index": 0,
                "content_block": {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search"}
            })
            .to_string(),
        )
        .unwrap_err();
    assert!(error.contains("server_tool_use"), "{error}");

    let mut fresh = ResponseTranslator::with_created_at(1_700_000_000);
    assert!(
        fresh
            .push_event("content_block_stop", &json!({"index": 0}).to_string())
            .is_err()
    );
    assert!(fresh.finish().is_err());
    let mut no_start = ResponseTranslator::with_created_at(1_700_000_000);
    assert!(no_start.finish().is_err());
    assert!(no_start.push_event("ping", "{}").unwrap().is_empty());
}

#[test]
fn ignores_unknown_events_and_done_marker() {
    let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
    push(&mut translator, "message_start", message_start("claude-x"));
    assert!(
        push(
            &mut translator,
            "some_future_event",
            json!({"type": "some_future_event"})
        )
        .is_empty()
    );
    // `[DONE]` is treated as an interruption when no message_stop arrived.
    let events = translator.push_event("", "[DONE]").unwrap();
    assert_eq!(types(&events), vec!["response.failed"]);
}

// ---------------------------------------------------------------------------
// Thinking round-trip
// ---------------------------------------------------------------------------

fn run_tool_turn(tool_id: &str, thinking: &str, signature: &str, command: &str) -> Vec<Value> {
    let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
    let mut events = Vec::new();
    events.extend(push(
        &mut translator,
        "message_start",
        message_start("claude-x"),
    ));
    events.extend(push(
        &mut translator,
        "content_block_start",
        json!({"index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_delta",
        json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": thinking}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_delta",
        json!({"index": 0, "delta": {"type": "signature_delta", "signature": signature}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_stop",
        json!({"index": 0}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_start",
        json!({
            "index": 1,
            "content_block": {"type": "tool_use", "id": tool_id, "name": "shell", "input": {}}
        }),
    ));
    let arguments = format!("{{\"command\":\"{command}\"}}");
    let (first, second) = arguments.split_at(arguments.len() - 1);
    events.extend(push(
        &mut translator,
        "content_block_delta",
        json!({"index": 1, "delta": {"type": "input_json_delta", "partial_json": first}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_delta",
        json!({"index": 1, "delta": {"type": "input_json_delta", "partial_json": second}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_stop",
        json!({"index": 1}),
    ));
    events.extend(push(
        &mut translator,
        "message_delta",
        json!({"delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 42}}),
    ));
    events.extend(push(
        &mut translator,
        "message_stop",
        json!({"type": "message_stop"}),
    ));
    events
}

fn run_thinking_tool_stream() -> Vec<Value> {
    run_tool_turn("toolu_9", "I should inspect the repo.", "sig-abc-123", "ls")
}

fn reasoning_item(events: &[Value]) -> &Value {
    events
        .iter()
        .find(|event| {
            event["type"].as_str() == Some("response.output_item.done")
                && event["item"]["type"].as_str() == Some("reasoning")
        })
        .expect("reasoning output item")
}

fn encrypted_content(events: &[Value]) -> String {
    reasoning_item(events)["item"]["encrypted_content"]
        .as_str()
        .expect("encrypted_content")
        .to_owned()
}

#[test]
fn round_trips_signed_thinking_through_a_tool_turn() {
    let events = run_thinking_tool_stream();
    let encrypted = reasoning_item(&events)["item"]["encrypted_content"]
        .as_str()
        .expect("encrypted_content");
    assert!(encrypted.starts_with(THINKING_ENVELOPE_PREFIX));

    // Thinking item is completed at message_stop but keeps output_index 0.
    let reasoning_done = reasoning_item(&events);
    assert_eq!(reasoning_done["output_index"], 0);
    assert_eq!(
        reasoning_done["item"]["summary"],
        json!([{"type": "summary_text", "text": "I should inspect the repo."}])
    );

    let request = json!({
        "instructions": "system text",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]},
            {
                "type": "reasoning",
                "id": "rs_0",
                "summary": [{"type": "summary_text", "text": "I should inspect the repo."}],
                "encrypted_content": encrypted
            },
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Listing."}]},
            {"type": "function_call", "name": "shell", "arguments": "{\"command\":\"ls\"}", "call_id": "toolu_9"},
            {"type": "function_call_output", "call_id": "toolu_9", "output": "a.txt"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "thanks"}]}
        ],
        "max_output_tokens": 256,
        "stream": true,
        "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}}]
    });

    let translated = translate_request(&request, "claude-x").expect("replay translates");
    let messages = translated["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["content"][0]["text"], "list files");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"][0],
        json!({"type": "thinking", "thinking": "I should inspect the repo.", "signature": "sig-abc-123"})
    );
    assert_eq!(
        messages[1]["content"][1],
        json!({"type": "text", "text": "Listing."})
    );
    assert_eq!(
        messages[1]["content"][2],
        json!({"type": "tool_use", "id": "toolu_9", "name": "shell", "input": {"command": "ls"}})
    );
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(
        messages[2]["content"][0],
        json!({"type": "tool_result", "tool_use_id": "toolu_9", "content": "a.txt"})
    );
    assert_eq!(messages[2]["content"][1]["type"], "text");
}

#[test]
fn rejects_reasoning_replayed_against_different_tool_calls() {
    let events = run_thinking_tool_stream();
    let encrypted = reasoning_item(&events)["item"]["encrypted_content"]
        .as_str()
        .unwrap();

    let request = json!({
        "input": [
            {"role": "user", "content": "list files"},
            {"type": "reasoning", "encrypted_content": encrypted},
            {"type": "function_call", "name": "shell", "arguments": "{\"command\":\"pwd\"}", "call_id": "toolu_other"},
            {"type": "function_call_output", "call_id": "toolu_other", "output": "/tmp"}
        ],
        "max_output_tokens": 128,
        "stream": true
    });
    let error = translate_request(&request, "claude-x").unwrap_err();
    assert!(error.contains("provenance"), "{error}");
}

#[test]
fn drops_thinking_from_another_model() {
    let events = run_thinking_tool_stream();
    let encrypted = reasoning_item(&events)["item"]["encrypted_content"]
        .as_str()
        .unwrap();

    let request = json!({
        "input": [
            {"role": "user", "content": "list files"},
            {"type": "reasoning", "encrypted_content": encrypted},
            {"type": "function_call", "name": "shell", "arguments": "{\"command\":\"ls\"}", "call_id": "toolu_9"},
            {"type": "function_call_output", "call_id": "toolu_9", "output": "a.txt"}
        ],
        "max_output_tokens": 128,
        "stream": true
    });
    let translated = translate_request(&request, "claude-other").unwrap();
    assert_eq!(
        translated["messages"][1]["content"][0]["type"], "tool_use",
        "foreign-model thinking should be dropped rather than replayed"
    );
}

#[test]
fn rejects_malformed_thinking_envelope() {
    let request = json!({
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "reasoning", "encrypted_content": format!("{THINKING_ENVELOPE_PREFIX}zz")}
        ],
        "max_output_tokens": 64,
        "stream": true
    });
    assert!(translate_request(&request, "claude-x").is_err());
}

#[test]
fn redacted_thinking_round_trips_without_display_text() {
    let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
    let mut events = Vec::new();
    events.extend(push(
        &mut translator,
        "message_start",
        message_start("claude-x"),
    ));
    events.extend(push(
        &mut translator,
        "content_block_start",
        json!({"index": 0, "content_block": {"type": "redacted_thinking", "data": "opaque-redacted"}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_stop",
        json!({"index": 0}),
    ));
    events.extend(push(
        &mut translator,
        "message_delta",
        json!({"delta": {"stop_reason": "end_turn"}}),
    ));
    events.extend(push(
        &mut translator,
        "message_stop",
        json!({"type": "message_stop"}),
    ));

    let item = reasoning_item(&events);
    assert_eq!(item["item"]["summary"], json!([]));
    let encrypted = item["item"]["encrypted_content"].as_str().unwrap();

    let request = json!({
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "reasoning", "encrypted_content": encrypted},
            {"role": "assistant", "content": "previous answer"}
        ],
        "max_output_tokens": 64,
        "stream": true
    });
    let translated = translate_request(&request, "claude-x").unwrap();
    assert_eq!(
        translated["messages"][1]["content"][0],
        json!({"type": "redacted_thinking", "data": "opaque-redacted"})
    );
}

#[test]
fn parallel_tool_calls_are_accepted_but_cannot_be_disabled() {
    let request = json!({
        "input": "hi",
        "max_output_tokens": 64,
        "stream": true,
        "parallel_tool_calls": false,
        "tools": [{"type": "function", "name": "shell"}]
    });
    let translated = translate_request(&request, "m").unwrap();
    assert!(translated.get("parallel_tool_calls").is_none());
}

#[test]
fn replays_two_tool_turns_with_thinking_in_order() {
    let first = run_tool_turn("toolu_1", "first thought", "sig-1", "ls");
    let second = run_tool_turn("toolu_2", "second thought", "sig-2", "pwd");
    let first_encrypted = encrypted_content(&first);
    let second_encrypted = encrypted_content(&second);

    let request = json!({
        "input": [
            {"type": "message", "role": "developer", "content": "base instructions"},
            {"type": "message", "role": "user", "content": "start"},
            {"type": "reasoning", "encrypted_content": first_encrypted},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "first answer"}]},
            {"type": "function_call", "name": "shell", "arguments": "{\"command\":\"ls\"}", "call_id": "toolu_1"},
            {"type": "function_call_output", "call_id": "toolu_1", "output": "a.txt"},
            {"type": "reasoning", "encrypted_content": second_encrypted},
            {"type": "function_call", "name": "shell", "arguments": "{\"command\":\"pwd\"}", "call_id": "toolu_2"},
            {"type": "function_call_output", "call_id": "toolu_2", "output": "/tmp"}
        ],
        "stream": true
    });

    let translated = translate_request(&request, "claude-x").unwrap();
    assert_eq!(translated["system"][0]["text"], "base instructions");
    assert_eq!(
        without_cache_markers(&translated["messages"]),
        json!([
            {"role": "user", "content": [{"type": "text", "text": "start"}]},
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "first thought", "signature": "sig-1"},
                    {"type": "text", "text": "first answer"},
                    {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {"command": "ls"}}
                ]
            },
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt"}]},
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "second thought", "signature": "sig-2"},
                    {"type": "tool_use", "id": "toolu_2", "name": "shell", "input": {"command": "pwd"}}
                ]
            },
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_2", "content": "/tmp"}]}
        ])
    );
}

#[test]
fn records_configured_model_alias_in_thinking_envelope() {
    let mut translator =
        ResponseTranslator::with_created_at_and_model(1_700_000_000, "claude-alias");
    let mut events = Vec::new();
    events.extend(push(
        &mut translator,
        "message_start",
        message_start("claude-alias-20260101"),
    ));
    events.extend(push(
        &mut translator,
        "content_block_start",
        json!({"index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_delta",
        json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": "plan"}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_delta",
        json!({"index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
    ));
    events.extend(push(
        &mut translator,
        "content_block_stop",
        json!({"index": 0}),
    ));
    events.extend(push(
        &mut translator,
        "message_delta",
        json!({"delta": {"stop_reason": "end_turn"}}),
    ));
    events.extend(push(
        &mut translator,
        "message_stop",
        json!({"type": "message_stop"}),
    ));

    let encrypted = encrypted_content(&events);
    let request = json!({
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "reasoning", "encrypted_content": encrypted},
            {"role": "assistant", "content": "answer"}
        ],
        "stream": true
    });
    let translated = translate_request(&request, "claude-alias").unwrap();
    assert_eq!(
        translated["messages"][1]["content"][0],
        json!({"type": "thinking", "thinking": "plan", "signature": "sig"})
    );
}

#[test]
fn accepts_dated_model_snapshots_for_thinking_provenance() {
    let events = run_thinking_tool_stream();
    let encrypted = encrypted_content(&events);

    // The default translator records the snapshot echoed by message_start;
    // replaying against the configured alias must still work.
    let request = json!({
        "input": [
            {"role": "user", "content": "list files"},
            {"type": "reasoning", "encrypted_content": encrypted},
            {"type": "function_call", "name": "shell", "arguments": "{\"command\":\"ls\"}", "call_id": "toolu_9"},
            {"type": "function_call_output", "call_id": "toolu_9", "output": "a.txt"}
        ],
        "stream": true
    });
    let translated = translate_request(&request, "claude-x").unwrap();
    assert_eq!(translated["messages"][1]["content"][0]["type"], "thinking");
}

#[test]
fn default_constructor_is_deterministic() {
    let mut translator = ResponseTranslator::new();
    let events = push(&mut translator, "message_start", message_start("claude-x"));
    assert_eq!(events[0]["response"]["created_at"], 0);
}

#[test]
fn closes_output_items_in_output_index_order() {
    let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
    push(&mut translator, "message_start", message_start("claude-x"));
    push(
        &mut translator,
        "content_block_start",
        json!({"index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
    );
    push(
        &mut translator,
        "content_block_delta",
        json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": "plan"}}),
    );
    push(
        &mut translator,
        "content_block_delta",
        json!({"index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
    );
    push(&mut translator, "content_block_stop", json!({"index": 0}));
    push(
        &mut translator,
        "content_block_start",
        json!({"index": 1, "content_block": {"type": "text", "text": ""}}),
    );
    push(
        &mut translator,
        "content_block_delta",
        json!({"index": 1, "delta": {"type": "text_delta", "text": "Running."}}),
    );
    push(&mut translator, "content_block_stop", json!({"index": 1}));
    push(
        &mut translator,
        "content_block_start",
        json!({"index": 2, "content_block": {"type": "tool_use", "id": "toolu_2", "name": "shell", "input": {}}}),
    );
    push(
        &mut translator,
        "content_block_delta",
        json!({"index": 2, "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
    );
    push(&mut translator, "content_block_stop", json!({"index": 2}));
    push(
        &mut translator,
        "message_delta",
        json!({"delta": {"stop_reason": "tool_use"}}),
    );
    let events = push(
        &mut translator,
        "message_stop",
        json!({"type": "message_stop"}),
    );

    let done: Vec<(u64, &str)> = events
        .iter()
        .filter(|event| event["type"].as_str() == Some("response.output_item.done"))
        .map(|event| {
            (
                event["output_index"].as_u64().unwrap(),
                event["item"]["type"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        done,
        vec![(0, "reasoning"), (1, "message"), (2, "function_call")]
    );
    assert_eq!(events.last().unwrap()["type"], "response.completed");
}

#[test]
fn marks_prompt_cache_breakpoints_on_the_system_prompt_and_the_last_turn() {
    // Anthropic caches only what a request marks. Codex replays the whole
    // conversation each turn, so the system prompt (which also covers the
    // tools) and the end of the conversation are both marked.
    let request = json!({
        "instructions": "You are a coding agent.",
        "input": [
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "answer"},
            {"role": "user", "content": "second"}
        ],
        "stream": true
    });
    let translated = translate_request(&request, "claude-upstream").unwrap();
    let ephemeral = json!({"type": "ephemeral"});
    assert_eq!(translated["system"].as_array().unwrap().len(), 1);
    assert_eq!(translated["system"][0]["cache_control"], ephemeral);

    let messages = translated["messages"].as_array().unwrap();
    let last = messages.last().unwrap()["content"].as_array().unwrap();
    assert_eq!(last.last().unwrap()["cache_control"], ephemeral);
    // Exactly one marker in the conversation: earlier turns are found through
    // the cache lookback, and Anthropic allows only four markers per request.
    let marked = messages
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap())
        .filter(|block| block.get("cache_control").is_some())
        .count();
    assert_eq!(marked, 1);

    // The same request translates to the same bytes, or the cache never hits.
    assert_eq!(
        serde_json::to_string(&translated).unwrap(),
        serde_json::to_string(&translate_request(&request, "claude-upstream").unwrap()).unwrap()
    );
}

/// The conversation with its prompt-cache markers removed, for tests that
/// are about message order and content rather than caching.
fn without_cache_markers(value: &Value) -> Value {
    match value {
        Value::Array(items) => items.iter().map(without_cache_markers).collect(),
        Value::Object(fields) => fields
            .iter()
            .filter(|(name, _)| *name != "cache_control")
            .map(|(name, value)| (name.clone(), without_cache_markers(value)))
            .collect::<serde_json::Map<_, _>>()
            .into(),
        other => other.clone(),
    }
}

#[test]
fn a_later_turn_extends_the_earlier_request_block_for_block() {
    // The provider cache matches on the exact prefix. Whatever one turn sent
    // must come back unchanged, in order, at the start of the next turn's
    // request; only the cache marker moves to the new end.
    let mut input = vec![
        json!({"type": "message", "role": "developer", "content": "base instructions"}),
        json!({"role": "user", "content": "list files"}),
        json!({"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"command\":\"ls\"}"}),
        json!({"type": "function_call_output", "call_id": "call_1", "output": "a.txt"}),
    ];
    let tools = json!([{
        "type": "function",
        "name": "shell",
        "description": "Run a command",
        "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}
    }]);
    let translate = |input: &[Value]| {
        translate_request(
            &json!({"input": input, "tools": tools, "stream": true}),
            "claude-upstream",
        )
        .unwrap()
    };
    let blocks = |request: &Value| -> Vec<(String, Value)> {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| {
                let role = message["role"].as_str().unwrap().to_owned();
                message["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(move |block| (role.clone(), without_cache_markers(block)))
            })
            .collect()
    };

    let first = translate(&input);
    input.extend([
        json!({"role": "assistant", "content": "There is one file."}),
        json!({"role": "user", "content": "now read it"}),
        json!({"type": "function_call", "call_id": "call_2", "name": "shell", "arguments": "{\"command\":\"cat a.txt\"}"}),
        json!({"type": "function_call_output", "call_id": "call_2", "output": "hello"}),
    ]);
    let second = translate(&input);

    assert_eq!(first["system"], second["system"]);
    assert_eq!(first["tools"], second["tools"]);
    let (earlier, later) = (blocks(&first), blocks(&second));
    assert!(later.len() > earlier.len());
    assert_eq!(later[..earlier.len()], earlier[..]);
}

// ---------------------------------------------------------------------------
// Regression: assistant item ids must be unique across a thread
// ---------------------------------------------------------------------------

/// Codex keys transcript rows by `(turn, item_id)` and appends a row only the
/// first time it sees a key. Ids built from the content-block `index` repeat on
/// every response, so a later reply rewrote the earlier row in place.
#[test]
fn assistant_item_ids_are_unique_across_responses() {
    // Each response carries its own upstream message id, as Anthropic does.
    fn first_text_item_id(message_id: &str) -> String {
        let mut translator = ResponseTranslator::with_created_at(1_700_000_000);
        let mut start = message_start("claude-x");
        start["message"]["id"] = json!(message_id);
        push(&mut translator, "message_start", start);
        push(
            &mut translator,
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
        );
        push(
            &mut translator,
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
        );
        push(&mut translator, "content_block_stop", json!({"index": 0}));
        let events = push(
            &mut translator,
            "message_stop",
            json!({"type": "message_stop"}),
        );
        events
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .and_then(|event| event["item"]["id"].as_str())
            .expect("a closed text item")
            .to_owned()
    }

    let first = first_text_item_id("msg_a");
    let second = first_text_item_id("msg_b");
    assert_ne!(
        first, second,
        "two responses produced the same assistant item id {first:?}; \
         Codex collapses them onto one transcript row"
    );
    assert!(first.starts_with("msg_"), "{first}");
}

// ---------------------------------------------------------------------------
// Regression: namespaced injected context (Codex goal objectives)
// ---------------------------------------------------------------------------

/// A Codex goal injects its objective as a `function_call_output` with `name` +
/// `namespace` and no `call_id`. It is context, not a tool answer, so it must
/// reach the model rather than failing the turn.
#[test]
fn forwards_namespaced_injected_context_without_a_call_id() {
    let request = json!({
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "function_call_output", "name": "goal_objective", "namespace": "agent",
             "output": "Inspect the repo and describe the UI/UX."}
        ],
        "stream": true
    });
    let body = translate_request(&request, "claude-upstream")
        .expect("injected context must not abort the turn");
    let messages = body["messages"].as_array().unwrap();
    let text = messages
        .iter()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|block| block["text"].as_str().map(str::to_owned))
        .collect::<String>();
    assert!(text.contains("Inspect the repo"), "{messages:?}");
    assert!(text.contains("agent.goal_objective"), "{messages:?}");
    // No fabricated tool_use/tool_result pair.
    assert!(
        !serde_json::to_string(messages)
            .unwrap()
            .contains("tool_result"),
        "injected context must not fabricate a tool result: {messages:?}"
    );
}

/// A real tool result still requires its `call_id`.
#[test]
fn still_requires_a_call_id_for_real_tool_outputs() {
    let request = json!({
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "function_call_output", "output": "orphan"}
        ],
        "stream": true
    });
    let error = translate_request(&request, "claude-upstream").unwrap_err();
    assert!(error.contains("call_id"), "{error}");
}

#[test]
fn maps_images_in_messages_and_tool_results() {
    let request = json!({
        "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
                {"type": "input_image", "image_url": "https://example.test/x.jpg"}
            ]},
            {"type": "function_call", "call_id": "c1", "name": "view_image", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": [
                {"type": "input_image", "image_url": "data:image/jpeg;base64,BBBB"}
            ]}
        ],
        "max_output_tokens": 64,
        "stream": true
    });
    let out = translate_request(&request, "m").expect("translates");
    let messages = out["messages"].as_array().unwrap();
    let user = messages[0]["content"].as_array().unwrap();
    assert_eq!(user[1]["type"], "image");
    assert_eq!(user[1]["source"]["type"], "base64");
    assert_eq!(user[1]["source"]["media_type"], "image/png");
    assert_eq!(user[1]["source"]["data"], "AAAA");
    assert_eq!(user[2]["source"]["type"], "url");
    assert_eq!(user[2]["source"]["url"], "https://example.test/x.jpg");
    let result = messages[2]["content"][0]["content"].as_array().unwrap();
    assert_eq!(result[0]["type"], "image");
    assert_eq!(result[0]["source"]["media_type"], "image/jpeg");
}

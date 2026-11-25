//! Offline tests for the Chat Completions adapter.
//!
//! The module is loaded by relative path so the adapter can be built and tested
//! before the relay wires it in. No sockets, no credentials, no clock: every SSE
//! frame below is a literal and the fixture is a recorded upstream stream.

#[path = "../src/adapters/chat_completions.rs"]
mod chat_completions;

use std::fs;

use chat_completions::{
    AdapterOptions, DeveloperRole, MaxTokensField, ReasoningEffort, ResponseTranslator,
    translate_request, translate_request_with,
};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Request translation
// ---------------------------------------------------------------------------

#[test]
fn preserves_instructions_message_order_and_text() {
    let input = json!({
        "model": "codex-visible-slug",
        "instructions": "You are Codex.\nBe precise.",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "  keep   spacing\n"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ack"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "üñïçøde ✓"}]}
        ],
        "stream": true
    });

    let body = translate_request(&input, "upstream-model").unwrap();

    assert_eq!(body["model"], "upstream-model");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert!(body.get("input").is_none());
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages[0],
        json!({"role": "system", "content": "You are Codex.\nBe precise."})
    );
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "  keep   spacing\n");
    assert_eq!(messages[2], json!({"role": "assistant", "content": "ack"}));
    assert_eq!(messages[3]["content"], "üñïçøde ✓");
}

#[test]
fn accepts_a_plain_string_input() {
    let body = translate_request(&json!({"input": "hello"}), "m").unwrap();
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": "hello"}])
    );
}

#[test]
fn translates_function_tools_with_and_without_optional_fields() {
    let input = json!({
        "input": "hi",
        "tools": [
            {
                "type": "function",
                "name": "shell",
                "description": "Run a command",
                "strict": false,
                "parameters": {
                    "type": "object",
                    "properties": {"command": {"type": "array", "items": {"type": "string"}}},
                    "required": ["command"]
                }
            },
            {"type": "function", "name": "read_file"}
        ],
        "tool_choice": {"type": "function", "name": "shell"},
        "parallel_tool_calls": false
    });

    let body = translate_request(&input, "m").unwrap();
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(
        tools[0],
        json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run a command",
                "parameters": {
                    "type": "object",
                    "properties": {"command": {"type": "array", "items": {"type": "string"}}},
                    "required": ["command"]
                },
                "strict": false
            }
        })
    );
    assert_eq!(tools[1]["function"]["name"], "read_file");
    assert_eq!(
        tools[1]["function"]["parameters"],
        json!({"type": "object", "properties": {}})
    );
    assert_eq!(
        body["tool_choice"],
        json!({"type": "function", "function": {"name": "shell"}})
    );
    assert_eq!(body["parallel_tool_calls"], false);
}

#[test]
fn forwards_baseline_tool_choice_strings() {
    for mode in ["auto", "none", "required"] {
        let body = translate_request(&json!({"input": "hi", "tool_choice": mode}), "m").unwrap();
        assert_eq!(body["tool_choice"], mode);
    }
}

#[test]
fn rejects_hosted_and_freeform_tools() {
    for kind in [
        "web_search",
        "web_search_preview",
        "file_search",
        "computer_use_preview",
        "image_generation",
        "code_interpreter",
        "mcp",
        "local_shell",
        "custom",
    ] {
        let input = json!({"input": "hi", "tools": [{"type": kind, "name": "hosted"}]});
        let error = translate_request(&input, "m").unwrap_err();
        assert!(error.contains(kind), "unexpected error for {kind}: {error}");
        assert!(error.contains("not supported"), "{error}");
    }
}

#[test]
fn rejects_hosted_tool_choices() {
    let input = json!({
        "input": "hi",
        "tools": [{"type": "function", "name": "shell"}],
        "tool_choice": {"type": "web_search_preview"}
    });
    let error = translate_request(&input, "m").unwrap_err();
    assert!(error.contains("web_search_preview"), "{error}");
    assert!(error.contains("tool_choice"), "{error}");
}

#[test]
fn preserves_tool_history_order_and_stable_ids() {
    let input = json!({
        "instructions": "sys",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "checking"}]},
            {
                "type": "function_call",
                "call_id": "call_abc",
                "name": "shell",
                "arguments": "{\"command\":[\"ls\"]}"
            },
            {
                "type": "function_call",
                "call_id": "call_def",
                "name": "shell",
                "arguments": "{\"command\":[\"pwd\"]}"
            },
            {"type": "function_call_output", "call_id": "call_abc", "output": "a.rs\nb.rs"},
            {
                "type": "function_call_output",
                "call_id": "call_def",
                "output": [{"type": "input_text", "text": "/repo"}]
            },
            {"type": "message", "role": "user", "content": "thanks"}
        ]
    });

    let body = translate_request(&input, "m").unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 6);
    assert_eq!(messages[0], json!({"role": "system", "content": "sys"}));
    assert_eq!(messages[1]["content"], "list files");
    // Two calls in one turn fold into one assistant message, in input order, with
    // their original call IDs so the tool results still resolve.
    assert_eq!(
        messages[2],
        json!({
            "role": "assistant",
            "content": "checking",
            "tool_calls": [
                {
                    "id": "call_abc",
                    "type": "function",
                    "function": {"name": "shell", "arguments": "{\"command\":[\"ls\"]}"}
                },
                {
                    "id": "call_def",
                    "type": "function",
                    "function": {"name": "shell", "arguments": "{\"command\":[\"pwd\"]}"}
                }
            ]
        })
    );
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "call_abc", "content": "a.rs\nb.rs"})
    );
    assert_eq!(
        messages[4],
        json!({"role": "tool", "tool_call_id": "call_def", "content": "/repo"})
    );
    assert_eq!(messages[5], json!({"role": "user", "content": "thanks"}));
}

#[test]
fn creates_an_assistant_turn_when_calls_have_no_preceding_message() {
    let input = json!({
        "input": [
            {"type": "message", "role": "user", "content": "go"},
            {"type": "function_call", "call_id": "call_a", "name": "shell", "arguments": "{}"}
        ]
    });
    let body = translate_request(&input, "m").unwrap();
    assert_eq!(
        body["messages"][1],
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {"id": "call_a", "type": "function", "function": {"name": "shell", "arguments": "{}"}}
            ]
        })
    );
}

#[test]
fn drops_replayable_reasoning_items_without_reordering() {
    let input = json!({
        "input": [
            {"type": "message", "role": "user", "content": "first"},
            {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "opaque"},
            {"type": "message", "role": "user", "content": "second"}
        ]
    });
    let body = translate_request(&input, "m").unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["content"], "first");
    assert_eq!(messages[1]["content"], "second");
}

#[test]
fn rejects_requests_that_produce_no_messages() {
    let error = translate_request(&json!({"input": []}), "m").unwrap_err();
    assert!(error.contains("no messages"), "{error}");

    // Instructions alone still produce a usable conversation.
    let body = translate_request(&json!({"instructions": "sys", "input": []}), "m").unwrap();
    assert_eq!(
        body["messages"],
        json!([{"role": "system", "content": "sys"}])
    );
}

#[test]
fn rejects_server_state_dependencies() {
    for (input, needle) in [
        (
            json!({"input": "hi", "previous_response_id": "resp_1"}),
            "previous_response_id",
        ),
        (
            json!({"input": "hi", "conversation": "conv_1"}),
            "conversation",
        ),
        (json!({"input": "hi", "store": true}), "store"),
        (json!({"input": "hi", "background": true}), "background"),
    ] {
        let error = translate_request(&input, "m").unwrap_err();
        assert!(error.contains(needle), "unexpected error: {error}");
        assert!(error.contains("not supported"), "{error}");
    }

    // Explicit nulls are what Codex sends for absent features and must pass.
    let body = translate_request(
        &json!({"input": "hi", "previous_response_id": null, "store": false, "background": false}),
        "m",
    )
    .unwrap();
    assert_eq!(body["messages"][0]["content"], "hi");
}

#[test]
fn rejects_unhandled_item_types() {
    for kind in [
        "item_reference",
        "local_shell_call",
        "custom_tool_call",
        "computer_call",
    ] {
        let input = json!({"input": [{"type": kind, "id": "x"}]});
        let error = translate_request(&input, "m").unwrap_err();
        assert!(error.contains(kind), "unexpected error for {kind}: {error}");
        assert!(error.contains("not supported"), "{error}");
    }
}

#[test]
fn rejects_non_streaming_and_invalid_requests() {
    let error = translate_request(&json!({"input": "hi", "stream": false}), "m").unwrap_err();
    assert!(error.contains("only supports stream: true"), "{error}");

    assert!(translate_request(&json!({"input": "hi", "stream": "yes"}), "m").is_err());
    assert!(translate_request(&json!("hi"), "m").is_err());
    assert!(translate_request(&json!({}), "m").is_err());
    assert!(translate_request(&json!({"input": "hi"}), "").is_err());
    assert!(translate_request(&json!({"input": 7}), "m").is_err());
    assert!(translate_request(&json!({"input": "hi", "instructions": 7}), "m").is_err());
}

#[test]
fn rejects_unknown_roles_and_content_part_types() {
    let input = json!({"input": [{"type": "message", "role": "critic", "content": "hi"}]});
    assert!(
        translate_request(&input, "m")
            .unwrap_err()
            .contains("critic")
    );

    let input = json!({"input": [{"type": "message", "role": "user", "content": [{"type": "input_audio"}]}]});
    assert!(
        translate_request(&input, "m")
            .unwrap_err()
            .contains("input_audio")
    );
}

#[test]
fn maps_image_parts_only_for_user_messages() {
    let input = json!({"input": [{"type": "message", "role": "user", "content": [
        {"type": "input_text", "text": "what is this"},
        {"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "high"}
    ]}]});
    let body = translate_request(&input, "m").unwrap();
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            {"type": "text", "text": "what is this"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA", "detail": "high"}}
        ])
    );

    let input = json!({"input": [{"type": "message", "role": "assistant", "content": [
        {"type": "input_image", "image_url": "https://example.com/x.png"}
    ]}]});
    assert!(
        translate_request(&input, "m")
            .unwrap_err()
            .contains("cannot carry image")
    );

    // Codex view_image returns an image as tool output. Tool messages are text-only,
    // so the image follows the last tool result of the turn as a user message.
    let input = json!({"input": [
        {"type": "function_call", "call_id": "call_a", "name": "view_image", "arguments": "{}"},
        {"type": "function_call", "call_id": "call_b", "name": "shell", "arguments": "{}"},
        {
            "type": "function_call_output",
            "call_id": "call_a",
            "output": [{"type": "input_image", "image_url": "data:image/png;base64,AAAA"}]
        },
        {"type": "function_call_output", "call_id": "call_b", "output": "ok"},
        {"type": "message", "role": "user", "content": "next"}
    ]});
    let body = translate_request(&input, "m").unwrap();
    let messages = body["messages"].as_array().unwrap();
    let roles: Vec<_> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["assistant", "tool", "tool", "user", "user"]);
    assert_eq!(messages[1]["content"], "[image attached below]");
    assert_eq!(
        messages[3]["content"][1],
        json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}})
    );

    let input = json!({"input": [{
        "type": "function_call_output",
        "call_id": "call_a",
        "output": [{"type": "input_audio", "data": "x"}]
    }]});
    assert!(
        translate_request(&input, "m")
            .unwrap_err()
            .contains("tool output part")
    );
}

#[test]
fn forwards_sampling_fields_and_maps_limits() {
    let input = json!({
        "input": "hi",
        "max_output_tokens": 512,
        "temperature": 0.2,
        "top_p": 0.9,
        "seed": 7,
        "user": "oscar",
        "metadata": {"thread": "t1"},
        "service_tier": "auto",
        "prompt_cache_key": "dropped",
        "reasoning": {"effort": "high", "summary": "auto"}
    });

    let body = translate_request(&input, "m").unwrap();
    assert_eq!(body["max_tokens"], 512);
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["top_p"], 0.9);
    assert_eq!(body["seed"], 7);
    assert_eq!(body["user"], "oscar");
    assert_eq!(body["metadata"], json!({"thread": "t1"}));
    assert_eq!(body["service_tier"], "auto");
    // Nothing outside the documented seam leaks upstream.
    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("reasoning").is_none());
    assert!(body.get("reasoning_effort").is_none());

    assert!(translate_request(&json!({"input": "hi", "max_output_tokens": -1}), "m").is_err());
}

#[test]
fn maps_text_format_to_response_format() {
    let schema = json!({
        "type": "json_schema",
        "name": "answer",
        "schema": {"type": "object", "properties": {"a": {"type": "string"}}},
        "strict": true
    });
    let body = translate_request(&json!({"input": "hi", "text": {"format": schema}}), "m").unwrap();
    assert_eq!(
        body["response_format"],
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": "answer",
                "schema": {"type": "object", "properties": {"a": {"type": "string"}}},
                "strict": true
            }
        })
    );

    let body = translate_request(
        &json!({"input": "hi", "text": {"format": {"type": "json_object"}}}),
        "m",
    )
    .unwrap();
    assert_eq!(body["response_format"], json!({"type": "json_object"}));

    let body = translate_request(
        &json!({"input": "hi", "text": {"format": {"type": "text"}}}),
        "m",
    )
    .unwrap();
    assert!(body.get("response_format").is_none());

    let error = translate_request(
        &json!({"input": "hi", "text": {"format": {"type": "json_schema"}}}),
        "m",
    )
    .unwrap_err();
    assert!(error.contains("json_schema"), "{error}");
}

#[test]
fn options_expose_only_documented_seams() {
    let input = json!({
        "input": [{"type": "message", "role": "developer", "content": "policy"}],
        "max_output_tokens": 64,
        "reasoning": {"effort": "high", "summary": "auto"}
    });

    let options = AdapterOptions {
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
        reasoning_effort: ReasoningEffort::Forward,
        developer_role: DeveloperRole::Developer,
        include_stream_usage: false,
    };
    let body = translate_request_with(&input, "m", &options).unwrap();
    assert_eq!(body["max_completion_tokens"], 64);
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["reasoning_effort"], "high");
    assert!(body.get("stream_options").is_none());
    assert_eq!(body["messages"][0]["role"], "developer");
    // `reasoning.summary` has no Chat Completions equivalent and is never invented.
    assert_eq!(body["reasoning_effort"], "high");

    let body = translate_request(&input, "m").unwrap();
    assert_eq!(body["max_tokens"], 64);
    assert!(body.get("reasoning_effort").is_none());
    assert_eq!(body["messages"][0]["role"], "system");
}

// ---------------------------------------------------------------------------
// Stream translation
// ---------------------------------------------------------------------------

fn new_translator() -> ResponseTranslator {
    ResponseTranslator::new("resp_test", "upstream-model", 1_700_000_000)
}

fn chunk(delta: Value) -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000,
        "model": "upstream-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
    })
}

fn role_chunk() -> Value {
    chunk(json!({"role": "assistant", "content": ""}))
}

fn finish_chunk(reason: &str) -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000,
        "model": "upstream-model",
        "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]
    })
}

fn usage_chunk() -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000,
        "model": "upstream-model",
        "choices": [],
        "usage": {
            "prompt_tokens": 11,
            "completion_tokens": 4,
            "total_tokens": 15,
            "prompt_tokens_details": {"cached_tokens": 3},
            "completion_tokens_details": {"reasoning_tokens": 0}
        }
    })
}

fn push(translator: &mut ResponseTranslator, body: &Value) -> Vec<Value> {
    translator
        .push_event("message", &body.to_string())
        .unwrap_or_else(|error| panic!("chunk {body} failed: {error}"))
}

fn kinds(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect()
}

fn of_kind<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["type"] == kind)
        .collect()
}

fn one<'a>(events: &'a [Value], kind: &str) -> &'a Value {
    let matching = of_kind(events, kind);
    assert_eq!(
        matching.len(),
        1,
        "expected one {kind} in {:?}",
        kinds(events)
    );
    matching[0]
}

fn text(events: &[Value]) -> String {
    of_kind(events, "response.output_text.delta")
        .iter()
        .filter_map(|event| event["delta"].as_str())
        .collect()
}

#[test]
fn streams_text_as_responses_events() {
    let mut translator = new_translator();
    let mut events = push(&mut translator, &role_chunk());
    events.extend(push(&mut translator, &chunk(json!({"content": "Hel"}))));
    events.extend(push(&mut translator, &chunk(json!({"content": "lo, "}))));
    events.extend(push(&mut translator, &chunk(json!({"content": "world"}))));
    events.extend(push(&mut translator, &finish_chunk("stop")));
    events.extend(push(&mut translator, &usage_chunk()));
    events.extend(
        translator
            .push_event("message", "[DONE]")
            .expect("done completes the stream"),
    );

    assert_eq!(
        kinds(&events),
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    let sequences = events
        .iter()
        .map(|event| event["sequence_number"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(sequences, (0..events.len() as u64).collect::<Vec<_>>());
    assert_eq!(text(&events), "Hello, world");

    let created = one(&events, "response.created");
    assert_eq!(created["response"]["id"], "resp_test");
    assert_eq!(created["response"]["status"], "in_progress");
    assert_eq!(created["response"]["output"], json!([]));
    assert_eq!(created["response"]["object"], "response");

    let added = one(&events, "response.output_item.added");
    assert_eq!(added["output_index"], 0);
    assert_eq!(
        added["item"],
        json!({"id": "msg_resp_test_0", "type": "message", "status": "in_progress", "role": "assistant", "content": []})
    );
    let part_added = one(&events, "response.content_part.added");
    assert_eq!(part_added["content_index"], 0);
    assert_eq!(
        part_added["part"],
        json!({"type": "output_text", "text": "", "annotations": []})
    );

    let text_done = one(&events, "response.output_text.done");
    assert_eq!(text_done["text"], "Hello, world");
    assert_eq!(text_done["item_id"], "msg_resp_test_0");

    let completed = one(&events, "response.completed");
    assert_eq!(completed["response"]["id"], "resp_test");
    assert_eq!(completed["response"]["model"], "upstream-model");
    assert_eq!(completed["response"]["created_at"], 1_700_000_000);
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(completed["response"]["incomplete_details"], Value::Null);
    assert_eq!(
        completed["response"]["output"],
        json!([{
            "id": "msg_resp_test_0",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "Hello, world", "annotations": []}]
        }])
    );
    assert_eq!(
        completed["response"]["usage"],
        json!({
            "input_tokens": 11,
            "input_tokens_details": {"cached_tokens": 3},
            "output_tokens": 4,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 15
        })
    );
    assert!(translator.is_finished());
}

#[test]
fn streams_interleaved_tool_call_indexes() {
    let mut translator = new_translator();
    let mut events = push(
        &mut translator,
        &chunk(json!({"tool_calls": [
            {"index": 0, "id": "call_a", "type": "function", "function": {"name": "shell", "arguments": ""}}
        ]})),
    );
    events.extend(push(
        &mut translator,
        &chunk(json!({"tool_calls": [
            {"index": 1, "id": "call_b", "type": "function", "function": {"name": "read_file", "arguments": "{\"pa"}}
        ]})),
    ));
    events.extend(push(
        &mut translator,
        &chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"comm"}}]})),
    ));
    events.extend(push(
        &mut translator,
        &chunk(json!({"tool_calls": [{"index": 1, "function": {"arguments": "th\":\"a.rs\"}"}}]})),
    ));
    events.extend(push(
        &mut translator,
        &chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "and\":[\"ls\"]}"}}]})),
    ));
    events.extend(push(&mut translator, &finish_chunk("tool_calls")));
    events.extend(push(&mut translator, &usage_chunk()));
    events.extend(translator.push_event("message", "[DONE]").unwrap());

    let added = of_kind(&events, "response.output_item.added");
    assert_eq!(added.len(), 2);
    assert_eq!(added[0]["output_index"], 0);
    assert_eq!(
        added[0]["item"],
        json!({
            "id": "fc_call_a",
            "type": "function_call",
            "status": "in_progress",
            "call_id": "call_a",
            "name": "shell",
            "arguments": ""
        })
    );
    assert_eq!(added[1]["output_index"], 1);
    assert_eq!(added[1]["item"]["call_id"], "call_b");
    assert_eq!(added[1]["item"]["name"], "read_file");

    let deltas = of_kind(&events, "response.function_call_arguments.delta");
    let by_item = |item_id: &str| -> String {
        deltas
            .iter()
            .filter(|delta| delta["item_id"] == item_id)
            .filter_map(|delta| delta["delta"].as_str())
            .collect()
    };
    assert_eq!(by_item("fc_call_a"), "{\"command\":[\"ls\"]}");
    assert_eq!(by_item("fc_call_b"), "{\"path\":\"a.rs\"}");
    // Deltas for one call never jump to the other call's index.
    let order = deltas
        .iter()
        .map(|delta| delta["output_index"].as_u64().unwrap())
        .collect::<Vec<_>>();
    // Chunk order is index 1 (its first delta is the buffered announce), 0, 1, 0:
    // each delta keeps the index it arrived under.
    assert_eq!(order, [1, 0, 1, 0]);
    assert_eq!(deltas.len(), 4);

    let done = of_kind(&events, "response.function_call_arguments.done");
    assert_eq!(done.len(), 2);
    assert_eq!(done[0]["arguments"], "{\"command\":[\"ls\"]}");
    assert_eq!(done[1]["arguments"], "{\"path\":\"a.rs\"}");

    let completed = one(&events, "response.completed");
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(
        completed["response"]["output"],
        json!([
            {
                "id": "fc_call_a",
                "type": "function_call",
                "status": "completed",
                "call_id": "call_a",
                "name": "shell",
                "arguments": "{\"command\":[\"ls\"]}"
            },
            {
                "id": "fc_call_b",
                "type": "function_call",
                "status": "completed",
                "call_id": "call_b",
                "name": "read_file",
                "arguments": "{\"path\":\"a.rs\"}"
            }
        ])
    );
}

#[test]
fn buffers_arguments_that_arrive_before_the_name() {
    let mut translator = new_translator();
    let mut events = push(
        &mut translator,
        &chunk(json!({"tool_calls": [
            {"index": 0, "id": "call_x", "type": "function", "function": {"arguments": "{\"a\":1}"}}
        ]})),
    );
    events.extend(push(
        &mut translator,
        &chunk(json!({"tool_calls": [{"index": 0, "function": {"name": "shell"}}]})),
    ));

    let added = one(&events, "response.output_item.added");
    assert_eq!(added["item"]["name"], "shell");
    assert_eq!(added["item"]["arguments"], "");
    let deltas = of_kind(&events, "response.function_call_arguments.delta");
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0]["delta"], "{\"a\":1}");

    events.extend(push(&mut translator, &finish_chunk("tool_calls")));
    events.extend(translator.push_event("message", "[DONE]").unwrap());
    assert_eq!(text(&events), "");
    assert_eq!(
        one(&events, "response.completed")["response"]["output"][0]["arguments"],
        "{\"a\":1}"
    );
}

#[test]
fn keeps_the_first_upstream_id_when_chunks_repeat_it() {
    let mut translator = new_translator();
    let mut events = push(
        &mut translator,
        &chunk(json!({"tool_calls": [
            {"index": 0, "id": "call_first", "type": "function", "function": {"name": "shell", "arguments": ""}}
        ]})),
    );
    events.extend(push(
        &mut translator,
        &chunk(json!({"tool_calls": [
            {"index": 0, "id": "call_second", "type": "function", "function": {"arguments": "{}"}}
        ]})),
    ));
    events.extend(push(&mut translator, &finish_chunk("tool_calls")));
    events.extend(translator.push_event("message", "[DONE]").unwrap());

    let added = one(&events, "response.output_item.added");
    assert_eq!(added["item"]["call_id"], "call_first");
    assert_eq!(added["item"]["id"], "fc_call_first");
    assert_eq!(
        one(&events, "response.completed")["response"]["output"][0]["call_id"],
        "call_first"
    );
}

#[test]
fn maps_legacy_function_call_deltas() {
    let mut translator = new_translator();
    let mut events = push(
        &mut translator,
        &chunk(json!({"function_call": {"name": "shell", "arguments": "{\"x\""}})),
    );
    events.extend(push(
        &mut translator,
        &chunk(json!({"function_call": {"arguments": ":1}"}})),
    ));
    events.extend(push(&mut translator, &finish_chunk("function_call")));
    events.extend(translator.push_event("message", "[DONE]").unwrap());

    let completed = one(&events, "response.completed");
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(
        completed["response"]["output"],
        json!([{
            "id": "fc_call_compat_0",
            "type": "function_call",
            "status": "completed",
            "call_id": "call_compat_0",
            "name": "shell",
            "arguments": "{\"x\":1}"
        }])
    );
}

#[test]
fn reports_length_as_an_incomplete_response() {
    let mut translator = new_translator();
    let mut events = push(&mut translator, &chunk(json!({"content": "partial"})));
    events.extend(push(&mut translator, &finish_chunk("length")));
    events.extend(push(&mut translator, &usage_chunk()));
    events.extend(translator.push_event("message", "[DONE]").unwrap());

    assert!(!of_kind(&events, "response.incomplete").is_empty());
    assert!(of_kind(&events, "response.completed").is_empty());
    let incomplete = one(&events, "response.incomplete");
    assert_eq!(incomplete["response"]["status"], "incomplete");
    assert_eq!(
        incomplete["response"]["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );
    assert_eq!(text(&events), "partial");
}

#[test]
fn does_not_complete_tool_calls_from_an_incomplete_response() {
    for reason in ["length", "content_filter"] {
        let mut translator = new_translator();
        let mut events = push(
            &mut translator,
            &chunk(json!({"tool_calls": [{
                "index": 0,
                "id": "call_partial",
                "function": {"name": "shell", "arguments": "{}"}
            }]})),
        );
        events.extend(push(&mut translator, &finish_chunk(reason)));
        events.extend(translator.push_event("message", "[DONE]").unwrap());

        assert!(of_kind(&events, "response.function_call_arguments.done").is_empty());
        assert!(of_kind(&events, "response.output_item.done").is_empty());
        let incomplete = one(&events, "response.incomplete");
        assert_eq!(incomplete["response"]["output"], json!([]));
    }
}

#[test]
fn reports_content_filter_as_an_incomplete_response() {
    let mut translator = new_translator();
    let mut events = push(&mut translator, &chunk(json!({"content": "nope"})));
    events.extend(push(&mut translator, &finish_chunk("content_filter")));
    events.extend(translator.push_event("message", "[DONE]").unwrap());

    let incomplete = one(&events, "response.incomplete");
    assert_eq!(
        incomplete["response"]["incomplete_details"],
        json!({"reason": "content_filter"})
    );
}

#[test]
fn rejects_unknown_finish_reasons() {
    let mut translator = new_translator();
    let events = push(&mut translator, &chunk(json!({"content": "x"})));
    assert!(!events.is_empty());
    // The unknown reason fails as soon as it arrives, before any item is closed.
    let error = translator
        .push_event("message", &finish_chunk("abort").to_string())
        .unwrap_err();
    assert!(error.contains("abort"), "{error}");
    assert!(!translator.is_finished());
    assert!(translator.finish().unwrap_err().contains("finish_reason"));
}

#[test]
fn streams_refusals_as_refusal_content_parts() {
    let mut translator = new_translator();
    let mut events = push(&mut translator, &role_chunk());
    events.extend(push(&mut translator, &chunk(json!({"refusal": "I can"}))));
    events.extend(push(&mut translator, &chunk(json!({"refusal": "not."}))));
    events.extend(push(&mut translator, &finish_chunk("stop")));
    events.extend(translator.push_event("message", "[DONE]").unwrap());

    let parts_added = of_kind(&events, "response.content_part.added");
    assert_eq!(parts_added.len(), 1);
    assert_eq!(
        parts_added[0]["part"],
        json!({"type": "refusal", "refusal": ""})
    );
    let done = one(&events, "response.refusal.done");
    assert_eq!(done["refusal"], "I cannot.");
    assert_eq!(
        one(&events, "response.completed")["response"]["output"][0]["content"],
        json!([{"type": "refusal", "refusal": "I cannot."}])
    );
}

#[test]
fn finishes_when_the_transport_ends_without_done() {
    let mut translator = new_translator();
    let mut events = push(&mut translator, &chunk(json!({"content": "done early"})));
    events.extend(push(&mut translator, &finish_chunk("stop")));
    assert!(!translator.is_finished());
    events.extend(translator.finish().expect("finish_reason was seen"));
    assert_eq!(
        one(&events, "response.completed")["response"]["status"],
        "completed"
    );
    assert!(translator.is_finished());

    // Idempotent: a late transport close returns nothing extra.
    assert!(translator.finish().unwrap().is_empty());
}

#[test]
fn rejects_interrupted_streams() {
    // Transport ends mid-message.
    let mut translator = new_translator();
    push(&mut translator, &chunk(json!({"content": "half"})));
    let error = translator.finish().unwrap_err();
    assert!(error.contains("before a finish_reason"), "{error}");

    // `[DONE]` without a finish reason is truncation too.
    let mut translator = new_translator();
    push(&mut translator, &chunk(json!({"content": "half"})));
    let error = translator.push_event("message", "[DONE]").unwrap_err();
    assert!(error.contains("finish_reason"), "{error}");

    // A stream that ends before its first chunk is still an error.
    let mut translator = new_translator();
    assert!(translator.finish().unwrap_err().contains("finish_reason"));
}

#[test]
fn rejects_a_tool_call_that_finished_without_a_name() {
    let mut translator = new_translator();
    push(
        &mut translator,
        &chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_x", "function": {"arguments": "{}"}}]}),
        ),
    );
    let error = translator
        .push_event("message", &finish_chunk("tool_calls").to_string())
        .unwrap_err();
    assert!(error.contains("before the upstream named it"), "{error}");
}

#[test]
fn rejects_malformed_chunks() {
    let mut translator = new_translator();
    let error = translator.push_event("message", "{not json").unwrap_err();
    assert!(error.contains("malformed"), "{error}");

    assert!(
        translator
            .push_event("message", "[]")
            .unwrap_err()
            .contains("JSON object")
    );

    let error = translator
        .push_event("message", r#"{"id":"x"}"#)
        .unwrap_err();
    assert!(error.contains("choices"), "{error}");

    let error = translator
        .push_event(
            "message",
            &chunk(json!({"content": ["not", "a", "string"]})).to_string(),
        )
        .unwrap_err();
    assert!(error.contains("must be a string"), "{error}");

    let error = translator
        .push_event(
            "message",
            &json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"name": "f", "arguments": 5}}]}}]})
                .to_string(),
        )
        .unwrap_err();
    assert!(error.contains("arguments must be a string"), "{error}");

    let error = translator
        .push_event(
            "message",
            &json!({"choices": [], "usage": {"prompt_tokens": 1}}).to_string(),
        )
        .unwrap_err();
    assert!(error.contains("completion_tokens"), "{error}");
}

#[test]
fn surfaces_upstream_errors() {
    let mut translator = new_translator();
    let error = translator
        .push_event(
            "error",
            r#"{"error": {"message": "upstream overloaded", "type": "server_error"}}"#,
        )
        .unwrap_err();
    assert!(error.contains("upstream overloaded"), "{error}");

    let mut translator = new_translator();
    let error = translator
        .push_event(
            "message",
            r#"{"error": {"message": "bad schema", "code": "invalid_request"}}"#,
        )
        .unwrap_err();
    assert!(error.contains("bad schema"), "{error}");

    let mut translator = new_translator();
    let error = translator
        .push_event("error", "gateway timeout")
        .unwrap_err();
    assert!(error.contains("gateway timeout"), "{error}");
}

#[test]
fn rejects_events_after_completion() {
    let mut translator = new_translator();
    push(&mut translator, &chunk(json!({"content": "x"})));
    push(&mut translator, &finish_chunk("stop"));
    translator.push_event("message", "[DONE]").unwrap();

    let error = translator.push_event("message", "[DONE]").unwrap_err();
    assert!(
        error.contains("after the chat completion stream finished"),
        "{error}"
    );

    let error = translator
        .push_event("message", &chunk(json!({"content": "late"})).to_string())
        .unwrap_err();
    assert!(error.contains("finished"), "{error}");
}

#[test]
fn ignores_sse_keepalive_frames() {
    let mut translator = new_translator();
    assert!(translator.push_event("ping", "{}").unwrap().is_empty());
    assert!(translator.push_event("keepalive", "").unwrap().is_empty());
    let mut events = push(&mut translator, &chunk(json!({"content": "after ping"})));
    events.extend(push(&mut translator, &finish_chunk("stop")));
    events.extend(translator.push_event("message", "[DONE]").unwrap());
    assert_eq!(text(&events), "after ping");
    // No response was started by the heartbeats, so sequence numbers still start at 0.
    assert_eq!(events[0]["sequence_number"], 0);
}

#[test]
fn ignores_other_choices_in_the_chunk() {
    let mut translator = new_translator();
    let mut body = chunk(json!({"content": "kept"}));
    body["choices"].as_array_mut().unwrap().push(json!({
        "index": 1,
        "delta": {"content": "dropped"},
        "finish_reason": null
    }));
    let events = push(&mut translator, &body);
    assert_eq!(text(&events), "kept");
}

// ---------------------------------------------------------------------------
// Recorded SSE fixture
// ---------------------------------------------------------------------------

/// Minimal SSE framing, as the relay's HTTP layer will do it: strip comments,
/// join multi-line `data:` fields, dispatch on blank lines.
fn run_sse(translator: &mut ResponseTranslator, sse: &str) -> Result<Vec<Value>, String> {
    let mut events = Vec::new();
    let mut name = String::from("message");
    let mut data = String::new();
    let mut framed = false;
    let mut dispatch = |translator: &mut ResponseTranslator,
                        name: &str,
                        data: &mut String,
                        framed: &mut bool|
     -> Result<(), String> {
        if !*framed {
            return Ok(());
        }
        let payload = std::mem::take(data);
        *framed = false;
        events.extend(translator.push_event(name, &payload)?);
        Ok(())
    };
    for line in sse.lines() {
        if line.is_empty() {
            dispatch(translator, &name, &mut data, &mut framed)?;
            name = String::from("message");
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            name = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if framed {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            framed = true;
        }
    }
    dispatch(translator, &name, &mut data, &mut framed)?;
    Ok(std::mem::take(&mut events))
}

#[test]
fn translates_a_recorded_upstream_stream() {
    let sse = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/chat_completions/text_stream.sse"
    ))
    .unwrap();
    let mut translator = new_translator();
    let events = run_sse(&mut translator, &sse).unwrap();

    assert_eq!(text(&events), "Fixture reply.");
    let completed = one(&events, "response.completed");
    assert_eq!(
        completed["response"]["output"][0]["content"],
        json!([{"type": "output_text", "text": "Fixture reply.", "annotations": []}])
    );
    assert_eq!(completed["response"]["usage"]["total_tokens"], 15);
    assert_eq!(
        completed["response"]["usage"]["input_tokens_details"],
        json!({"cached_tokens": 3})
    );
    assert!(translator.is_finished());
}

#[test]
fn fixture_framing_tolerates_a_truncated_stream() {
    let sse = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/chat_completions/text_stream.sse"
    ))
    .unwrap();
    // Drop the final `[DONE]` and the finish reason: the upstream was cut short.
    let truncated = sse
        .lines()
        .filter(|line| !line.contains("[DONE]") && !line.contains("\"stop\""))
        .collect::<Vec<_>>()
        .join("\n");
    let mut translator = new_translator();
    let mut events = run_sse(&mut translator, &truncated).unwrap();
    let error = translator.finish().unwrap_err();
    assert!(error.contains("finish_reason"), "{error}");
    assert_eq!(text(&events), "Fixture reply.");
    events.clear();
}

// ---------------------------------------------------------------------------
// Regression: assistant item ids must be unique across a thread
// ---------------------------------------------------------------------------

/// Codex keys transcript rows by `(turn, item_id)` and appends a row only the
/// first time it sees a key. Ids built from a per-stream `output_index` repeat
/// on every response, so a second reply rewrote the first row in place instead
/// of adding a new one. Two separate responses must never share an item id.
#[test]
fn assistant_item_ids_are_unique_across_responses() {
    let mut first = ResponseTranslator::new("resp_a", "upstream-model", 1_700_000_000);
    push(&mut first, &role_chunk());
    push(&mut first, &chunk(json!({"content": "first"})));
    push(&mut first, &finish_chunk("stop"));
    let mut first_events = push(&mut first, &usage_chunk());
    first_events.extend(first.push_event("message", "[DONE]").expect("done"));
    let first_id = one(&first_events, "response.completed")["response"]["output"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut second = ResponseTranslator::new("resp_b", "upstream-model", 1_700_000_001);
    push(&mut second, &role_chunk());
    push(&mut second, &chunk(json!({"content": "second"})));
    push(&mut second, &finish_chunk("stop"));
    let mut second_events = push(&mut second, &usage_chunk());
    second_events.extend(second.push_event("message", "[DONE]").expect("done"));
    let second_id = one(&second_events, "response.completed")["response"]["output"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    assert_ne!(
        first_id, second_id,
        "two responses produced the same assistant item id {first_id:?}; \
         Codex collapses them onto one transcript row"
    );
    // Both are still distinct message ids, and neither is the bare position.
    assert!(first_id.starts_with("msg_"), "{first_id}");
    assert!(second_id.starts_with("msg_"), "{second_id}");
}

// ---------------------------------------------------------------------------
// Regression: namespaced injected context (Codex goal objectives)
// ---------------------------------------------------------------------------

/// When a thread has a goal, Codex injects the objective as a
/// `function_call_output` carrying `name` + `namespace` and no `call_id`. It is
/// context, not the answer to a call, so demanding a `call_id` aborted the whole
/// turn with "input item N needs a call_id". It must reach the model instead.
#[test]
fn forwards_namespaced_injected_context_without_a_call_id() {
    let input = json!({"input": [
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
        {
            "type": "function_call_output",
            "name": "goal_objective",
            "namespace": "agent",
            "output": "Inspect the repo and describe the UI/UX."
        }
    ]});
    let body = translate_request(&input, "m").expect("injected context must not abort the turn");
    let messages = body["messages"].as_array().unwrap();
    let context = messages
        .iter()
        .find(|message| {
            message["content"]
                .as_str()
                .is_some_and(|content| content.contains("Inspect the repo"))
        })
        .expect("the objective reached the model");

    // It is context, not an answer to a tool call: no invented `tool` role.
    assert_eq!(context["role"], "user");
    assert!(context.get("tool_call_id").is_none());
    assert!(
        messages.iter().all(|message| message["role"] != "tool"),
        "injected context must not fabricate a tool result: {messages:?}"
    );
    // The label keeps the provenance discoverable.
    assert!(
        context["content"]
            .as_str()
            .unwrap()
            .contains("agent.goal_objective")
    );
}

/// A real tool result still requires its `call_id`; the injected-context
/// exemption must not swallow genuine protocol errors.
#[test]
fn still_requires_a_call_id_for_real_tool_outputs() {
    let input = json!({"input": [
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
        {"type": "function_call_output", "output": "orphan output"}
    ]});
    let error = translate_request(&input, "m").unwrap_err();
    assert!(error.contains("needs a call_id"), "{error}");
}

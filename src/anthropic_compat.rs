//! Anthropic Messages API compatibility layer (`/v1/messages`).
//!
//! Translates Anthropic wire format to the internal OpenAI-shaped body so
//! Claude Code / any Anthropic-SDK client can use the same free-model lanes.
//! Non-streaming first: the upstream response is aggregated by the existing
//! pipeline, then re-emitted as a complete Anthropic message (and as a valid
//! one-shot SSE event sequence when the client asked for `stream`).

use axum::response::IntoResponse;
use serde_json::{json, Value};

/// Convert an Anthropic `/v1/messages` request body into an OpenAI
/// `/v1/chat/completions` body understood by `Router::execute_chat`.
pub fn anthropic_to_openai(body: &Value) -> Value {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("kilo-auto/free")
        .to_string();

    let mut messages: Vec<Value> = Vec::new();
    if let Some(sys) = body.get("system") {
        // system is a string or an array of content blocks
        let text = match sys {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    if let Some(msgs) = body.get("messages").and_then(|v| v.as_array()) {
        for m in msgs {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = match m.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .filter_map(|b| match b.get("type").and_then(|v| v.as_str()) {
                        Some("text") => b.get("text").and_then(|t| t.as_str()).map(String::from),
                        // tool_result blocks carry prior tool output; surface as text
                        Some("tool_result") => b.get("content").and_then(|c| c.as_str()).map(String::from),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            messages.push(json!({"role": role, "content": content}));
        }
    }

    let mut out = json!({"model": model, "messages": messages, "stream": false});
    if let Some(mt) = body.get("max_tokens").and_then(|v| v.as_u64()) {
        // Anthropic max_tokens == OpenAI max_tokens; reasoning models need headroom
        out["max_tokens"] = json!(mt.max(150));
        out["max_completion_tokens"] = json!(mt.max(150));
    }
    out
}

/// Extract assistant text (and reasoning, if present) from an aggregated
/// OpenAI chat-completion response.
fn openai_text(resp: &Value) -> String {
    let msg = &resp["choices"][0]["message"];
    let content = msg
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if content.trim().is_empty() {
        msg.get("reasoning_content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    } else {
        content.to_string()
    }
}

/// Build a complete Anthropic message response from an aggregated OpenAI one.
pub fn openai_to_anthropic(resp: &Value, requested_model: &str) -> Value {
    let text = openai_text(resp);
    let stop = resp["choices"][0]
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn");
    let stop_reason = match stop {
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        _ => "end_turn",
    };
    let usage = resp.get("usage");
    let id = resp
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("msg_lanehub")
        .to_string();
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": [{"type": "text", "text": text}],
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage.and_then(|u| u.get("prompt_tokens")).and_then(|v| v.as_u64()).unwrap_or(0),
            "output_tokens": usage.and_then(|u| u.get("completion_tokens")).and_then(|v| v.as_u64()).unwrap_or(0),
        }
    })
}

/// Render the complete Anthropic SSE event sequence for a finished message
/// (Claude Code accepts a single-shot stream: message_start → content deltas
/// → message_stop with nothing in between).
pub fn anthropic_sse_frames(message: &Value) -> Vec<u8> {
    let mut out = String::new();
    let frame = |event: &str, data: &Value, out: &mut String| {
        out.push_str("event: ");
        out.push_str(event);
        out.push_str("\ndata: ");
        out.push_str(&data.to_string());
        out.push_str("\n\n");
    };

    let text = message["content"][0]
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    frame(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": message["id"], "type": "message", "role": "assistant",
                "model": message["model"], "content": [],
                "usage": {"input_tokens": message["usage"]["input_tokens"], "output_tokens": 0}
            }
        }),
        &mut out,
    );
    frame(
        "content_block_start",
        &json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        &mut out,
    );
    // emit the text in a few deltas so clients render progressively
    let chunk = (text.len() / 4).max(1).min(512);
    let mut index = 0usize;
    for piece in text.as_bytes().chunks(chunk.max(1)) {
        let piece = String::from_utf8_lossy(piece).to_string();
        frame(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": piece}}),
            &mut out,
        );
        index += 1;
    }
    frame(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": 0}),
        &mut out,
    );
    frame(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": message["stop_reason"], "stop_sequence": Value::Null},
            "usage": {"output_tokens": message["usage"]["output_tokens"]}
        }),
        &mut out,
    );
    frame("message_stop", &json!({"type": "message_stop"}), &mut out);
    out.into_bytes()
}

/// Convenience for the axum handler: error payload in Anthropic shape.
pub fn anthropic_error(status: u16, message: &str) -> axum::response::Response {
    (
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::BAD_GATEWAY),
        axum::Json(json!({
            "type": "error",
            "error": {"type": "api_error", "message": message}
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_body_translates_with_system() {
        let src = json!({
            "model": "kilo-auto/free",
            "max_tokens": 64,
            "system": "You are terse.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]}
            ]
        });
        let out = anthropic_to_openai(&src);
        assert_eq!(out["model"], "kilo-auto/free");
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][1]["role"], "user");
        assert_eq!(out["messages"][1]["content"], "hello");
        assert!(out["max_tokens"].as_u64().unwrap() >= 150);
    }

    #[test]
    fn anthropic_string_content_passes_through() {
        let src = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "plain"}]
        });
        let out = anthropic_to_openai(&src);
        assert_eq!(out["messages"][0]["content"], "plain");
    }

    #[test]
    fn openai_response_maps_to_anthropic_shape() {
        let resp = json!({
            "id": "chatcmpl-x",
            "choices": [{"finish_reason": "stop", "message": {"content": "hi there"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4}
        });
        let msg = openai_to_anthropic(&resp, "kilo-auto/free");
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["content"][0]["text"], "hi there");
        assert_eq!(msg["stop_reason"], "end_turn");
        assert_eq!(msg["usage"]["output_tokens"], 4);
    }

    #[test]
    fn sse_frames_form_valid_sequence() {
        let resp = json!({
            "id": "x", "choices": [{"finish_reason": "stop", "message": {"content": "abc"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let msg = openai_to_anthropic(&resp, "m");
        let bytes = anthropic_sse_frames(&msg);
        let s = String::from_utf8(bytes).unwrap();
        for ev in ["message_start", "content_block_start", "content_block_delta", "message_stop"] {
            assert!(s.contains(&format!("event: {ev}")), "missing {ev}");
        }
        assert!(s.ends_with("\n\n"));
    }
}

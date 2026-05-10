//! Anthropic Messages API translation: `ChatRequest` → request body JSON, and
//! SSE event payloads → [`ChatChunk`]s.
//!
//! ## Wire format produced
//!
//! ```json
//! {
//!   "model": "<model>",
//!   "max_tokens": <max_tokens>,
//!   "system": "<sys>",                  // omitted entirely when None
//!   "messages": [ ... ],
//!   "tools": [],
//!   "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
//!   "stream": true
//! }
//! ```
//!
//! Notable shape decisions:
//!
//! - The Anthropic API does NOT accept a top-level `system` role inside
//!   `messages`; `system` is a sibling field on the request body.
//! - `tool_choice.disable_parallel_tool_use` is the inverse of the
//!   [`ChatRequest::parallel_tool_calls_allowed`] flag: when the flag is
//!   `false` (its default until F-599), the request asks Anthropic to
//!   disable parallel tool use.
//! - `tools` is emitted as an empty array — F-583 only plumbs tool **calls**
//!   and **tool results** within messages; tool-schema definitions are
//!   out of scope and will be added by a later task.
//! - Multiple `text` blocks in one assistant message are preserved in order.
//!
//! ## SSE event consumption
//!
//! [`AnthropicEventAccumulator`] consumes Anthropic's named SSE events
//! (`content_block_start`, `content_block_delta`, `content_block_stop`,
//! `message_delta`, `message_stop`) and emits [`ChatChunk`]s:
//!
//! - `content_block_delta` with `delta.type == "text_delta"` →
//!   [`ChatChunk::TextDelta`].
//! - `tool_use` blocks: track the active block by its `index`; accumulate
//!   `partial_json` from `input_json_delta` events; on `content_block_stop`
//!   parse the accumulated string and emit [`ChatChunk::ToolCall`].
//! - `message_delta` carries the upcoming `stop_reason`; remember it.
//! - `message_stop` → [`ChatChunk::Done`] carrying the upcoming `stop_reason`,
//!   defaulting to `"end_turn"` if the prior `message_delta` did not carry one.

use crate::{ChatBlock, ChatChunk, ChatMessage, ChatRequest, ChatRole};
use serde::Serialize;
use serde_json::{json, Value};

/// Serialize a [`ChatRequest`] into an Anthropic Messages API request body.
///
/// `parallel_tool_calls_allowed` controls the `tool_choice.disable_parallel_tool_use`
/// flag: when `false`, parallel tool use is disabled (the F-583 default until
/// F-599 lands).
///
/// Wire-shape note (F-599): `disable_parallel_tool_use` only appears on
/// the request body once tool schemas land — until then `tools` is
/// empty and we omit `tool_choice` entirely (Anthropic rejects the
/// field with no tools attached). Once tools are wired, the flag will
/// reach the model for any model that supports it; pre-claude-3.5
/// vintages may silently ignore the field rather than honour it, which
/// is acceptable degradation (parallel tools are an opt-in optimisation,
/// not a correctness requirement).
pub fn serialize_request(
    req: &ChatRequest,
    model: &str,
    max_tokens: u32,
    parallel_tool_calls_allowed: bool,
) -> std::result::Result<Vec<u8>, serde_json::Error> {
    // Anthropic's Messages API rejects (HTTP 400) any request that sends a
    // `tool_choice` field without a non-empty `tools` array. Until tool
    // schemas land, both fields stay omitted and the inversion encoded by
    // `parallel_tool_calls_allowed` is a no-op on the wire.
    let tools: &[Value] = &[];
    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some(ToolChoice {
            kind: "auto",
            disable_parallel_tool_use: !parallel_tool_calls_allowed,
        })
    };
    let body = AnthropicBody {
        model,
        max_tokens,
        system: req.system.as_deref(),
        messages: AnthropicMessages {
            messages: &req.messages,
        },
        tools,
        tool_choice,
        stream: true,
    };
    serde_json::to_vec(&body)
}

#[derive(Serialize)]
struct AnthropicBody<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: AnthropicMessages<'a>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct ToolChoice<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    disable_parallel_tool_use: bool,
}

struct AnthropicMessages<'a> {
    messages: &'a [ChatMessage],
}

impl<'a> Serialize for AnthropicMessages<'a> {
    fn serialize<S>(&self, s: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeSeq;

        // Worst-case sequence size: one Anthropic message per ChatMessage,
        // BUT a single ChatMessage that mixes text/tool_call blocks with
        // tool_result blocks must be split (tool_results live inside a
        // user-role message, distinct from any other role). Over-counting
        // is harmless when the serializer ignores the size hint.
        let cap = self.messages.iter().map(|m| m.content.len() + 1).sum();
        let mut seq = s.serialize_seq(Some(cap))?;

        for msg in self.messages {
            // Anthropic's content shape allows interleaved text + tool_use
            // blocks within one assistant message, but tool_result blocks
            // are user-role only. Emit (a) all tool_results as their own
            // user message, then (b) any remaining text/tool_use as the
            // original-role message — preserving block order within each.
            let mut tool_results: Vec<&ChatBlock> = Vec::new();
            let mut other_blocks: Vec<&ChatBlock> = Vec::new();
            for block in &msg.content {
                match block {
                    ChatBlock::ToolResult { .. } => tool_results.push(block),
                    _ => other_blocks.push(block),
                }
            }

            if !tool_results.is_empty() {
                seq.serialize_element(&AnthropicToolResultMessage {
                    blocks: &tool_results,
                })?;
            }
            if !other_blocks.is_empty() {
                seq.serialize_element(&AnthropicTextMessage {
                    role: anthropic_role(&msg.role),
                    blocks: &other_blocks,
                })?;
            }
        }
        seq.end()
    }
}

fn anthropic_role(role: &ChatRole) -> &'static str {
    match role {
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
    }
}

struct AnthropicTextMessage<'a> {
    role: &'a str,
    blocks: &'a [&'a ChatBlock],
}

impl<'a> Serialize for AnthropicTextMessage<'a> {
    fn serialize<S>(&self, s: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::{SerializeMap, SerializeSeq};
        let mut m = s.serialize_map(Some(2))?;
        m.serialize_entry("role", self.role)?;

        struct Blocks<'a> {
            blocks: &'a [&'a ChatBlock],
        }
        impl<'a> Serialize for Blocks<'a> {
            fn serialize<S>(&self, s: S) -> std::result::Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let mut seq = s.serialize_seq(Some(self.blocks.len()))?;
                for block in self.blocks {
                    match block {
                        ChatBlock::Text(t) => {
                            seq.serialize_element(&json!({"type": "text", "text": t}))?;
                        }
                        ChatBlock::ToolCall { id, name, args } => {
                            seq.serialize_element(&json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": args,
                            }))?;
                        }
                        ChatBlock::ToolResult { .. } => {
                            // Filtered upstream; defensive no-op.
                        }
                    }
                }
                seq.end()
            }
        }
        m.serialize_entry(
            "content",
            &Blocks {
                blocks: self.blocks,
            },
        )?;
        m.end()
    }
}

struct AnthropicToolResultMessage<'a> {
    blocks: &'a [&'a ChatBlock],
}

impl<'a> Serialize for AnthropicToolResultMessage<'a> {
    fn serialize<S>(&self, s: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::{SerializeMap, SerializeSeq};
        let mut m = s.serialize_map(Some(2))?;
        m.serialize_entry("role", "user")?;

        struct Blocks<'a> {
            blocks: &'a [&'a ChatBlock],
        }
        impl<'a> Serialize for Blocks<'a> {
            fn serialize<S>(&self, s: S) -> std::result::Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let mut seq = s.serialize_seq(Some(self.blocks.len()))?;
                for block in self.blocks {
                    if let ChatBlock::ToolResult { id, result } = block {
                        // Anthropic's `tool_result.content` is a string
                        // (or a list of content blocks). Render the JSON
                        // result to a string so structured payloads
                        // round-trip without the model having to rely on
                        // a nested-object content shape.
                        let payload =
                            serde_json::to_string(result).map_err(serde::ser::Error::custom)?;
                        seq.serialize_element(&json!({
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": payload,
                        }))?;
                    }
                }
                seq.end()
            }
        }
        m.serialize_entry(
            "content",
            &Blocks {
                blocks: self.blocks,
            },
        )?;
        m.end()
    }
}

// ─── SSE event accumulator ────────────────────────────────────────────────────

/// Accumulator state that consumes Anthropic SSE event payloads in order and
/// emits one or more [`ChatChunk`]s per event.
///
/// Anthropic streams interleave a `content_block_start` / N×
/// `content_block_delta` / `content_block_stop` triple per content block,
/// and the block's `index` is the only field that ties deltas back to their
/// starting block. The accumulator tracks the active tool-use block by its
/// index; when the matching `content_block_stop` arrives it parses the
/// accumulated `partial_json` chunks into a structured `args` value and
/// emits the [`ChatChunk::ToolCall`] — preserving the Anthropic-assigned
/// `toolu_…` id captured at `content_block_start` so callers can
/// reference it as `tool_use_id` on the follow-up `tool_result`.
#[derive(Default)]
pub struct AnthropicEventAccumulator {
    /// Block index → in-progress tool_use block.
    active_tool_use: Option<ToolUseInProgress>,
    /// Stop reason captured from a prior `message_delta`. Read on
    /// `message_stop` and defaulted to `"end_turn"` if absent.
    pending_stop_reason: Option<String>,
}

struct ToolUseInProgress {
    index: u64,
    /// Anthropic-assigned `toolu_…` id from `content_block_start`.
    /// Surfaced on the emitted [`ChatChunk::ToolCall`] so the orchestrator
    /// can round-trip it back as `tool_use_id` on the matching
    /// `tool_result`.
    id: String,
    name: String,
    /// Accumulated `partial_json` strings from `input_json_delta` events.
    args_buf: String,
}

impl AnthropicEventAccumulator {
    /// Consume one SSE event and return any `ChatChunk`s it produces.
    pub fn consume(&mut self, event: &crate::sse::SseEvent) -> Vec<ChatChunk> {
        match event.event.as_str() {
            "content_block_start" => self.handle_content_block_start(&event.data),
            "content_block_delta" => self.handle_content_block_delta(&event.data),
            "content_block_stop" => self.handle_content_block_stop(&event.data),
            "message_delta" => self.handle_message_delta(&event.data),
            "message_stop" => self.handle_message_stop(),
            _ => Vec::new(),
        }
    }

    fn handle_content_block_start(&mut self, data: &[u8]) -> Vec<ChatChunk> {
        let Ok(v) = serde_json::from_slice::<Value>(data) else {
            return Vec::new();
        };
        let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        let block = match v.get("content_block") {
            Some(b) => b,
            None => return Vec::new(),
        };
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if block_type == "tool_use" {
            let id = block
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let name = block
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            self.active_tool_use = Some(ToolUseInProgress {
                index,
                id,
                name,
                args_buf: String::new(),
            });
        }
        Vec::new()
    }

    fn handle_content_block_delta(&mut self, data: &[u8]) -> Vec<ChatChunk> {
        let Ok(v) = serde_json::from_slice::<Value>(data) else {
            return Vec::new();
        };
        let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        let delta = match v.get("delta") {
            Some(d) => d,
            None => return Vec::new(),
        };
        let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match delta_type {
            "text_delta" => {
                let text = delta
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![ChatChunk::TextDelta(text)]
                }
            }
            "input_json_delta" => {
                if let Some(active) = self.active_tool_use.as_mut() {
                    if active.index == index {
                        if let Some(partial) = delta.get("partial_json").and_then(|p| p.as_str()) {
                            active.args_buf.push_str(partial);
                        }
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_content_block_stop(&mut self, data: &[u8]) -> Vec<ChatChunk> {
        let Ok(v) = serde_json::from_slice::<Value>(data) else {
            return Vec::new();
        };
        let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        if let Some(active) = self.active_tool_use.as_ref() {
            if active.index == index {
                let active = self.active_tool_use.take().expect("checked above");
                let args: Value = if active.args_buf.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&active.args_buf).unwrap_or(json!({}))
                };
                return vec![ChatChunk::ToolCall {
                    id: active.id,
                    name: active.name,
                    args,
                }];
            }
        }
        Vec::new()
    }

    fn handle_message_delta(&mut self, data: &[u8]) -> Vec<ChatChunk> {
        let Ok(v) = serde_json::from_slice::<Value>(data) else {
            return Vec::new();
        };
        if let Some(reason) = v
            .get("delta")
            .and_then(|d| d.get("stop_reason"))
            .and_then(|r| r.as_str())
        {
            self.pending_stop_reason = Some(reason.to_string());
        }
        Vec::new()
    }

    fn handle_message_stop(&mut self) -> Vec<ChatChunk> {
        let reason = self
            .pending_stop_reason
            .take()
            .unwrap_or_else(|| "end_turn".to_string());
        vec![ChatChunk::Done(reason)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseEvent;
    use bytes::Bytes;
    use std::sync::Arc;

    fn parse(req: &ChatRequest, model: &str, max_tokens: u32, parallel: bool) -> Value {
        let bytes = serialize_request(req, model, max_tokens, parallel).expect("serialize");
        serde_json::from_slice(&bytes).expect("re-parse")
    }

    fn user_text(text: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::User,
            content: vec![ChatBlock::Text(text.into())],
        }
    }

    #[test]
    fn serialize_simple_user_message_no_system() {
        let req = ChatRequest {
            system: None,
            messages: vec![user_text("hi")],
            parallel_tool_calls_allowed: false,
        };
        let v = parse(&req, "claude-3-5-sonnet", 4096, false);
        assert_eq!(
            v,
            json!({
                "model": "claude-3-5-sonnet",
                "max_tokens": 4096,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "hi"}]}
                ],
                "stream": true
            })
        );
        assert!(
            v.get("system").is_none(),
            "system key must be absent when None"
        );
    }

    #[test]
    fn serialize_with_system_hoists_to_top_level() {
        let req = ChatRequest {
            system: Some(Arc::from("be helpful")),
            messages: vec![user_text("hi")],
            parallel_tool_calls_allowed: false,
        };
        let v = parse(&req, "claude-3-5-sonnet", 4096, false);
        assert_eq!(v.get("system").and_then(|s| s.as_str()), Some("be helpful"));
        // System must NOT also appear inside messages.
        let messages = v.get("messages").and_then(|m| m.as_array()).unwrap();
        for msg in messages {
            assert_ne!(
                msg.get("role").and_then(|r| r.as_str()),
                Some("system"),
                "system role must not appear inside messages: {msg}"
            );
        }
    }

    #[test]
    fn serialize_tool_call_and_result() {
        let req = ChatRequest {
            system: None,
            messages: vec![
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![
                        ChatBlock::Text("calling".into()),
                        ChatBlock::ToolCall {
                            id: "toolu_01".into(),
                            name: "get_weather".into(),
                            args: json!({"city": "sf"}),
                        },
                    ],
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatBlock::ToolResult {
                        id: "toolu_01".into(),
                        result: json!({"temp": 60}),
                    }],
                },
            ],
            parallel_tool_calls_allowed: false,
        };
        let v = parse(&req, "claude-3-5-sonnet", 4096, false);
        let messages = v.get("messages").and_then(|m| m.as_array()).unwrap();
        assert_eq!(messages.len(), 2);

        // Message 0: assistant with text + tool_use.
        assert_eq!(
            messages[0],
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "calling"},
                    {"type": "tool_use", "id": "toolu_01", "name": "get_weather", "input": {"city": "sf"}}
                ]
            })
        );

        // Message 1: user with tool_result; content is the JSON-as-string per
        // Anthropic's wire convention.
        assert_eq!(
            messages[1],
            json!({
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01", "content": "{\"temp\":60}"}
                ]
            })
        );
    }

    #[test]
    fn empty_tools_omits_tool_choice_and_tools() {
        // Anthropic's Messages API rejects (HTTP 400) any request that sends a
        // `tool_choice` field without a non-empty `tools` array. Until tool
        // schemas land, both fields must be absent from the wire body.
        let req = ChatRequest {
            system: None,
            messages: vec![user_text("hi")],
            parallel_tool_calls_allowed: false,
        };
        let v = parse(&req, "claude-3-5-sonnet", 4096, false);
        assert!(v.get("tools").is_none(), "tools must be absent when empty");
        assert!(
            v.get("tool_choice").is_none(),
            "tool_choice must be absent when tools is empty",
        );
    }

    // The wire-level inversion of `parallel_tool_calls_allowed` →
    // `tool_choice.disable_parallel_tool_use` is unobservable while `tools`
    // is empty (both fields are correctly omitted; see
    // `empty_tools_omits_tool_choice_and_tools`). When tool schemas land in a
    // follow-up task, re-introduce a wire-level test that passes a non-empty
    // tools array and asserts on `disable_parallel_tool_use` for both
    // `parallel_tool_calls_allowed = true` and `false`.

    #[test]
    fn multiple_text_blocks_preserve_order() {
        let req = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ChatBlock::Text("a".into()),
                    ChatBlock::Text("b".into()),
                    ChatBlock::Text("c".into()),
                ],
            }],
            parallel_tool_calls_allowed: false,
        };
        let v = parse(&req, "claude-3-5-sonnet", 4096, false);
        let messages = v.get("messages").and_then(|m| m.as_array()).unwrap();
        let content = messages[0]
            .get("content")
            .and_then(|c| c.as_array())
            .unwrap();
        let texts: Vec<&str> = content
            .iter()
            .map(|b| b.get("text").and_then(|t| t.as_str()).unwrap())
            .collect();
        assert_eq!(texts, vec!["a", "b", "c"]);
    }

    // ── SSE event accumulator ─────────────────────────────────────────────

    fn ev(name: &str, data: &str) -> SseEvent {
        SseEvent {
            event: name.to_string(),
            data: Bytes::copy_from_slice(data.as_bytes()),
        }
    }

    #[test]
    fn text_delta_emits_text_chunk() {
        let mut acc = AnthropicEventAccumulator::default();
        let chunks = acc.consume(&ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        ));
        assert_eq!(chunks, vec![ChatChunk::TextDelta("hi".into())]);
    }

    #[test]
    fn tool_use_block_accumulates_and_emits_on_stop() {
        let mut acc = AnthropicEventAccumulator::default();
        assert!(acc
            .consume(&ev(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01","name":"get_weather","input":{}}}"#,
            ))
            .is_empty());
        assert!(acc
            .consume(&ev(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            ))
            .is_empty());
        assert!(acc
            .consume(&ev(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"sf\"}"}}"#,
            ))
            .is_empty());

        let chunks = acc.consume(&ev(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ));
        assert_eq!(
            chunks,
            vec![ChatChunk::ToolCall {
                id: "toolu_01".into(),
                name: "get_weather".into(),
                args: json!({"city": "sf"}),
            }]
        );
    }

    #[test]
    fn message_stop_emits_done_with_prior_stop_reason() {
        let mut acc = AnthropicEventAccumulator::default();
        assert!(acc
            .consume(&ev(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null}}"#,
            ))
            .is_empty());
        let chunks = acc.consume(&ev("message_stop", r#"{"type":"message_stop"}"#));
        assert_eq!(chunks, vec![ChatChunk::Done("tool_use".into())]);
    }

    #[test]
    fn message_stop_without_prior_delta_defaults_to_end_turn() {
        let mut acc = AnthropicEventAccumulator::default();
        let chunks = acc.consume(&ev("message_stop", r#"{"type":"message_stop"}"#));
        assert_eq!(chunks, vec![ChatChunk::Done("end_turn".into())]);
    }

    #[test]
    fn unknown_event_names_are_ignored() {
        let mut acc = AnthropicEventAccumulator::default();
        assert!(acc.consume(&ev("ping", "{}")).is_empty());
        assert!(acc.consume(&ev("message_start", "{}")).is_empty());
        assert!(acc
            .consume(&ev(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ))
            .is_empty());
    }
}

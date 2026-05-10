//! OpenAI Chat Completions API translation: `ChatRequest` → request body JSON,
//! and SSE event payloads → [`ChatChunk`]s.
//!
//! ## Wire format produced
//!
//! ```json
//! {
//!   "model": "<model>",
//!   "stream": true,
//!   "max_tokens": <opt>,                // omitted when None
//!   "messages": [
//!     {"role": "system", "content": "<sys>"},               // FIRST when present
//!     {"role": "user", "content": [{"type": "text", "text": "..."}]},
//!     {"role": "assistant", "content": null,
//!      "tool_calls": [{"id": "...", "type": "function",
//!                      "function": {"name": "...", "arguments": "<json-string>"}}]},
//!     {"role": "tool", "tool_call_id": "...", "content": "<result-as-json-string>"}
//!   ]
//! }
//! ```
//!
//! Notable shape decisions:
//!
//! - Unlike Anthropic, OpenAI carries the system prompt as the FIRST message
//!   with `role: "system"`, not as a sibling field on the request body.
//! - `tool_calls.function.arguments` is a **string** (serialized JSON), not a
//!   nested JSON object — that's what OpenAI emits on the wire and what it
//!   expects back.
//! - Tool results use a dedicated `role: "tool"` message with a top-level
//!   `tool_call_id` field; the result content is also rendered as a string.
//! - `tools` is emitted as an empty array — F-584 only plumbs tool **calls**
//!   and **tool results** within messages; tool-schema definitions are out of
//!   scope and will be added by a later task. OpenAI rejects (HTTP 400) any
//!   request that sends `tool_choice` without a non-empty `tools` array, so
//!   both fields stay omitted from the wire body.
//! - `max_tokens` is optional for OpenAI; pass `None` to omit it.
//!
//! ## SSE event consumption
//!
//! [`OpenAiEventAccumulator`] consumes OpenAI's anonymous-event SSE stream
//! (every event is a `chat.completion.chunk` JSON object with no `event:`
//! field) plus the literal `[DONE]` sentinel. It emits [`ChatChunk`]s:
//!
//! - `choices[0].delta.content` (non-empty string) → [`ChatChunk::TextDelta`].
//! - `choices[0].delta.tool_calls[i]`: track each call by its `index`; record
//!   `id` and `function.name` from the first delta and concatenate
//!   `function.arguments` from subsequent deltas. The full tool-call set is
//!   emitted on `[DONE]` (after `finish_reason: "tool_calls"`).
//! - `choices[0].finish_reason`: cached and emitted as the
//!   [`ChatChunk::Done`] payload when the `[DONE]` sentinel arrives.

use crate::{ChatBlock, ChatChunk, ChatMessage, ChatRequest, ChatRole};
use serde::Serialize;
use serde_json::{json, Value};

/// Serialize a [`ChatRequest`] into an OpenAI Chat Completions API request body.
///
/// `max_tokens` is optional for OpenAI; `None` omits the field on the wire.
///
/// F-599: when `req.parallel_tool_calls_allowed` is `false` AND `tools` is
/// non-empty, the body emits `parallel_tool_calls: false` to disable
/// parallel tool calls. OpenAI's default behavior is parallel-enabled,
/// so the `true` case omits the field and inherits that default. The
/// flag is also omitted when `tools` is empty — OpenAI rejects the
/// field without a tools array, mirroring the existing `tool_choice`
/// gating.
pub fn serialize_request(
    req: &ChatRequest,
    model: &str,
    max_tokens: Option<u32>,
) -> std::result::Result<Vec<u8>, serde_json::Error> {
    // OpenAI's Chat Completions API rejects (HTTP 400) any request that sends
    // a `tool_choice` field without a non-empty `tools` array. Until tool
    // schemas land, both fields stay omitted from the wire body.
    let tools: &[Value] = &[];
    let tool_choice: Option<&str> = if tools.is_empty() { None } else { Some("auto") };
    // F-599: only meaningful (and only accepted by the API) when `tools`
    // is non-empty. With tools empty the inversion is a no-op and the
    // field stays absent.
    let parallel_tool_calls = if !tools.is_empty() && !req.parallel_tool_calls_allowed {
        Some(false)
    } else {
        None
    };
    let body = OpenAiBody {
        model,
        stream: true,
        max_tokens,
        messages: OpenAiMessages {
            system: req.system.as_deref(),
            messages: &req.messages,
        },
        tools,
        tool_choice,
        parallel_tool_calls,
    };
    serde_json::to_vec(&body)
}

#[derive(Serialize)]
struct OpenAiBody<'a> {
    model: &'a str,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    messages: OpenAiMessages<'a>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
}

struct OpenAiMessages<'a> {
    system: Option<&'a str>,
    messages: &'a [ChatMessage],
}

impl<'a> Serialize for OpenAiMessages<'a> {
    fn serialize<S>(&self, s: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeSeq;

        // Capacity hint: optional system + worst case per ChatMessage where a
        // single ChatMessage that mixes text/tool_call blocks with tool_result
        // blocks may yield multiple wire messages (tool_results map to one
        // `role: "tool"` message each). Over-counting is harmless.
        let cap = self.system.is_some() as usize
            + self
                .messages
                .iter()
                .map(|m| m.content.len() + 1)
                .sum::<usize>();
        let mut seq = s.serialize_seq(Some(cap))?;

        if let Some(sys) = self.system {
            seq.serialize_element(&json!({"role": "system", "content": sys}))?;
        }

        for msg in self.messages {
            // Split blocks: tool_results become individual `role: "tool"`
            // messages (OpenAI carries them outside the originating role);
            // remaining text/tool_call blocks form one assistant/user message.
            let mut tool_results: Vec<&ChatBlock> = Vec::new();
            let mut other_blocks: Vec<&ChatBlock> = Vec::new();
            for block in &msg.content {
                match block {
                    ChatBlock::ToolResult { .. } => tool_results.push(block),
                    _ => other_blocks.push(block),
                }
            }

            if !other_blocks.is_empty() {
                seq.serialize_element(&OpenAiAssistantOrUserMessage {
                    role: openai_role(&msg.role),
                    blocks: &other_blocks,
                })?;
            }
            for block in tool_results {
                if let ChatBlock::ToolResult { id, result } = block {
                    let payload =
                        serde_json::to_string(result).map_err(serde::ser::Error::custom)?;
                    seq.serialize_element(&json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": payload,
                    }))?;
                }
            }
        }
        seq.end()
    }
}

fn openai_role(role: &ChatRole) -> &'static str {
    match role {
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
    }
}

struct OpenAiAssistantOrUserMessage<'a> {
    role: &'a str,
    blocks: &'a [&'a ChatBlock],
}

impl<'a> Serialize for OpenAiAssistantOrUserMessage<'a> {
    fn serialize<S>(&self, s: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::{SerializeMap, SerializeSeq};

        // Partition into text vs tool_call. OpenAI's assistant message carries
        // tool calls in a sibling `tool_calls` array, not inside `content`.
        let mut text_blocks: Vec<&ChatBlock> = Vec::new();
        let mut tool_calls: Vec<&ChatBlock> = Vec::new();
        for b in self.blocks {
            match b {
                ChatBlock::Text(_) => text_blocks.push(b),
                ChatBlock::ToolCall { .. } => tool_calls.push(b),
                ChatBlock::ToolResult { .. } => { /* filtered upstream */ }
            }
        }

        // 3 entries when both content and tool_calls are present, otherwise 2.
        let entries = 1 // role
            + 1 // content (always present, may be null for assistant-with-tool-calls-only)
            + (!tool_calls.is_empty() && self.role == "assistant") as usize;
        let mut m = s.serialize_map(Some(entries))?;
        m.serialize_entry("role", self.role)?;

        // OpenAI accepts assistant `content: null` when only tool_calls are
        // present; otherwise we emit the structured content array. User
        // messages always carry a content array.
        if text_blocks.is_empty() && !tool_calls.is_empty() && self.role == "assistant" {
            m.serialize_entry("content", &Value::Null)?;
        } else {
            struct TextBlocks<'a> {
                blocks: &'a [&'a ChatBlock],
            }
            impl<'a> Serialize for TextBlocks<'a> {
                fn serialize<S>(&self, s: S) -> std::result::Result<S::Ok, S::Error>
                where
                    S: serde::Serializer,
                {
                    let mut seq = s.serialize_seq(Some(self.blocks.len()))?;
                    for b in self.blocks {
                        if let ChatBlock::Text(t) = b {
                            seq.serialize_element(&json!({"type": "text", "text": t}))?;
                        }
                    }
                    seq.end()
                }
            }
            m.serialize_entry(
                "content",
                &TextBlocks {
                    blocks: &text_blocks,
                },
            )?;
        }

        if !tool_calls.is_empty() && self.role == "assistant" {
            struct ToolCalls<'a> {
                blocks: &'a [&'a ChatBlock],
            }
            impl<'a> Serialize for ToolCalls<'a> {
                fn serialize<S>(&self, s: S) -> std::result::Result<S::Ok, S::Error>
                where
                    S: serde::Serializer,
                {
                    let mut seq = s.serialize_seq(Some(self.blocks.len()))?;
                    for b in self.blocks {
                        if let ChatBlock::ToolCall { id, name, args } = b {
                            // OpenAI carries the arguments as a JSON-encoded
                            // string, not a nested object — round-trip through
                            // serde_json::to_string to match the wire shape.
                            let args_str =
                                serde_json::to_string(args).map_err(serde::ser::Error::custom)?;
                            seq.serialize_element(&json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": args_str,
                                }
                            }))?;
                        }
                    }
                    seq.end()
                }
            }
            m.serialize_entry(
                "tool_calls",
                &ToolCalls {
                    blocks: &tool_calls,
                },
            )?;
        }

        m.end()
    }
}

// ─── SSE event accumulator ────────────────────────────────────────────────────

/// Accumulator that consumes OpenAI's `chat.completion.chunk` SSE event
/// payloads in order and emits one or more [`ChatChunk`]s per event.
///
/// OpenAI streams text and tool calls as `delta` patches against an evolving
/// assistant message. Multiple tool calls per turn are distinguished by their
/// `index` field (NOT by `id` — `id` is sent only on the first delta for each
/// index, and subsequent deltas may carry partial `arguments` only). The
/// accumulator tracks one in-progress tool call per index and emits each
/// completed call on the `[DONE]` sentinel.
///
/// Termination protocol:
///
/// 1. The last real chunk before `[DONE]` carries `finish_reason` (one of
///    `"stop"`, `"tool_calls"`, `"length"`, etc.); the accumulator caches it.
/// 2. The literal `data: [DONE]` line arrives as an event whose `data`
///    payload is exactly `b"[DONE]"`.
/// 3. On `[DONE]`, the accumulator first emits any tracked tool calls in
///    `index` order — preserving the OpenAI-assigned `call_…` id captured
///    on the first delta for each `index` — then emits [`ChatChunk::Done`]
///    with the cached reason (defaulting to `"stop"` if none was seen).
///    Callers reference that id as `tool_call_id` on the follow-up
///    `role: "tool"` message.
#[derive(Default)]
pub struct OpenAiEventAccumulator {
    /// In-progress tool calls keyed by their delta `index`.
    tool_calls: Vec<ToolCallInProgress>,
    /// Most recent non-null `finish_reason` from a `choices[0]` payload.
    pending_stop_reason: Option<String>,
}

struct ToolCallInProgress {
    index: u64,
    /// OpenAI-assigned `call_…` id from the first delta for this index.
    /// Surfaced on the emitted [`ChatChunk::ToolCall`] so the orchestrator
    /// can round-trip it back as `tool_call_id` on the matching
    /// `role: "tool"` message.
    id: String,
    name: String,
    args_buf: String,
}

impl OpenAiEventAccumulator {
    /// Consume one SSE event and return any `ChatChunk`s it produces.
    pub fn consume(&mut self, event: &crate::sse::SseEvent) -> Vec<ChatChunk> {
        // OpenAI emits no `event:` field; dispatch on the data payload.
        let data = event.data.as_ref();

        // The `[DONE]` sentinel terminates the stream. It is NOT JSON.
        if is_done_sentinel(data) {
            return self.handle_done();
        }

        let Ok(v) = serde_json::from_slice::<Value>(data) else {
            return Vec::new();
        };

        // Standard chat.completion.chunk shape: {choices: [{delta, finish_reason}]}.
        let Some(choice) = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
        else {
            return Vec::new();
        };

        let mut out = Vec::new();

        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    out.push(ChatChunk::TextDelta(text.to_string()));
                }
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tool_calls {
                    self.merge_tool_call_delta(tc);
                }
            }
        }

        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            self.pending_stop_reason = Some(reason.to_string());
        }

        out
    }

    fn merge_tool_call_delta(&mut self, delta: &Value) {
        let Some(index) = delta.get("index").and_then(|i| i.as_u64()) else {
            return;
        };
        let entry = match self.tool_calls.iter_mut().find(|t| t.index == index) {
            Some(e) => e,
            None => {
                self.tool_calls.push(ToolCallInProgress {
                    index,
                    id: String::new(),
                    name: String::new(),
                    args_buf: String::new(),
                });
                self.tool_calls.last_mut().expect("just pushed")
            }
        };

        // First delta for an index carries the `call_…` id; later deltas
        // typically omit it. Only overwrite when non-empty so a stray
        // `"id": ""` does not clobber.
        if let Some(id) = delta.get("id").and_then(|i| i.as_str()) {
            if !id.is_empty() {
                entry.id = id.to_string();
            }
        }

        if let Some(name) = delta
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
        {
            // First delta carries name; later deltas typically omit it. Only
            // overwrite when non-empty so a stray `"name": ""` does not clobber.
            if !name.is_empty() {
                entry.name = name.to_string();
            }
        }
        if let Some(args) = delta
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|a| a.as_str())
        {
            entry.args_buf.push_str(args);
        }
    }

    fn handle_done(&mut self) -> Vec<ChatChunk> {
        let mut out = Vec::new();
        // Emit tool calls in ascending index order (the order is already the
        // insertion order, but stable-sort by index for safety).
        self.tool_calls.sort_by_key(|t| t.index);
        for tc in std::mem::take(&mut self.tool_calls) {
            let args: Value = if tc.args_buf.is_empty() {
                json!({})
            } else {
                serde_json::from_str(&tc.args_buf).unwrap_or(json!({}))
            };
            out.push(ChatChunk::ToolCall {
                id: tc.id,
                name: tc.name,
                args,
            });
        }
        let reason = self
            .pending_stop_reason
            .take()
            .unwrap_or_else(|| "stop".to_string());
        out.push(ChatChunk::Done(reason));
        out
    }
}

fn is_done_sentinel(data: &[u8]) -> bool {
    // Per OpenAI's protocol the sentinel is the literal token `[DONE]` on a
    // `data:` line; the SSE adapter strips the `data: ` prefix already.
    let trimmed = data.trim_ascii();
    trimmed == b"[DONE]"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseEvent;
    use bytes::Bytes;
    use std::sync::Arc;

    fn parse(req: &ChatRequest, model: &str, max_tokens: Option<u32>) -> Value {
        let bytes = serialize_request(req, model, max_tokens).expect("serialize");
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
        let v = parse(&req, "gpt-4o", None);
        assert_eq!(
            v,
            json!({
                "model": "gpt-4o",
                "stream": true,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "hi"}]}
                ]
            })
        );
        assert!(v.get("max_tokens").is_none(), "max_tokens absent when None");
        assert!(v.get("tools").is_none(), "tools absent when empty");
        assert!(
            v.get("tool_choice").is_none(),
            "tool_choice absent when tools empty"
        );
    }

    #[test]
    fn serialize_with_system_renders_as_first_message() {
        let req = ChatRequest {
            system: Some(Arc::from("be helpful")),
            messages: vec![user_text("hi")],
            parallel_tool_calls_allowed: false,
        };
        let v = parse(&req, "gpt-4o", Some(2048));
        assert_eq!(v.get("max_tokens").and_then(|m| m.as_u64()), Some(2048));
        // System must NOT be a top-level field.
        assert!(
            v.get("system").is_none(),
            "system must not be a top-level field on OpenAI"
        );
        let messages = v.get("messages").and_then(|m| m.as_array()).unwrap();
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": "be helpful"}),
            "system must be the FIRST message"
        );
        assert_eq!(
            messages[1].get("role").and_then(|r| r.as_str()),
            Some("user")
        );
    }

    #[test]
    fn serialize_assistant_tool_call_and_user_tool_result() {
        let req = ChatRequest {
            system: None,
            messages: vec![
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![
                        ChatBlock::Text("calling".into()),
                        ChatBlock::ToolCall {
                            id: "call_01".into(),
                            name: "get_weather".into(),
                            args: json!({"city": "sf"}),
                        },
                    ],
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatBlock::ToolResult {
                        id: "call_01".into(),
                        result: json!({"temp": 60}),
                    }],
                },
            ],
            parallel_tool_calls_allowed: false,
        };
        let v = parse(&req, "gpt-4o", None);
        let messages = v.get("messages").and_then(|m| m.as_array()).unwrap();
        assert_eq!(messages.len(), 2);

        // Message 0: assistant with text content + sibling tool_calls array.
        assert_eq!(
            messages[0],
            json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "calling"}],
                "tool_calls": [{
                    "id": "call_01",
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "arguments": "{\"city\":\"sf\"}"
                    }
                }]
            })
        );

        // Message 1: dedicated `role: "tool"` carrying the result-as-string.
        assert_eq!(
            messages[1],
            json!({
                "role": "tool",
                "tool_call_id": "call_01",
                "content": "{\"temp\":60}"
            })
        );
    }

    #[test]
    fn serialize_assistant_with_only_tool_call_emits_null_content() {
        let req = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ChatBlock::ToolCall {
                    id: "call_01".into(),
                    name: "get_weather".into(),
                    args: json!({"city": "sf"}),
                }],
            }],
            parallel_tool_calls_allowed: false,
        };
        let v = parse(&req, "gpt-4o", None);
        let messages = v.get("messages").and_then(|m| m.as_array()).unwrap();
        assert_eq!(messages[0].get("content"), Some(&Value::Null));
        assert!(messages[0].get("tool_calls").is_some());
    }

    #[test]
    fn empty_tools_omits_tool_choice_and_tools() {
        // OpenAI rejects (HTTP 400) any request that sends `tool_choice`
        // without a non-empty `tools` array. Until tool schemas land, both
        // fields must be absent from the wire body.
        let req = ChatRequest {
            system: None,
            messages: vec![user_text("hi")],
            parallel_tool_calls_allowed: false,
        };
        let v = parse(&req, "gpt-4o", None);
        assert!(v.get("tools").is_none(), "tools must be absent when empty");
        assert!(
            v.get("tool_choice").is_none(),
            "tool_choice must be absent when tools is empty",
        );
    }

    /// F-599: while `tools` is empty (until tool schemas land), the
    /// `parallel_tool_calls` field must be absent from the wire body
    /// regardless of the `parallel_tool_calls_allowed` flag. OpenAI
    /// rejects the field without a tools array, so this gate has the
    /// same shape as the `tool_choice` gate above. Once non-empty
    /// `tools` lands, the inversion (`allowed=false` → `false` on the
    /// wire; `allowed=true` → field omitted, inheriting the default)
    /// becomes observable; the inline branch in `serialize_request`
    /// already encodes that policy.
    #[test]
    fn empty_tools_omits_parallel_tool_calls_field_regardless_of_flag() {
        for flag in [true, false] {
            let req = ChatRequest {
                system: None,
                messages: vec![user_text("hi")],
                parallel_tool_calls_allowed: flag,
            };
            let v = parse(&req, "gpt-4o", None);
            assert!(
                v.get("parallel_tool_calls").is_none(),
                "parallel_tool_calls must be absent when tools is empty (flag={flag})",
            );
        }
    }

    #[test]
    fn stream_flag_is_always_true() {
        let req = ChatRequest::default();
        let v = parse(&req, "gpt-4o", None);
        assert_eq!(v.get("stream").and_then(|s| s.as_bool()), Some(true));
    }

    // ── SSE event accumulator ─────────────────────────────────────────────

    fn ev(data: &str) -> SseEvent {
        // OpenAI events have no `event:` name; the SSE adapter exposes them
        // with `event: ""`.
        SseEvent {
            event: String::new(),
            data: Bytes::copy_from_slice(data.as_bytes()),
        }
    }

    #[test]
    fn text_delta_emits_text_chunk() {
        let mut acc = OpenAiEventAccumulator::default();
        let chunks = acc.consume(&ev(
            r#"{"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#,
        ));
        assert_eq!(chunks, vec![ChatChunk::TextDelta("hello".into())]);
    }

    #[test]
    fn empty_content_delta_emits_nothing() {
        let mut acc = OpenAiEventAccumulator::default();
        // The first OpenAI delta typically carries `role: "assistant"` and
        // `content: ""` — no chunk should be emitted.
        let chunks = acc.consume(&ev(
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
        ));
        assert!(
            chunks.is_empty(),
            "empty content must not emit, got {chunks:?}"
        );
    }

    #[test]
    fn tool_call_assembles_across_multiple_deltas_and_emits_on_done() {
        let mut acc = OpenAiEventAccumulator::default();
        // First delta: id + name, empty arguments.
        assert!(acc
            .consume(&ev(
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#,
            ))
            .is_empty());
        // Second delta: only partial arguments (no id, no name).
        assert!(acc
            .consume(&ev(
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]},"finish_reason":null}]}"#,
            ))
            .is_empty());
        // Third delta: more partial arguments.
        assert!(acc
            .consume(&ev(
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"sf\"}"}}]},"finish_reason":null}]}"#,
            ))
            .is_empty());
        // Finish_reason chunk (no further deltas).
        assert!(acc
            .consume(&ev(
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ))
            .is_empty());

        // [DONE] sentinel: emit the tool call (carrying the `call_…` id
        // captured on the first delta), then Done.
        let chunks = acc.consume(&ev("[DONE]"));
        assert_eq!(
            chunks,
            vec![
                ChatChunk::ToolCall {
                    id: "call_abc".into(),
                    name: "get_weather".into(),
                    args: json!({"city": "sf"}),
                },
                ChatChunk::Done("tool_calls".into()),
            ]
        );
    }

    #[test]
    fn done_without_prior_finish_reason_defaults_to_stop() {
        let mut acc = OpenAiEventAccumulator::default();
        let chunks = acc.consume(&ev("[DONE]"));
        assert_eq!(chunks, vec![ChatChunk::Done("stop".into())]);
    }

    #[test]
    fn done_carries_prior_finish_reason() {
        let mut acc = OpenAiEventAccumulator::default();
        assert!(
            acc.consume(&ev(
                r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
            ))
            .len()
                == 1
        );
        assert!(acc
            .consume(&ev(
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#,
            ))
            .is_empty());
        let chunks = acc.consume(&ev("[DONE]"));
        assert_eq!(chunks, vec![ChatChunk::Done("length".into())]);
    }

    #[test]
    fn parallel_tool_calls_emit_in_index_order() {
        let mut acc = OpenAiEventAccumulator::default();
        // Two tool calls interleaved: index 1 starts before index 0 finishes.
        assert!(acc
            .consume(&ev(
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"b","arguments":"{}"}}]},"finish_reason":null}]}"#,
            ))
            .is_empty());
        assert!(acc
            .consume(&ev(
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"a","arguments":"{}"}}]},"finish_reason":null}]}"#,
            ))
            .is_empty());
        assert!(acc
            .consume(&ev(
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ))
            .is_empty());

        let chunks = acc.consume(&ev("[DONE]"));
        assert_eq!(
            chunks,
            vec![
                ChatChunk::ToolCall {
                    id: "call_a".into(),
                    name: "a".into(),
                    args: json!({}),
                },
                ChatChunk::ToolCall {
                    id: "call_b".into(),
                    name: "b".into(),
                    args: json!({}),
                },
                ChatChunk::Done("tool_calls".into()),
            ]
        );
    }

    #[test]
    fn malformed_json_chunks_are_ignored() {
        let mut acc = OpenAiEventAccumulator::default();
        assert!(acc.consume(&ev("not json at all")).is_empty());
        assert!(acc.consume(&ev(r#"{"choices":[]}"#)).is_empty());
    }

    #[test]
    fn done_sentinel_tolerates_surrounding_whitespace() {
        let mut acc = OpenAiEventAccumulator::default();
        let chunks = acc.consume(&ev("  [DONE]  "));
        assert_eq!(chunks, vec![ChatChunk::Done("stop".into())]);
    }
}

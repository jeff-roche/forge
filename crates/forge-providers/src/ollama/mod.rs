//! Ollama provider — keyless, NDJSON-streamed `/api/chat`.
//!
//! Ollama is the local-first provider: the user runs `ollama serve` on
//! their machine and the daemon talks to it over plain HTTP. There is no
//! credential to inject; the `--provider` spec carries the model name and
//! optional base URL only.
//!
//! Wire format: the response body is NDJSON — one JSON object per line —
//! with each non-terminal line carrying a `message.content` fragment and
//! optional `message.tool_calls`, and the terminal line carrying
//! `"done": true` plus a `done_reason`. Reference:
//! <https://github.com/ollama/ollama/blob/main/docs/api.md#generate-a-chat-completion>.

use std::io;

use crate::http_util::{self, HttpClientConfig};
use crate::{
    ChatBlock, ChatChunk, ChatMessage, ChatRequest, ChatRole, Provider, ProviderAuth,
    StreamErrorKind,
};
use forge_core::Result;
use futures::stream::{self, BoxStream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;

pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

#[derive(Clone)]
pub struct OllamaProvider {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl OllamaProvider {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            client: http_util::build_stream_client(
                &HttpClientConfig::DEFAULT,
                reqwest::redirect::Policy::none(),
            ),
        }
    }

    pub fn request_url(&self) -> String {
        format!("{}/api/chat", self.base_url.trim_end_matches('/'))
    }
}

impl Provider for OllamaProvider {
    /// F-744: explicit no-op override. Ollama is keyless, so the
    /// per-turn credential from the orchestrator is dropped here without
    /// reaching the network. The default trait impl would do the same
    /// thing, but the explicit override documents the intent and pins
    /// the contract so a future refactor can't silently start
    /// authenticating against a local daemon.
    fn chat_with_auth(
        &self,
        req: ChatRequest,
        _auth: ProviderAuth,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
        self.chat(req)
    }

    #[tracing::instrument(
        name = "provider.chat",
        skip_all,
        fields(provider = "ollama", model = %self.model),
    )]
    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send {
        let url = self.request_url();
        let client = self.client.clone();
        let body = serialize_request(&req, &self.model);

        async move {
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("ollama chat request failed: {e}"))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body_text = resp.text().await.unwrap_or_default();
                let preview = http_util::truncate(&body_text, 256);
                return Ok(Box::pin(stream::once(async move {
                    ChatChunk::Error {
                        kind: StreamErrorKind::Transport,
                        message: format!("ollama HTTP {status}: {preview}"),
                    }
                })) as BoxStream<'static, ChatChunk>);
            }

            let byte_stream = resp.bytes_stream().map_err(io::Error::other);
            let reader = StreamReader::new(byte_stream);
            let lines = FramedRead::new(reader, LinesCodec::new());

            let chunks = lines.flat_map(|line_result| {
                let chunks = match line_result {
                    Ok(line) if line.trim().is_empty() => Vec::new(),
                    Ok(line) => match parse_ollama_line(&line) {
                        Ok(cs) => cs,
                        Err(e) => vec![ChatChunk::Error {
                            kind: StreamErrorKind::Transport,
                            message: format!("ollama NDJSON parse error: {e}"),
                        }],
                    },
                    Err(e) => vec![ChatChunk::Error {
                        kind: StreamErrorKind::Transport,
                        message: format!("ollama stream read error: {e}"),
                    }],
                };
                stream::iter(chunks)
            });

            Ok(Box::pin(chunks) as BoxStream<'static, ChatChunk>)
        }
    }
}

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    stream: bool,
    messages: Vec<OllamaMessage>,
}

#[derive(Debug, Serialize)]
struct OllamaMessage {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCallOut>>,
}

/// Outbound assistant tool-call. Mirrors the wire shape Ollama emits in
/// streaming responses (`message.tool_calls[].function.{name,arguments}`)
/// so that multi-turn transcripts preserve structured tool-call identity
/// instead of degrading to a plaintext marker the model can't parse.
#[derive(Debug, Serialize)]
struct OllamaToolCallOut {
    function: OllamaToolFunctionOut,
}

#[derive(Debug, Serialize)]
struct OllamaToolFunctionOut {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OllamaLine {
    message: Option<OllamaResponseMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaResponseMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCall>,
}

#[derive(Debug, Deserialize)]
struct OllamaToolCall {
    function: OllamaToolFunction,
}

#[derive(Debug, Deserialize)]
struct OllamaToolFunction {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

fn serialize_request<'a>(req: &ChatRequest, model: &'a str) -> OllamaRequest<'a> {
    let mut messages = Vec::new();
    if let Some(sys) = req.system.as_deref() {
        messages.push(OllamaMessage {
            role: "system",
            content: sys.to_string(),
            tool_calls: None,
        });
    }
    for msg in &req.messages {
        messages.extend(translate_message(msg));
    }
    OllamaRequest {
        model,
        stream: true,
        messages,
    }
}

/// Translate an in-memory `ChatMessage` into one or more `OllamaMessage`s
/// matching Ollama's `/api/chat` wire shape.
///
/// Assistant turns containing `ChatBlock::ToolCall` blocks emit a
/// structured `tool_calls` array on the assistant message (mirroring the
/// shape Ollama emits in responses) so multi-turn tool use round-trips
/// correctly. User turns don't carry tool-call blocks in practice; if one
/// appears it's silently dropped — the model never authors them on a User
/// role and surfacing it as text would mislead the model.
fn translate_message(msg: &ChatMessage) -> Vec<OllamaMessage> {
    let role = match msg.role {
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
    };

    let mut out = Vec::new();
    let mut text = String::new();
    let mut tool_calls: Vec<OllamaToolCallOut> = Vec::new();

    for block in &msg.content {
        match block {
            ChatBlock::Text(t) => text.push_str(t),
            ChatBlock::ToolCall { name, args, .. } => {
                if matches!(msg.role, ChatRole::Assistant) {
                    tool_calls.push(OllamaToolCallOut {
                        function: OllamaToolFunctionOut {
                            name: name.clone(),
                            arguments: args.clone(),
                        },
                    });
                }
                // For User-role messages, ToolCall blocks are nonsensical
                // (the model never authors them) — drop them so we don't
                // mislead the model with a plaintext marker.
            }
            ChatBlock::ToolResult { result, .. } => {
                // Flush any accumulated text or tool_calls before emitting
                // the tool message so ordering is preserved.
                if !text.is_empty() || !tool_calls.is_empty() {
                    out.push(OllamaMessage {
                        role,
                        content: std::mem::take(&mut text),
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(std::mem::take(&mut tool_calls))
                        },
                    });
                }
                out.push(OllamaMessage {
                    role: "tool",
                    content: result.to_string(),
                    tool_calls: None,
                });
            }
        }
    }

    if !text.is_empty() || !tool_calls.is_empty() || out.is_empty() {
        out.push(OllamaMessage {
            role,
            content: text,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
        });
    }

    out
}

fn parse_ollama_line(line: &str) -> serde_json::Result<Vec<ChatChunk>> {
    let parsed: OllamaLine = serde_json::from_str(line)?;
    let mut chunks = Vec::new();

    if let Some(msg) = parsed.message {
        if !msg.content.is_empty() {
            chunks.push(ChatChunk::TextDelta(msg.content));
        }
        for tc in msg.tool_calls {
            chunks.push(ChatChunk::ToolCall {
                // Ollama does not assign tool-call ids on the wire. Generate
                // a stable id so the orchestrator can round-trip the tool
                // result back when the next turn is built.
                id: uuid::Uuid::new_v4().to_string(),
                name: tc.function.name,
                args: tc.function.arguments,
            });
        }
    }

    if parsed.done {
        chunks.push(ChatChunk::Done(
            parsed.done_reason.unwrap_or_else(|| "stop".to_string()),
        ));
    }

    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_line_text_only() {
        let line = r#"{"message":{"role":"assistant","content":"Hello"},"done":false}"#;
        let chunks = parse_ollama_line(line).unwrap();
        assert_eq!(chunks, vec![ChatChunk::TextDelta("Hello".into())]);
    }

    #[test]
    fn parse_line_done_with_reason() {
        let line =
            r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop"}"#;
        let chunks = parse_ollama_line(line).unwrap();
        assert_eq!(chunks, vec![ChatChunk::Done("stop".into())]);
    }

    #[test]
    fn parse_line_done_without_reason_defaults_to_stop() {
        let line = r#"{"done":true}"#;
        let chunks = parse_ollama_line(line).unwrap();
        assert_eq!(chunks, vec![ChatChunk::Done("stop".into())]);
    }

    #[test]
    fn parse_line_tool_call_generates_id() {
        let line = r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"fs.read","arguments":{"path":"a.txt"}}}]},"done":false}"#;
        let chunks = parse_ollama_line(line).unwrap();
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            ChatChunk::ToolCall { id, name, args } => {
                assert!(!id.is_empty(), "id should be auto-generated");
                assert_eq!(name, "fs.read");
                assert_eq!(args, &json!({"path": "a.txt"}));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn serialize_request_emits_system_then_messages() {
        let req = ChatRequest {
            system: Some(std::sync::Arc::from("be helpful")),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::Text("hi".into())],
            }],
            parallel_tool_calls_allowed: false,
        };
        let body = serialize_request(&req, "llama3.2");
        let json_body = serde_json::to_value(&body).unwrap();
        assert_eq!(json_body["model"], "llama3.2");
        assert_eq!(json_body["stream"], true);
        assert_eq!(json_body["messages"][0]["role"], "system");
        assert_eq!(json_body["messages"][0]["content"], "be helpful");
        assert_eq!(json_body["messages"][1]["role"], "user");
        assert_eq!(json_body["messages"][1]["content"], "hi");
    }

    #[test]
    fn serialize_request_assistant_tool_call_emits_structured_tool_calls() {
        // Regression: assistant ToolCall blocks must serialize as a
        // structured `tool_calls` array (matching Ollama's response wire
        // shape) rather than the previous plaintext `[tool_call:...]`
        // marker, which the model could not parse on the next turn.
        let req = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ChatBlock::ToolCall {
                    id: "tc-1".into(),
                    name: "fs.read".into(),
                    args: json!({"path": "hello.txt"}),
                }],
            }],
            parallel_tool_calls_allowed: false,
        };
        let body = serialize_request(&req, "llama3.2");
        let json_body = serde_json::to_value(&body).unwrap();
        let msg = &json_body["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "");
        let tcs = msg["tool_calls"].as_array().expect("tool_calls array");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["function"]["name"], "fs.read");
        assert_eq!(
            tcs[0]["function"]["arguments"],
            json!({"path": "hello.txt"})
        );
        // No plaintext leak from the old format.
        assert!(
            !msg["content"].as_str().unwrap().contains("[tool_call:"),
            "must not emit the legacy plaintext tool_call marker"
        );
    }

    #[test]
    fn serialize_request_omits_tool_calls_when_none() {
        // For plain text turns, `tool_calls` must be omitted from the
        // wire shape entirely (not serialized as `null`). Ollama treats
        // any present `tool_calls` field as authoritative.
        let req = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::Text("hi".into())],
            }],
            parallel_tool_calls_allowed: false,
        };
        let body = serialize_request(&req, "llama3.2");
        let json_body = serde_json::to_value(&body).unwrap();
        let msg = json_body["messages"][0].as_object().unwrap();
        assert!(
            !msg.contains_key("tool_calls"),
            "tool_calls must be omitted when no calls are present"
        );
    }

    #[test]
    fn serialize_request_tool_result_emits_tool_role_message() {
        let req = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::ToolResult {
                    id: "ignored".into(),
                    result: json!({"ok": true}),
                }],
            }],
            parallel_tool_calls_allowed: false,
        };
        let body = serialize_request(&req, "llama3.2");
        let json_body = serde_json::to_value(&body).unwrap();
        assert_eq!(json_body["messages"][0]["role"], "tool");
        let content = json_body["messages"][0]["content"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content).unwrap();
        assert_eq!(parsed, json!({"ok": true}));
    }
}

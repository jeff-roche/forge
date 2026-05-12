# forge-providers

The streaming chat-provider abstraction and built-in provider implementations. Defines the provider-agnostic `ChatRequest` / `ChatMessage` / `ChatBlock` shape that the rest of Forge speaks, the `Provider` trait that each backend implements, and the normalised `ChatChunk` stream variants (text deltas, tool calls, terminal `Done`, structured stream errors). Ships `AnthropicProvider`, `OpenAiProvider` (the OpenAI-compatible backend that also covers self-hosted endpoints like Ollama via the `custom_openai:<name>` preset), and a `MockProvider` used heavily in tests.

## Role in the workspace

- Depended on by: `forge-session` (drives chat turns), `forge-shell` (provider status / probe), and tests in dependent crates.
- Depends on: `forge-core`, `reqwest` (rustls TLS, streaming bodies), `tokio`, `tokio-util` (codec/io), `futures`.

## Key types / entry points

- `Provider` — the streaming chat trait every backend implements. Returns `BoxStream<'static, ChatChunk>`.
- `ChatRequest`, `ChatMessage`, `ChatRole`, `ChatBlock` — provider-agnostic conversation shape, including tool-call and tool-result blocks.
- `ChatChunk` — normalised stream chunk: `TextDelta`, `ToolCall`, `Done`, terminal `Error { kind, message }`.
- `StreamErrorKind` — terminal stream-failure classifier (`LineTooLong`, `IdleTimeout`, `WallClockTimeout`, `Transport`).
- `MockProvider` — file-backed and scripted-sequence test double; `from_responses(...)` plus `recorded_requests()` for assertions.
- `anthropic` — the live Anthropic Messages backend (SSE streaming over HTTPS).
- `openai` — the OpenAI-compatible backend (SSE streaming); also drives self-hosted endpoints (Ollama, vLLM, LM Studio) via `custom_openai:<name>` presets.

## Further reading

- [Crate architecture — `forge-providers`](../../docs/architecture/crate-architecture.md#32-forge-providers)
- [Provider abstraction](../../docs/architecture/provider-abstraction.md)

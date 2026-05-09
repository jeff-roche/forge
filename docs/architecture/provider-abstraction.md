# Provider Abstraction

> Extracted from IMPLEMENTATION.md §6 — unified chat request, provider-specific translation, streaming chunks, and tool classification

---

## 6. Provider abstraction

### 6.1 Unified chat request

Defined in `crates/forge-providers/src/lib.rs`. The shipped shape is intentionally narrow — sampling parameters (temperature, max-tokens, stop sequences) are not modelled here; providers either default them or read them from provider-specific configuration.

```rust
pub struct ChatRequest {
    pub system: Option<Arc<str>>,
    pub messages: Vec<ChatMessage>,
    pub parallel_tool_calls_allowed: bool,
}

pub struct ChatMessage {
    pub role: ChatRole,
    pub content: Vec<ChatBlock>,
}

pub enum ChatRole { User, Assistant }

pub enum ChatBlock {
    Text(String),
    ToolCall    { id: String, name: String, args: serde_json::Value },
    ToolResult  { id: String, result: serde_json::Value },
}
```

`system` is `Option<Arc<str>>` (F-566): the orchestrator hot loop calls `req.clone()` once per iteration, and the AGENTS.md prefix is large enough (often 256 KiB) that a deep copy was measurable. Construction sites wrap once with `Arc::from(s)`; readers go through `as_deref()` so call-site shapes are unchanged.

`parallel_tool_calls_allowed` is the hard gate the orchestrator sets *before* dispatch. F-583 plumbs the flag through; F-599 will turn it on when only read-only tools are declared.

System prompts live on `ChatRequest::system`, not as a `ChatRole` variant. Tool interactions ride inside an assistant- or user-authored message as `ChatBlock::ToolCall` / `ChatBlock::ToolResult` content blocks; there is no dedicated tool role.

### 6.2 Provider-specific translation
Each provider implementation has a `translate.rs` module that maps `ChatBlock` content (including tool calls/results) into the provider's native wire format, and normalizes the streaming response back into `ChatChunk` variants.

### 6.3 Tool classification

Every tool has a `read_only: bool` bit. Built-in tools (`fs.read`, `fs.list`, `fs.write`, `shell.exec`, `agent.spawn`, etc.) declare this at definition time. MCP tools inherit from the server's `readOnly` hint when advertised; otherwise default to `false` (mutating, safe).

Forge uses this to decide whether a batch of tool calls in one turn can run in parallel.

### 6.4 MCP tools

MCP-advertised tools are adapted to the session `Tool` trait by `McpTool` (`crates/forge-session/src/tools/mcp.rs`). One adapter instance is registered per advertised tool at turn-start from an `McpManager::list()` snapshot, so a mid-turn `tools/list` refresh cannot change the dispatch table under the running loop.

Conventions the adapter enforces — any MCP-specific handling elsewhere in the stack should assume these shapes:

1. **Naming (`<server>.<tool>`, split at the first dot).** The adapter's `name()` returns the fully-namespaced string that providers see on the wire and that `ToolDispatcher` keys on. `McpTool::new` splits once at the first `.` — everything after belongs to the tool, so MCP tool names may themselves contain dots (e.g. `srv.nested.tool.name` → server `srv`, tool `nested.tool.name`). A name with no dot is rejected (returns `None`) rather than panicking.

2. **Adapter location.** `crates/forge-session/src/tools/mcp.rs` — wraps a single namespaced MCP tool behind `Tool`, holds an `Arc<McpManager>`, and delegates every `invoke` to `manager.call(server, tool, args)`.

3. **`readOnlyHint` inheritance.** `read_only()` is a pass-through of the `readOnlyHint` annotation `McpManager` extracts when caching `tools/list` (see `forge_mcp::manager::parse_tools_list`). A missing annotation defaults to `false` (treated as mutating — the conservative choice for parallel-tool-call eligibility). The adapter does no re-inspection.

4. **Approval-preview format.** `approval_preview` emits `"MCP <server>.<tool>: <description>"` (or just `"MCP <server>.<tool>"` when the description is empty) so the approval UI can distinguish an MCP-sourced prompt from a built-in one at a glance. The description is deliberately terse — the approval UI renders the full args payload separately.

5. **Error envelope.** `invoke` uniformizes JSON-RPC failures into `{ "error": "mcp: <detail>" }`. Success-path payloads are passed through verbatim. The envelope shape matches built-in tool errors so the session-turn loop's `StepOutcome::Error` classification treats MCP errors identically to built-in errors downstream.

### 6.5 Provider trait

```rust
pub trait Provider: Send + Sync {
    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl std::future::Future<Output = Result<BoxStream<'static, ChatChunk>>> + Send;
}
```

The `impl Future` return makes the trait **not object-safe** — `Arc<dyn Provider>` does not compile. That constraint shapes the rest of this section: hot-swap (§6.6) cannot use trait objects and instead uses a tagged-union enum.

Streamed output is yielded as `ChatChunk` variants:

```rust
pub enum ChatChunk {
    TextDelta(String),
    ToolCall { name: String, args: serde_json::Value },
    Done(String),
    Error { kind: StreamErrorKind, message: String },
}

pub enum StreamErrorKind {
    LineTooLong,        // single NDJSON line exceeded the per-line cap
    IdleTimeout,        // no bytes within the inter-chunk idle window
    WallClockTimeout,   // overall stream exceeded its wall-clock budget
    Transport,          // transport-level error from the underlying reader
}
```

`Error` is terminal — the stream closes after yielding it, and callers must treat the current turn as aborted.

### 6.6 Hot-swap (F-586 / F-640)

The dashboard's provider picker can change the active provider mid-session without restarting the daemon. Two types implement the swap:

```rust
pub enum RuntimeProvider {
    Ollama(Arc<OllamaProvider>),
    Anthropic(Arc<AnthropicProvider>),
    OpenAi(Arc<OpenAiProvider>),
    CustomOpenAi(Arc<CustomOpenAiProvider>),
    #[cfg(any(test, feature = "testing"))]
    Mock(Arc<MockProvider>),
}

pub struct SwappableProvider {
    inner: Arc<RwLock<RuntimeProvider>>,
}

impl SwappableProvider {
    pub fn new(initial: RuntimeProvider) -> Self;
    pub fn swap(&self, next: RuntimeProvider);
    pub fn active_id(&self) -> String; // "ollama" | "anthropic" | "openai" | "custom_openai" | "mock"
}
```

**Why an enum.** `Provider::chat` returns `impl Future`, so the trait is not object-safe; `Arc<dyn Provider>` is not viable. `RuntimeProvider` is a tagged union over the four shipped concrete providers (plus a test-gated `Mock`). Each `chat()` call dispatches via a `match` arm — one arm cost, negligible against a network round-trip.

**Why each variant is `Arc<…>`.** The concrete `chat()` futures borrow `&self` per the trait signature, so a `parking_lot::RwLockReadGuard` cannot survive the network round-trip (the guard is not `Send`). `SwappableProvider::chat` snapshots by `Arc::clone`-ing the active variant under a brief read lock, drops the guard, then `await`s the cloned snapshot. The clone is a refcount bump.

**Swap semantics.** `swap()` atomically replaces `*inner.write()`. The replacement takes effect on the *next* `chat()` invocation. Any in-flight stream continues against the previous inner because it captured the boxed future *before* the swap; mid-stream replacement is out of scope.

**Wiring.** `forge-session::serve_with_session` wraps its provider in `Arc<SwappableProvider>`. The dashboard's `set_active_provider` IPC command rewrites settings via `set_setting` and emits a `provider:changed` Tauri event app-wide (`crates/forge-shell/src/providers_ipc.rs:PROVIDER_CHANGED_EVENT = "provider:changed"`). Each session window's bridge forwards the event onto the per-session UDS as an `Event::ProviderChanged { provider_id }` (`crates/forge-core/src/event.rs`), which the orchestrator's listener consumes and turns into a `SwappableProvider::swap` call. The next `run_turn` then dispatches against the new inner.

`Mock` is gated behind `#[cfg(any(test, feature = "testing"))]` so a production binary cannot construct it — without the gate any caller could `swap()` a real session onto a scripted source.

See [event-conventions.md](./event-conventions.md) for the `provider:changed` payload shape.

### 6.7 Shipped providers and auth shapes

Four production providers ship in `forge-providers`:

| Slug              | Module                                  | Auth                                                    |
|-------------------|-----------------------------------------|---------------------------------------------------------|
| `ollama`          | `crates/forge-providers/src/ollama.rs`  | None — keyless, local                                   |
| `anthropic`       | `crates/forge-providers/src/anthropic/` | `x-api-key: <key>` + `anthropic-version` header         |
| `openai`          | `crates/forge-providers/src/openai/`    | `Authorization: Bearer <key>`                           |
| `custom_openai`   | `crates/forge-providers/src/openai/custom.rs` | One of `Bearer` / `Header { name }` / `None`      |

`CustomOpenAiProvider` reuses the `openai` request translation and SSE pipeline verbatim — every byte on the wire matches what `OpenAiProvider` would send for the same `ChatRequest`. The two knobs that differ:

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum AuthShape {
    Bearer,                  // Authorization: Bearer <key>          (vanilla OpenAI shape)
    Header { name: String }, // <name>: <key>                         (proxies using e.g. X-API-Key)
    None,                    // no auth header                        (private network, public mock)
}
```

Settings TOML declares e.g. `auth = { shape = "header", name = "X-API-Key" }`. `Bearer` and `Header` require an `api_key`; `None` does not. The header *name* is validated as a legal HTTP token at construction (`reqwest::header::HeaderName::parse`), and missing-key/invalid-name combinations are rejected with named diagnostics — misconfiguration surfaces at startup, not as an opaque 401 on first use.

### 6.8 Pricing table

Cost lookup is a pure function of `(provider, model, tokens_in, tokens_out)`, defined in `crates/forge-providers/src/pricing.rs`.

The table source-of-truth is `crates/forge-providers/data/prices.toml`, **`include_str!`-embedded** at compile time:

```rust
pub const PRICES_TOML: &str = include_str!("../data/prices.toml");
```

Every binary that links `forge-providers` (daemon, shell, CLI, tests) shares the same lookup — no runtime file dependency. The table is parsed lazily on first access via `OnceLock`:

```rust
impl PriceTable {
    pub fn embedded() -> &'static Self {
        static TABLE: OnceLock<PriceTable> = OnceLock::new();
        TABLE.get_or_init(|| PriceTable::parse(PRICES_TOML).expect("…release blocker"))
    }
}
```

A unit test (`embedded_table_parses`) gates release on the in-tree TOML being well-formed, so a typo cannot ship as a runtime panic on first cost lookup.

**Lookup precedence.** Exact `(provider, model)` first; a row with `model = "*"` is the wildcard fallback. The wildcard row exists for Ollama (every locally-hosted model is free at point of use, so a single `ollama / *` row covers any checkpoint a user might pull) — exact rows beat the wildcard so a future paid-per-token Ollama variant could be priced precisely without removing the wildcard.

**Cost formula.** `tokens_in × prompt_per_million / 1_000_000 + tokens_out × completion_per_million / 1_000_000`. A missing key returns `None`, which `forge_core::usage` surfaces to the UI as `cost: null` — never `0`, so "free" and "we don't know" stay distinguishable.

**Updates are manual.** No live pricing fetch; bumping a rate is a file edit + commit.

### 6.9 Limitations

- **CustomOpenAI SSRF guard is construction-time, not per-request.** `CustomOpenAiProvider::new` calls `forge_core::url_safety::check_url(&base_url)` against the F-346 SSRF guard. HTTPS public hosts and loopback HTTP (debug builds only) are accepted; RFC-1918, link-local/IMDS, IPv6 unique-local, and non-http schemes are rejected. **Redirects are not re-validated** — a public HTTPS endpoint that 3xx-redirects to an internal host bypasses the guard. Tracked under the SEC-04..07 cluster: [F-644](https://github.com/forge-ide/forge/issues/680) (DNS rebinding), [F-645/F-646/F-647](https://github.com/forge-ide/forge/issues/681) (redirect-bypass).

- **Provider hot-swap takes effect on the next turn, not mid-stream.** A `provider:changed` event delivered while a `chat()` stream is active does not interrupt it — the in-flight future captured an `Arc` to the prior inner before the swap. The new provider takes effect on the next `run_turn`. This is intentional (matches F-586 DoD), not a bug.

- **`ChatRequest` does not carry sampling knobs.** Temperature, max-tokens, and stop-sequences are not in the unified request. Provider-specific config holds them where needed (e.g. `CustomOpenAiProvider::max_tokens`). Adding a knob no provider honours would be cargo-culting; a future feature that needs one should either extend the type behind a feature flag or wrap at the provider boundary.

- **Pricing data is hand-curated.** No upstream API is polled; rates drift until a human edits `data/prices.toml`. The release-blocker test catches malformed TOML but not stale rates.

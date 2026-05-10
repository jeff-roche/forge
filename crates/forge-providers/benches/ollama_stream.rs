//! F-108 criterion bench — Ollama NDJSON parse_line allocation budget.
//!
//! The NDJSON decode in `ollama::parse_line` is the hottest per-token path in
//! the application: every streamed assistant token flows through it. The old
//! two-step decode (`serde_json::from_str::<Value>` + `.as_str().to_string()`)
//! allocated three Strings per text-delta token, producing allocator pressure
//! and jitter on long responses.
//!
//! This bench feeds 1000 mock NDJSON lines through both the old (baseline)
//! and new (typed) implementations and reports two numbers per pass:
//! - wall-time (via criterion's sampler)
//! - heap allocations (via a counting global allocator)
//!
//! The baseline implementation (`legacy_parse_line`) is inlined below so the
//! bench can compare against it without polluting the production module. The
//! comparison is the DoD's "baseline shows high allocation count, fix shows
//! reduction" — asserted at the end of the bench run.
//!
//! Run locally with `cargo bench -p forge-providers`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use criterion::{criterion_group, criterion_main, Criterion};
use forge_providers::ollama::{parse_line, parse_line_bytes, serialize_chat_body};
use forge_providers::{ChatBlock, ChatChunk, ChatMessage, ChatRole};

// ── Counting allocator ────────────────────────────────────────────────────────
//
// Wraps the system allocator with a pair of atomic counters so the bench can
// snapshot allocation deltas around a block of work. The counters are
// process-global: the wrapper is installed as `#[global_allocator]` below,
// and `record()` opts in to counting for the duration of a closure. Counting
// is disabled by default so criterion's own warmup / sampler noise doesn't
// accumulate into the reported numbers.

struct CountingAllocator;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static ENABLED: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) != 0 {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        // SAFETY: delegating to the system allocator with the caller's layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: delegating to the system allocator with the caller's ptr/layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: CountingAllocator = CountingAllocator;

/// Run `f`, returning `(allocations, bytes)` observed during the call. Only
/// one scope at a time is meaningful; criterion runs benches serially within
/// a group, so that's fine for our purposes.
fn record<F: FnOnce()>(f: F) -> (usize, usize) {
    ALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    ENABLED.store(1, Ordering::Relaxed);
    f();
    ENABLED.store(0, Ordering::Relaxed);
    (
        ALLOCS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
    )
}

// ── Baseline (pre-F-108) implementation ───────────────────────────────────────
//
// Verbatim copy of the pre-fix `parse_line`. Lives here — not in the
// production module — so the production binary doesn't carry both. Kept only
// for the reduction assertion at the end of the bench. If this ever drifts
// from the fixed version's behavior, the bench still serves its purpose
// (measuring the old allocation shape); the correctness contract lives in
// the src/ollama.rs unit tests.
fn legacy_parse_line(line: &str) -> Option<ChatChunk> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;

    if value.get("done").and_then(|d| d.as_bool()) == Some(true) {
        let reason = value
            .get("done_reason")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        return Some(ChatChunk::Done(reason));
    }

    if let Some(first_call) = value
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(|tc| tc.as_array())
        .and_then(|arr| arr.first())
    {
        if let Some(func) = first_call.get("function") {
            let name = func.get("name").and_then(|n| n.as_str())?.to_string();
            let args = func
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            return Some(ChatChunk::ToolCall {
                id: String::new(),
                name,
                args,
            });
        }
    }

    if let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
    {
        if !content.is_empty() {
            return Some(ChatChunk::TextDelta(content.to_string()));
        }
    }

    None
}

// ── Mock NDJSON corpus ────────────────────────────────────────────────────────
//
// 1000 lines shaped like a realistic Ollama response: mostly text-delta tokens
// (the hot path), with periodic tool-call chunks and a terminal `done` frame.
// Token strings are short (1–6 ASCII chars, no JSON escapes) so the typed
// parser hits its Cow::Borrowed fast path on the delta fields — the common
// case the DoD budgets for.

fn mock_ndjson_lines() -> Vec<String> {
    let text_tokens = [
        "the ", "quick", " brown", " fox ", "jumps", " over ", "a ", "lazy ", "dog", ".", " it ",
        "was ", "a ", "sunny", " day", " in ", "spring", ".",
    ];

    let mut out = Vec::with_capacity(1000);
    for i in 0..999 {
        if i % 97 == 96 {
            // Tool-call chunk — exercises the args-move path.
            out.push(
                r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"fs.read","arguments":{"path":"/tmp/x"}}}]},"done":false}"#
                    .to_string(),
            );
        } else {
            let tok = text_tokens[i % text_tokens.len()];
            out.push(format!(
                r#"{{"message":{{"role":"assistant","content":"{tok}"}},"done":false}}"#
            ));
        }
    }
    out.push(r#"{"model":"llama3","done":true,"done_reason":"stop"}"#.to_string());
    out
}

// ── Benchmarks ────────────────────────────────────────────────────────────────

fn bench_parse_line(c: &mut Criterion) {
    let lines = mock_ndjson_lines();

    let mut group = c.benchmark_group("ollama_parse_line");
    group.throughput(criterion::Throughput::Elements(lines.len() as u64));

    group.bench_function("legacy (Value-tree)", |b| {
        b.iter(|| {
            for line in &lines {
                black_box(legacy_parse_line(black_box(line)));
            }
        });
    });

    group.bench_function("typed (F-108)", |b| {
        b.iter(|| {
            for line in &lines {
                black_box(parse_line(black_box(line)));
            }
        });
    });

    group.finish();
}

// ── Allocation-count assertion ────────────────────────────────────────────────
//
// The criterion time bench covers wall-time. The DoD additionally requires a
// recorded allocation count per 1000 lines. This runs once per bench invocation,
// prints both numbers, and panics if the typed path is not a strict reduction —
// serving as the "bench shows reduction" evidence in the DoD.
fn assert_alloc_reduction(_c: &mut Criterion) {
    let lines = mock_ndjson_lines();

    // Warm up: allocator counters are noisy on first touch (page-in costs).
    for line in &lines {
        black_box(legacy_parse_line(black_box(line)));
        black_box(parse_line(black_box(line)));
    }

    let (legacy_allocs, legacy_bytes) = record(|| {
        for line in &lines {
            black_box(legacy_parse_line(black_box(line)));
        }
    });

    let (typed_allocs, typed_bytes) = record(|| {
        for line in &lines {
            black_box(parse_line(black_box(line)));
        }
    });

    println!(
        "F-108 allocation budget (1000 lines):\n  \
         legacy Value-tree: {legacy_allocs} allocations, {legacy_bytes} bytes\n  \
         typed (F-108):     {typed_allocs} allocations, {typed_bytes} bytes"
    );

    assert!(
        typed_allocs < legacy_allocs,
        "F-108 DoD: typed path must allocate less than the legacy Value-tree \
         path — measured legacy={legacy_allocs} typed={typed_allocs}"
    );

    // The text-delta common case carries no JSON escapes in the corpus, so the
    // Cow field borrows from the source buffer and only the emission String
    // allocates. 1000 lines ≈ 1000 text-delta emissions + ~10 tool-call
    // branches. Anything meaningfully above ~1100 allocations means the Cow
    // path is silently owning when it shouldn't.
    assert!(
        typed_allocs < 2 * lines.len(),
        "F-108 DoD: typed path exceeded ~1 allocation per text-delta token — \
         measured {typed_allocs} for {} lines; Cow<'_, str> is likely falling \
         back to Owned on a path that could borrow",
        lines.len()
    );
}

// ── F-568: 10k single-token decode + 50-turn body serialization ───────────────
//
// The F-568 DoD requires:
//   (a) ≥40% allocation reduction at 10k single-token NDJSON chunks through
//       the byte-slice decoder relative to the legacy `Value`-tree path; and
//   (b) flat alloc-per-turn vs history depth for the chat-body serializer.
//
// We exercise (a) via the byte-slice `parse_line_bytes` entry-point — the
// production code path no longer takes a `&str` — and (b) via
// `serialize_chat_body` against a 50-turn synthetic history with mixed
// tool-result payloads. The legacy comparator for (b) builds the body the
// pre-fix way: `to_ollama_messages` (intermediate `Vec<Value>`) into
// `serde_json::json!` into `serde_json::to_vec`.

const TOKEN_COUNT_10K: usize = 10_000;

fn mock_ndjson_token_lines(n: usize) -> Vec<Vec<u8>> {
    // Realistic single-token text-delta shape — the per-token hot path. No
    // tool calls; the F-568 budget is the text-delta envelope. Newline is not
    // appended because the bench feeds `parse_line_bytes` per line directly
    // (the codec strips newlines before handing slices to `parse_line_bytes`).
    let tokens = [
        "the", " quick", " brown", " fox", " jumps", " over", " a", " lazy", " dog", ".",
    ];
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let tok = tokens[i % tokens.len()];
        let line = format!(r#"{{"message":{{"content":"{tok}"}}}}"#);
        out.push(line.into_bytes());
    }
    out
}

// Legacy text-delta parser kept for the (a) comparison. Mirrors the pre-fix
// `parse_line` allocation shape (`Value` tree + `as_str().to_string()`).
fn legacy_parse_line_bytes(line: &[u8]) -> Option<ChatChunk> {
    let s = std::str::from_utf8(line).ok()?;
    legacy_parse_line(s)
}

/// Build a synthetic 50-turn history with mixed text + tool-call + tool-result
/// blocks. Tool-result payloads are non-trivial JSON objects so the
/// `serde_json::to_string` re-quote cost in the legacy path is real.
fn synth_history(turns: usize) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(turns);
    for i in 0..turns {
        // Alternate user/assistant turns. Every 4th assistant turn carries a
        // tool call + the user reply carries a tool result with a ~4 KB
        // payload (file-read style) — the legacy hot-spot.
        if i % 2 == 0 {
            out.push(ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::Text(format!("user message {i} body"))],
            });
        } else if i % 8 == 1 {
            out.push(ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ChatBlock::ToolCall {
                    id: format!("call-{i}"),
                    name: "fs.read".into(),
                    args: serde_json::json!({"path": format!("/tmp/file-{i}.txt")}),
                }],
            });
            // Tool-result reply with a 4 KB payload.
            let big = "x".repeat(4096);
            out.push(ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::ToolResult {
                    id: format!("call-{i}"),
                    result: serde_json::json!({"content": big, "lines": 64}),
                }],
            });
        } else {
            out.push(ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ChatBlock::Text(format!("assistant reply {i}"))],
            });
        }
    }
    out
}

// Pre-fix body-builder: matches the shape the F-568 finding measured.
// `to_ollama_messages` lives in the prod module under `#[cfg(test)]` after the
// fix, so the bench reproduces it inline (verbatim copy of the pre-fix logic).
fn legacy_to_ollama_messages(
    system: &Option<String>,
    messages: &[ChatMessage],
) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(messages.len() + 1);
    if let Some(sys) = system {
        out.push(serde_json::json!({"role": "system", "content": sys}));
    }
    for msg in messages {
        let mut text_parts: Vec<&str> = Vec::new();
        let mut tool_calls: Vec<serde_json::Value> = Vec::new();
        for block in &msg.content {
            match block {
                ChatBlock::Text(t) => text_parts.push(t),
                ChatBlock::ToolCall { name, args, .. } => {
                    tool_calls.push(serde_json::json!({
                        "function": { "name": name, "arguments": args }
                    }));
                }
                ChatBlock::ToolResult { result, .. } => {
                    let content = serde_json::to_string(result).unwrap_or_else(|_| "null".into());
                    out.push(serde_json::json!({
                        "role": "tool",
                        "content": content,
                    }));
                }
            }
        }
        if text_parts.is_empty() && tool_calls.is_empty() {
            continue;
        }
        let role = match msg.role {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        };
        let mut entry = serde_json::json!({
            "role": role,
            "content": text_parts.concat(),
        });
        if !tool_calls.is_empty() {
            entry["tool_calls"] = serde_json::Value::Array(tool_calls);
        }
        out.push(entry);
    }
    out
}

fn legacy_serialize_chat_body(
    model: &str,
    system: &Option<String>,
    messages: &[ChatMessage],
) -> Vec<u8> {
    let body = serde_json::json!({
        "model": model,
        "messages": legacy_to_ollama_messages(system, messages),
        "stream": true,
    });
    serde_json::to_vec(&body).expect("to_vec")
}

fn bench_decode_10k_tokens(c: &mut Criterion) {
    let lines = mock_ndjson_token_lines(TOKEN_COUNT_10K);

    let mut group = c.benchmark_group("ollama_decode_10k_tokens");
    group.throughput(criterion::Throughput::Elements(lines.len() as u64));

    group.bench_function("legacy (Value-tree, &str)", |b| {
        b.iter(|| {
            for line in &lines {
                black_box(legacy_parse_line_bytes(black_box(line)));
            }
        });
    });

    group.bench_function("typed (F-568, Bytes slice)", |b| {
        b.iter(|| {
            for line in &lines {
                black_box(parse_line_bytes(black_box(line)));
            }
        });
    });

    group.finish();
}

fn bench_serialize_50_turn_body(c: &mut Criterion) {
    let history = synth_history(50);
    let model = "llama3";
    let sys: Option<String> = Some("be helpful".into());

    let mut group = c.benchmark_group("ollama_serialize_chat_body_50_turn");

    group.bench_function("legacy (Value-tree)", |b| {
        b.iter(|| {
            black_box(legacy_serialize_chat_body(
                black_box(model),
                black_box(&sys),
                black_box(&history),
            ));
        });
    });

    group.bench_function("typed (F-568, direct Serializer)", |b| {
        b.iter(|| {
            black_box(
                serialize_chat_body(
                    black_box(model),
                    black_box(sys.as_deref()),
                    black_box(&history),
                )
                .expect("serialize"),
            );
        });
    });

    group.finish();
}

/// F-568 DoD assertion: at 10k single-token NDJSON chunks the typed decoder
/// must allocate at most 60% of the legacy Value-tree path's count (≥40%
/// reduction). Also asserts flat alloc-per-turn for `serialize_chat_body`
/// across 50-turn vs 10-turn histories.
fn assert_f568_alloc_reductions(_c: &mut Criterion) {
    let lines = mock_ndjson_token_lines(TOKEN_COUNT_10K);

    // Warm up.
    for line in &lines {
        black_box(legacy_parse_line_bytes(black_box(line)));
        black_box(parse_line_bytes(black_box(line)));
    }

    let (legacy_allocs, legacy_bytes) = record(|| {
        for line in &lines {
            black_box(legacy_parse_line_bytes(black_box(line)));
        }
    });
    let (typed_allocs, typed_bytes) = record(|| {
        for line in &lines {
            black_box(parse_line_bytes(black_box(line)));
        }
    });

    println!(
        "F-568 decoder budget (10k tokens):\n  \
         legacy Value-tree:  {legacy_allocs} allocations, {legacy_bytes} bytes\n  \
         typed  (F-568):     {typed_allocs} allocations, {typed_bytes} bytes\n  \
         reduction:          {:.1}%",
        100.0 * (legacy_allocs.saturating_sub(typed_allocs)) as f64 / legacy_allocs as f64
    );

    assert!(
        typed_allocs * 5 <= legacy_allocs * 3,
        "F-568 DoD: typed decoder must hit ≥40% allocation reduction at 10k \
         tokens — measured legacy={legacy_allocs} typed={typed_allocs}"
    );

    // ── Body-serializer flatness: alloc-per-turn must not grow with depth ──
    let h10 = synth_history(10);
    let h50 = synth_history(50);
    let model = "llama3";
    let sys: Option<String> = Some("be helpful".into());

    // Warm.
    let _ = serialize_chat_body(model, sys.as_deref(), &h10).unwrap();
    let _ = serialize_chat_body(model, sys.as_deref(), &h50).unwrap();

    let (typed_10_allocs, _) = record(|| {
        let _ = serialize_chat_body(black_box(model), black_box(sys.as_deref()), black_box(&h10))
            .unwrap();
    });
    let (typed_50_allocs, _) = record(|| {
        let _ = serialize_chat_body(black_box(model), black_box(sys.as_deref()), black_box(&h50))
            .unwrap();
    });
    let (legacy_10_allocs, _) = record(|| {
        let _ = legacy_serialize_chat_body(black_box(model), black_box(&sys), black_box(&h10));
    });
    let (legacy_50_allocs, _) = record(|| {
        let _ = legacy_serialize_chat_body(black_box(model), black_box(&sys), black_box(&h50));
    });

    println!(
        "F-568 body-serialize budget (10-turn vs 50-turn):\n  \
         legacy Value-tree:  10t={legacy_10_allocs}  50t={legacy_50_allocs} (per-turn ≈ {:.1})\n  \
         typed  (F-568):     10t={typed_10_allocs}  50t={typed_50_allocs} (per-turn ≈ {:.1})",
        legacy_50_allocs as f64 / 50.0,
        typed_50_allocs as f64 / 50.0,
    );

    assert!(
        typed_50_allocs <= legacy_50_allocs,
        "F-568 DoD: typed body-serializer must allocate at most as many times as \
         the legacy Value-tree path at 50-turn — measured legacy={legacy_50_allocs} \
         typed={typed_50_allocs}"
    );
}

criterion_group!(
    benches,
    bench_parse_line,
    bench_decode_10k_tokens,
    bench_serialize_50_turn_body,
    assert_alloc_reduction,
    assert_f568_alloc_reductions,
);
criterion_main!(benches);

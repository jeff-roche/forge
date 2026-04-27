use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use chrono::Utc;
use forge_core::{
    apply_superseded,
    credentials::Credentials,
    ids::{MessageId, ProviderId, StepId, ToolCallId},
    read_since, ApprovalScope, ApprovalSource, Event, RerunVariant, StepKind, StepOutcome,
};
use forge_providers::{ChatBlock, ChatChunk, ChatMessage, ChatRequest, ChatRole, Provider};
use futures::StreamExt;
use tokio::sync::{oneshot, Mutex};

/// F-587: per-turn credential pull binding.
///
/// `run_turn` consults `store.get(provider_id)` exactly once at turn start
/// (before the request loop opens the model step). The result is **not**
/// passed into the request loop — the [`Provider`] trait doesn't yet take
/// per-turn auth (Phase 1 ships a keyless `OllamaProvider`; Anthropic /
/// OpenAI providers are wiring this as their auth-injection seam).
///
/// What lands today:
///
/// * The orchestrator pulls the credential on every turn (DoD #5).
/// * A `tracing::trace!` records hit / miss with `provider_id` only —
///   never the value, never even at `debug` level.
/// * Errors from the store propagate as `Err(_)` so a hostile or broken
///   keyring backend fails the turn instead of silently downgrading to
///   keyless.
///
/// When provider-level auth lands, the call site in `run_turn` will hand
/// `credential` into the provider's per-turn auth shape; nothing else
/// about this struct should need to change.
#[derive(Clone)]
pub struct CredentialContext {
    pub store: Arc<dyn Credentials>,
    pub provider_id: String,
}

impl std::fmt::Debug for CredentialContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omits `store` (Box<dyn _> doesn't `Debug` well) and
        // never prints the credential value. `provider_id` is non-secret
        // by contract.
        f.debug_struct("CredentialContext")
            .field("provider_id", &self.provider_id)
            .finish_non_exhaustive()
    }
}

/// F-587: pull the active provider's credential, exactly once, before a
/// turn or rerun's request loop opens.
///
/// Shared between `run_turn`, `Orchestrator::rerun_replace`,
/// `Orchestrator::rerun_branch`, and `Orchestrator::rerun_fresh` so the
/// pull contract is identical on every turn-shaped path. The pulled value
/// is dropped immediately on this Phase-1 keyless build; when Phase-3
/// providers consume `CredentialContext`, the value is handed into
/// per-request auth shape via `secrecy::ExposeSecret::expose_secret` at
/// the network boundary, never logged or stringified.
///
/// Backend errors propagate. A misconfigured Secret Service daemon or a
/// locked Keychain is more useful as a turn-level failure than a silent
/// fall-through to "no auth" that the provider would later 401 on.
async fn pull_active_credential(ctx: &Option<CredentialContext>) -> Result<()> {
    if let Some(ctx) = ctx.as_ref() {
        let pulled = ctx.store.get(&ctx.provider_id).await?;
        tracing::trace!(
            target: "forge_session::orchestrator::credentials",
            provider_id = %ctx.provider_id,
            hit = pulled.is_some(),
            "credential pull",
        );
        drop(pulled);
    }
    Ok(())
}

use crate::byte_budget::ByteBudget;
use crate::compaction::{compact, AUTO_COMPACT_THRESHOLD, DEFAULT_COMPACT_FRACTION};
use crate::dispatcher_cache::DispatcherCache;
use crate::sandbox::ChildRegistry;
use crate::session::Session;
use crate::tools::{
    AgentRuntime, AgentSpawnCtx, AgentSpawnTool, FsEditTool, FsReadTool, FsWriteTool, McpTool,
    ShellExecTool, ToolCtx, ToolDispatcher, ToolError,
};
use forge_mcp::McpManager;

/// Client decision for a pending tool call approval. Carries the
/// client-supplied `ApprovalScope` on approval so the event log records
/// the scope faithfully (F-053); `Rejected` collapses scope since there's
/// nothing to carry forward.
#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    Approved(ApprovalScope),
    Rejected,
}

/// Pending tool call approvals: maps ToolCallId → sender for the approval result.
pub type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>;

/// F-139: short SHA-256 prefix of a JSON-serialized args payload.
///
/// Used on `ToolInvoked` so downstream consumers can correlate a tool
/// invocation with the matching `ToolCallStarted.args` without shipping
/// the payload twice. 8 hex chars (32 bits) is ample for UI correlation
/// within a single turn — collisions are not a security boundary here.
fn args_digest(args: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(args).unwrap_or_default();
    let full = Sha256::digest(&bytes);
    let mut s = String::with_capacity(8);
    for b in full.iter().take(4) {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// F-599: extract a printable message from the boxed payload returned by
/// [`std::panic::catch_unwind`] / `FuturesExt::catch_unwind`. Tools that
/// `panic!("…")` produce a `&'static str`; `panic!("{}", x)` produces a
/// `String`. Anything else falls back to a generic marker — callers
/// surface it inside the tool result's `error` field.
fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Run a complete turn for the given user text. Emits all session events for:
/// UserMessage → StepStarted(Model) → AssistantMessage(open) → AssistantDelta* →
/// [StepStarted(Tool) → ToolCallStarted → ToolCallApprovalRequested →
///  ToolCallApproved → ToolInvoked → ToolReturned → ToolCallCompleted →
///  StepFinished(Tool)]* → AssistantMessage(finalised) → StepFinished(Model)
///
/// F-139 ordering invariant (pinned by `tests/step_events.rs`):
///  * every `StepStarted` is followed by exactly one `StepFinished` with
///    the same `step_id`, in LIFO order relative to other open steps;
///  * `ToolInvoked` / `ToolReturned` for a given `step_id` fall strictly
///    between that step's `StepStarted` and `StepFinished`;
///  * `AssistantMessage` / `AssistantDelta` for a turn fall inside the
///    enclosing `Model` step's window;
///  * on abnormal exits (stream error, tool rejection) the inner step is
///    closed with `StepOutcome::Error` and the outer Model step is
///    closed as `Ok` before the function returns — so replay never sees
///    an unterminated step window.
///
/// Tool calls block until the client sends `ToolCallApproved` / `ToolCallRejected`
/// through `pending_approvals`. `allowed_paths` is the set of glob patterns the
/// agent is permitted to access via `fs.read`.
///
/// `agents_md` is the cached workspace `AGENTS.md` contents (loaded once at
/// session start in `serve_with_session`). When `Some`, its contents are
/// injected into `ChatRequest.system` as a labeled section — see F-135.
#[allow(clippy::too_many_arguments)]
pub async fn run_turn<P: Provider>(
    session: Arc<Session>,
    provider: Arc<P>,
    text: String,
    pending_approvals: PendingApprovals,
    allowed_paths: Vec<String>,
    auto_approve: bool,
    workspace_root: Option<std::path::PathBuf>,
    child_registry: Option<ChildRegistry>,
    byte_budget: Option<Arc<ByteBudget>>,
    agents_md: Option<Arc<str>>,
    agent_runtime: Option<AgentRuntime>,
    mcp: Option<Arc<McpManager>>,
    // F-567: optional shared dispatcher cache. When `Some`, the cache hands
    // out a single `Arc<ToolDispatcher>` across turns and only rebuilds when
    // the MCP tools-list epoch advances — eliminating the per-turn HashMap
    // alloc, the M·T `McpTool` adapter allocations, and the `McpManager`
    // lock acquisition that previously sat on the time-to-first-token path.
    // `None` preserves the legacy "build a fresh dispatcher per turn"
    // behavior for tests and embedders without the cache wired up.
    dispatcher_cache: Option<Arc<DispatcherCache>>,
    // F-587: optional per-turn credential binding. When `Some`, the
    // orchestrator pulls the credential for `provider_id` from `store`
    // exactly once before the request loop opens. The pulled value is
    // never logged (trace-level only records hit/miss + provider_id), and
    // backend errors fail the turn rather than silently downgrading to
    // keyless. See [`CredentialContext`] for the seam contract.
    credentials: Option<CredentialContext>,
) -> Result<()> {
    // F-587: pull the credential for the active provider before any
    // model-side work begins. Shared with the rerun paths via
    // `pull_active_credential` so every turn-shaped entry point honors the
    // same pull-once contract. A credential-pull failure fails the turn
    // before compaction runs — that's the right ordering: we won't burn a
    // privileged summary call on a turn that was going to fail anyway.
    pull_active_credential(&credentials).await?;

    // F-598: auto-trigger context compaction at byte-budget >= 98%. Runs
    // BEFORE the new turn's UserMessage is logged so the summary marker
    // cleanly separates pre-compaction history from the new prompt.
    // Errors during auto-compaction are non-fatal — we log and proceed
    // with the turn rather than block the user on a misbehaving summary
    // provider. Two-layer re-entrancy guard: the outer `is_compacting()`
    // is a fast-path skip (avoids reading the event log on the steady
    // state) while the inner `try_claim_compacting()` inside `compact()`
    // is the true safety barrier (atomic, race-free against a concurrent
    // manual trigger).
    //
    // The provider id and model stamped on the synthetic summary message
    // match the values `run_request_loop` uses for live turns below
    // (currently the synthetic "mock" pair — when real providers thread
    // their own ids, both sites move together).
    if let Some(budget) = byte_budget.as_ref() {
        let limit = budget.limit();
        let consumed = budget.consumed();
        if limit > 0
            && (consumed as f64) >= (limit as f64) * AUTO_COMPACT_THRESHOLD
            && !session.is_compacting()
        {
            let pinned = std::collections::HashSet::new();
            if let Err(e) = compact(
                Arc::clone(&session),
                Arc::clone(&provider),
                ProviderId::new(),
                "mock".to_string(),
                DEFAULT_COMPACT_FRACTION,
                &pinned,
                forge_core::CompactTrigger::AutoAt98Pct,
            )
            .await
            {
                tracing::warn!(
                    target: "forge_session::orchestrator",
                    error = %e,
                    consumed,
                    limit,
                    "auto-compaction failed; proceeding with turn",
                );
            }
        }
    }

    let msg_id = MessageId::new();

    session
        .emit(Event::UserMessage {
            id: msg_id.clone(),
            at: Utc::now(),
            // F-112: wrap at the forge-core boundary. When F-108 lands, the
            // upstream producer will hand us an `Arc<str>` and this becomes a
            // move; for now `Arc::from` is a single allocation (same count as
            // the previous `clone`).
            text: Arc::from(text.as_str()),
            context: vec![],
            branch_parent: None,
        })
        .await?;

    // F-135: Inject workspace `AGENTS.md` into the system prompt. Placement
    // follows the DoD: a leading `\n\n---\n` separator so a future base-persona
    // prepend slots cleanly before the labeled section.
    //
    // F-566: `agents_md` is now the **already-wrapped** labeled prefix, built
    // once at session start in `serve_with_session::build_system_prompt`. The
    // orchestrator clones the `Arc<str>` into `req.system` (refcount bump),
    // never the underlying bytes. Eliminates the per-turn `format!()` against
    // a potentially 256 KiB AGENTS.md.
    let system: Option<Arc<str>> = agents_md.clone();

    // F-599: opt the provider into emitting parallel tool calls. The
    // orchestrator only dispatches a batch concurrently when every tool
    // in the batch is read-only (`group_tool_calls`); a mixed batch
    // collapses to sequential singletons. So allowing the provider to
    // emit parallel calls is always safe — the gating happens
    // dispatcher-side. F-583/F-584 wired the flag into the Anthropic /
    // OpenAI request bodies; this is the site the DoD pins as "flipped
    // to true".
    let initial_req = ChatRequest {
        system,
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatBlock::Text(text)],
        }],
        parallel_tool_calls_allowed: true,
    };

    // F-567: pull the dispatcher from the session-scoped cache when one is
    // wired (steady state: a single `Arc::clone`). When the cache is absent
    // — tests / embedders that pass `None` — fall back to the legacy
    // build-per-turn shape so the runtime contract on those paths stays
    // identical to pre-F-567 behavior.
    //
    // F-134 / F-132: builtins (`fs.read` / `fs.write` / `fs.edit` /
    // `shell.exec` / `agent.spawn`) plus one `McpTool` adapter per
    // currently-advertised tool. The cache snapshots at turn start (not
    // per-chunk) just like the original inline build, so a mid-turn
    // `tools/list` refresh can never mutate the dispatch table under the
    // running loop. Un-connected servers contribute zero tools — fail-open,
    // matches the prior behavior.
    let dispatcher: Arc<ToolDispatcher> = match dispatcher_cache.as_ref() {
        Some(cache) => cache.get(mcp.as_ref()).await,
        None => Arc::new(build_legacy_dispatcher(mcp.as_ref()).await),
    };

    let agent_ctx = agent_runtime.as_ref().map(|rt| AgentSpawnCtx {
        agent_defs: Arc::clone(&rt.agent_defs),
        orchestrator: Arc::clone(&rt.orchestrator),
        session: Arc::clone(&session),
        parent_instance_id: rt.parent_instance_id.clone(),
        current_msg_id: msg_id.clone(),
    });
    let instance_id = agent_runtime
        .as_ref()
        .map(|rt| rt.parent_instance_id.clone());
    let ctx = ToolCtx {
        allowed_paths,
        workspace_root,
        child_registry,
        byte_budget,
        agent_ctx,
        mcp,
    };

    run_request_loop(
        session,
        provider,
        initial_req,
        msg_id,
        None, // branch_parent — top-level turns are never a branch variant
        0,    // branch_variant_index — root position
        pending_approvals,
        dispatcher.as_ref(),
        &ctx,
        auto_approve,
        instance_id,
    )
    .await
}

/// F-567 fallback: build a fresh dispatcher when no [`DispatcherCache`] is
/// wired (tests, embedders that don't pass one). Mirrors the historical
/// inline body so behavior on the uncached path stays identical.
async fn build_legacy_dispatcher(mcp: Option<&Arc<McpManager>>) -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register(Box::new(FsReadTool))
        .expect("fs.read must register on a fresh dispatcher");
    dispatcher
        .register(Box::new(FsWriteTool))
        .expect("fs.write must register on a fresh dispatcher");
    dispatcher
        .register(Box::new(FsEditTool))
        .expect("fs.edit must register on a fresh dispatcher");
    dispatcher
        .register(Box::new(ShellExecTool))
        .expect("shell.exec must register on a fresh dispatcher");
    dispatcher
        .register(Box::new(AgentSpawnTool))
        .expect("agent.spawn must register on a fresh dispatcher");

    if let Some(mgr) = mcp {
        for server in mgr.list().await {
            for tool in server.tools {
                if let Some(adapter) = McpTool::new(
                    tool.name.clone(),
                    tool.description,
                    tool.read_only,
                    mgr.clone(),
                ) {
                    let _ = dispatcher.register(Box::new(adapter));
                }
            }
        }
    }

    dispatcher
}

/// Drives the provider request loop for one logical turn.
/// On tool calls: waits for approval, executes stub, appends result to the
/// next request, and continues until the provider returns `Done` with no
/// pending tool calls.
///
/// `pub(crate)` so rerun paths (F-143+) can reuse the loop with a pre-built
/// `ChatRequest` and a pre-chosen `msg_id`, instead of going through
/// [`run_turn`] which synthesizes a fresh `UserMessage` event.
///
/// `branch_parent` / `branch_variant_index` (F-144) are threaded onto every
/// `AssistantMessage` event this loop emits for `msg_id`:
///   * `None` / `0` — top-level turn or the root variant of a branch point.
///   * `Some(root_id)` / `N >= 1` — a Branch-rerun generation; both the
///     original and this new message co-exist in the transcript. Consumers
///     choose which to display via `BranchSelected`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_request_loop<P: Provider>(
    session: Arc<Session>,
    provider: Arc<P>,
    mut req: ChatRequest,
    msg_id: MessageId,
    branch_parent: Option<MessageId>,
    branch_variant_index: u32,
    pending_approvals: PendingApprovals,
    dispatcher: &ToolDispatcher,
    ctx: &ToolCtx,
    auto_approve: bool,
    // F-140: populated with the session's root `AgentInstanceId` when the
    // caller has a wired `AgentRuntime`. Threaded onto every `StepStarted`
    // so the Agent Monitor can group a session's trace under a stable
    // parent id. `None` preserves the pre-F-140 behaviour for embedders
    // that don't have an agent runtime wired up.
    instance_id: Option<forge_core::ids::AgentInstanceId>,
) -> Result<()> {
    // Fixed provider/model identifiers for the mock provider.
    let provider_id = ProviderId::new();
    let model = "mock".to_string();

    // F-606: 1-based step counter for this turn. Incremented before every
    // `StepStarted` emission (Model + Tool) so the AgentMonitor §9.2 chip
    // can render `step N`. The total is unknown in advance — the model
    // streams steps turn-by-turn — so `total` rides as `None` for now;
    // the UI displays `step N` and a follow-up may emit a retroactive
    // companion event when the turn ends.
    let mut step_index: u32 = 0;

    loop {
        // F-603: between-step pause checkpoint. Sits *before* the next
        // `StepStarted(Model)` emission so a pause request takes effect on
        // a clean step boundary — any in-flight tool call from the
        // previous iteration has already returned and `StepFinished`
        // landed before we arrived here. Returns immediately when the
        // session is running; parks on `resume_notify` while paused and
        // wakes on the `Paused → Running` transition. Tool-in-flight
        // invariant: the checkpoint never sits inside the stream loop
        // or the dispatcher, so a mid-stream pause request waits one
        // step before taking effect.
        session.wait_if_paused().await;

        // F-139: open a `Model` step around each provider pass. The step
        // envelopes every event this iteration emits (AssistantMessage*,
        // AssistantDelta, Tool*) — downstream consumers (Agent Monitor,
        // replay readers) rely on StepStarted preceding any per-turn
        // event with the same step_id.
        //
        // F-140: `instance_id` carries the session's root `AgentInstanceId`
        // when a caller wired an `AgentRuntime`; callers without one (legacy
        // tests, embedders with no agent wiring) still emit `None`. The
        // Agent Monitor groups a session's trace under the stable id here.
        let model_step_id = StepId::new();
        let model_step_started = Instant::now();
        step_index += 1;
        session
            .emit(Event::StepStarted {
                step_id: model_step_id.clone(),
                instance_id: instance_id.clone(),
                kind: StepKind::Model,
                started_at: Utc::now(),
                index: step_index,
                total: None,
            })
            .await?;

        // Emit AssistantMessage(open) before any chunks arrive — ensures the
        // event is present even when the first chunk is a tool call (not text).
        session
            .emit(Event::AssistantMessage {
                id: msg_id.clone(),
                provider: provider_id.clone(),
                model: model.clone(),
                at: Utc::now(),
                stream_finalised: false,
                // F-112: empty Arc<str> — no allocation (matches `Arc::<str>::from("")`).
                text: Arc::from(""),
                branch_parent: branch_parent.clone(),
                branch_variant_index,
            })
            .await?;

        let mut stream = provider.chat(req.clone()).await?;
        let mut assistant_text = String::new();
        // F-599: buffer every `ChatChunk::ToolCall` emitted in this stream
        // pass and dispatch the buffered set as one batch after the stream
        // closes. Buffering lets the dispatcher group consecutive
        // read-only calls and run them concurrently via `JoinSet` while
        // preserving original-call order on the wire (event log + chat
        // continuation). Text deltas are still emitted live — only tool
        // events are deferred.
        let mut pending_calls: Vec<PendingToolCall> = vec![];

        // F-604: set when the stream loop observed an interrupt request
        // and broke out at a chunk boundary. The post-loop tail is
        // skipped — we emit a dedicated interrupt finalisation
        // (`SessionInterrupted` + `AssistantMessage(final)` +
        // `StepFinished(Error: "interrupted")`) and return cleanly so
        // the session stays alive for the next user message. Tool
        // calls buffered in `pending_calls` are dropped without
        // emitting partial Tool* events — same drop semantics as the
        // `ChatChunk::Error` arm.
        let mut interrupted = false;

        while let Some(chunk) = stream.next().await {
            match chunk {
                ChatChunk::TextDelta(delta) => {
                    assistant_text.push_str(&delta);
                    session
                        .emit(Event::AssistantDelta {
                            id: msg_id.clone(),
                            at: Utc::now(),
                            // F-112: wrap the per-token String in Arc<str> at this
                            // boundary. Once F-108 lands, `parse_line` will hand
                            // us `Arc<str>` directly and this becomes a move.
                            delta: Arc::from(delta),
                        })
                        .await?;
                }

                ChatChunk::ToolCall { name, args } => {
                    pending_calls.push(PendingToolCall {
                        name,
                        args,
                        call_id: ToolCallId::new(),
                    });
                }

                ChatChunk::Done(_) => {
                    // Stream closed cleanly. Tool dispatch (if any) and the
                    // final AssistantMessage emission both happen below,
                    // *after* the stream loop unwinds — that way buffered
                    // tool calls get their event window inside the model
                    // step, and `AssistantMessage(stream_finalised=true)`
                    // still pins the end of the model step on the wire.
                    break;
                }

                ChatChunk::Error { kind, message } => {
                    // Stream aborted under a provider-layer bound (line cap,
                    // idle timeout, wall-clock timeout, transport error).
                    // Drop any buffered tool calls — they never reached
                    // the dispatcher and emitting partial Tool* events
                    // would leave a half-bracketed step window.
                    session
                        .emit(Event::AssistantMessage {
                            id: msg_id.clone(),
                            provider: provider_id.clone(),
                            model: model.clone(),
                            at: Utc::now(),
                            stream_finalised: true,
                            // F-112: wrap at boundary.
                            text: Arc::from(assistant_text.as_str()),
                            branch_parent: branch_parent.clone(),
                            branch_variant_index,
                        })
                        .await?;
                    // F-139: close the Model step with an Error outcome
                    // so late-joining subscribers see a well-formed
                    // step window even on provider abort.
                    session
                        .emit(Event::StepFinished {
                            step_id: model_step_id.clone(),
                            outcome: StepOutcome::Error {
                                reason: format!("provider stream aborted ({kind:?}): {message}"),
                            },
                            duration_ms: model_step_started.elapsed().as_millis() as u64,
                            token_usage: None,
                        })
                        .await?;
                    return Err(anyhow::anyhow!(
                        "provider stream aborted ({kind:?}): {message}"
                    ));
                }
            }

            // F-604: yield to the runtime before the interrupt check
            // so the daemon's IPC reader task gets a fair scheduling
            // slot to process an `InterruptSession` frame that arrived
            // while we were emitting the previous chunk's events.
            // Without this yield, a synchronous provider stream (e.g.
            // `MockProvider` backed by `futures::stream::iter`) drains
            // every chunk before the IPC reader gets a chance to flip
            // the flag, and the interrupt boundary check below never
            // observes a true. Real SSE-backed providers yield
            // naturally on socket reads; this `yield_now` gives the
            // mock provider — and any pathological zero-latency
            // provider — the same scheduling fairness. Cost is a
            // single tokio scheduler trip per chunk, negligible
            // against the per-token IPC cost.
            tokio::task::yield_now().await;

            // F-604: interrupt checkpoint — checked after each chunk so
            // a mid-stream request takes effect at the next clean
            // chunk boundary (where `assistant_text` reflects every
            // delta we already emitted via `AssistantDelta`). Distinct
            // from the F-603 pause checkpoint: pause parks at the
            // *between-step* boundary and keeps the turn alive;
            // interrupt cuts the current turn and hands the partial
            // back as a refine handoff. Set the flag and break — the
            // post-loop tail (below) sees `interrupted = true` and
            // takes the dedicated finalisation path.
            if session.is_interrupt_requested() {
                interrupted = true;
                break;
            }
        }

        // F-604: dedicated interrupt finalisation. Emit
        // `SessionInterrupted` carrying the captured partial text and
        // the step / message anchors, then close the model step with
        // an Error outcome (mirroring the `ChatChunk::Error` arm) so
        // late-joining replay consumers see a well-formed step window.
        // Drop any buffered tool calls — they never reached the
        // dispatcher and emitting partial Tool* events would leave a
        // half-bracketed step window. Publish the capture on the
        // session so the IPC handler awaiting
        // `await_interrupt_capture` unblocks with the same data the
        // event carries. Return `Ok(())` — the turn ends, the session
        // stays alive, the next `SendUserMessage` continues normally.
        if interrupted {
            let partial_arc: Arc<str> = Arc::from(assistant_text.as_str());
            session
                .emit(Event::AssistantMessage {
                    id: msg_id.clone(),
                    provider: provider_id.clone(),
                    model: model.clone(),
                    at: Utc::now(),
                    stream_finalised: true,
                    text: Arc::clone(&partial_arc),
                    branch_parent: branch_parent.clone(),
                    branch_variant_index,
                })
                .await?;
            session
                .emit(Event::SessionInterrupted {
                    at: Utc::now(),
                    partial_text: Arc::clone(&partial_arc),
                    captured_at_step_id: model_step_id.clone(),
                    captured_at_msg_id: msg_id.clone(),
                })
                .await?;
            session
                .emit(Event::StepFinished {
                    step_id: model_step_id.clone(),
                    outcome: StepOutcome::Error {
                        reason: "interrupted".to_string(),
                    },
                    duration_ms: model_step_started.elapsed().as_millis() as u64,
                    token_usage: None,
                })
                .await?;
            session
                .publish_interrupt_capture(crate::session::InterruptCapture {
                    partial_text: assistant_text,
                    captured_at_msg_id: msg_id.to_string(),
                    captured_at_step_id: model_step_id.to_string(),
                })
                .await;
            return Ok(());
        }

        // F-599: stream closed (Done or end-of-stream). Dispatch any
        // buffered tool calls before finalising the assistant message so
        // `AssistantMessage(stream_finalised=true)` pins the end of the
        // model step *after* every Tool* event the model logically owns.
        let had_tool_calls = !pending_calls.is_empty();
        let DispatchOutcome {
            tc_blocks,
            tr_blocks,
            rejected,
        } = dispatch_tool_calls(
            &session,
            std::mem::take(&mut pending_calls),
            &msg_id,
            &instance_id,
            &pending_approvals,
            dispatcher,
            ctx,
            auto_approve,
            &mut step_index,
        )
        .await?;

        // If the user denied a singleton non-read-only call, every
        // remaining call in `pending_calls` was already collapsed into
        // a synthetic "rejected" tool result; surface the same early
        // exit shape the pre-F-599 code did so the LIFO step invariant
        // holds for embedders that pin on it.
        if rejected {
            session
                .emit(Event::AssistantMessage {
                    id: msg_id.clone(),
                    provider: provider_id.clone(),
                    model: model.clone(),
                    at: Utc::now(),
                    stream_finalised: true,
                    text: Arc::from(assistant_text.as_str()),
                    branch_parent: branch_parent.clone(),
                    branch_variant_index,
                })
                .await?;
            session
                .emit(Event::StepFinished {
                    step_id: model_step_id.clone(),
                    outcome: StepOutcome::Ok,
                    duration_ms: model_step_started.elapsed().as_millis() as u64,
                    token_usage: None,
                })
                .await?;
            return Ok(());
        }

        // Always finalise the assistant message — whether or not tool
        // calls fired this turn.
        session
            .emit(Event::AssistantMessage {
                id: msg_id.clone(),
                provider: provider_id.clone(),
                model: model.clone(),
                at: Utc::now(),
                stream_finalised: true,
                text: Arc::from(assistant_text.as_str()),
                branch_parent: branch_parent.clone(),
                branch_variant_index,
            })
            .await?;
        session
            .emit(Event::StepFinished {
                step_id: model_step_id.clone(),
                outcome: StepOutcome::Ok,
                duration_ms: model_step_started.elapsed().as_millis() as u64,
                token_usage: None,
            })
            .await?;

        if !had_tool_calls {
            return Ok(());
        }

        // Build continuation: the assistant message includes the text + tool calls;
        // the tool results are a new user message.
        let mut assistant_content = vec![ChatBlock::Text(assistant_text)];
        assistant_content.extend(tc_blocks);

        req.messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: assistant_content,
        });
        req.messages.push(ChatMessage {
            role: ChatRole::User,
            content: tr_blocks,
        });
    }
}

/// F-599: one tool call buffered between stream consumption and dispatch.
struct PendingToolCall {
    name: String,
    args: serde_json::Value,
    call_id: ToolCallId,
}

/// F-599: outcome of dispatching the buffered tool batch for one model
/// pass. `rejected = true` means a non-read-only call was denied by the
/// user — the request loop should bail without continuing the
/// conversation. `tc_blocks` / `tr_blocks` are the assistant tool-call
/// blocks and the matching tool-result blocks in original-call order;
/// the caller appends them to the continuation request.
struct DispatchOutcome {
    tc_blocks: Vec<ChatBlock>,
    tr_blocks: Vec<ChatBlock>,
    rejected: bool,
}

/// F-599: a contiguous run of buffered tool calls that will be executed
/// together. A run is parallel iff it contains 2+ calls and *every* tool
/// is read-only; mixed and singleton runs collapse to sequential
/// dispatch (preserving the pre-F-599 behaviour exactly).
#[derive(Debug, PartialEq)]
struct ToolBatch {
    /// Indices into the buffered `pending_calls` slice. Non-empty.
    indices: Vec<usize>,
    /// `true` when the batch should run concurrently (length >= 2 AND
    /// every member tool's `read_only()` is true).
    parallel: bool,
}

/// F-599: partition a slice of buffered tool calls into [`ToolBatch`]es.
///
/// A `read_only_flags[i] == None` means the tool at index `i` was not
/// found on the dispatcher (UnknownTool / lookup error). Unknown tools
/// are treated as **non-read-only** for batching — we can't risk
/// dispatching them in parallel with read-only siblings.
///
/// Grouping rule (mirrors the DoD): walk the slice in order; consecutive
/// read-only calls coalesce into one parallel batch (`parallel = true`
/// once length >= 2). Any non-read-only or unknown call breaks the run
/// and dispatches as its own singleton (`parallel = false`).
fn group_tool_calls(read_only_flags: &[Option<bool>]) -> Vec<ToolBatch> {
    let mut batches: Vec<ToolBatch> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_all_ro = true;
    for (i, ro) in read_only_flags.iter().enumerate() {
        let is_ro = matches!(ro, Some(true));
        if is_ro {
            if !current_all_ro && !current.is_empty() {
                // `current` had a non-read-only entry — flush it as a
                // sequence of singletons (never mix RO + non-RO in one
                // parallel batch).
                for idx in current.drain(..) {
                    batches.push(ToolBatch {
                        indices: vec![idx],
                        parallel: false,
                    });
                }
                current_all_ro = true;
            }
            current.push(i);
        } else {
            // Flush any in-flight RO run as one batch (parallel iff >= 2),
            // then emit this non-RO call as its own singleton.
            if !current.is_empty() {
                let parallel = current_all_ro && current.len() >= 2;
                batches.push(ToolBatch {
                    indices: std::mem::take(&mut current),
                    parallel,
                });
                current_all_ro = true;
            }
            batches.push(ToolBatch {
                indices: vec![i],
                parallel: false,
            });
        }
    }
    if !current.is_empty() {
        let parallel = current_all_ro && current.len() >= 2;
        batches.push(ToolBatch {
            indices: current,
            parallel,
        });
    }
    batches
}

/// F-599: dispatch every buffered tool call for the current model pass.
///
/// * Parallel batches (>= 2 read-only calls) emit `ToolCallStarted` with
///   `parallel_group: Some(g)` (one shared id per batch) and run on a
///   `tokio::task::JoinSet`. Results are collected in original-call
///   order so the wire shape (continuation request blocks + every Tool*
///   event) is deterministic regardless of which task finishes first.
/// * Singleton batches keep the pre-F-599 sequential shape exactly,
///   including the user-approval prompt for non-read-only tools.
/// * If a singleton's user approval is denied, dispatch stops; remaining
///   buffered calls never reach the dispatcher and the caller bails the
///   request loop. `tc_blocks` / `tr_blocks` returned at that point
///   contain only the calls processed before the rejection.
#[allow(clippy::too_many_arguments)]
async fn dispatch_tool_calls(
    session: &Arc<Session>,
    pending: Vec<PendingToolCall>,
    msg_id: &MessageId,
    instance_id: &Option<forge_core::ids::AgentInstanceId>,
    pending_approvals: &PendingApprovals,
    dispatcher: &ToolDispatcher,
    ctx: &ToolCtx,
    auto_approve: bool,
    // F-606: turn-scoped step counter. Each `StepStarted(Tool)` emission in
    // this dispatch increments the counter; the `index` field carries the
    // updated value so the AgentMonitor chip renders `step N` consistently
    // across Model and Tool steps within the same turn.
    step_index: &mut u32,
) -> Result<DispatchOutcome> {
    if pending.is_empty() {
        return Ok(DispatchOutcome {
            tc_blocks: vec![],
            tr_blocks: vec![],
            rejected: false,
        });
    }

    // Look up read-only flags for grouping. Unknown tools coerce to
    // `None`, which `group_tool_calls` treats as non-read-only.
    let read_only_flags: Vec<Option<bool>> = pending
        .iter()
        .map(|p| dispatcher.get(&p.name).ok().map(|t| t.read_only()))
        .collect();
    let batches = group_tool_calls(&read_only_flags);

    let mut tc_blocks: Vec<ChatBlock> = Vec::with_capacity(pending.len());
    let mut tr_blocks: Vec<ChatBlock> = Vec::with_capacity(pending.len());

    // Indexed slot for each buffered call's result so we can reassemble
    // in original-call order after a parallel batch.
    let mut results: Vec<Option<serde_json::Value>> = vec![None; pending.len()];
    // Per-call `(tool_step_id, started_instant)` so a parallel batch's
    // `ToolReturned`/`StepFinished(Tool)` events reference the same step
    // id that the batch's `StepStarted(Tool)` opened.
    let mut tool_steps: Vec<Option<(StepId, Instant)>> = pending.iter().map(|_| None).collect();

    for batch in batches {
        if batch.parallel {
            // F-599: pull from the session-scoped counter so the id is
            // unique across model passes (UI can key on
            // `(session_id, parallel_group)` as a stable batch id).
            let group_id = session.next_parallel_group();

            // Open a Tool step + emit `ToolCallStarted` (with the shared
            // `parallel_group`) for every member of the batch BEFORE
            // spawning, so the event log shows the full batch starting
            // before any task completes.
            for &idx in &batch.indices {
                let pc = &pending[idx];
                let tool_step_id = StepId::new();
                let started = Instant::now();
                tool_steps[idx] = Some((tool_step_id.clone(), started));
                *step_index += 1;
                session
                    .emit(Event::StepStarted {
                        step_id: tool_step_id.clone(),
                        instance_id: instance_id.clone(),
                        kind: StepKind::Tool,
                        started_at: Utc::now(),
                        index: *step_index,
                        total: None,
                    })
                    .await?;
                session
                    .emit(Event::ToolCallStarted {
                        id: pc.call_id.clone(),
                        msg: msg_id.clone(),
                        tool: pc.name.clone(),
                        args: pc.args.clone(),
                        at: Utc::now(),
                        parallel_group: Some(group_id),
                    })
                    .await?;
            }

            // F-599: parallel batches trigger only when every member is
            // read-only — `ToolCallApproved(Auto)` is emitted for each
            // call so replay sees a terminated approval event. The
            // batched approval prompt the DoD references is a no-op
            // here: read-only tools never raise a user prompt today.
            for &idx in &batch.indices {
                let pc = &pending[idx];
                session
                    .emit(Event::ToolCallApproved {
                        id: pc.call_id.clone(),
                        by: ApprovalSource::Auto,
                        scope: ApprovalScope::Once,
                        at: Utc::now(),
                    })
                    .await?;
            }

            // Emit `ToolInvoked` for each call before spawning so every
            // step window's Invoked marker precedes its Returned marker
            // even when tasks finish out of order.
            for &idx in &batch.indices {
                let pc = &pending[idx];
                let (tool_step_id, _) = tool_steps[idx].as_ref().expect("tool step opened");
                session
                    .emit(Event::ToolInvoked {
                        step_id: tool_step_id.clone(),
                        tool_call_id: pc.call_id.clone(),
                        tool_id: pc.name.clone(),
                        args_digest: args_digest(&pc.args),
                    })
                    .await?;
            }

            // Spawn every call concurrently. Each task moves an
            // `Arc<dyn Tool>` + cloned args + cloned `ToolCtx` so the
            // future is `'static + Send`.
            let mut joinset: tokio::task::JoinSet<(usize, serde_json::Value)> =
                tokio::task::JoinSet::new();
            for &idx in &batch.indices {
                let pc = &pending[idx];
                let tool = match dispatcher.get_arc(&pc.name) {
                    Ok(t) => t,
                    Err(ToolError::UnknownTool(n)) => {
                        // Unknown tool — record a synthetic error inline
                        // without spawning. The pre-F-599 code emitted
                        // `ToolInvoked` here too; we already emitted it
                        // above so just stash the result.
                        results[idx] =
                            Some(serde_json::json!({ "error": format!("unknown tool '{n}'") }));
                        continue;
                    }
                    Err(e) => {
                        results[idx] = Some(serde_json::json!({ "error": e.to_string() }));
                        continue;
                    }
                };
                let args = pc.args.clone();
                let ctx_owned = ctx.clone();
                joinset.spawn(async move {
                    // F-077 / F-599: route through the shared budget
                    // gate. Bypassing it (calling `tool.invoke`
                    // directly) would lose the per-session aggregate
                    // cap on the parallel path.
                    //
                    // F-599 panic attribution: wrap the invocation in
                    // `AssertUnwindSafe(...).catch_unwind()` so a
                    // panicking tool (programmer error) becomes a
                    // typed error result attributed to *this* `idx`
                    // rather than surfacing as an opaque `JoinError`
                    // we'd have to misattribute by walking the empty
                    // result slots.
                    use futures::FutureExt;
                    let invoke_fut = std::panic::AssertUnwindSafe(
                        crate::tools::invoke_with_budget(tool.as_ref(), &args, &ctx_owned),
                    )
                    .catch_unwind();
                    let r = match invoke_fut.await {
                        Ok(v) => v,
                        Err(panic) => {
                            let msg = panic_message(panic);
                            serde_json::json!({
                                "error": format!("tool task panicked: {msg}")
                            })
                        }
                    };
                    (idx, r)
                });
            }

            // Drain the JoinSet. Spawned futures `catch_unwind` their
            // own panics, so `JoinError` here only fires on cancellation
            // / abort — which we don't trigger. If it does fire we
            // mark the first empty slot, but in practice this branch
            // is unreachable.
            while let Some(joined) = joinset.join_next().await {
                match joined {
                    Ok((idx, result)) => {
                        results[idx] = Some(result);
                    }
                    Err(join_err) => {
                        if let Some(slot) = results.iter_mut().find(|r| r.is_none()) {
                            *slot = Some(serde_json::json!({
                                "error": format!("tool task aborted: {join_err}")
                            }));
                        }
                    }
                }
            }

            // Reassemble per-call events in original-call order so
            // downstream consumers see deterministic Tool* sequencing.
            for &idx in &batch.indices {
                let pc = &pending[idx];
                let (tool_step_id, started) = tool_steps[idx].as_ref().expect("tool step opened");
                let result = results[idx]
                    .take()
                    .expect("every batch index must have a result");
                let result_bytes = serde_json::to_string(&result)
                    .map(|s| s.len() as u64)
                    .unwrap_or(0);
                let result_ok = result.get("error").is_none();
                let duration_ms = started.elapsed().as_millis() as u64;

                session
                    .emit(Event::ToolReturned {
                        step_id: tool_step_id.clone(),
                        tool_call_id: pc.call_id.clone(),
                        ok: result_ok,
                        bytes_out: result_bytes,
                    })
                    .await?;
                session
                    .emit(Event::ToolCallCompleted {
                        id: pc.call_id.clone(),
                        result: result.clone(),
                        duration_ms,
                        at: Utc::now(),
                    })
                    .await?;
                session
                    .emit(Event::StepFinished {
                        step_id: tool_step_id.clone(),
                        outcome: if result_ok {
                            StepOutcome::Ok
                        } else {
                            StepOutcome::Error {
                                reason: result
                                    .get("error")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("tool returned error")
                                    .to_string(),
                            }
                        },
                        duration_ms,
                        token_usage: None,
                    })
                    .await?;

                tc_blocks.push(ChatBlock::ToolCall {
                    id: pc.call_id.to_string(),
                    name: pc.name.clone(),
                    args: pc.args.clone(),
                });
                tr_blocks.push(ChatBlock::ToolResult {
                    id: pc.call_id.to_string(),
                    result,
                });
            }
        } else {
            // Sequential path — one call. Mirrors the pre-F-599 inline
            // body exactly so the wire shape on this path stays
            // identical (including the user-approval prompt for
            // non-read-only tools and the same rejection unwind).
            for &idx in &batch.indices {
                let pc = &pending[idx];
                let tool_step_id = StepId::new();
                let tool_step_started = Instant::now();
                *step_index += 1;
                session
                    .emit(Event::StepStarted {
                        step_id: tool_step_id.clone(),
                        instance_id: instance_id.clone(),
                        kind: StepKind::Tool,
                        started_at: Utc::now(),
                        index: *step_index,
                        total: None,
                    })
                    .await?;
                session
                    .emit(Event::ToolCallStarted {
                        id: pc.call_id.clone(),
                        msg: msg_id.clone(),
                        tool: pc.name.clone(),
                        args: pc.args.clone(),
                        at: Utc::now(),
                        parallel_group: None,
                    })
                    .await?;

                let started = Instant::now();
                let tool_lookup = dispatcher.get(&pc.name);

                let result = match tool_lookup {
                    Ok(tool) => {
                        if auto_approve || tool.read_only() {
                            session
                                .emit(Event::ToolCallApproved {
                                    id: pc.call_id.clone(),
                                    by: ApprovalSource::Auto,
                                    scope: ApprovalScope::Once,
                                    at: Utc::now(),
                                })
                                .await?;
                        } else {
                            let (tx, rx) = oneshot::channel::<ApprovalDecision>();
                            pending_approvals
                                .lock()
                                .await
                                .insert(pc.call_id.to_string(), tx);
                            session
                                .emit(Event::ToolCallApprovalRequested {
                                    id: pc.call_id.clone(),
                                    preview: tool.approval_preview(&pc.args),
                                })
                                .await?;
                            let decision = rx.await.unwrap_or(ApprovalDecision::Rejected);
                            let scope = match decision {
                                ApprovalDecision::Approved(scope) => scope,
                                ApprovalDecision::Rejected => {
                                    session
                                        .emit(Event::ToolCallRejected {
                                            id: pc.call_id.clone(),
                                            reason: Some("rejected by client".to_string()),
                                        })
                                        .await?;
                                    session
                                        .emit(Event::StepFinished {
                                            step_id: tool_step_id.clone(),
                                            outcome: StepOutcome::Error {
                                                reason: "rejected by client".to_string(),
                                            },
                                            duration_ms: tool_step_started.elapsed().as_millis()
                                                as u64,
                                            token_usage: None,
                                        })
                                        .await?;
                                    return Ok(DispatchOutcome {
                                        tc_blocks,
                                        tr_blocks,
                                        rejected: true,
                                    });
                                }
                            };
                            session
                                .emit(Event::ToolCallApproved {
                                    id: pc.call_id.clone(),
                                    by: ApprovalSource::User,
                                    scope,
                                    at: Utc::now(),
                                })
                                .await?;
                        }

                        session
                            .emit(Event::ToolInvoked {
                                step_id: tool_step_id.clone(),
                                tool_call_id: pc.call_id.clone(),
                                tool_id: pc.name.clone(),
                                args_digest: args_digest(&pc.args),
                            })
                            .await?;

                        // F-077 / F-599: route through the shared budget
                        // gate. Bypassing it (calling `tool.invoke`
                        // directly) would lose the per-session aggregate
                        // cap on the sequential path, mirroring the
                        // pre-F-599 dispatcher behaviour.
                        crate::tools::invoke_with_budget(tool, &pc.args, ctx).await
                    }
                    Err(ToolError::UnknownTool(n)) => {
                        session
                            .emit(Event::ToolInvoked {
                                step_id: tool_step_id.clone(),
                                tool_call_id: pc.call_id.clone(),
                                tool_id: pc.name.clone(),
                                args_digest: args_digest(&pc.args),
                            })
                            .await?;
                        serde_json::json!({ "error": format!("unknown tool '{n}'") })
                    }
                    Err(e) => {
                        session
                            .emit(Event::ToolInvoked {
                                step_id: tool_step_id.clone(),
                                tool_call_id: pc.call_id.clone(),
                                tool_id: pc.name.clone(),
                                args_digest: args_digest(&pc.args),
                            })
                            .await?;
                        serde_json::json!({ "error": e.to_string() })
                    }
                };

                let duration_ms = started.elapsed().as_millis() as u64;
                let result_bytes = serde_json::to_string(&result)
                    .map(|s| s.len() as u64)
                    .unwrap_or(0);
                let result_ok = result.get("error").is_none();
                session
                    .emit(Event::ToolReturned {
                        step_id: tool_step_id.clone(),
                        tool_call_id: pc.call_id.clone(),
                        ok: result_ok,
                        bytes_out: result_bytes,
                    })
                    .await?;
                session
                    .emit(Event::ToolCallCompleted {
                        id: pc.call_id.clone(),
                        result: result.clone(),
                        duration_ms,
                        at: Utc::now(),
                    })
                    .await?;
                session
                    .emit(Event::StepFinished {
                        step_id: tool_step_id.clone(),
                        outcome: if result_ok {
                            StepOutcome::Ok
                        } else {
                            StepOutcome::Error {
                                reason: result
                                    .get("error")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("tool returned error")
                                    .to_string(),
                            }
                        },
                        duration_ms: tool_step_started.elapsed().as_millis() as u64,
                        token_usage: None,
                    })
                    .await?;

                tc_blocks.push(ChatBlock::ToolCall {
                    id: pc.call_id.to_string(),
                    name: pc.name.clone(),
                    args: pc.args.clone(),
                });
                tr_blocks.push(ChatBlock::ToolResult {
                    id: pc.call_id.to_string(),
                    result,
                });
            }
        }
    }

    Ok(DispatchOutcome {
        tc_blocks,
        tr_blocks,
        rejected: false,
    })
}

// ── F-143: Orchestrator + rerun_message ────────────────────────────────────

/// Top-level entry point for session-level operations that span beyond a
/// single user turn — today only `rerun_message`; F-144 (Branch) and
/// F-145 (Fresh) will extend it.
///
/// The type is zero-sized on purpose: it is a namespace / trait-like façade
/// for the operations documented in `docs/architecture/ipc-contracts.md §4.1`
/// and keeps `run_turn` (the per-turn free function used by `server.rs`)
/// untouched. When later features accumulate shared state, the struct can
/// carry fields without breaking the call-site shape.
#[derive(Debug, Default, Clone, Copy)]
pub struct Orchestrator;

impl Orchestrator {
    pub fn new() -> Self {
        Self
    }

    /// Re-run an existing assistant message.
    ///
    /// Three variants (see [`RerunVariant`]):
    ///
    /// * [`RerunVariant::Replace`] (F-143) — truncate logically at `msg_id`'s
    ///   assistant turn, regenerate, and emit `MessageSuperseded` so replay
    ///   hides the original.
    /// * [`RerunVariant::Branch`] (F-144) — keep both versions. Spawns a
    ///   new `AssistantMessage` with `branch_parent` threaded to the target's
    ///   branch root and `branch_variant_index = prev_max + 1`. Both the
    ///   original and the new message remain visible in replay; consumers
    ///   pick which to display via `BranchSelected`.
    /// * [`RerunVariant::Fresh`] (F-144) — truncate back to the originating
    ///   user message (discarding intermediate turns / tool calls) and
    ///   regenerate from that user message alone. The new AssistantMessage
    ///   is a new root (`branch_parent = None`); the original turn is
    ///   superseded via `MessageSuperseded`.
    ///
    /// Ordering matters for Replace / Fresh: the `MessageSuperseded` marker
    /// is emitted *after* the new assistant message is finalised. If
    /// regeneration errors mid-stream the marker is never written and the
    /// original message stays authoritative — we don't point the UI at a
    /// half-written new_id. For Branch the original is never hidden, so
    /// no supersede marker is emitted.
    #[allow(clippy::too_many_arguments)]
    pub async fn rerun_message<P: Provider>(
        &self,
        session: Arc<crate::session::Session>,
        provider: Arc<P>,
        msg_id: MessageId,
        variant: RerunVariant,
        pending_approvals: PendingApprovals,
        allowed_paths: Vec<String>,
        auto_approve: bool,
        workspace_root: Option<std::path::PathBuf>,
        child_registry: Option<crate::sandbox::ChildRegistry>,
        byte_budget: Option<Arc<crate::byte_budget::ByteBudget>>,
        agent_runtime: Option<AgentRuntime>,
        // F-587: rerun is a turn — every variant must honor the same
        // credential-pull contract `run_turn` does. Threaded through to
        // each delegate so a missing or erroring keyring fails the rerun
        // before it reaches the provider, identical to a fresh turn.
        credentials: Option<CredentialContext>,
    ) -> Result<MessageId> {
        match variant {
            RerunVariant::Replace => {
                self.rerun_replace(
                    session,
                    provider,
                    msg_id,
                    pending_approvals,
                    allowed_paths,
                    auto_approve,
                    workspace_root,
                    child_registry,
                    byte_budget,
                    agent_runtime,
                    credentials,
                )
                .await
            }
            RerunVariant::Branch => {
                self.rerun_branch(
                    session,
                    provider,
                    msg_id,
                    pending_approvals,
                    allowed_paths,
                    auto_approve,
                    workspace_root,
                    child_registry,
                    byte_budget,
                    agent_runtime,
                    credentials,
                )
                .await
            }
            RerunVariant::Fresh => {
                self.rerun_fresh(
                    session,
                    provider,
                    msg_id,
                    pending_approvals,
                    allowed_paths,
                    auto_approve,
                    workspace_root,
                    child_registry,
                    byte_budget,
                    agent_runtime,
                    credentials,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn rerun_replace<P: Provider>(
        &self,
        session: Arc<crate::session::Session>,
        provider: Arc<P>,
        target: MessageId,
        pending_approvals: PendingApprovals,
        allowed_paths: Vec<String>,
        auto_approve: bool,
        workspace_root: Option<std::path::PathBuf>,
        child_registry: Option<crate::sandbox::ChildRegistry>,
        byte_budget: Option<Arc<crate::byte_budget::ByteBudget>>,
        agent_runtime: Option<AgentRuntime>,
        // F-587: see `rerun_message`.
        credentials: Option<CredentialContext>,
    ) -> Result<MessageId> {
        // F-587: pull the active provider's credential before regeneration
        // begins, identical to `run_turn`. Backend errors fail the rerun.
        pull_active_credential(&credentials).await?;

        // Read the log up to the current tip; filter prior supersede markers
        // so a second rerun doesn't rebuild context from already-hidden
        // messages.
        let history = read_since(&session.log_path, 0)
            .await
            .map_err(|e| anyhow!("rerun_replace: read event log: {e}"))?;
        let history = apply_superseded(history);

        let req = build_request_up_to(&history, &target)?;

        // Register the same tool dispatcher `run_turn` uses — rerun must be
        // able to re-execute tool calls if the regenerated stream emits
        // them.
        let mut dispatcher = crate::tools::ToolDispatcher::new();
        dispatcher
            .register(Box::new(crate::tools::FsReadTool))
            .expect("fs.read must register on a fresh dispatcher");
        dispatcher
            .register(Box::new(crate::tools::FsWriteTool))
            .expect("fs.write must register on a fresh dispatcher");
        dispatcher
            .register(Box::new(crate::tools::FsEditTool))
            .expect("fs.edit must register on a fresh dispatcher");
        dispatcher
            .register(Box::new(crate::tools::ShellExecTool))
            .expect("shell.exec must register on a fresh dispatcher");
        // F-134: register `agent.spawn` on the rerun dispatcher too. F-140
        // additional mandate — when the caller supplies an `AgentRuntime`,
        // thread it so a regenerated turn that emits `agent.spawn`
        // actually spawns the child against the session's shared
        // orchestrator. `None` preserves the pre-F-140 "not configured"
        // shape for embedders with no runtime wired up.
        dispatcher
            .register(Box::new(crate::tools::AgentSpawnTool))
            .expect("agent.spawn must register on a fresh dispatcher");
        let new_id = MessageId::new();
        let agent_ctx = agent_runtime.as_ref().map(|rt| AgentSpawnCtx {
            agent_defs: Arc::clone(&rt.agent_defs),
            orchestrator: Arc::clone(&rt.orchestrator),
            session: Arc::clone(&session),
            parent_instance_id: rt.parent_instance_id.clone(),
            current_msg_id: new_id.clone(),
        });
        let instance_id = agent_runtime
            .as_ref()
            .map(|rt| rt.parent_instance_id.clone());
        let ctx = crate::tools::ToolCtx {
            allowed_paths,
            workspace_root,
            child_registry,
            byte_budget,
            agent_ctx,
            // F-132: rerun paths do not re-enter the MCP path. Rerun
            // regenerates a prior assistant turn against already-recorded
            // context — the running session's MCP manager already
            // populated any MCP tool calls in the original transcript,
            // and replaying those same tool calls through a fresh manager
            // would be wrong (the external state may have moved on).
            mcp: None,
        };

        run_request_loop(
            Arc::clone(&session),
            provider,
            req,
            new_id.clone(),
            // Replace does not create a branch — the regenerated message
            // takes the original's place rather than sitting alongside it.
            None,
            0,
            pending_approvals,
            &dispatcher,
            &ctx,
            auto_approve,
            instance_id,
        )
        .await?;

        // Emit the supersede marker only after regeneration succeeded. If
        // run_request_loop returned Err, we bailed above — the original
        // assistant message remains authoritative in the transcript.
        session
            .emit(Event::MessageSuperseded {
                old_id: target,
                new_id: new_id.clone(),
            })
            .await?;

        Ok(new_id)
    }

    /// F-144: re-run producing a new Branch sibling of `target`.
    ///
    /// Semantics (per `docs/ui-specs/branching.md §15.1` and CONCEPT.md
    /// §10.3 "Branch"):
    ///   1. Read + filter history. Compute the branch root:
    ///      `root = target.branch_parent.unwrap_or(target)`. This coalesces
    ///      "branch of a branch" so every variant of the same original
    ///      response threads to the same root id (otherwise chained branches
    ///      would form a tree rather than a flat list of siblings).
    ///   2. Walk the filtered history to find `prev_max`, the highest
    ///      `branch_variant_index` among any `AssistantMessage` with
    ///      `branch_parent.unwrap_or(id) == root`. The root itself sits at
    ///      variant 0; the first Branch re-run produces variant 1.
    ///   3. Rebuild the provider request up to `target` via the same
    ///      `build_request_up_to` helper Replace uses — a branch is a
    ///      sibling generation from the same prompt state.
    ///   4. Drive `run_request_loop` with `branch_parent = Some(root)` and
    ///      `branch_variant_index = prev_max + 1`. No supersede marker is
    ///      emitted — both the original and the new sibling stay visible
    ///      in replay; consumers choose which to display via
    ///      `BranchSelected` (emitted separately by `select_branch`).
    #[allow(clippy::too_many_arguments)]
    async fn rerun_branch<P: Provider>(
        &self,
        session: Arc<crate::session::Session>,
        provider: Arc<P>,
        target: MessageId,
        pending_approvals: PendingApprovals,
        allowed_paths: Vec<String>,
        auto_approve: bool,
        workspace_root: Option<std::path::PathBuf>,
        child_registry: Option<crate::sandbox::ChildRegistry>,
        byte_budget: Option<Arc<crate::byte_budget::ByteBudget>>,
        agent_runtime: Option<AgentRuntime>,
        // F-587: see `rerun_message`.
        credentials: Option<CredentialContext>,
    ) -> Result<MessageId> {
        // F-587: same pull-once contract as `run_turn` / `rerun_replace`.
        pull_active_credential(&credentials).await?;

        let history = read_since(&session.log_path, 0)
            .await
            .map_err(|e| anyhow!("rerun_branch: read event log: {e}"))?;
        let history = apply_superseded(history);

        // Resolve the branch root. `target.branch_parent ?? target.id` —
        // spec §15.1. Also capture `prev_max`, the highest variant index
        // already at this branch point (root sits at 0 implicitly).
        let (root, prev_max) = find_branch_root_and_max(&history, &target)?;
        let next_index = prev_max
            .checked_add(1)
            .ok_or_else(|| anyhow!("rerun_branch: branch_variant_index overflow"))?;

        let req = build_request_up_to(&history, &target)?;

        let mut dispatcher = crate::tools::ToolDispatcher::new();
        dispatcher
            .register(Box::new(crate::tools::FsReadTool))
            .expect("fs.read must register on a fresh dispatcher");
        dispatcher
            .register(Box::new(crate::tools::FsWriteTool))
            .expect("fs.write must register on a fresh dispatcher");
        dispatcher
            .register(Box::new(crate::tools::FsEditTool))
            .expect("fs.edit must register on a fresh dispatcher");
        dispatcher
            .register(Box::new(crate::tools::ShellExecTool))
            .expect("shell.exec must register on a fresh dispatcher");
        dispatcher
            .register(Box::new(crate::tools::AgentSpawnTool))
            .expect("agent.spawn must register on a fresh dispatcher");
        let new_id = MessageId::new();
        let agent_ctx = agent_runtime.as_ref().map(|rt| AgentSpawnCtx {
            agent_defs: Arc::clone(&rt.agent_defs),
            orchestrator: Arc::clone(&rt.orchestrator),
            session: Arc::clone(&session),
            parent_instance_id: rt.parent_instance_id.clone(),
            current_msg_id: new_id.clone(),
        });
        let instance_id = agent_runtime
            .as_ref()
            .map(|rt| rt.parent_instance_id.clone());
        let ctx = crate::tools::ToolCtx {
            allowed_paths,
            workspace_root,
            child_registry,
            byte_budget,
            agent_ctx,
            // F-132: rerun paths do not re-enter the MCP path. Rerun
            // regenerates a prior assistant turn against already-recorded
            // context — the running session's MCP manager already
            // populated any MCP tool calls in the original transcript,
            // and replaying those same tool calls through a fresh manager
            // would be wrong (the external state may have moved on).
            mcp: None,
        };

        run_request_loop(
            Arc::clone(&session),
            provider,
            req,
            new_id.clone(),
            Some(root),
            next_index,
            pending_approvals,
            &dispatcher,
            &ctx,
            auto_approve,
            instance_id,
        )
        .await?;

        // Branch does not emit MessageSuperseded: both versions co-exist.
        Ok(new_id)
    }

    /// F-144: re-run discarding intermediate turns — the "Fresh" variant.
    ///
    /// Semantics (per CONCEPT.md §10.3 "Fresh"): regenerate from the
    /// originating user message alone, losing all intermediate tool calls
    /// and sub-agent context. The new AssistantMessage is a new root
    /// (`branch_parent = None`); the original target assistant turn is
    /// logically superseded via `MessageSuperseded` so replay consumers
    /// hide it.
    ///
    /// Steps:
    ///   1. Read + filter history.
    ///   2. Locate the `UserMessage` that immediately precedes `target`'s
    ///      assistant turn; build a one-message `ChatRequest` from it.
    ///      This is the key behavioural difference from Replace (which
    ///      carries *all* prior turns) — Fresh discards everything between
    ///      the user message and the target.
    ///   3. Drive `run_request_loop` with root branch metadata (None / 0).
    ///   4. Emit `MessageSuperseded { old_id: target, new_id }` on success
    ///      so a fresh subscriber sees only the regenerated message.
    ///
    /// Ordering invariant matches Replace: if the regenerated stream errors
    /// mid-flight, the supersede marker is never emitted and the original
    /// message stays authoritative.
    #[allow(clippy::too_many_arguments)]
    async fn rerun_fresh<P: Provider>(
        &self,
        session: Arc<crate::session::Session>,
        provider: Arc<P>,
        target: MessageId,
        pending_approvals: PendingApprovals,
        allowed_paths: Vec<String>,
        auto_approve: bool,
        workspace_root: Option<std::path::PathBuf>,
        child_registry: Option<crate::sandbox::ChildRegistry>,
        byte_budget: Option<Arc<crate::byte_budget::ByteBudget>>,
        agent_runtime: Option<AgentRuntime>,
        // F-587: see `rerun_message`.
        credentials: Option<CredentialContext>,
    ) -> Result<MessageId> {
        // F-587: same pull-once contract as `run_turn` / `rerun_replace`.
        pull_active_credential(&credentials).await?;

        let history = read_since(&session.log_path, 0)
            .await
            .map_err(|e| anyhow!("rerun_fresh: read event log: {e}"))?;
        let history = apply_superseded(history);

        let req = build_fresh_request_for(&history, &target)?;

        let mut dispatcher = crate::tools::ToolDispatcher::new();
        dispatcher
            .register(Box::new(crate::tools::FsReadTool))
            .expect("fs.read must register on a fresh dispatcher");
        dispatcher
            .register(Box::new(crate::tools::FsWriteTool))
            .expect("fs.write must register on a fresh dispatcher");
        dispatcher
            .register(Box::new(crate::tools::FsEditTool))
            .expect("fs.edit must register on a fresh dispatcher");
        dispatcher
            .register(Box::new(crate::tools::ShellExecTool))
            .expect("shell.exec must register on a fresh dispatcher");
        dispatcher
            .register(Box::new(crate::tools::AgentSpawnTool))
            .expect("agent.spawn must register on a fresh dispatcher");
        let new_id = MessageId::new();
        let agent_ctx = agent_runtime.as_ref().map(|rt| AgentSpawnCtx {
            agent_defs: Arc::clone(&rt.agent_defs),
            orchestrator: Arc::clone(&rt.orchestrator),
            session: Arc::clone(&session),
            parent_instance_id: rt.parent_instance_id.clone(),
            current_msg_id: new_id.clone(),
        });
        let instance_id = agent_runtime
            .as_ref()
            .map(|rt| rt.parent_instance_id.clone());
        let ctx = crate::tools::ToolCtx {
            allowed_paths,
            workspace_root,
            child_registry,
            byte_budget,
            agent_ctx,
            // F-132: rerun paths do not re-enter the MCP path. Rerun
            // regenerates a prior assistant turn against already-recorded
            // context — the running session's MCP manager already
            // populated any MCP tool calls in the original transcript,
            // and replaying those same tool calls through a fresh manager
            // would be wrong (the external state may have moved on).
            mcp: None,
        };

        run_request_loop(
            Arc::clone(&session),
            provider,
            req,
            new_id.clone(),
            None,
            0,
            pending_approvals,
            &dispatcher,
            &ctx,
            auto_approve,
            instance_id,
        )
        .await?;

        session
            .emit(Event::MessageSuperseded {
                old_id: target,
                new_id: new_id.clone(),
            })
            .await?;

        Ok(new_id)
    }

    /// F-144: activate a specific branch variant for replay / UI.
    ///
    /// Resolves `variant_index` against the filtered event log and emits
    /// `Event::BranchSelected { parent, selected }`.
    ///
    /// Resolution rules:
    ///   * `variant_index == 0` — `selected` is `parent` itself (the root's
    ///     own id).
    ///   * `variant_index >= 1` — `selected` is the `AssistantMessage` with
    ///     `branch_parent == Some(parent)` and matching
    ///     `branch_variant_index`.
    ///
    /// An unknown variant index returns `Err` and does **not** emit
    /// `BranchSelected`. Emitting for a nonexistent variant would corrupt
    /// the event log — downstream consumers reasonably assume every
    /// `BranchSelected.selected` points at a real message.
    pub async fn select_branch(
        &self,
        session: Arc<crate::session::Session>,
        parent: MessageId,
        variant_index: u32,
    ) -> Result<()> {
        let history = read_since(&session.log_path, 0)
            .await
            .map_err(|e| anyhow!("select_branch: read event log: {e}"))?;
        let history = apply_superseded(history);

        let selected = resolve_branch_variant(&history, &parent, variant_index)?;
        session
            .emit(Event::BranchSelected { parent, selected })
            .await?;
        Ok(())
    }

    /// F-145: tombstone a branch variant. Resolves `(parent, variant_index)`
    /// against the filtered event log (reusing `resolve_branch_variant` so
    /// unknown variants surface the same diagnostic), then emits
    /// `Event::BranchDeleted { parent, variant_index }`.
    ///
    /// Refuses to delete `variant_index == 0` when sibling variants remain:
    /// the root is the original message and removing it would orphan the
    /// siblings whose `branch_parent` points at it. Deleting the root when
    /// it is the *only* variant would leave the turn empty on screen; the
    /// UI gates against that path too (the strip only renders when there
    /// are two or more variants), but the orchestrator enforces the
    /// invariant server-side so a compromised webview cannot bypass it.
    pub async fn delete_branch(
        &self,
        session: Arc<crate::session::Session>,
        parent: MessageId,
        variant_index: u32,
    ) -> Result<()> {
        let history = read_since(&session.log_path, 0)
            .await
            .map_err(|e| anyhow!("delete_branch: read event log: {e}"))?;
        let history = apply_superseded(history);

        // Resolve the target — shares its error message with select_branch so
        // clients see a consistent "variant not found" diagnostic regardless
        // of which action triggered the lookup.
        let _ = resolve_branch_variant(&history, &parent, variant_index)
            .map_err(|e| anyhow!("delete_branch: {e}"))?;

        // Refuse root deletion when siblings exist. Count live siblings
        // (branch_parent == Some(parent)) — if any remain, deleting the root
        // would leave them parent-less on replay.
        if variant_index == 0 {
            let sibling_count = history
                .iter()
                .filter(|(_, ev)| {
                    matches!(
                        ev,
                        Event::AssistantMessage { branch_parent: Some(bp), .. } if bp == &parent
                    )
                })
                .count();
            if sibling_count > 0 {
                return Err(anyhow!(
                    "delete_branch: refusing to delete root variant while {sibling_count} sibling(s) remain"
                ));
            }
        }

        session
            .emit(Event::BranchDeleted {
                parent,
                variant_index,
            })
            .await?;
        Ok(())
    }
}

/// Walk `history` (a superseded-filtered `(seq, Event)` replay) and
/// rebuild the [`ChatRequest`] that was in front of the provider when
/// `target` was produced.
///
/// Rules:
/// - Events up to and including the `UserMessage` immediately preceding
///   `target`'s assistant turn are translated to `ChatMessage`s.
/// - `target`'s own assistant turn (including its deltas, tool calls,
///   tool results, and the terminal `AssistantMessage { stream_finalised:
///   true }`) is dropped.
/// - Unknown/non-conversation events (SessionStarted, UsageTick, etc.)
///   are skipped.
fn build_request_up_to(history: &[(u64, Event)], target: &MessageId) -> Result<ChatRequest> {
    let mut messages: Vec<ChatMessage> = Vec::new();
    let mut finalised_assistant_text: HashMap<MessageId, String> = HashMap::new();
    let mut current_assistant: Option<MessageId> = None;

    for (_, ev) in history {
        match ev {
            Event::UserMessage { text, .. } => {
                // A UserMessage implicitly closes any open assistant turn.
                if let Some(id) = current_assistant.take() {
                    flush_assistant(&mut messages, &id, &finalised_assistant_text);
                }
                messages.push(ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatBlock::Text(text.to_string())],
                });
            }
            Event::AssistantMessage {
                id,
                stream_finalised,
                text,
                ..
            } => {
                if id == target {
                    // We've reached the target's turn — stop **before** it.
                    // Anything already flushed is the reconstructed context.
                    return finalise_request(messages);
                }
                current_assistant = Some(id.clone());
                if *stream_finalised {
                    finalised_assistant_text.insert(id.clone(), text.to_string());
                }
            }
            _ => {
                // Skip deltas, tool call events, etc. The finalised
                // AssistantMessage text is authoritative for context
                // reconstruction; per-delta replay is unnecessary here.
            }
        }
    }

    // Target not found in history — this is a client/server state drift.
    Err(anyhow!(
        "rerun_message: target message {target:?} not found in session log"
    ))
}

fn flush_assistant(
    messages: &mut Vec<ChatMessage>,
    id: &MessageId,
    finalised: &HashMap<MessageId, String>,
) {
    if let Some(text) = finalised.get(id) {
        messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: vec![ChatBlock::Text(text.clone())],
        });
    }
}

fn finalise_request(messages: Vec<ChatMessage>) -> Result<ChatRequest> {
    // The request must contain at least the preceding UserMessage. If the
    // target was the very first event (shouldn't happen — an assistant
    // message always follows a user turn) we return an informative error.
    if messages.is_empty() {
        return Err(anyhow!(
            "rerun_message: no conversation context before target message"
        ));
    }
    Ok(ChatRequest {
        system: None,
        messages,
        // F-599: see `run_turn` — the orchestrator's per-batch grouping
        // makes the wire flag safe to enable on every turn-shaped path.
        parallel_tool_calls_allowed: true,
    })
}

/// F-144: given a filtered history and a rerun target, return the branch
/// root id and the highest `branch_variant_index` already present at that
/// root. Used by `rerun_branch` to thread the new variant as
/// `(root, prev_max + 1)`.
///
/// Root resolution follows spec §15.1:
///   * If `target.branch_parent == Some(root)` — target is already a
///     branch variant; the new sibling shares the same root.
///   * If `target.branch_parent == None` — target is a root message; the
///     new sibling's root is `target.id` itself.
///
/// `prev_max` starts at 0 (the root's implicit variant). Any sibling with
/// `branch_parent == Some(root)` bumps it to at least its own
/// `branch_variant_index`.
///
/// Walks the filtered `(seq, Event)` list twice: once to find `target`'s
/// own AssistantMessage (to read its `branch_parent`), once to scan for
/// siblings. Both passes are O(n) in history size — single-threaded reruns
/// are not a hot path.
fn find_branch_root_and_max(
    history: &[(u64, Event)],
    target: &MessageId,
) -> Result<(MessageId, u32)> {
    // Pass 1: locate target's own `branch_parent`.
    let target_branch_parent = history
        .iter()
        .find_map(|(_, ev)| match ev {
            Event::AssistantMessage {
                id, branch_parent, ..
            } if id == target => Some(branch_parent.clone()),
            _ => None,
        })
        .ok_or_else(|| {
            anyhow!("rerun_branch: target message {target:?} not found in session log")
        })?;

    let root = target_branch_parent.unwrap_or_else(|| target.clone());

    // Pass 2: scan siblings. `prev_max` is the highest `branch_variant_index`
    // seen for any AssistantMessage whose branch_parent coalesces to `root`.
    // `id == root` covers the root itself (which is the implicit variant 0
    // but we tolerate any value its own variant field may carry).
    let mut prev_max: u32 = 0;
    for (_, ev) in history {
        if let Event::AssistantMessage {
            id,
            branch_parent,
            branch_variant_index,
            ..
        } = ev
        {
            let belongs_to_root = match branch_parent {
                Some(p) => p == &root,
                None => id == &root,
            };
            if belongs_to_root && *branch_variant_index > prev_max {
                prev_max = *branch_variant_index;
            }
        }
    }

    Ok((root, prev_max))
}

/// F-144: build the single-message `ChatRequest` for the Fresh re-run
/// variant.
///
/// Behaviourally distinct from `build_request_up_to`: where Replace /
/// Branch carry *all* prior turns into the request, Fresh discards
/// everything between the originating user message and `target`. The
/// returned request contains exactly one message — the last `UserMessage`
/// before `target`'s assistant turn.
///
/// If `target` is not found, or no `UserMessage` precedes it, returns an
/// informative error.
fn build_fresh_request_for(history: &[(u64, Event)], target: &MessageId) -> Result<ChatRequest> {
    let mut last_user: Option<String> = None;

    for (_, ev) in history {
        match ev {
            Event::UserMessage { text, .. } => {
                last_user = Some(text.to_string());
            }
            Event::AssistantMessage { id, .. } if id == target => {
                let text = last_user.ok_or_else(|| {
                    anyhow!("rerun_fresh: no user message precedes target {target:?}")
                })?;
                return Ok(ChatRequest {
                    system: None,
                    messages: vec![ChatMessage {
                        role: ChatRole::User,
                        content: vec![ChatBlock::Text(text)],
                    }],
                    // F-599: see `run_turn` for the rationale.
                    parallel_tool_calls_allowed: true,
                });
            }
            _ => {}
        }
    }

    Err(anyhow!(
        "rerun_fresh: target message {target:?} not found in session log"
    ))
}

/// F-144: resolve `(parent, variant_index)` to the MessageId to report in
/// `BranchSelected.selected`.
///
/// * `variant_index == 0` returns `parent` directly — the root variant is
///   represented by the original message id itself.
/// * `variant_index >= 1` scans `history` for an `AssistantMessage` with
///   `branch_parent == Some(parent)` and `branch_variant_index` equal to
///   the requested index.
///
/// Unknown variants return `Err`. Callers **must** propagate rather than
/// emit `BranchSelected { parent, selected: parent }` as a fallback —
/// replay consumers assume every selected id is live.
fn resolve_branch_variant(
    history: &[(u64, Event)],
    parent: &MessageId,
    variant_index: u32,
) -> Result<MessageId> {
    if variant_index == 0 {
        // Sanity-check that a root with this id actually exists. A random
        // parent id should not be silently accepted — the UI would gate
        // display of a nonexistent message.
        let exists = history
            .iter()
            .any(|(_, ev)| matches!(ev, Event::AssistantMessage { id, .. } if id == parent));
        if !exists {
            return Err(anyhow!(
                "select_branch: parent message {parent:?} not found in session log"
            ));
        }
        return Ok(parent.clone());
    }

    history
        .iter()
        .find_map(|(_, ev)| match ev {
            Event::AssistantMessage {
                id,
                branch_parent: Some(bp),
                branch_variant_index,
                ..
            } if bp == parent && *branch_variant_index == variant_index => Some(id.clone()),
            _ => None,
        })
        .ok_or_else(|| {
            anyhow!("select_branch: no variant with index {variant_index} under parent {parent:?}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_providers::MockProvider;
    use std::collections::HashMap;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    // F-135: verify AGENTS.md content is injected into `ChatRequest.system`
    // exactly once, at the correct position, and that the cached value is
    // reused across multiple turns (no re-read of the file on disk).
    #[tokio::test]
    async fn agents_md_injected_into_system_prompt_and_cached_across_turns() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        // Script an end-of-turn response for each of two turns.
        let script = "{\"done\":\"end_turn\"}\n".to_string();
        let provider = Arc::new(
            MockProvider::from_responses(vec![script.clone(), script]).expect("construct mock"),
        );

        // F-566: simulate the labeled-prefix cache that `serve_with_session`
        // would build once via `build_system_prompt`. `run_turn` no longer
        // reformats per turn — it clones the prebuilt `Arc<str>`.
        let original = "be helpful";
        let expected = format!("\n\n---\nAGENTS.md (workspace):\n{original}");
        let agents_md: Option<Arc<str>> = Some(Arc::from(expected.as_str()));

        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));

        run_turn(
            Arc::clone(&session),
            Arc::clone(&provider),
            "first turn".to_string(),
            Arc::clone(&pending),
            vec![],
            true,
            None,
            None,
            None,
            agents_md.clone(),
            None,
            None,
            None,
            None, // F-587: no credentials wired in this AGENTS.md test.
        )
        .await
        .unwrap();

        // Between turns, overwrite the hypothetical on-disk AGENTS.md. The
        // cache is an `Arc<str>` captured at session start, so the second
        // turn must still observe the original content — proving no re-read.
        // (We don't have a workspace wired in this unit test; the assertion
        // below on request #2 is what proves cache reuse.)

        run_turn(
            Arc::clone(&session),
            Arc::clone(&provider),
            "second turn".to_string(),
            pending,
            vec![],
            true,
            None,
            None,
            None,
            agents_md,
            None,
            None,
            None,
            None, // F-587
        )
        .await
        .unwrap();

        let reqs = provider.recorded_requests();
        assert_eq!(reqs.len(), 2, "exactly two turns dispatched");

        assert_eq!(
            reqs[0].system.as_deref(),
            Some(expected.as_str()),
            "first turn: AGENTS.md injected at the correct position with exact delimiter"
        );
        assert_eq!(
            reqs[1].system.as_deref(),
            Some(expected.as_str()),
            "second turn: cached value reused, no re-read"
        );

        // "Injection appears once" — the labeled header must not be
        // duplicated inside the system string (e.g. by accidental double
        // prepend on continuation requests within a turn).
        assert_eq!(
            reqs[0]
                .system
                .as_deref()
                .unwrap()
                .matches("AGENTS.md (workspace):")
                .count(),
            1,
            "label must appear exactly once in the system prompt"
        );
    }

    // F-135: when no AGENTS.md is cached (file absent or workspace unset),
    // `ChatRequest.system` stays `None` — no session failure, no empty
    // labeled block.
    #[tokio::test]
    async fn system_prompt_is_none_when_agents_md_absent() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        let provider = Arc::new(
            MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()])
                .expect("construct mock"),
        );

        run_turn(
            session,
            Arc::clone(&provider),
            "hello".to_string(),
            Arc::new(Mutex::new(HashMap::new())),
            vec![],
            true,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // F-587
        )
        .await
        .unwrap();

        let reqs = provider.recorded_requests();
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0].system.is_none(),
            "no cache → no injection, system stays None"
        );
    }

    // F-144: branch-of-a-branch re-runs must thread to the same root. Spec
    // §15.1: `branch_parent_id = M.branch_parent_id ?? M.id`. If the target
    // is already a branch variant, the new sibling shares target's parent
    // (not target itself). This is what keeps variants flat — without
    // coalescing, chained Branch reruns would form a tree of parents and
    // `prev_max + 1` couldn't resolve sibling order.
    #[test]
    fn find_branch_root_coalesces_when_target_is_itself_a_variant() {
        use chrono::Utc;

        let root = MessageId::new();
        let variant1 = MessageId::new();
        let variant2 = MessageId::new();

        fn assistant_with_branch(id: &MessageId, parent: Option<&MessageId>, idx: u32) -> Event {
            Event::AssistantMessage {
                id: id.clone(),
                provider: ProviderId::new(),
                model: "mock".into(),
                at: Utc::now(),
                stream_finalised: true,
                text: Arc::from(""),
                branch_parent: parent.cloned(),
                branch_variant_index: idx,
            }
        }

        let history = vec![
            (1, assistant_with_branch(&root, None, 0)),
            (2, assistant_with_branch(&variant1, Some(&root), 1)),
            (3, assistant_with_branch(&variant2, Some(&root), 2)),
        ];

        // Branch-ing `variant1` must coalesce to `root`, and prev_max must
        // reflect variant2 (index 2) — not just variant1's own index.
        let (resolved_root, prev_max) =
            find_branch_root_and_max(&history, &variant1).expect("resolve");
        assert_eq!(
            resolved_root, root,
            "branch-of-a-branch must resolve to root"
        );
        assert_eq!(
            prev_max, 2,
            "prev_max must scan all siblings, not just target's own variant_index"
        );

        // Branch-ing the root directly lands at the same root and the same
        // prev_max — the family of variants does not grow by targeting the
        // root.
        let (root_again, same_max) =
            find_branch_root_and_max(&history, &root).expect("resolve root");
        assert_eq!(root_again, root);
        assert_eq!(same_max, 2);
    }

    // F-144: resolve_branch_variant must refuse unknown variant indices.
    // A well-meaning bug that emits `BranchSelected { parent, selected:
    // parent }` as a fallback on unknown index would corrupt replay —
    // downstream UIs assume every selected id is a live message.
    #[test]
    fn resolve_branch_variant_rejects_unknown_index() {
        use chrono::Utc;

        let root = MessageId::new();
        let variant1 = MessageId::new();
        let history = vec![
            (
                1,
                Event::AssistantMessage {
                    id: root.clone(),
                    provider: ProviderId::new(),
                    model: "mock".into(),
                    at: Utc::now(),
                    stream_finalised: true,
                    text: Arc::from(""),
                    branch_parent: None,
                    branch_variant_index: 0,
                },
            ),
            (
                2,
                Event::AssistantMessage {
                    id: variant1.clone(),
                    provider: ProviderId::new(),
                    model: "mock".into(),
                    at: Utc::now(),
                    stream_finalised: true,
                    text: Arc::from(""),
                    branch_parent: Some(root.clone()),
                    branch_variant_index: 1,
                },
            ),
        ];

        // Known variants resolve.
        assert_eq!(
            resolve_branch_variant(&history, &root, 0).expect("root resolves"),
            root
        );
        assert_eq!(
            resolve_branch_variant(&history, &root, 1).expect("variant 1 resolves"),
            variant1
        );

        // Unknown indexes return Err without silently falling back.
        assert!(resolve_branch_variant(&history, &root, 99).is_err());
        // Parent id that doesn't exist in the log is also rejected —
        // even at variant_index 0, where the naïve implementation would
        // return the parent unchanged.
        let orphan = MessageId::new();
        assert!(resolve_branch_variant(&history, &orphan, 0).is_err());
    }

    // F-598: when the byte budget is at >= 98% of capacity at run_turn
    // entry, the orchestrator must auto-trigger compaction BEFORE the new
    // UserMessage lands. Wire the budget to a tiny limit and pre-charge it
    // past the threshold so a single run_turn invocation observes the
    // condition.
    #[tokio::test]
    async fn run_turn_auto_triggers_compaction_at_98pct() {
        use forge_core::Event as Ev;

        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        // Seed prior turns so compaction has something to summarize.
        for n in 0..3 {
            session
                .emit(Ev::UserMessage {
                    id: MessageId::new(),
                    at: Utc::now(),
                    text: Arc::from(format!("prior question {n}").as_str()),
                    context: vec![],
                    branch_parent: None,
                })
                .await
                .unwrap();
            session
                .emit(Ev::AssistantMessage {
                    id: MessageId::new(),
                    provider: ProviderId::new(),
                    model: "mock".into(),
                    at: Utc::now(),
                    stream_finalised: true,
                    text: Arc::from(format!("prior answer {n}").as_str()),
                    branch_parent: None,
                    branch_variant_index: 0,
                })
                .await
                .unwrap();
        }

        // Provider script: first call is the compaction summary, second
        // call is the new turn itself.
        let summary_script =
            "{\"delta\":\"compacted summary\"}\n{\"done\":\"end_turn\"}\n".to_string();
        let turn_script = "{\"delta\":\"new answer\"}\n{\"done\":\"end_turn\"}\n".to_string();
        let provider = Arc::new(
            MockProvider::from_responses(vec![summary_script, turn_script])
                .expect("construct mock"),
        );

        // Tiny budget pre-charged past 98% so run_turn trips the gate
        // immediately on entry.
        let budget = Arc::new(ByteBudget::new(1_000));
        budget.charge(990);

        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));

        let mut rx = session.event_tx.subscribe();

        run_turn(
            Arc::clone(&session),
            Arc::clone(&provider),
            "fresh prompt".to_string(),
            pending,
            vec![],
            true,
            None,
            None,
            Some(Arc::clone(&budget)),
            None,
            None,
            None,
            None,
            None, // F-587: no credentials wired in this auto-trigger test.
        )
        .await
        .unwrap();

        // Drain emissions and look for ContextCompacted before the
        // *new* UserMessage of the actual turn. The order check is the
        // load-bearing assertion: marker first, then the new turn.
        let mut saw_compacted = false;
        let mut saw_new_user_msg_after = false;
        while let Ok((_, ev)) = rx.try_recv() {
            match ev {
                Ev::ContextCompacted { trigger, .. } => {
                    assert_eq!(trigger, forge_core::CompactTrigger::AutoAt98Pct);
                    saw_compacted = true;
                }
                Ev::UserMessage { text, .. } if &*text == "fresh prompt" && saw_compacted => {
                    saw_new_user_msg_after = true;
                }
                _ => {}
            }
        }
        assert!(saw_compacted, "auto-trigger must emit ContextCompacted");
        assert!(
            saw_new_user_msg_after,
            "the new turn's UserMessage must follow ContextCompacted"
        );
    }

    // F-598: with the byte budget under 98% the orchestrator MUST NOT
    // fire compaction. Negative-side gate.
    #[tokio::test]
    async fn run_turn_does_not_compact_below_threshold() {
        use forge_core::Event as Ev;

        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        let provider = Arc::new(
            MockProvider::from_responses(vec!["{\"done\":\"end_turn\"}\n".into()])
                .expect("construct mock"),
        );

        let budget = Arc::new(ByteBudget::new(1_000));
        budget.charge(500); // 50% — well under 98%
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));

        let mut rx = session.event_tx.subscribe();

        run_turn(
            Arc::clone(&session),
            Arc::clone(&provider),
            "below threshold".to_string(),
            pending,
            vec![],
            true,
            None,
            None,
            Some(budget),
            None,
            None,
            None,
            None,
            None, // F-587: no credentials wired in this negative-gate test.
        )
        .await
        .unwrap();

        while let Ok((_, ev)) = rx.try_recv() {
            assert!(
                !matches!(ev, Ev::ContextCompacted { .. }),
                "compaction must not fire below 98%"
            );
        }
    }

    // ── F-599: parallel read-only tool dispatch ────────────────────────────

    use crate::tools::{Tool, ToolCtx, ToolDispatcher};
    use forge_core::ApprovalPreview;

    /// Test stub: a tool whose `read_only` flag is configurable and whose
    /// `invoke` sleeps for a configurable duration before returning a
    /// synthetic JSON payload. Used to drive the latency / ordering /
    /// grouping assertions below.
    struct SleepyTool {
        name: String,
        read_only: bool,
        sleep_ms: u64,
    }

    #[async_trait::async_trait]
    impl Tool for SleepyTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn approval_preview(&self, _args: &serde_json::Value) -> ApprovalPreview {
            ApprovalPreview {
                description: format!("sleepy {}", self.name),
            }
        }
        fn read_only(&self) -> bool {
            self.read_only
        }
        async fn invoke(&self, _args: &serde_json::Value, _ctx: &ToolCtx) -> serde_json::Value {
            tokio::time::sleep(std::time::Duration::from_millis(self.sleep_ms)).await;
            serde_json::json!({ "tool": self.name.clone() })
        }
    }

    #[test]
    fn group_tool_calls_singleton_read_only_is_one_sequential_batch() {
        let groups = group_tool_calls(&[Some(true)]);
        assert_eq!(
            groups,
            vec![ToolBatch {
                indices: vec![0],
                parallel: false,
            }]
        );
    }

    #[test]
    fn group_tool_calls_consecutive_read_only_coalesce_into_one_parallel_batch() {
        let groups = group_tool_calls(&[Some(true), Some(true), Some(true)]);
        assert_eq!(
            groups,
            vec![ToolBatch {
                indices: vec![0, 1, 2],
                parallel: true,
            }]
        );
    }

    #[test]
    fn group_tool_calls_non_read_only_breaks_run_into_singletons() {
        // RO, RO, WRITE, RO  →  [RO RO] (parallel) | [WRITE] | [RO]
        let groups = group_tool_calls(&[Some(true), Some(true), Some(false), Some(true)]);
        assert_eq!(
            groups,
            vec![
                ToolBatch {
                    indices: vec![0, 1],
                    parallel: true,
                },
                ToolBatch {
                    indices: vec![2],
                    parallel: false,
                },
                ToolBatch {
                    indices: vec![3],
                    parallel: false,
                },
            ]
        );
    }

    #[test]
    fn group_tool_calls_unknown_tool_is_treated_as_non_read_only() {
        // RO, ?, RO  →  [RO] | [?] | [RO]   (no parallel batch — unknown
        // breaks the run because we can't be sure it's safe)
        let groups = group_tool_calls(&[Some(true), None, Some(true)]);
        assert_eq!(
            groups,
            vec![
                ToolBatch {
                    indices: vec![0],
                    parallel: false,
                },
                ToolBatch {
                    indices: vec![1],
                    parallel: false,
                },
                ToolBatch {
                    indices: vec![2],
                    parallel: false,
                },
            ]
        );
    }

    /// F-599 DoD: 3 read-only tool calls in one turn dispatch concurrently.
    /// Wall-clock latency must be substantially less than the sequential
    /// reference (3 × per-tool sleep). We measure both:
    ///   * a sequential baseline by running 3 RO tools as a *mixed* batch
    ///     (interleaved with non-RO singletons so they don't coalesce into
    ///     a parallel batch), then
    ///   * the parallel run with the same per-tool sleep.
    ///
    /// Asserts the parallel run finishes in under 2/3 of the sequential
    /// run. That ratio is the theoretical lower bound for "any
    /// concurrency at all" with three equal-cost tools — a serial
    /// bypass would sit at ~1.0 — and is independent of the absolute
    /// per-tool sleep, so the test is robust on slow CI workers.
    #[tokio::test]
    async fn parallel_dispatch_3_read_only_tools_finishes_under_sequential_reference() {
        const SLEEP_MS: u64 = 100;

        async fn measure_run(parallel: bool) -> std::time::Duration {
            let dir = TempDir::new().unwrap();
            let log_path = dir.path().join("events.jsonl");
            let session = Arc::new(Session::create(log_path).await.unwrap());

            // Sequential reference: interleave each RO tool with a
            // non-RO singleton so `group_tool_calls` breaks the run
            // and the orchestrator falls onto the sequential branch.
            // The non-RO tools are zero-cost so the only meaningful
            // contribution to elapsed time is the three SLEEP_MS RO
            // tools.
            let initial = if parallel {
                "{\"tool_call\":{\"name\":\"ro.a\",\"args\":{}}}\n\
                 {\"tool_call\":{\"name\":\"ro.b\",\"args\":{}}}\n\
                 {\"tool_call\":{\"name\":\"ro.c\",\"args\":{}}}\n\
                 {\"done\":\"tool_use\"}\n"
            } else {
                "{\"tool_call\":{\"name\":\"ro.a\",\"args\":{}}}\n\
                 {\"tool_call\":{\"name\":\"breaker\",\"args\":{}}}\n\
                 {\"tool_call\":{\"name\":\"ro.b\",\"args\":{}}}\n\
                 {\"tool_call\":{\"name\":\"breaker\",\"args\":{}}}\n\
                 {\"tool_call\":{\"name\":\"ro.c\",\"args\":{}}}\n\
                 {\"done\":\"tool_use\"}\n"
            };
            let cont = "{\"done\":\"end_turn\"}\n";
            let provider = Arc::new(
                MockProvider::from_responses(vec![initial.into(), cont.into()])
                    .expect("construct mock"),
            );

            let mut dispatcher = ToolDispatcher::new();
            for n in ["ro.a", "ro.b", "ro.c"] {
                dispatcher
                    .register(Box::new(SleepyTool {
                        name: n.into(),
                        read_only: true,
                        sleep_ms: SLEEP_MS,
                    }))
                    .unwrap();
            }
            if !parallel {
                dispatcher
                    .register(Box::new(SleepyTool {
                        name: "breaker".into(),
                        read_only: false,
                        sleep_ms: 0,
                    }))
                    .unwrap();
            }
            let dispatcher = Arc::new(dispatcher);

            let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
            let ctx = ToolCtx::default();
            let req = ChatRequest {
                system: None,
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatBlock::Text("kick off".into())],
                }],
                parallel_tool_calls_allowed: true,
            };

            let started = std::time::Instant::now();
            run_request_loop(
                Arc::clone(&session),
                Arc::clone(&provider),
                req,
                MessageId::new(),
                None,
                0,
                pending,
                dispatcher.as_ref(),
                &ctx,
                true,
                None,
            )
            .await
            .expect("turn completes");
            started.elapsed()
        }

        let sequential = measure_run(false).await;
        let parallel = measure_run(true).await;

        // Parallel must beat 2/3 of the sequential reference. With three
        // equal-cost tools any real concurrency lands near 1/3; 2/3 is
        // the slop boundary that catches a regression to fully-sequential
        // dispatch even on a heavily-loaded CI worker.
        let threshold = sequential.mul_f64(2.0 / 3.0);
        assert!(
            parallel < threshold,
            "parallel dispatch did not beat 2/3 of sequential reference: \
             sequential={sequential:?}, parallel={parallel:?}, threshold={threshold:?}"
        );
    }

    /// F-599 DoD: every `ToolCallStarted` in the parallel batch carries
    /// the same `Some(parallel_group)` id; results come back in
    /// original-call order in the continuation request even if the
    /// underlying tasks finish out of order (call B sleeps the longest
    /// but is at index 1).
    #[tokio::test]
    async fn parallel_dispatch_emits_shared_group_and_reassembles_in_call_order() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        let initial = "{\"tool_call\":{\"name\":\"ro.a\",\"args\":{}}}\n\
                       {\"tool_call\":{\"name\":\"ro.b\",\"args\":{}}}\n\
                       {\"tool_call\":{\"name\":\"ro.c\",\"args\":{}}}\n\
                       {\"done\":\"tool_use\"}\n";
        let cont = "{\"done\":\"end_turn\"}\n";
        let provider = Arc::new(
            MockProvider::from_responses(vec![initial.into(), cont.into()])
                .expect("construct mock"),
        );

        let mut dispatcher = ToolDispatcher::new();
        // Make B the slowest so its task finishes last — order must
        // still be a, b, c on the continuation.
        dispatcher
            .register(Box::new(SleepyTool {
                name: "ro.a".into(),
                read_only: true,
                sleep_ms: 30,
            }))
            .unwrap();
        dispatcher
            .register(Box::new(SleepyTool {
                name: "ro.b".into(),
                read_only: true,
                sleep_ms: 120,
            }))
            .unwrap();
        dispatcher
            .register(Box::new(SleepyTool {
                name: "ro.c".into(),
                read_only: true,
                sleep_ms: 60,
            }))
            .unwrap();
        let dispatcher = Arc::new(dispatcher);

        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let ctx = ToolCtx::default();

        let req = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::Text("kick off".into())],
            }],
            parallel_tool_calls_allowed: true,
        };

        let mut rx = session.event_tx.subscribe();

        run_request_loop(
            Arc::clone(&session),
            Arc::clone(&provider),
            req,
            MessageId::new(),
            None,
            0,
            pending,
            dispatcher.as_ref(),
            &ctx,
            true,
            None,
        )
        .await
        .expect("turn completes");

        // Drain events; collect ToolCallStarted parallel_group ids in
        // original-call order. All three must share the same group.
        let mut groups: Vec<Option<u32>> = vec![];
        let mut completed_names: Vec<String> = vec![];
        while let Ok((_, ev)) = rx.try_recv() {
            match ev {
                Event::ToolCallStarted { parallel_group, .. } => {
                    groups.push(parallel_group);
                }
                Event::ToolCallCompleted { result, .. } => {
                    if let Some(t) = result.get("tool").and_then(|v| v.as_str()) {
                        completed_names.push(t.into());
                    }
                }
                _ => {}
            }
        }
        assert_eq!(groups.len(), 3, "three tool calls started");
        assert!(
            groups.iter().all(|g| g.is_some()),
            "all three carry a parallel_group: got {:?}",
            groups
        );
        let g0 = groups[0];
        assert!(
            groups.iter().all(|g| *g == g0),
            "all three share the same parallel_group id: got {:?}",
            groups
        );
        // Reassembly: ToolCallCompleted events must arrive in
        // original-call order regardless of task completion order.
        assert_eq!(
            completed_names,
            vec!["ro.a", "ro.b", "ro.c"],
            "results re-emitted in original-call order"
        );

        // Continuation request must include tool-result blocks in
        // original-call order — provider receives them as user blocks.
        let reqs = provider.recorded_requests();
        assert_eq!(reqs.len(), 2, "two provider passes");
        let user_blocks: Vec<&ChatBlock> = reqs[1]
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::User)
            .flat_map(|m| m.content.iter())
            .collect();
        let result_tools: Vec<&str> = user_blocks
            .iter()
            .filter_map(|b| match b {
                ChatBlock::ToolResult { result, .. } => result.get("tool").and_then(|v| v.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            result_tools,
            vec!["ro.a", "ro.b", "ro.c"],
            "tool results in continuation are in original-call order"
        );
    }

    /// F-599: a singleton tool call (one tool in the turn) does NOT
    /// receive a `parallel_group` id — it stays `None` so existing
    /// consumers that filter on `parallel_group.is_some()` keep working.
    #[tokio::test]
    async fn singleton_tool_call_has_no_parallel_group() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        let initial = "{\"tool_call\":{\"name\":\"ro.solo\",\"args\":{}}}\n\
                       {\"done\":\"tool_use\"}\n";
        let cont = "{\"done\":\"end_turn\"}\n";
        let provider = Arc::new(
            MockProvider::from_responses(vec![initial.into(), cont.into()])
                .expect("construct mock"),
        );

        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register(Box::new(SleepyTool {
                name: "ro.solo".into(),
                read_only: true,
                sleep_ms: 5,
            }))
            .unwrap();
        let dispatcher = Arc::new(dispatcher);

        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let ctx = ToolCtx::default();
        let mut rx = session.event_tx.subscribe();

        run_request_loop(
            Arc::clone(&session),
            Arc::clone(&provider),
            ChatRequest {
                system: None,
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatBlock::Text("kick off".into())],
                }],
                parallel_tool_calls_allowed: true,
            },
            MessageId::new(),
            None,
            0,
            pending,
            dispatcher.as_ref(),
            &ctx,
            true,
            None,
        )
        .await
        .expect("turn completes");

        let mut groups: Vec<Option<u32>> = vec![];
        while let Ok((_, ev)) = rx.try_recv() {
            if let Event::ToolCallStarted { parallel_group, .. } = ev {
                groups.push(parallel_group);
            }
        }
        assert_eq!(groups, vec![None], "singleton stays parallel_group: None");
    }

    /// F-599: a mixed batch (read-only + non-read-only) collapses to
    /// sequential singletons — `parallel_group` is `None` on every call,
    /// preserving the pre-F-599 wire shape so any non-read-only tool is
    /// still individually approvable.
    #[tokio::test]
    async fn mixed_batch_collapses_to_sequential_singletons() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        // Two RO + one WRITE in the same turn.
        let initial = "{\"tool_call\":{\"name\":\"ro.a\",\"args\":{}}}\n\
                       {\"tool_call\":{\"name\":\"ro.b\",\"args\":{}}}\n\
                       {\"tool_call\":{\"name\":\"write.x\",\"args\":{}}}\n\
                       {\"done\":\"tool_use\"}\n";
        let cont = "{\"done\":\"end_turn\"}\n";
        let provider = Arc::new(
            MockProvider::from_responses(vec![initial.into(), cont.into()])
                .expect("construct mock"),
        );

        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register(Box::new(SleepyTool {
                name: "ro.a".into(),
                read_only: true,
                sleep_ms: 5,
            }))
            .unwrap();
        dispatcher
            .register(Box::new(SleepyTool {
                name: "ro.b".into(),
                read_only: true,
                sleep_ms: 5,
            }))
            .unwrap();
        dispatcher
            .register(Box::new(SleepyTool {
                name: "write.x".into(),
                read_only: false,
                sleep_ms: 5,
            }))
            .unwrap();
        let dispatcher = Arc::new(dispatcher);

        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let ctx = ToolCtx::default();
        let mut rx = session.event_tx.subscribe();

        run_request_loop(
            Arc::clone(&session),
            Arc::clone(&provider),
            ChatRequest {
                system: None,
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatBlock::Text("kick".into())],
                }],
                parallel_tool_calls_allowed: true,
            },
            MessageId::new(),
            None,
            0,
            pending,
            dispatcher.as_ref(),
            &ctx,
            true, // auto-approve so write.x doesn't block on a prompt
            None,
        )
        .await
        .expect("turn completes");

        // Two RO calls coalesce into one parallel batch (Some(g)) — the
        // write.x singleton breaks the run, so it's `None` and the prior
        // RO pair completes as a single parallel group.
        let mut groups: Vec<Option<u32>> = vec![];
        while let Ok((_, ev)) = rx.try_recv() {
            if let Event::ToolCallStarted { parallel_group, .. } = ev {
                groups.push(parallel_group);
            }
        }
        assert_eq!(groups.len(), 3);
        assert!(groups[0].is_some(), "ro.a in parallel batch");
        assert_eq!(groups[1], groups[0], "ro.b shares ro.a's group");
        assert_eq!(groups[2], None, "write.x is its own singleton (None)");
    }

    /// F-599: errors from one tool in a parallel batch don't
    /// short-circuit the others. Every call's `ToolCallCompleted` fires
    /// and the error propagates back through `tool_call_id`-keyed
    /// continuation blocks.
    #[tokio::test]
    async fn parallel_batch_one_tool_error_does_not_short_circuit_others() {
        struct ErroringTool;
        #[async_trait::async_trait]
        impl Tool for ErroringTool {
            fn name(&self) -> &str {
                "ro.err"
            }
            fn approval_preview(&self, _args: &serde_json::Value) -> ApprovalPreview {
                ApprovalPreview {
                    description: "errs".into(),
                }
            }
            fn read_only(&self) -> bool {
                true
            }
            async fn invoke(&self, _args: &serde_json::Value, _ctx: &ToolCtx) -> serde_json::Value {
                serde_json::json!({ "error": "synthetic failure" })
            }
        }

        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        let initial = "{\"tool_call\":{\"name\":\"ro.a\",\"args\":{}}}\n\
                       {\"tool_call\":{\"name\":\"ro.err\",\"args\":{}}}\n\
                       {\"tool_call\":{\"name\":\"ro.c\",\"args\":{}}}\n\
                       {\"done\":\"tool_use\"}\n";
        let cont = "{\"done\":\"end_turn\"}\n";
        let provider = Arc::new(
            MockProvider::from_responses(vec![initial.into(), cont.into()])
                .expect("construct mock"),
        );

        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register(Box::new(SleepyTool {
                name: "ro.a".into(),
                read_only: true,
                sleep_ms: 5,
            }))
            .unwrap();
        dispatcher.register(Box::new(ErroringTool)).unwrap();
        dispatcher
            .register(Box::new(SleepyTool {
                name: "ro.c".into(),
                read_only: true,
                sleep_ms: 5,
            }))
            .unwrap();
        let dispatcher = Arc::new(dispatcher);

        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let ctx = ToolCtx::default();
        let mut rx = session.event_tx.subscribe();

        run_request_loop(
            Arc::clone(&session),
            Arc::clone(&provider),
            ChatRequest {
                system: None,
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatBlock::Text("kick".into())],
                }],
                parallel_tool_calls_allowed: true,
            },
            MessageId::new(),
            None,
            0,
            pending,
            dispatcher.as_ref(),
            &ctx,
            true,
            None,
        )
        .await
        .expect("turn completes");

        let mut completed_count = 0;
        let mut tool_returned_oks: Vec<bool> = vec![];
        while let Ok((_, ev)) = rx.try_recv() {
            match ev {
                Event::ToolCallCompleted { .. } => completed_count += 1,
                Event::ToolReturned { ok, .. } => tool_returned_oks.push(ok),
                _ => {}
            }
        }
        assert_eq!(
            completed_count, 3,
            "every tool in the batch produces a Completed event"
        );
        // The middle tool errored; the other two reported ok.
        assert_eq!(tool_returned_oks, vec![true, false, true]);
    }

    /// F-599 regression: the byte budget must apply on the parallel
    /// dispatch path. Prior to this fix the parallel branch called
    /// `tool.invoke` directly, bypassing the dispatcher's budget gate —
    /// a session could read unbounded data through a parallel batch
    /// after the per-call cap would have refused further calls.
    ///
    /// This test charges the budget close to its limit, then runs a
    /// parallel batch of three RO tools whose results push the counter
    /// past the limit mid-batch. After the batch the budget must be
    /// exhausted, and a *follow-up* call (sequential singleton) must
    /// see the synthetic "session byte budget exceeded" error rather
    /// than executing the tool.
    #[tokio::test]
    async fn parallel_batch_respects_session_byte_budget_and_blocks_followups() {
        struct PayloadTool {
            name: String,
            // Each call returns a {"content": "<n bytes>"} so the
            // dispatcher charges the budget by `n` bytes (matches the
            // `result_byte_cost` `fs.read` shape).
            payload_bytes: usize,
        }
        #[async_trait::async_trait]
        impl Tool for PayloadTool {
            fn name(&self) -> &str {
                &self.name
            }
            fn approval_preview(&self, _args: &serde_json::Value) -> ApprovalPreview {
                ApprovalPreview {
                    description: "payload".into(),
                }
            }
            fn read_only(&self) -> bool {
                true
            }
            async fn invoke(&self, _args: &serde_json::Value, _ctx: &ToolCtx) -> serde_json::Value {
                serde_json::json!({ "content": "x".repeat(self.payload_bytes) })
            }
        }

        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        let initial = "{\"tool_call\":{\"name\":\"ro.a\",\"args\":{}}}\n\
                       {\"tool_call\":{\"name\":\"ro.b\",\"args\":{}}}\n\
                       {\"tool_call\":{\"name\":\"ro.c\",\"args\":{}}}\n\
                       {\"done\":\"tool_use\"}\n";
        let followup_initial = "{\"tool_call\":{\"name\":\"ro.a\",\"args\":{}}}\n\
                                {\"done\":\"tool_use\"}\n";
        let cont = "{\"done\":\"end_turn\"}\n";
        let provider = Arc::new(
            MockProvider::from_responses(vec![
                initial.into(),
                cont.into(),
                followup_initial.into(),
                cont.into(),
            ])
            .expect("construct mock"),
        );

        let mut dispatcher = ToolDispatcher::new();
        for n in ["ro.a", "ro.b", "ro.c"] {
            dispatcher
                .register(Box::new(PayloadTool {
                    name: n.into(),
                    payload_bytes: 100,
                }))
                .unwrap();
        }
        let dispatcher = Arc::new(dispatcher);

        let budget = Arc::new(crate::byte_budget::ByteBudget::new(200));
        let ctx = ToolCtx {
            byte_budget: Some(Arc::clone(&budget)),
            ..ToolCtx::default()
        };
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));

        run_request_loop(
            Arc::clone(&session),
            Arc::clone(&provider),
            ChatRequest {
                system: None,
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatBlock::Text("first".into())],
                }],
                parallel_tool_calls_allowed: true,
            },
            MessageId::new(),
            None,
            0,
            Arc::clone(&pending),
            dispatcher.as_ref(),
            &ctx,
            true,
            None,
        )
        .await
        .expect("first turn completes");

        // Budget is exhausted: at least 2 of 3 tools landed (parallel
        // tasks observe the same atomic counter; the third may either
        // see the exhaustion pre-check or land a charge that pushes the
        // counter past the limit). Either way a follow-up call must be
        // refused.
        assert!(
            budget.is_exhausted(),
            "budget should be exhausted after parallel batch: consumed={}/{}",
            budget.consumed(),
            budget.limit()
        );

        let mut rx = session.event_tx.subscribe();
        run_request_loop(
            Arc::clone(&session),
            Arc::clone(&provider),
            ChatRequest {
                system: None,
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatBlock::Text("second".into())],
                }],
                parallel_tool_calls_allowed: true,
            },
            MessageId::new(),
            None,
            0,
            pending,
            dispatcher.as_ref(),
            &ctx,
            true,
            None,
        )
        .await
        .expect("follow-up turn completes");

        // The follow-up tool result must carry the budget-exceeded
        // error — proving the *sequential* path also routes through
        // `invoke_with_budget`.
        let mut saw_exceeded = false;
        while let Ok((_, ev)) = rx.try_recv() {
            if let Event::ToolCallCompleted { result, .. } = ev {
                if let Some(err) = result.get("error").and_then(|v| v.as_str()) {
                    if err.contains("session byte budget exceeded") {
                        saw_exceeded = true;
                    }
                }
            }
        }
        assert!(
            saw_exceeded,
            "follow-up tool call after budget exhaustion must surface the budget error"
        );
    }

    /// F-599 regression: a panicking tool in a parallel batch is
    /// attributed to the *correct* index, the orchestrator does not
    /// itself panic, and the other batch members complete normally.
    ///
    /// Pre-fix the spawned task panicked out of the future, the
    /// JoinSet returned a `JoinError` with no idx, and the recovery
    /// code mis-attributed the error to the first empty slot — or
    /// triggered the unconditional `.expect("every batch index must
    /// have a result")` later in the reassembly loop.
    #[tokio::test]
    async fn parallel_batch_panicking_tool_is_attributed_to_correct_index() {
        struct PanickingTool;
        #[async_trait::async_trait]
        impl Tool for PanickingTool {
            fn name(&self) -> &str {
                "ro.panic"
            }
            fn approval_preview(&self, _args: &serde_json::Value) -> ApprovalPreview {
                ApprovalPreview {
                    description: "panics".into(),
                }
            }
            fn read_only(&self) -> bool {
                true
            }
            async fn invoke(&self, _args: &serde_json::Value, _ctx: &ToolCtx) -> serde_json::Value {
                panic!("synthetic panic for index attribution test");
            }
        }

        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let session = Arc::new(Session::create(log_path).await.unwrap());

        // Order: ro.a (ok) → ro.panic (panics) → ro.c (ok). Without
        // the catch_unwind fix the JoinError arrives last (after the
        // OK siblings finish) and the "first empty slot" recovery
        // would either misattribute it or have nowhere to put it
        // (panicking the orchestrator on the .expect).
        let initial = "{\"tool_call\":{\"name\":\"ro.a\",\"args\":{}}}\n\
                       {\"tool_call\":{\"name\":\"ro.panic\",\"args\":{}}}\n\
                       {\"tool_call\":{\"name\":\"ro.c\",\"args\":{}}}\n\
                       {\"done\":\"tool_use\"}\n";
        let cont = "{\"done\":\"end_turn\"}\n";
        let provider = Arc::new(
            MockProvider::from_responses(vec![initial.into(), cont.into()])
                .expect("construct mock"),
        );

        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register(Box::new(SleepyTool {
                name: "ro.a".into(),
                read_only: true,
                sleep_ms: 5,
            }))
            .unwrap();
        dispatcher.register(Box::new(PanickingTool)).unwrap();
        dispatcher
            .register(Box::new(SleepyTool {
                name: "ro.c".into(),
                read_only: true,
                sleep_ms: 5,
            }))
            .unwrap();
        let dispatcher = Arc::new(dispatcher);

        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let ctx = ToolCtx::default();
        let mut rx = session.event_tx.subscribe();

        run_request_loop(
            Arc::clone(&session),
            Arc::clone(&provider),
            ChatRequest {
                system: None,
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatBlock::Text("kick".into())],
                }],
                parallel_tool_calls_allowed: true,
            },
            MessageId::new(),
            None,
            0,
            pending,
            dispatcher.as_ref(),
            &ctx,
            true,
            None,
        )
        .await
        .expect("turn completes despite tool panic");

        // Reassemble (tool_id → result.error?) tuples in event order.
        let mut tool_results: Vec<(String, Option<String>)> = vec![];
        while let Ok((_, ev)) = rx.try_recv() {
            if let Event::ToolCallStarted { tool, .. } = &ev {
                tool_results.push((tool.clone(), None));
            }
            if let Event::ToolCallCompleted { result, .. } = &ev {
                if let Some(slot) = tool_results.iter_mut().rev().find(|(_, e)| e.is_none()) {
                    slot.1 = Some(
                        result
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("<ok>")
                            .to_string(),
                    );
                }
            }
        }

        let by_name: std::collections::HashMap<&str, &str> = tool_results
            .iter()
            .map(|(name, err)| (name.as_str(), err.as_deref().unwrap_or("<no result>")))
            .collect();

        assert_eq!(
            by_name.get("ro.a"),
            Some(&"<ok>"),
            "ro.a completed normally"
        );
        assert_eq!(
            by_name.get("ro.c"),
            Some(&"<ok>"),
            "ro.c completed normally"
        );
        let panic_err = by_name.get("ro.panic").copied().unwrap_or("");
        assert!(
            panic_err.contains("tool task panicked")
                && panic_err.contains("synthetic panic for index attribution test"),
            "ro.panic must carry a panic-attributed error; got: {panic_err}"
        );
    }
}

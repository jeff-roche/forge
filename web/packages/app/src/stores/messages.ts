import { createStore, produce, reconcile } from 'solid-js/store';
import type { SessionId } from '@forge/ipc';

// ---------------------------------------------------------------------------
// Event shapes arriving from the IPC bridge (session:event payload)
// ---------------------------------------------------------------------------

/** Preview data from the shell for a pending tool call approval. */
export interface ApprovalPreview {
  /** Human-readable description of what the tool call will do. */
  description: string;
}

export type SessionEvent =
  | { kind: 'UserMessage'; text: string; message_id: string }
  // F-145: `branch_parent` + `branch_variant_index` + provider/model/timestamp
  // are optional so the bulk of the adapter (UserMessage-only scenarios, older
  // fixtures) need not spell them out. The Rust wire shape always includes
  // `branch_parent` / `branch_variant_index` for AssistantMessage; omission
  // on the TS side maps to "root variant" (parent=null, index=0).
  | {
      kind: 'AssistantMessage';
      text: string;
      message_id: string;
      branch_parent?: string | null;
      branch_variant_index?: number;
      provider?: string;
      model?: string;
      at?: string;
    }
  | { kind: 'AssistantDelta'; delta: string; message_id: string }
  | { kind: 'ToolCallStarted'; tool_call_id: string; tool_name: string; args_json: string; batch_id?: string }
  // tool_name/args_json are optional — the Rust wire event carries only id+preview,
  // and the approval always follows a ToolCallStarted, so the store normally
  // transitions an existing placeholder. They remain for the fallback branch
  // (placeholder missing) used by the pre-wire unit tests.
  | { kind: 'ToolCallApprovalRequested'; tool_call_id: string; tool_name?: string; args_json?: string; preview: ApprovalPreview }
  | {
      kind: 'ToolCallCompleted';
      tool_call_id: string;
      result_summary: string;
      /**
       * F-447: structured carry-through from the Rust wire `result` object.
       * `result_summary` is retained for existing consumers (truncated JSON
       * blob); the fields below are what the expanded card body reads so it
       * doesn't have to re-parse the summary.
       */
      result_ok?: boolean;
      result_preview?: string;
      /**
       * F-447: preserved for parity with the wire shape. The store routes
       * `result.ok === false` through the `ToolCallFailed` code path so the
       * expanded card can render the error verbatim instead of a truncated
       * JSON blob.
       */
      result_error?: string;
      /**
       * F-447: authoritative duration from the daemon's `tool_call_completed`
       * wire event. Phase 2 recomputed duration as `Date.now() - started_at`,
       * which drifts whenever the daemon and client clocks disagree or the
       * completion arrives out-of-order. Prefer the wire value when present.
       */
      duration_ms?: number;
    }
  | { kind: 'ToolCallFailed'; tool_call_id: string; error: string }
  // F-144 / F-145: branch-tree mutations. `BranchSelected` flips the active
  // variant for a given branch point; `BranchDeleted` tombstones a variant.
  | { kind: 'BranchSelected'; parent: string; selected: string }
  | { kind: 'BranchDeleted'; parent: string; variant_index: number }
  | { kind: 'Error'; message: string }
  | { kind: 'StreamingStarted' }
  | { kind: 'StreamingStopped' }
  // F-136: orchestrator spawned a sub-agent; ChatPane mounts a SubAgentBanner
  // inline at the spawn position. `agent_name` is optional because the Rust
  // `SubAgentSpawned` wire event carries only parent/child/from_msg. When the
  // name is known (F-137 stores it on the orchestrator instance, but it does
  // not ride the event today), the shell may enrich the payload; otherwise
  // the banner falls back to `child` id as the label.
  | {
      kind: 'SubAgentSpawned';
      parent_instance_id: string;
      child_instance_id: string;
      from_msg: string;
      agent_name?: string;
      // F-448 Phase 3: header chips. Optional because the orchestrator does
      // not always know the child's model / tool surface at spawn time —
      // omission is the norm today, and the banner hides each chip cleanly
      // when the corresponding field is undefined.
      model?: string;
      tool_count?: number;
    }
  // F-136: sub-agent background lifecycle reached a terminal state. Flips any
  // banner whose child id matches from `running` → `done`. Emitted by
  // `forge_session::BackgroundAgentRegistry` (F-137).
  | {
      kind: 'BackgroundAgentCompleted';
      instance_id: string;
    }
  // F-598: orchestrator collapsed `summarized_turns` older turns into the
  // privileged summary message at `summary_msg_id`. The store mounts an
  // inline marker turn at the summary's position so the user sees a
  // visible boundary between compacted history and live context.
  | {
      kind: 'ContextCompacted';
      summary_msg_id: string;
      summarized_turns: number;
      trigger: 'AutoAt98Pct' | 'UserRequested';
    }
  // F-749: typed provider-call failure surfaced inline in the transcript.
  // ChatPane renders a `<TurnErrorCard>` for each turn that hits this state
  // at the position where the assistant response would have appeared. The
  // store discards any partial assistant turn at `message_id` because the
  // orchestrator's protocol guarantees no `AssistantDelta` lands after a
  // `TurnError` — the empty bubble would otherwise dangle.
  | {
      kind: 'TurnError';
      message_id: string;
      error_kind:
        | 'auth'
        | 'rate_limit'
        | 'network'
        | 'server'
        | 'malformed_response'
        | 'unknown';
      message: string;
      retriable: boolean;
      retry_after_secs?: number;
      raw?: string;
    };

// ---------------------------------------------------------------------------
// Chat turn shapes (derived, used for rendering)
// ---------------------------------------------------------------------------

export type ToolCallStatus = 'in-progress' | 'awaiting-approval' | 'completed' | 'errored';

/** F-136: sub-agent banner lifecycle state as rendered in the ChatPane. */
export type SubAgentStatus = 'queued' | 'running' | 'done' | 'error' | 'killed';

export type ChatTurn =
  | { type: 'user'; text: string; message_id: string }
  | {
      type: 'assistant';
      text: string;
      message_id: string;
      isStreaming: boolean;
      /**
       * F-145: branch tree coordinates. `branch_parent` points at the branch
       * root (the original message's id); `null` for non-branched turns.
       * `branch_variant_index` is 0 for the root and N>=1 for siblings.
       * Provider/model/at support the metadata popover (spec §15.5).
       */
      branch_parent: string | null;
      branch_variant_index: number;
      provider?: string;
      model?: string;
      at?: string;
    }
  | {
      type: 'tool_placeholder';
      tool_call_id: string;
      tool_name: string;
      args_json: string;
      batch_id?: string;
      status: ToolCallStatus;
      started_at: number;
      duration_ms?: number;
      result_summary?: string;
      /** F-447: structured pieces of the completed-call `result` object. */
      result_ok?: boolean;
      result_preview?: string;
      error?: string;
      /** Populated when status is 'awaiting-approval'. */
      preview?: ApprovalPreview;
    }
  | {
      type: 'sub_agent_banner';
      /** Child `AgentInstanceId` the banner tracks. */
      child_instance_id: string;
      /** Emitting parent agent instance id (often the session itself today). */
      parent_instance_id: string;
      /** Optional display name — falls back to the child id's short prefix. */
      agent_name?: string;
      /** Live state. Starts `running` on spawn; flips to `done` on terminal. */
      status: SubAgentStatus;
      /** ms epoch at which the banner was mounted (spawn event seen). */
      started_at: number;
      /** Last observed step summary. Populated by future step-routing work. */
      last_step_summary?: string;
      /** Count of steps seen so far — reserved for future step-routing work. */
      step_count?: number;
      /** F-448 Phase 3: child agent's active model label (e.g. "sonnet-4.5"). */
      model?: string;
      /** F-448 Phase 3: number of tools exposed to the child agent. */
      tool_count?: number;
      /**
       * F-399: Human-readable error detail set when `status === 'error'`.
       * Rendered as an error-detail row below the header.
       */
      error_detail?: string;
    }
  | { type: 'error'; message: string }
  // F-598: marker turn rendered inline at the position of the summary
  // message. Anchors the UI to the summary so a click-through can scroll
  // the user back to the (collapsed) summary content.
  | {
      type: 'context_compacted';
      summary_msg_id: string;
      summarized_turns: number;
      trigger: 'AutoAt98Pct' | 'UserRequested';
    }
  // F-749: failed-turn marker. ChatPane renders the `<TurnErrorCard>` at
  // this turn's position with action buttons keyed off `error_kind`.
  | {
      type: 'turn_error';
      message_id: string;
      error_kind:
        | 'auth'
        | 'rate_limit'
        | 'network'
        | 'server'
        | 'malformed_response'
        | 'unknown';
      message: string;
      retriable: boolean;
      retry_after_secs?: number;
      raw?: string;
    };

// ---------------------------------------------------------------------------
// F-145 — branch groups
//
// Every AssistantMessage has an implicit "branch root" id: `branch_parent`
// if set, else its own `message_id`. Assistant turns that share a root form
// a branch group; the group tracks all variant ids, the currently-active id
// (flipped by `BranchSelected`), and the variants that have been tombstoned
// via `BranchDeleted`.
//
// The ChatPane renders only the active variant of a multi-variant group (the
// root alone is rendered identically to today's chrome-less assistant turn).
// The strip / gutter / popover components read straight off this shape.
// ---------------------------------------------------------------------------

export interface BranchGroup {
  /**
   * Ordered list of variant `message_id`s under this branch root — positions
   * match variant indices (element 0 is the root variant; element N is
   * `branch_variant_index == N`). `null` at a position means the variant
   * has been deleted. Storing a sparse array keeps the index→id mapping
   * stable across deletes — the strip's `N of M` label counts live variants
   * but the popover keeps numbering aligned with the log.
   */
  variantIds: Array<string | null>;
  activeVariantId: string;
  /** Tombstoned variant indices (mirrors `variantIds[i] === null`). */
  deletedIndices: number[];
}

// ---------------------------------------------------------------------------
// Per-session messages state
// ---------------------------------------------------------------------------

export interface MessagesState {
  turns: ChatTurn[];
  awaitingResponse: boolean;
  streamingMessageId: string | null;
  /** F-145: branch-root id → group tracking. Empty when no assistant turn has branched. */
  branchGroups: Record<string, BranchGroup>;
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

const [messagesStore, setMessagesStore] = createStore<Record<string, MessagesState>>({});

function ensureSession(sessionId: SessionId): void {
  if (!messagesStore[sessionId]) {
    setMessagesStore(sessionId, {
      turns: [],
      awaitingResponse: false,
      streamingMessageId: null,
      branchGroups: {},
    });
  }
}

export function getMessagesState(sessionId: SessionId): MessagesState {
  ensureSession(sessionId);
  return messagesStore[sessionId]!;
}

export function setAwaitingResponse(sessionId: SessionId, value: boolean): void {
  ensureSession(sessionId);
  setMessagesStore(sessionId, 'awaitingResponse', value);
}

/**
 * F-391: locally drop the "streaming lock" on the composer after the user
 * invokes Stop / Esc. The backend cancel is fired-and-forgotten; we don't
 * wait for its ack before re-enabling the UI, because spec §4.1 treats Stop
 * as an immediate interaction.
 */
export function cancelStream(sessionId: SessionId): void {
  ensureSession(sessionId);
  setMessagesStore(sessionId, 'awaitingResponse', false);
  setMessagesStore(sessionId, 'streamingMessageId', null);
}

/**
 * F-749: locally drop the failed exchange (the originating user turn + the
 * error card) before retrying. The orchestrator's protocol pairs each
 * `TurnError` with the immediately preceding `UserMessage` event, so the
 * range we splice is exactly `[errorIdx - 1, errorIdx]`.
 *
 * Why store-side and not orchestrator-side (`MessageSuperseded`):
 *
 * The orchestrator-side path would emit a new `MessageSuperseded { old_id }`
 * event from a new `rerun_user_message` IPC primitive, and the existing
 * `MessageSuperseded` handler would filter the old turns visually. That's
 * architecturally cleaner — it keeps the log authoritative and the UI a
 * pure projection — but it's a much larger change: new IPC method, new
 * event variant, new orchestrator code path, plus the new event has to
 * survive replay across daemon restart. For F-749 the failed exchange is
 * known-bad and the user is explicitly asking to discard it; the store-side
 * splice produces an identical user-visible transcript without the IPC
 * surface-area expansion. If we later need cross-replay durability for
 * "this turn was retried", we can lift this into `MessageSuperseded` then.
 *
 * The local-only nature of the splice is also why this isn't a SessionEvent
 * — there's no wire payload to dispatch, just a UI-state mutation.
 */
export function removeFailedTurnPair(
  sessionId: SessionId,
  errorTurnIndex: number,
): void {
  ensureSession(sessionId);
  setMessagesStore(
    sessionId,
    produce((state: MessagesState) => {
      // Defensive bounds — `errorTurnIndex` should always be >= 1 (there's
      // always a `UserMessage` immediately before a `TurnError` per the
      // orchestrator's protocol), but a stale index from a re-render race
      // would otherwise splice off the start of the array silently.
      if (errorTurnIndex < 1 || errorTurnIndex >= state.turns.length) return;
      state.turns.splice(errorTurnIndex - 1, 2);
    }),
  );
}

// ---------------------------------------------------------------------------
// F-145 — branch-group helpers
// ---------------------------------------------------------------------------

/**
 * Insert or update an entry in a branch group's `variantIds` at the given
 * index. Grows the array with `null` slots if the variant arrives out-of-
 * order (e.g. replay skipped variant 1 and landed variant 2 first). If the
 * root variant (index 0) has not yet been seen, the group is still created
 * so siblings can attach — the root id defaults to the `rootId` argument,
 * which is the branch-parent (which is always the root's own id).
 */
function registerVariant(
  state: MessagesState,
  rootId: string,
  variantIndex: number,
  messageId: string,
): void {
  let group = state.branchGroups[rootId];
  if (!group) {
    group = {
      variantIds: [rootId],
      activeVariantId: rootId,
      deletedIndices: [],
    };
    state.branchGroups[rootId] = group;
  }
  // Grow the sparse array if needed.
  while (group.variantIds.length <= variantIndex) {
    group.variantIds.push(null);
  }
  group.variantIds[variantIndex] = messageId;
  // If this is a newly-observed sibling (N>=1), it becomes the active
  // variant. The server's `rerun_branch` does **not** auto-emit a
  // `BranchSelected` for the new sibling (see orchestrator.rs doc-comment
  // on `select_branch`: "BranchSelected emitted separately"), so the UI
  // papers over that gap by flipping active on first-observation. Spec
  // §15.1 is silent on the client-vs-server-first activation order; this
  // keeps the UX identical to Claude.ai (new variant lands as active). A
  // later `BranchSelected` event then overrides the choice explicitly.
  // The root (index 0) is "active by default" — registering its own
  // message must not clobber a prior selection.
  if (variantIndex >= 1) {
    group.activeVariantId = messageId;
  } else if (group.activeVariantId === rootId) {
    // First time we see the root — keep activeVariantId pointing at it.
    group.activeVariantId = messageId;
  }
}

/**
 * Count live (non-deleted) variants in a branch group. Used by the strip
 * to render `N of M` and the popover's `M variants of this response` header.
 */
export function liveVariantCount(group: BranchGroup): number {
  return group.variantIds.filter((v) => v !== null).length;
}

/**
 * Return the active variant's position among live variants (1-indexed) and
 * the total live count. Returns `null` when the group is empty or the
 * active id is not in it. The strip renders `${position} of ${total}`.
 */
export function activeVariantPosition(
  group: BranchGroup,
): { position: number; total: number } | null {
  const live = group.variantIds
    .map((id, i) => ({ id, i }))
    .filter((e): e is { id: string; i: number } => e.id !== null);
  if (live.length === 0) return null;
  const pos = live.findIndex((e) => e.id === group.activeVariantId);
  if (pos < 0) return null;
  return { position: pos + 1, total: live.length };
}

/**
 * Cycle the active variant in `direction` (`prev` or `next`) and return the
 * id that should become active. Wraps at boundaries. Skips tombstoned
 * variants. Returns `null` when no live variant exists or the current
 * active id is not in the group.
 */
export function neighbourVariantId(
  group: BranchGroup,
  direction: 'prev' | 'next',
): string | null {
  const live = group.variantIds.filter((v): v is string => v !== null);
  if (live.length === 0) return null;
  const currentIdx = live.indexOf(group.activeVariantId);
  if (currentIdx < 0) return null;
  const step = direction === 'next' ? 1 : -1;
  const nextIdx = (currentIdx + step + live.length) % live.length;
  return live[nextIdx]!;
}

export function pushEvent(sessionId: SessionId, event: SessionEvent): void {
  ensureSession(sessionId);

  switch (event.kind) {
    case 'UserMessage': {
      setMessagesStore(
        produce((s) => {
          s[sessionId]!.turns.push({
            type: 'user',
            text: event.text,
            message_id: event.message_id,
          });
        }),
      );
      break;
    }

    case 'AssistantMessage': {
      setMessagesStore(
        produce((s) => {
          const state = s[sessionId]!;
          // F-145: branch-root resolution — `branch_parent` (when set) is
          // the root id; otherwise this turn is itself a root and the group
          // keys on its own message_id. Missing on pre-F-145 wire events,
          // in which case we treat the turn as a lone root (variant_index 0).
          const branchParent = event.branch_parent ?? null;
          const variantIndex = event.branch_variant_index ?? 0;
          const rootId = branchParent ?? event.message_id;

          // If there's a streaming turn for this message_id, update it in place.
          const idx = state.turns.findIndex(
            (t) => t.type === 'assistant' && t.message_id === event.message_id,
          );
          if (idx >= 0) {
            const turn = state.turns[idx] as Extract<ChatTurn, { type: 'assistant' }>;
            turn.text = event.text;
            turn.isStreaming = false;
            turn.branch_parent = branchParent;
            turn.branch_variant_index = variantIndex;
            if (event.provider !== undefined) turn.provider = event.provider;
            if (event.model !== undefined) turn.model = event.model;
            if (event.at !== undefined) turn.at = event.at;
          } else {
            const next: Extract<ChatTurn, { type: 'assistant' }> = {
              type: 'assistant',
              text: event.text,
              message_id: event.message_id,
              isStreaming: false,
              branch_parent: branchParent,
              branch_variant_index: variantIndex,
            };
            if (event.provider !== undefined) next.provider = event.provider;
            if (event.model !== undefined) next.model = event.model;
            if (event.at !== undefined) next.at = event.at;
            state.turns.push(next);
          }

          registerVariant(state, rootId, variantIndex, event.message_id);

          state.streamingMessageId = null;
          state.awaitingResponse = false;
        }),
      );
      break;
    }

    case 'AssistantDelta': {
      setMessagesStore(
        produce((s) => {
          const state = s[sessionId]!;
          state.awaitingResponse = false;
          const idx = state.turns.findIndex(
            (t) => t.type === 'assistant' && t.message_id === event.message_id,
          );
          if (idx >= 0) {
            const turn = state.turns[idx] as Extract<ChatTurn, { type: 'assistant' }>;
            turn.text += event.delta;
          } else {
            state.turns.push({
              type: 'assistant',
              text: event.delta,
              message_id: event.message_id,
              isStreaming: true,
              branch_parent: null,
              branch_variant_index: 0,
            });
            state.streamingMessageId = event.message_id;
          }
        }),
      );
      break;
    }

    case 'BranchSelected': {
      setMessagesStore(
        produce((s) => {
          const state = s[sessionId]!;
          const group = state.branchGroups[event.parent];
          if (group) {
            group.activeVariantId = event.selected;
          } else {
            // A BranchSelected arriving before its AssistantMessages is a
            // replay-order drift; initialise a minimal group so the strip
            // has something to bind to when the messages land.
            state.branchGroups[event.parent] = {
              variantIds: [event.parent],
              activeVariantId: event.selected,
              deletedIndices: [],
            };
          }
        }),
      );
      break;
    }

    case 'BranchDeleted': {
      setMessagesStore(
        produce((s) => {
          const state = s[sessionId]!;
          const group = state.branchGroups[event.parent];
          if (!group) return;
          const idx = event.variant_index;
          const deletedId =
            idx < group.variantIds.length ? group.variantIds[idx] : null;
          if (idx < group.variantIds.length) group.variantIds[idx] = null;
          if (!group.deletedIndices.includes(idx)) {
            group.deletedIndices.push(idx);
          }
          // If the active variant was the one deleted, fall back to the
          // lowest-indexed live variant. Preference: the root (0), else the
          // first live sibling. The orchestrator refuses a root-delete with
          // live siblings, so one of these will always exist.
          if (deletedId !== null && group.activeVariantId === deletedId) {
            const fallback = group.variantIds.find((v): v is string => v !== null);
            if (fallback) {
              group.activeVariantId = fallback;
            }
          }
        }),
      );
      break;
    }

    case 'ToolCallStarted': {
      setMessagesStore(
        produce((s) => {
          s[sessionId]!.turns.push({
            type: 'tool_placeholder',
            tool_call_id: event.tool_call_id,
            tool_name: event.tool_name,
            args_json: event.args_json,
            ...(event.batch_id !== undefined ? { batch_id: event.batch_id } : {}),
            status: 'in-progress',
            started_at: Date.now(),
          });
        }),
      );
      break;
    }

    case 'ToolCallApprovalRequested': {
      setMessagesStore(
        produce((s) => {
          const state = s[sessionId]!;
          // If there's already a tool_placeholder for this call (from ToolCallStarted),
          // transition it to awaiting-approval and attach the preview.
          const idx = state.turns.findIndex(
            (t) => t.type === 'tool_placeholder' && t.tool_call_id === event.tool_call_id,
          );
          if (idx >= 0) {
            const turn = state.turns[idx] as Extract<ChatTurn, { type: 'tool_placeholder' }>;
            turn.status = 'awaiting-approval';
            turn.preview = event.preview;
          } else {
            // No prior ToolCallStarted — push a fresh placeholder. In the
            // Rust wire path this branch is unreachable (approval always
            // follows a started event), so tool_name/args_json fall back to
            // safe defaults when the event omits them.
            state.turns.push({
              type: 'tool_placeholder',
              tool_call_id: event.tool_call_id,
              tool_name: event.tool_name ?? 'unknown',
              args_json: event.args_json ?? '{}',
              status: 'awaiting-approval',
              started_at: Date.now(),
              preview: event.preview,
            });
          }
        }),
      );
      break;
    }

    case 'ToolCallCompleted': {
      setMessagesStore(
        produce((s) => {
          const state = s[sessionId]!;
          const idx = state.turns.findIndex(
            (t) => t.type === 'tool_placeholder' && t.tool_call_id === event.tool_call_id,
          );
          if (idx >= 0) {
            const turn = state.turns[idx] as Extract<ChatTurn, { type: 'tool_placeholder' }>;
            // F-447: prefer the wire-reported `result.ok === false` as the
            // errored signal so the status glyph reflects the daemon's own
            // verdict rather than assuming any completion is a success.
            turn.status = event.result_ok === false ? 'errored' : 'completed';
            turn.result_summary = event.result_summary;
            if (event.result_ok !== undefined) turn.result_ok = event.result_ok;
            if (event.result_preview !== undefined) {
              turn.result_preview = event.result_preview;
            }
            // F-447: a completed-but-failed call (ok=false) carries the
            // error string; route it into `turn.error` so the collapsed
            // row's ✗ glyph has message text to pair with.
            if (event.result_ok === false && event.result_error !== undefined) {
              turn.error = event.result_error;
            }
            // F-447: prefer the wire value; fall back to local elapsed time
            // when the daemon didn't carry a duration (older wire shapes or
            // orphaned completions in tests).
            turn.duration_ms = event.duration_ms ?? (Date.now() - turn.started_at);
          }
        }),
      );
      break;
    }

    case 'ToolCallFailed': {
      setMessagesStore(
        produce((s) => {
          const state = s[sessionId]!;
          const idx = state.turns.findIndex(
            (t) => t.type === 'tool_placeholder' && t.tool_call_id === event.tool_call_id,
          );
          if (idx >= 0) {
            const turn = state.turns[idx] as Extract<ChatTurn, { type: 'tool_placeholder' }>;
            turn.status = 'errored';
            turn.error = event.error;
            turn.duration_ms = Date.now() - turn.started_at;
          }
        }),
      );
      break;
    }

    case 'Error': {
      setMessagesStore(
        produce((s) => {
          const state = s[sessionId]!;
          state.turns.push({ type: 'error', message: event.message });
          state.awaitingResponse = false;
          state.streamingMessageId = null;
        }),
      );
      break;
    }

    // F-136: orchestrator spawn — mount a banner turn inline at the current
    // position. Duplicates (same child_instance_id twice in a row) are
    // ignored so a replay or event re-delivery doesn't stack multiple
    // banners for one child.
    case 'SubAgentSpawned': {
      setMessagesStore(
        produce((s) => {
          const state = s[sessionId]!;
          const existing = state.turns.find(
            (t) =>
              t.type === 'sub_agent_banner' &&
              t.child_instance_id === event.child_instance_id,
          );
          if (existing) return;
          const banner: Extract<ChatTurn, { type: 'sub_agent_banner' }> = {
            type: 'sub_agent_banner',
            child_instance_id: event.child_instance_id,
            parent_instance_id: event.parent_instance_id,
            status: 'running',
            started_at: Date.now(),
          };
          if (event.agent_name !== undefined) {
            banner.agent_name = event.agent_name;
          }
          if (event.model !== undefined) {
            banner.model = event.model;
          }
          if (event.tool_count !== undefined) {
            banner.tool_count = event.tool_count;
          }
          state.turns.push(banner);
        }),
      );
      break;
    }

    // F-136: child lifecycle terminal — flip matching banner to `done`.
    // Unknown child_instance_id is a no-op (replay or out-of-order delivery).
    case 'BackgroundAgentCompleted': {
      setMessagesStore(
        produce((s) => {
          const state = s[sessionId]!;
          const idx = state.turns.findIndex(
            (t) =>
              t.type === 'sub_agent_banner' &&
              t.child_instance_id === event.instance_id,
          );
          if (idx >= 0) {
            const turn = state.turns[idx] as Extract<
              ChatTurn,
              { type: 'sub_agent_banner' }
            >;
            turn.status = 'done';
          }
        }),
      );
      break;
    }

    case 'ContextCompacted': {
      // F-598: append a single inline marker so the user sees a clear
      // boundary between summarized history and live context. The event
      // arrives AFTER the AssistantMessage with `summary_msg_id` (the
      // daemon's emission ordering is load-bearing — see
      // `forge_session::compaction::compact`), so the summary turn is
      // already in `state.turns` when this case runs.
      setMessagesStore(
        produce((s) => {
          const state = s[sessionId]!;
          // Idempotency: a replayed event for the same summary should
          // not stack a second marker.
          const existing = state.turns.find(
            (t) =>
              t.type === 'context_compacted' &&
              t.summary_msg_id === event.summary_msg_id,
          );
          if (existing) return;
          state.turns.push({
            type: 'context_compacted',
            summary_msg_id: event.summary_msg_id,
            summarized_turns: event.summarized_turns,
            trigger: event.trigger,
          });
        }),
      );
      break;
    }

    // F-749: failed turn — replace the empty/streaming assistant placeholder
    // (the orchestrator emits an `AssistantMessage(stream_finalised=true,
    // text="")` immediately before the `TurnError`) with the typed error
    // turn so the card lands at the exact transcript slot the user
    // expects. The originating user turn stays untouched.
    //
    // Event-ordering assumption: the orchestrator emits the finalised
    // (empty) `AssistantMessage` BEFORE the `TurnError` on every error
    // path — see `orchestrator.rs` `ChatChunk::Error` arm. That ordering
    // is what lets us "replace" the assistant slot via the `idx >= 0`
    // branch. The fallback `idx < 0` branch covers out-of-order delivery
    // (replay glitches, log truncation between AssistantMessage and
    // TurnError, or a future orchestrator path that emits TurnError
    // standalone): we append the error card as a fresh turn rather than
    // dropping the failure on the floor, and we still clear
    // `streamingMessageId` + `awaitingResponse` so the composer unlocks
    // regardless of which arm fires.
    case 'TurnError': {
      setMessagesStore(
        produce((s) => {
          const state = s[sessionId]!;
          const idx = state.turns.findIndex(
            (t) => t.type === 'assistant' && t.message_id === event.message_id,
          );
          const next: Extract<ChatTurn, { type: 'turn_error' }> = {
            type: 'turn_error',
            message_id: event.message_id,
            error_kind: event.error_kind,
            message: event.message,
            retriable: event.retriable,
          };
          if (event.retry_after_secs !== undefined) {
            next.retry_after_secs = event.retry_after_secs;
          }
          if (event.raw !== undefined) {
            next.raw = event.raw;
          }
          if (idx >= 0) {
            state.turns[idx] = next;
          } else {
            state.turns.push(next);
          }
          state.streamingMessageId = null;
          state.awaitingResponse = false;
        }),
      );
      break;
    }

    case 'StreamingStarted':
    case 'StreamingStopped':
      // Handled implicitly via delta/message events.
      break;
  }
}

/** Test helper — clears all message state between tests. */
export function resetMessagesStore(): void {
  setMessagesStore(reconcile({}));
}

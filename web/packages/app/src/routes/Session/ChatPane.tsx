import {
  type Component,
  createSignal,
  createEffect,
  For,
  Show,
  createMemo,
} from 'solid-js';
import { Button } from '@forge/design';
import { activeSessionId, activeWorkspaceRoot } from '../../stores/session';
import {
  getMessagesState,
  pushEvent,
  setAwaitingResponse,
  cancelStream,
  activeVariantPosition,
  liveVariantCount,
  neighbourVariantId,
  type ChatTurn,
  type BranchGroup,
  type ToolCallStatus,
} from '../../stores/messages';
import { BranchSelectorStrip } from '../../components/BranchSelectorStrip';
import { BranchGutter } from '../../components/BranchGutter';
import {
  BranchMetadataPopover,
  type VariantRow,
} from '../../components/BranchMetadataPopover';
import { RerunPopover } from '../../components/RerunPopover';
import type {
  ApprovalLevel,
  ApprovalScope,
  RerunVariant,
  SessionId,
} from '@forge/ipc';
import {
  getApprovalWhitelist,
  addWhitelistEntry,
  revokeWhitelistEntry,
  matchWhitelistKey,
} from '../../stores/approvals';
import {
  removeApproval,
  saveApproval,
  sessionApproveTool,
  sessionRejectTool,
  sessionSendMessage,
  sessionCancel,
  selectBranch,
  deleteBranch,
  rerunMessage,
} from '../../ipc/session';
import { ApprovalPrompt } from '../../components/ApprovalPrompt/ApprovalPrompt';
import { WhitelistedPill } from '../../components/ApprovalPrompt/WhitelistedPill';
import { CompactButton } from './CompactButton';
import {
  ContextPicker,
  detectAtTrigger,
  type CategoryState,
  type PickerResult,
  type ContextCategory,
} from '../../components/ContextPicker';
import { ContextChip } from '../../components/ContextChip';
import { SubAgentBanner } from '../../components/SubAgentBanner';
import type { ProviderId } from '@forge/ipc';
import {
  buildRegistry,
  listCandidates,
  resolveChips,
  type BuildRegistryDeps,
  type ResolverRegistry,
} from '../../context/resolvers';
import { readFile as defaultReadFile } from '../../ipc/fs';
import './ChatPane.css';

// ---------------------------------------------------------------------------
// invoke rejection helper (F-079)
// ---------------------------------------------------------------------------

/**
 * Surface a rejected `invoke()` as an inline `error` turn in the chat. The
 * `Error` event handler in the messages store (see `stores/messages.ts`) also
 * clears `awaitingResponse` and `streamingMessageId`, which is what rolls back
 * the optimistic disable performed by `handleSend` before the call. Routing
 * every command-rejection through this single sink keeps the user-feedback
 * shape consistent across approve/reject/send call sites.
 */
function reportInvokeError(sessionId: SessionId, command: string, err: unknown): void {
  const detail = err instanceof Error ? err.message : String(err);
  pushEvent(sessionId, { kind: 'Error', message: `${command} failed: ${detail}` });
}

// ---------------------------------------------------------------------------
// ToolCallCard — inline tool call card with optional approval prompt
// (F-026 / F-027 / F-447)
// ---------------------------------------------------------------------------

/**
 * One-line arg summary for the collapsed tool-call card (F-041). Path-taking
 * tools (fs.read / fs.write / fs.edit / shell.exec) render their `path` whole;
 * anything else gets `JSON.stringify(args)` capped near 60 chars with an
 * ellipsis.
 *
 * F-080 item 6: `args_json` is produced exclusively by `fromRustEvent`
 * (`ipc/events.ts`) via `JSON.stringify(ev['args'] ?? null)`, so it is
 * guaranteed to be valid JSON at this boundary. The previous defensive
 * `try { JSON.parse } catch { return null }` was cognitive overhead with no
 * risk reduction; relying on the boundary contract collapses three sites in
 * this file to a single `JSON.parse` per call.
 */
function summarizeArgs(argsJson: string): string | null {
  const parsed = JSON.parse(argsJson) as unknown;
  if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
    const path = (parsed as Record<string, unknown>)['path'];
    if (typeof path === 'string' && path.length > 0) {
      return path;
    }
  }
  const stringified = JSON.stringify(parsed);
  if (stringified === undefined) return null;
  const MAX = 60;
  return stringified.length > MAX ? stringified.slice(0, MAX - 1) + '…' : stringified;
}

/**
 * Extract a `path` field from a tool call's `args_json` if present.
 *
 * F-080 item 6: shares the boundary contract documented on `summarizeArgs` —
 * `args_json` is always valid JSON. Returns `''` when the parsed value is not
 * an object with a string `path` field (e.g. `null`, an array, or a
 * non-path-taking tool).
 */
function extractPath(argsJson: string): string {
  const parsed = JSON.parse(argsJson) as unknown;
  if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
    const path = (parsed as Record<string, unknown>)['path'];
    if (typeof path === 'string') return path;
  }
  return '';
}

// F-447: the Phase 3 spec splits tools into three icon-tinted kinds. Pure
// read-only tools take the steel/info hue; agent spawns take ember-400;
// everything else (writes, shell.exec, network calls) inherits the warm
// ember-100 general-tool hue. The classification lives next to the
// component because the spec ties it to the visual treatment.
type ToolKind = 'read' | 'agent' | 'general';

const READ_ONLY_TOOLS = new Set(['fs.read', 'fs.list', 'fs.stat', 'fs.glob']);

function toolKind(toolName: string): ToolKind {
  if (READ_ONLY_TOOLS.has(toolName)) return 'read';
  if (toolName.startsWith('agent.') || toolName.startsWith('subagent.')) {
    return 'agent';
  }
  return 'general';
}

// F-447: writes, shell.exec, and any future destructive kind must surface a
// diff / command preview in the expanded body. Reads and queries do not.
function isDestructiveTool(toolName: string): boolean {
  return (
    toolName === 'fs.write' ||
    toolName === 'fs.edit' ||
    toolName === 'shell.exec' ||
    toolName.startsWith('process.')
  );
}

// F-447: the collapsed-row status glyph. Awaiting-approval renders with the
// warn `!` — the expanded body surfaces the approval prompt and the pending
// explanation, so the glyph alone is enough in the row.
function statusGlyph(status: ToolCallStatus): '✓' | '!' | '✗' {
  if (status === 'completed') return '✓';
  if (status === 'errored') return '✗';
  return '!';
}

function formatDuration(ms: number | undefined): string | null {
  if (ms === undefined) return null;
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const mins = Math.floor(ms / 60_000);
  const rem = Math.round((ms - mins * 60_000) / 1000);
  return `${mins}m ${rem}s`;
}

const RESULT_PREVIEW_CAP = 800;

/**
 * F-447: pretty-print a Rust-side args blob (arrives as valid JSON text).
 * Indents two spaces; handles the `"null"` edge case (no args) by returning
 * the literal so the expanded body still has readable text.
 */
function prettyPrintArgs(argsJson: string): string {
  try {
    return JSON.stringify(JSON.parse(argsJson), null, 2);
  } catch {
    return argsJson;
  }
}

const ToolCallCard: Component<{
  turn: Extract<ChatTurn, { type: 'tool_placeholder' }>;
  /**
   * F-447 §5.1: when the card is a child inside a parallel-reads group,
   * the aggregate header owns the expand affordance and the child renders
   * in a compact "row" mode — no border, no tinted expanded body, no
   * standalone chevron. Defaults to false.
   */
  nested?: boolean;
}> = (props) => {
  let cardRef: HTMLDivElement | undefined;
  const sessionId = () => activeSessionId();
  // F-447: expanded state is local to this card. Spec §5 defaults the
  // currently-streaming (in-progress) and awaiting-approval cards to
  // expanded; completed / errored collapse by default. The signal tracks
  // user overrides so a later click can flip either way.
  const [expanded, setExpanded] = createSignal(
    props.turn.status === 'awaiting-approval' || props.turn.status === 'in-progress',
  );
  // F-447: the "show more" toggle for the 800-char result-preview cap.
  const [showFullPreview, setShowFullPreview] = createSignal(false);

  // Check whitelist on each render
  const whitelistKey = createMemo(() => {
    const id = sessionId();
    if (!id) return null;
    if (props.turn.status !== 'awaiting-approval') return null;
    const path = extractPath(props.turn.args_json);
    const wl = getApprovalWhitelist(id);
    const keys = new Set(Object.keys(wl.entries));
    return matchWhitelistKey(keys, props.turn.tool_name, path);
  });

  const whitelistEntry = createMemo(() => {
    const id = sessionId();
    const key = whitelistKey();
    if (!id || !key) return null;
    return getApprovalWhitelist(id).entries[key] ?? null;
  });

  const whitelistLabel = () => whitelistEntry()?.label ?? null;
  const whitelistLevel = (): ApprovalLevel =>
    whitelistEntry()?.level ?? 'session';

  // Auto-approve when whitelist matches
  createEffect(() => {
    const id = sessionId();
    const key = whitelistKey();
    if (!id || !key || props.turn.status !== 'awaiting-approval') return;
    // Derive ApprovalScope from the whitelist key prefix — keys are authored
    // by `addWhitelistEntry` which follows the `tool:` / `pattern:` / `path:`
    // prefix conventions, so the cast is grounded in the key's construction
    // rather than being a stringly-typed guess.
    const scope: ApprovalScope = key.startsWith('tool:')
      ? 'ThisTool'
      : key.startsWith('pattern:')
        ? 'ThisPattern'
        : 'ThisFile';
    sessionApproveTool(id, props.turn.tool_call_id, scope).catch((err) =>
      reportInvokeError(id, 'session_approve_tool', err),
    );
  });

  // F-036: scope > Once approvals at workspace/user level need to persist on
  // disk too. We do the in-memory add first (so the UI reacts instantly) and
  // then the IPC save; failures surface as an inline error turn but don't
  // roll back the session-level entry — the user already approved the call.
  const handleApprove = (
    scope: ApprovalScope,
    level: ApprovalLevel,
    pattern?: string,
  ) => {
    const id = sessionId();
    if (!id) return;

    // Record whitelist for scopes > Once
    if (scope !== 'Once') {
      const path = extractPath(props.turn.args_json);
      const key = addWhitelistEntry(
        id,
        scope,
        props.turn.tool_name,
        path,
        pattern,
        level,
      );

      // Persist for workspace/user levels. Session-level stays in-memory.
      if (level !== 'session') {
        const root = activeWorkspaceRoot();
        if (root) {
          const label = getApprovalWhitelist(id).entries[key]?.label ?? '';
          void saveApproval(
            { scope_key: key, tool_name: props.turn.tool_name, label },
            level,
            root,
          ).catch((err) => reportInvokeError(id, 'save_approval', err));
        } else {
          // No workspace root — log and fall back to session-only. Surfaces
          // as a warning, not a user-visible error, because the call itself
          // is still being approved; only the persistence failed.
          console.warn('save_approval skipped — no active workspace root');
        }
      }
    }

    sessionApproveTool(id, props.turn.tool_call_id, scope).catch((err) =>
      reportInvokeError(id, 'session_approve_tool', err),
    );
  };

  const handleReject = () => {
    const id = sessionId();
    if (!id) return;
    sessionRejectTool(id, props.turn.tool_call_id).catch((err) =>
      reportInvokeError(id, 'session_reject_tool', err),
    );
  };

  // F-036: revoke removes from the in-memory whitelist and — for persistent
  // tiers — from the corresponding config file. The IPC call is fire-and-forget
  // for the UI (the pill is already gone by the time the rename completes).
  const handleRevoke = () => {
    const id = sessionId();
    const key = whitelistKey();
    if (!id || !key) return;
    const level = whitelistLevel();
    revokeWhitelistEntry(id, key);
    if (level !== 'session') {
      const root = activeWorkspaceRoot();
      if (root) {
        void removeApproval(key, level, root).catch((err) =>
          reportInvokeError(id, 'remove_approval', err),
        );
      } else {
        console.warn('remove_approval skipped — no active workspace root');
      }
    }
  };

  const kind = createMemo<ToolKind>(() => toolKind(props.turn.tool_name));
  const durationText = (): string | null => {
    if (props.turn.status === 'awaiting-approval' && whitelistKey() === null) {
      return 'awaiting approval';
    }
    return formatDuration(props.turn.duration_ms);
  };
  const canExpand = (): boolean => !props.nested;
  const toggleExpanded = (): void => {
    if (!canExpand()) return;
    setExpanded((v) => !v);
  };
  // F-447: the collapsed row doubles as a button. Clicking the chevron or
  // pressing Enter / Space anywhere on the header toggles expanded.
  const handleHeaderKeyDown = (e: KeyboardEvent): void => {
    if (e.key !== 'Enter' && e.key !== ' ') return;
    // While awaiting approval, the outer card owns activation and
    // ApprovalPrompt attaches a native `keydown` listener that treats Enter as
    // "approve once." Running the row handler would (a) collapse the diff the
    // user just read and (b) bubble to the approval listener, producing both a
    // collapse AND an approve on a single key press. No-op here so the prompt
    // stays in sole control of keyboard activation.
    if (props.turn.status === 'awaiting-approval') return;
    if (!canExpand()) return;
    e.preventDefault();
    toggleExpanded();
  };

  // F-447: the destructive preview is whatever the daemon handed us. For
  // completed writes/execs, this is populated by `result_preview`; for
  // awaiting approval it comes from `preview.description`. Either way,
  // renders in a dedicated block separated from the generic result block.
  const destructivePreview = (): string | null => {
    if (!isDestructiveTool(props.turn.tool_name)) return null;
    if (props.turn.status === 'awaiting-approval' && props.turn.preview) {
      return props.turn.preview.description;
    }
    return props.turn.result_preview ?? null;
  };

  const genericPreview = (): string | null => {
    if (isDestructiveTool(props.turn.tool_name)) return null;
    return props.turn.result_preview ?? null;
  };

  const truncatedPreview = (): { text: string; truncated: boolean } => {
    const raw = genericPreview() ?? '';
    if (raw.length <= RESULT_PREVIEW_CAP || showFullPreview()) {
      return { text: raw, truncated: false };
    }
    return {
      text: raw.slice(0, RESULT_PREVIEW_CAP),
      truncated: true,
    };
  };

  return (
    <div
      class="tool-call-card"
      data-testid={`tool-call-card-${props.turn.tool_call_id}`}
      data-tool-kind={kind()}
      data-status={props.turn.status}
      data-expanded={expanded() ? 'true' : 'false'}
      classList={{
        'tool-call-card--nested': props.nested === true,
        'tool-call-card--completed': props.turn.status === 'completed',
        'tool-call-card--errored': props.turn.status === 'errored',
        'tool-call-card--awaiting': props.turn.status === 'awaiting-approval',
        'tool-call-card--expanded': expanded(),
      }}
      tabIndex={props.turn.status === 'awaiting-approval' ? 0 : undefined}
      ref={cardRef}
    >
      {/* Collapsed row. Acts as the expand/collapse affordance; keyboard
          activation via Enter/Space on the role="button" element. */}
      <div
        class="tool-call-card__row"
        data-testid={`tool-call-row-${props.turn.tool_call_id}`}
        role={canExpand() ? 'button' : undefined}
        tabIndex={
          canExpand()
            ? props.turn.status === 'awaiting-approval'
              ? -1
              : 0
            : undefined
        }
        aria-expanded={canExpand() ? expanded() : undefined}
        aria-controls={
          canExpand() ? `tool-call-body-${props.turn.tool_call_id}` : undefined
        }
        onClick={() => toggleExpanded()}
        onKeyDown={handleHeaderKeyDown}
      >
        <span
          class="tool-call-card__icon"
          data-testid={`tool-call-icon-${props.turn.tool_call_id}`}
          aria-hidden="true"
        >
          ⚙
        </span>
        <span class="tool-call-card__name">{props.turn.tool_name}</span>

        {/* One-line arg summary — path for path-taking tools, otherwise a
            short stringified JSON. Skipped when args_json is unparseable. */}
        <Show when={summarizeArgs(props.turn.args_json)}>
          {(summary) => (
            <span
              class="tool-call-card__args"
              data-testid={`tool-call-args-${props.turn.tool_call_id}`}
            >
              {summary()}
            </span>
          )}
        </Show>

        {/* Whitelisted pill when auto-approved */}
        <Show when={whitelistKey() !== null && whitelistLabel() !== null}>
          <WhitelistedPill
            label={whitelistLabel()!}
            level={whitelistLevel()}
            onRevoke={handleRevoke}
          />
        </Show>

        {/* F-447: duration readout — right-aligned. Spec §5 collapses to
            `awaiting approval` when approval is pending so the user knows
            the call is parked on them rather than still running. */}
        <Show when={durationText()}>
          {(text) => (
            <span
              class="tool-call-card__duration"
              data-testid={`tool-call-duration-${props.turn.tool_call_id}`}
            >
              {text()}
            </span>
          )}
        </Show>

        {/* F-447: status glyph. Replaces the Phase 2 raw-text status. */}
        <span
          class="tool-call-card__status"
          data-testid={`tool-call-status-${props.turn.tool_call_id}`}
          data-status={props.turn.status}
        >
          {statusGlyph(props.turn.status)}
        </span>

        {/* F-447: chevron affordance; rotates 90° via CSS when expanded. */}
        <Show when={canExpand()}>
          <span
            class="tool-call-card__chevron"
            data-testid={`tool-call-chevron-${props.turn.tool_call_id}`}
            aria-hidden="true"
          >
            ›
          </span>
        </Show>
      </div>

      {/* F-447 expanded body — tinted background, pretty-printed args,
          result preview with show-more, metadata, destructive preview. */}
      <Show when={expanded() && canExpand()}>
        <div
          class="tool-call-card__body"
          id={`tool-call-body-${props.turn.tool_call_id}`}
          data-testid={`tool-call-body-${props.turn.tool_call_id}`}
        >
          <section class="tool-call-card__block">
            <header class="tool-call-card__block-label">args</header>
            <pre
              class="tool-call-card__args-json"
              data-testid={`tool-call-args-json-${props.turn.tool_call_id}`}
            >
              {prettyPrintArgs(props.turn.args_json)}
            </pre>
          </section>

          <Show when={destructivePreview()}>
            {(text) => (
              <section class="tool-call-card__block">
                <header class="tool-call-card__block-label">
                  {props.turn.tool_name === 'shell.exec' ? 'command' : 'diff'}
                </header>
                <pre
                  class="tool-call-card__diff"
                  data-testid={`tool-call-diff-${props.turn.tool_call_id}`}
                >
                  {text()}
                </pre>
              </section>
            )}
          </Show>

          <Show when={genericPreview()}>
            <section class="tool-call-card__block">
              <header class="tool-call-card__block-label">result</header>
              <pre
                class="tool-call-card__result"
                data-testid={`tool-call-result-${props.turn.tool_call_id}`}
              >
                {truncatedPreview().text}
              </pre>
              <Show when={truncatedPreview().truncated}>
                <button
                  type="button"
                  class="tool-call-card__show-more"
                  data-testid={`tool-call-show-more-${props.turn.tool_call_id}`}
                  onClick={(e) => {
                    e.stopPropagation();
                    setShowFullPreview(true);
                  }}
                >
                  show more
                </button>
              </Show>
            </section>
          </Show>

          <Show when={props.turn.error !== undefined}>
            <section class="tool-call-card__block tool-call-card__block--error">
              <header class="tool-call-card__block-label">error</header>
              <pre
                class="tool-call-card__error"
                data-testid={`tool-call-error-${props.turn.tool_call_id}`}
              >
                {props.turn.error}
              </pre>
            </section>
          </Show>
        </div>
      </Show>

      {/* Inline approval prompt */}
      <Show
        when={
          props.turn.status === 'awaiting-approval' &&
          props.turn.preview !== undefined &&
          whitelistKey() === null
        }
      >
        <ApprovalPrompt
          toolCallId={props.turn.tool_call_id}
          toolName={props.turn.tool_name}
          argsJson={props.turn.args_json}
          preview={props.turn.preview!}
          containerRef={cardRef!}
          onApprove={handleApprove}
          onReject={handleReject}
        />
      </Show>
    </div>
  );
};

// ---------------------------------------------------------------------------
// ParallelReadsGroup — F-447 §5.1
// ---------------------------------------------------------------------------

/**
 * Aggregate card for a run of read-only tool calls that share a `batch_id`.
 * Collapsed: `parallel reads · N calls   48ms ✓ ›`. Expanded: each child
 * renders as a nested compact row. Aggregate duration is the max across
 * children (spec §5.1). Writes never enter here — the store tags only reads
 * with a batch_id via the orchestrator's `parallel_group` field.
 */
const ParallelReadsGroup: Component<{
  batchId: string;
  calls: Array<Extract<ChatTurn, { type: 'tool_placeholder' }>>;
}> = (props) => {
  const [expanded, setExpanded] = createSignal(false);

  const aggregateDuration = createMemo<number | undefined>(() => {
    let max: number | undefined;
    for (const c of props.calls) {
      if (c.duration_ms === undefined) continue;
      if (max === undefined || c.duration_ms > max) max = c.duration_ms;
    }
    return max;
  });

  const aggregateStatus = createMemo<ToolCallStatus>(() => {
    if (props.calls.some((c) => c.status === 'errored')) return 'errored';
    if (props.calls.some((c) => c.status === 'in-progress')) return 'in-progress';
    if (props.calls.some((c) => c.status === 'awaiting-approval')) {
      return 'awaiting-approval';
    }
    return 'completed';
  });

  const handleKeyDown = (e: KeyboardEvent): void => {
    if (e.key !== 'Enter' && e.key !== ' ') return;
    e.preventDefault();
    setExpanded((v) => !v);
  };

  return (
    <div
      class="tool-call-group"
      data-testid={`tool-call-group-${props.batchId}`}
      data-expanded={expanded() ? 'true' : 'false'}
      data-status={aggregateStatus()}
    >
      <div
        class="tool-call-group__row"
        data-testid={`tool-call-group-row-${props.batchId}`}
        role="button"
        tabIndex={0}
        aria-expanded={expanded()}
        aria-controls={`tool-call-group-body-${props.batchId}`}
        aria-label={`parallel reads — ${props.calls.length} ${props.calls.length === 1 ? 'call' : 'calls'} (${aggregateStatus()})`}
        onClick={() => setExpanded((v) => !v)}
        onKeyDown={handleKeyDown}
      >
        <span class="tool-call-card__icon" aria-hidden="true">⚙</span>
        <span class="tool-call-card__name">parallel reads</span>
        <span
          class="tool-call-card__args"
          data-testid={`tool-call-group-count-${props.batchId}`}
        >
          · {props.calls.length} calls
        </span>
        <Show when={formatDuration(aggregateDuration())}>
          {(text) => (
            <span
              class="tool-call-card__duration"
              data-testid={`tool-call-group-duration-${props.batchId}`}
            >
              {text()}
            </span>
          )}
        </Show>
        <span
          class="tool-call-card__status"
          data-status={aggregateStatus()}
          data-testid={`tool-call-group-status-${props.batchId}`}
        >
          {statusGlyph(aggregateStatus())}
        </span>
        <span class="tool-call-card__chevron" aria-hidden="true">›</span>
      </div>
      <Show when={expanded()}>
        <div
          class="tool-call-group__body"
          id={`tool-call-group-body-${props.batchId}`}
          data-testid={`tool-call-group-body-${props.batchId}`}
        >
          <For each={props.calls}>
            {(call) => <ToolCallCard turn={call} nested />}
          </For>
        </div>
      </Show>
    </div>
  );
};

// ---------------------------------------------------------------------------
// Message turn renderers
// ---------------------------------------------------------------------------

const UserBubble: Component<{ turn: Extract<ChatTurn, { type: 'user' }> }> = (props) => (
  <article class="turn turn--user">
    <header class="turn__author">● you</header>
    <p class="turn__body">{props.turn.text}</p>
  </article>
);

const AssistantBubble: Component<{
  turn: Extract<ChatTurn, { type: 'assistant' }>;
  /**
   * F-600: when present, the bubble renders the per-turn `Re-run ▾` action
   * (chat-pane.md §4) wired to dispatch the picked variant. Omit on streaming
   * turns so the action only appears on finalised assistant messages.
   */
  sessionId?: SessionId;
}> = (props) => {
  const [rerunOpen, setRerunOpen] = createSignal(false);

  const handlePick = (variant: RerunVariant): void => {
    setRerunOpen(false);
    const sid = props.sessionId;
    if (sid === undefined) return;
    rerunMessage(sid, props.turn.message_id, variant).catch((err: unknown) =>
      reportInvokeError(sid, 'rerun_message', err),
    );
  };

  // F-600: hide the Re-run action while the turn is still streaming —
  // there's nothing to re-run until the original turn has finalised.
  const canRerun = (): boolean =>
    !props.turn.isStreaming && props.sessionId !== undefined;

  return (
    <article
      class="turn turn--assistant"
      aria-busy={props.turn.isStreaming ? 'true' : undefined}
      data-testid={`assistant-turn-${props.turn.message_id}`}
    >
      <header class="turn__author">
        <span>{'●'} assistant</span>
        <Show when={canRerun()}>
          <span class="turn__actions">
            <Button
              variant="ghost"
              size="sm"
              class="turn__action turn__action--rerun"
              data-testid={`rerun-trigger-${props.turn.message_id}`}
              aria-haspopup="menu"
              aria-expanded={rerunOpen()}
              onClick={() => setRerunOpen((v) => !v)}
            >
              RE-RUN {'▾'}
            </Button>
            <Show when={rerunOpen()}>
              <div class="turn__action-popover-anchor">
                <RerunPopover
                  onPick={handlePick}
                  onDismiss={() => setRerunOpen(false)}
                />
              </div>
            </Show>
          </span>
        </Show>
      </header>
      <p class="turn__body">
        {props.turn.text}
        <Show when={props.turn.isStreaming}>
          <span class="streaming-cursor" data-testid="streaming-cursor" aria-hidden="true" />
        </Show>
      </p>
    </article>
  );
};

// ---------------------------------------------------------------------------
// F-145 — branch-aware assistant turn wrapper
//
// Renders the AssistantBubble plus, when the owning branch group has more
// than one live variant, the 2px gutter line, the selector strip, and the
// on-demand metadata popover. Dispatches `select_branch` / `delete_branch`
// through `invoke`; Export serializes the active branch path and writes it
// to the clipboard.
// ---------------------------------------------------------------------------

const BranchedAssistantTurn: Component<{
  turn: Extract<ChatTurn, { type: 'assistant' }>;
  group: BranchGroup;
  turns: ChatTurn[];
  /**
   * F-145: the transcript filtered to the currently-active branch variant
   * per branch group. The Export action serialises this (not the full
   * `turns`) so a user sharing the "selected branch path" does not leak
   * inactive variants from other branch points into the exported JSON.
   */
  visibleTurns: ChatTurn[];
  sessionId: SessionId;
}> = (props) => {
  const [popoverOpen, setPopoverOpen] = createSignal(false);

  const position = createMemo(() => activeVariantPosition(props.group));
  const liveCount = createMemo(() => liveVariantCount(props.group));
  const rootId = createMemo(() => props.turn.branch_parent ?? props.turn.message_id);

  const cyclePrev = (): void => {
    const next = neighbourVariantId(props.group, 'prev');
    if (next !== null) dispatchSelect(next);
  };
  const cycleNext = (): void => {
    const next = neighbourVariantId(props.group, 'next');
    if (next !== null) dispatchSelect(next);
  };

  const dispatchSelect = (messageId: string): void => {
    // Spec §15.3: switching variants dispatches `select_branch` with the
    // variant's index. Find that index from the group.
    const idx = props.group.variantIds.indexOf(messageId);
    if (idx < 0) return;
    selectBranch(props.sessionId, rootId(), idx).catch((err: unknown) =>
      reportInvokeError(props.sessionId, 'select_branch', err),
    );
  };

  const dispatchDelete = (variantIndex: number): void => {
    deleteBranch(props.sessionId, rootId(), variantIndex).catch((err: unknown) =>
      reportInvokeError(props.sessionId, 'delete_branch', err),
    );
    setPopoverOpen(false);
  };

  /**
   * F-145: "Export copies the selected branch path to clipboard as JSON."
   *
   * Interpretation (spec §15.5 is silent on shape): the active variant's
   * sub-path — the user/assistant turns in the transcript ordered as they
   * would render, filtered to the active variant per branch group — is
   * serialized as a JSON array of `{ role, text, ... }` entries. Using
   * `visibleTurns` (not the full `turns`) ensures inactive variants from
   * OTHER branch points don't leak into the export; only the conversation
   * the user currently sees is copied. The shape is the minimum a
   * downstream tool needs to reconstruct the selected branch's conversation;
   * tool-call trees and deep provider metadata are out of scope for v1.
   */
  const handleExportAll = async (): Promise<void> => {
    const payload = props.visibleTurns
      .map((t) => {
        if (t.type === 'user') {
          return { role: 'user', text: t.text, message_id: t.message_id };
        }
        if (t.type === 'assistant') {
          const row: Record<string, unknown> = {
            role: 'assistant',
            text: t.text,
            message_id: t.message_id,
            branch_parent: t.branch_parent,
            branch_variant_index: t.branch_variant_index,
          };
          if (t.provider !== undefined) row.provider = t.provider;
          if (t.model !== undefined) row.model = t.model;
          if (t.at !== undefined) row.at = t.at;
          return row;
        }
        return null;
      })
      .filter((r): r is Record<string, unknown> => r !== null);
    const json = JSON.stringify(payload, null, 2);
    try {
      if (typeof navigator !== 'undefined' && navigator.clipboard) {
        await navigator.clipboard.writeText(json);
      }
    } catch (err) {
      reportInvokeError(props.sessionId, 'clipboard.writeText', err);
    }
    setPopoverOpen(false);
  };

  /**
   * Build the variant rows consumed by the popover. Preview text comes
   * from the matching turn in `props.turns` when available; a missing turn
   * (e.g. the variant has not yet streamed in after a BranchSelected
   * arrived first) falls back to an empty preview so the popover still
   * lists the placeholder row.
   */
  const variantRows = createMemo<VariantRow[]>(() => {
    const rows: VariantRow[] = [];
    const ids = props.group.variantIds;
    for (let idx = 0; idx < ids.length; idx++) {
      const id = ids[idx];
      if (id === null || id === undefined) continue;
      const matchId: string = id;
      const turn = props.turns.find(
        (t): t is Extract<ChatTurn, { type: 'assistant' }> =>
          t.type === 'assistant' && t.message_id === matchId,
      );
      rows.push({
        index: idx,
        message_id: matchId,
        preview: turn?.text ?? '',
        ...(turn?.provider !== undefined ? { provider: turn.provider } : {}),
        ...(turn?.model !== undefined ? { model: turn.model } : {}),
        ...(turn?.at !== undefined ? { at: turn.at } : {}),
      });
    }
    return rows;
  });

  return (
    <div
      class="turn-branch"
      data-testid={`branch-turn-${props.turn.message_id}`}
      data-branch-root={rootId()}
    >
      <BranchGutter depth={0} />
      <Show when={position()}>
        {(pos) => (
          <BranchSelectorStrip
            position={pos().position}
            total={pos().total}
            onPrev={cyclePrev}
            onNext={cycleNext}
            onToggleInfo={() => setPopoverOpen((v) => !v)}
            infoOpen={popoverOpen()}
          />
        )}
      </Show>
      <Show when={popoverOpen()}>
        <div class="turn-branch__popover-anchor">
          <BranchMetadataPopover
            variants={variantRows()}
            activeVariantId={props.group.activeVariantId}
            onSelect={(id) => {
              dispatchSelect(id);
              setPopoverOpen(false);
            }}
            onDelete={dispatchDelete}
            onExportAll={() => {
              void handleExportAll();
            }}
            onDismiss={() => setPopoverOpen(false)}
          />
        </div>
      </Show>
      <AssistantBubble turn={props.turn} sessionId={props.sessionId} />
      <Show when={liveCount() > 1 && !popoverOpen()}>
        {/* Keep a stable placeholder so layout tests can measure the
            post-strip gap even when the popover is closed. Empty by design. */}
        <span class="turn-branch__gap" aria-hidden="true" />
      </Show>
    </div>
  );
};

const ErrorTurn: Component<{ turn: Extract<ChatTurn, { type: 'error' }> }> = (props) => (
  <div class="turn turn--error" role="alert">
    <span class="turn__error-icon" aria-hidden="true">!</span>
    <span class="turn__error-message">{props.turn.message}</span>
  </div>
);

// ---------------------------------------------------------------------------
// Composer
// ---------------------------------------------------------------------------

/// F-080 item 5: composer-side message-byte cap. The Rust side
/// (`forge-shell::ipc::session_send_message`) enforces a 128 KiB cap on the
/// UTF-8 byte length of `text`; capping at 100 KiB on the TS side gives the
/// user feedback before the IPC round trip and stays comfortably below the
/// backend cap so a marginal extra prompt token does not race the boundary.
/// `TextEncoder` measures UTF-8 bytes (matching the Rust check) — `String.length`
/// would return UTF-16 code units and be wrong for non-BMP input.
export const MAX_COMPOSER_BYTES = 100 * 1024;

function utf8ByteLength(text: string): number {
  return new TextEncoder().encode(text).length;
}

export interface InsertedChip {
  /** Stable id so SolidJS can reconcile chip list updates. */
  id: string;
  category: ContextCategory;
  label: string;
  value: string;
}

/**
 * Remove the active `@text` span from the textarea content when a picker
 * result is selected (F-141). Exported for direct unit testing — the
 * integration in Composer calls this while also appending the chip to the
 * `ctx-chips` row and repositioning the caret.
 *
 * The span runs from `triggerStart` (index of the `@`) to `caret`. The
 * result is the concatenation of the text before `triggerStart` and
 * after `caret`, plus the new caret position at the join.
 */
export function removeAtSpan(
  text: string,
  triggerStart: number,
  caret: number,
): { text: string; caret: number } {
  const before = text.slice(0, triggerStart);
  const after = text.slice(caret);
  return { text: before + after, caret: before.length };
}

export interface ComposerProps {
  disabled: boolean;
  /**
   * Send handler. Receives the trimmed text and the currently-attached chips.
   * F-142 routes chips through a resolver registry (`onSend` in `ChatPane`
   * builds the provider-shaped context prefix) — callers that want the raw
   * string path can ignore `chips`.
   */
  onSend: (text: string, chips: InsertedChip[]) => void;
  /**
   * F-391: cancel the in-flight turn. Fired by the Stop button click and by
   * Esc while the composer is in the streaming/disabled state. The Composer
   * itself is purely a view — the owner (`ChatPane`) handles the IPC
   * dispatch and local state cleanup.
   */
  onCancel?: () => void;
  /**
   * Optional category-indexed items forwarded to the ContextPicker. Exposed
   * for tests that drive the end-to-end "type @ → pick → chip appears" flow
   * without booting the resolver registry.
   */
  items?: Partial<Record<ContextCategory, PickerResult[]>>;
  /**
   * F-142: resolver registry. When present, the composer fetches live picker
   * results from `listCandidates(registry, query)` on every query change.
   * `items` (if also present) wins — tests can pin the list without stubbing
   * the registry.
   */
  registry?: ResolverRegistry;
  /**
   * F-142: hover preview loader for file chips. Injected into `ContextChip`;
   * production passes `readFile(sessionId, path)`, tests pass a stub.
   */
  loadFilePreview?: (path: string) => Promise<string>;
}

export const Composer: Component<ComposerProps> = (props) => {
  const [text, setText] = createSignal('');
  const [caret, setCaret] = createSignal(0);
  const [chips, setChips] = createSignal<InsertedChip[]>([]);
  // F-141: Esc dismisses the picker but must *retain* the typed `@text`
  // (spec §7 "close, retain typed text"). We track the dismissed `@`-span
  // start so the trigger re-opens only after the user edits the text — not
  // just from a caret move back into the same span.
  const [dismissedAt, setDismissedAt] = createSignal<number | null>(null);
  // F-141: the ContextPicker pops open while an active `@token` sits at the
  // caret. `detectAtTrigger` drives this — whenever the caret or text moves,
  // we recompute whether we're still in a trigger.
  const trigger = createMemo(() => {
    const match = detectAtTrigger(text(), caret());
    if (!match) return null;
    if (dismissedAt() === match.start) return null;
    return match;
  });
  const pickerOpen = createMemo(() => trigger() !== null);
  // Anchor rect used by the ContextPicker for placement. Re-measured on open
  // and on viewport resize.
  const [anchorRect, setAnchorRect] = createSignal({
    top: 0,
    bottom: 0,
    left: 0,
    right: 0,
  });

  let textareaRef: HTMLTextAreaElement | undefined;
  let composerRef: HTMLDivElement | undefined;

  // Byte length of the *trimmed* value because that is what we actually send.
  const trimmedByteLength = createMemo(() => utf8ByteLength(text().trim()));
  const overCap = createMemo(() => trimmedByteLength() > MAX_COMPOSER_BYTES);

  // F-142: live items populated from the resolver registry. Every query
  // change fires a fan-out through `listCandidates`; the latest successful
  // result wins. When no registry is wired, `items()` stays `undefined` and
  // the picker renders empty tabs (or consumes `props.items` for tests).
  const [registryItems, setRegistryItems] = createSignal<
    Partial<Record<ContextCategory, CategoryState>> | undefined
  >(undefined);
  // Guard against races: the newest query wins when multiple in-flight
  // promises resolve out of order.
  let listToken = 0;

  createEffect(() => {
    const registry = props.registry;
    if (!registry) return;
    const match = trigger();
    const query = match ? match.query : '';
    const token = ++listToken;
    void listCandidates(registry, query).then((items) => {
      if (token !== listToken) return;
      setRegistryItems(items);
    });
  });

  const effectiveItems = createMemo(() => props.items ?? registryItems());

  const measureAnchor = () => {
    if (!composerRef) return;
    const r = composerRef.getBoundingClientRect();
    setAnchorRect({ top: r.top, bottom: r.bottom, left: r.left, right: r.right });
  };

  createEffect(() => {
    if (pickerOpen()) {
      measureAnchor();
    }
  });

  const handleInput = (e: InputEvent & { currentTarget: HTMLTextAreaElement }) => {
    const t = e.currentTarget.value;
    setText(t);
    setCaret(e.currentTarget.selectionStart ?? t.length);
    // Any text edit re-arms the trigger — the user's dismiss decision only
    // applied to the span they dismissed.
    setDismissedAt(null);
  };

  const handleSelect = (e: Event & { currentTarget: HTMLTextAreaElement }) => {
    setCaret(e.currentTarget.selectionStart ?? text().length);
  };

  const handleSend = () => {
    const value = text().trim();
    if (!value) return;
    if (utf8ByteLength(value) > MAX_COMPOSER_BYTES) return;
    const attached = chips();
    props.onSend(value, attached);
    setText('');
    setCaret(0);
    // F-142: chips are consumed on send. The caller (ChatPane) resolves and
    // prepends them to the message text through the provider adapter.
    setChips([]);
  };

  const handleKeyDown = (e: KeyboardEvent) => {
    // When the picker is open it owns Arrow/Enter/Tab/Escape — stop those
    // from reaching the textarea's send handler. The picker itself installs
    // a capturing window listener, so we just need to make sure the
    // textarea-level Enter-to-send doesn't fire while the picker is up.
    if (pickerOpen()) {
      if (
        e.key === 'Enter' ||
        e.key === 'Escape' ||
        e.key === 'Tab' ||
        e.key === 'ArrowUp' ||
        e.key === 'ArrowDown'
      ) {
        return;
      }
    }
    if (e.key !== 'Enter') return;
    // Option B: modifier keys = newline, bare Enter = send
    if (e.shiftKey || e.ctrlKey || e.metaKey) return;
    e.preventDefault();
    handleSend();
  };

  const replaceAtSpan = (result: PickerResult) => {
    // Remove the `@text` span from the textarea and append a chip to the
    // `ctx-chips` row. The span runs from `trigger.start` to the caret.
    const t = text();
    const c = caret();
    const match = trigger();
    if (!match) {
      // Picker was open with no active trigger — append the chip and move
      // on; the textarea stays as-is.
      setChips((prev) => [
        ...prev,
        {
          id: `chip-${Date.now()}-${prev.length}`,
          category: result.category,
          label: result.label,
          value: result.value,
        },
      ]);
      return;
    }
    const { text: next, caret: nextCaret } = removeAtSpan(t, match.start, c);
    setText(next);
    setCaret(nextCaret);
    setDismissedAt(null);
    setChips((prev) => [
      ...prev,
      {
        id: `chip-${Date.now()}-${prev.length}`,
        category: result.category,
        label: result.label,
        value: result.value,
      },
    ]);
    // Move the real textarea caret to the join point so subsequent typing
    // continues where the `@span` was.
    queueMicrotask(() => {
      if (textareaRef) {
        textareaRef.selectionStart = nextCaret;
        textareaRef.selectionEnd = nextCaret;
        textareaRef.focus();
      }
    });
  };

  const dismissPicker = () => {
    // "Esc: close, retain typed text" — the `@text` stays in the textarea.
    // We suppress re-opening the picker for this particular `@`-span; any
    // subsequent text edit (via handleInput) clears the suppression.
    const match = trigger();
    if (match) {
      setDismissedAt(match.start);
      queueMicrotask(() => {
        if (textareaRef) {
          textareaRef.focus();
        }
      });
    }
  };

  const dismissChip = (id: string) => {
    setChips((prev) => prev.filter((c) => c.id !== id));
  };

  // F-391: Esc must cancel the stream even when the textarea is disabled
  // (browsers skip keydown on disabled controls). Listening at the composer
  // wrapper instead keeps Esc live through the lock.
  const handleComposerKeyDown = (e: KeyboardEvent) => {
    if (e.key !== 'Escape') return;
    if (!props.disabled) return;
    if (pickerOpen()) return;
    e.preventDefault();
    props.onCancel?.();
  };

  return (
    <div class="composer" ref={composerRef} onKeyDown={handleComposerKeyDown}>
      {/* ctx-chips row — chips inserted from the picker live here. The spec
          places it above the textarea so chips are visually attached to
          the message being composed. */}
      <div
        class="composer__ctx-chips"
        data-testid="ctx-chips"
        classList={{ 'composer__ctx-chips--empty': chips().length === 0 }}
      >
        <For each={chips()}>
          {(chip) => (
            <ContextChip
              category={chip.category}
              label={chip.label}
              value={chip.value}
              {...(props.loadFilePreview !== undefined
                ? { loadPreview: props.loadFilePreview }
                : {})}
              onDismiss={() => dismissChip(chip.id)}
            />
          )}
        </For>
      </div>
      <textarea
        class="composer__textarea"
        data-testid="composer-textarea"
        placeholder="Ask, refine, or @-reference context"
        disabled={props.disabled}
        value={text()}
        onInput={handleInput}
        onSelect={handleSelect}
        onClick={handleSelect}
        onKeyUp={handleSelect}
        onKeyDown={handleKeyDown}
        rows={3}
        aria-invalid={overCap() ? true : undefined}
        ref={textareaRef}
      />
      <Show when={pickerOpen()}>
        <ContextPicker
          query={trigger()!.query}
          anchorRect={anchorRect()}
          {...(effectiveItems() ? { items: effectiveItems()! } : {})}
          onPick={replaceAtSpan}
          onDismiss={dismissPicker}
        />
      </Show>
      <div class="composer__bar">
        <span class="composer__hints">
          <Show
            when={overCap()}
            fallback={
              <>
                <span class="composer__hint">@ for context</span>
                <span class="composer__hint">/ for commands</span>
              </>
            }
          >
            <span
              class="composer__hint composer__hint--warning"
              data-testid="composer-overflow-warning"
              role="status"
            >
              Message is {trimmedByteLength().toLocaleString()} bytes — over the{' '}
              {MAX_COMPOSER_BYTES.toLocaleString()}-byte limit. Trim before sending.
            </span>
          </Show>
        </span>
        <div class="composer__actions">
          {/* F-391: Stop flips to primary/ember while streaming (spec §4.1) and
              fires `onCancel` — same path as Esc. */}
          <Show when={props.disabled}>
            <Button
              variant="primary"
              class="composer__btn composer__btn--primary"
              data-testid="composer-stop-btn"
              onClick={() => props.onCancel?.()}
            >
              STOP TURN
            </Button>
          </Show>
          {/* F-391: Send is a real primary/ember button, UPPERCASE per
              voice-terminology.md, disabled while streaming, and shares the
              bare-Enter code path via `handleSend`. */}
          <Button
            variant="primary"
            class="composer__btn composer__btn--primary"
            data-testid="composer-send-btn"
            disabled={props.disabled}
            onClick={handleSend}
          >
            SEND <span class="composer__btn-kbd">↵</span>
          </Button>
        </div>
      </div>
    </div>
  );
};

// ---------------------------------------------------------------------------
// ChatPane root
// ---------------------------------------------------------------------------

/**
 * ChatPane props — the default component reads the active session + provider
 * from stores, but tests can inject a fixed registry / provider to avoid
 * touching global state.
 */
export interface ChatPaneProps {
  /** F-142: override the default resolver registry built from active-session
   *  state. Tests pass a stub; production leaves it undefined. */
  registry?: ResolverRegistry;
  /** F-142: active provider for `adaptContextBlocks`. Production passes the
   *  user's selected provider id; tests can pin a flavour. */
  providerId?: ProviderId | null;
}

export const ChatPane: Component<ChatPaneProps> = (props) => {
  const sessionId = () => activeSessionId();
  const state = createMemo(() => {
    const id = sessionId();
    if (!id) return { turns: [], awaitingResponse: false, streamingMessageId: null, branchGroups: {} };
    return getMessagesState(id);
  });

  // F-142: default registry — builds resolvers lazily from the active session
  // and workspace root. Resolvers that need data we don't have (active
  // selection, focused terminal, transcripts of sibling agents) are absent
  // for v1 and their tabs show "No results"; spec §7.2-§7.7 allows this.
  const defaultRegistry = createMemo<ResolverRegistry>(() => {
    const id = sessionId();
    const root = activeWorkspaceRoot();
    if (!id || !root) return {};
    const deps: BuildRegistryDeps = {
      file: { sessionId: id, workspaceRoot: root },
      directory: { sessionId: id, workspaceRoot: root },
      // F-359: the URL resolver is now session-scoped — fetches go through
      // the Rust-side `context_fetch_url` IPC command which authz-checks
      // against the owning session window.
      url: { sessionId: id },
    };
    return buildRegistry(deps);
  });
  const registry = (): ResolverRegistry => props.registry ?? defaultRegistry();

  // F-142: lazy file preview loader. Chips are free-standing, so we
  // snapshot the session at preview time rather than closing over the live
  // signal — a stale sessionId is preferable to a null crash mid-hover.
  const loadFilePreview = async (path: string): Promise<string> => {
    const id = sessionId();
    if (!id) return '';
    const file = await defaultReadFile(id, path);
    return file.content;
  };

  // F-145: filter assistant turns to render only the active variant of each
  // branch group. Non-assistant turns pass through unchanged. When a root
  // message has siblings, we hide every assistant turn whose id doesn't
  // match the group's `activeVariantId` — the selector strip + popover
  // surface the hidden siblings as separately-selectable items.
  const visibleTurns = createMemo<ChatTurn[]>(() => {
    const { turns, branchGroups } = state();
    return turns.filter((turn) => {
      if (turn.type !== 'assistant') return true;
      const rootId = turn.branch_parent ?? turn.message_id;
      const group = branchGroups[rootId];
      if (!group) return true;
      if (group.deletedIndices.includes(turn.branch_variant_index)) return false;
      return turn.message_id === group.activeVariantId;
    });
  });

  // F-447 §5.1: parallel-reads grouping. Bucket consecutive tool_placeholder
  // turns that share a `batch_id` into a single synthetic group item so
  // `ParallelReadsGroup` can render the aggregate header + nested children.
  // Non-placeholder turns break the run; a batch_id mismatch also breaks it.
  // Runs of length 1 degrade to the standalone card — no group chrome — so
  // a solo read doesn't awkwardly render as "parallel reads · 1 call".
  type DisplayItem =
    | { kind: 'turn'; turn: ChatTurn }
    | {
        kind: 'tool_group';
        batchId: string;
        calls: Array<Extract<ChatTurn, { type: 'tool_placeholder' }>>;
      };

  const displayItems = createMemo<DisplayItem[]>(() => {
    const out: DisplayItem[] = [];
    const turns = visibleTurns();
    let i = 0;
    while (i < turns.length) {
      const t = turns[i]!;
      if (t.type === 'tool_placeholder' && t.batch_id !== undefined) {
        const bid = t.batch_id;
        const run: Array<Extract<ChatTurn, { type: 'tool_placeholder' }>> = [];
        while (i < turns.length) {
          const n = turns[i]!;
          if (n.type !== 'tool_placeholder' || n.batch_id !== bid) break;
          run.push(n);
          i += 1;
        }
        if (run.length > 1) {
          out.push({ kind: 'tool_group', batchId: bid, calls: run });
        } else {
          out.push({ kind: 'turn', turn: run[0]! });
        }
        continue;
      }
      out.push({ kind: 'turn', turn: t });
      i += 1;
    }
    return out;
  });

  let listRef: HTMLDivElement | undefined;
  // Track whether the user has scrolled up (released auto-pin)
  const [userScrolledUp, setUserScrolledUp] = createSignal(false);

  const scrollToBottom = () => {
    if (listRef) {
      listRef.scrollTop = listRef.scrollHeight;
    }
  };

  // Auto-scroll to bottom when turns change or streaming progresses,
  // as long as the user hasn't scrolled up to read earlier content.
  createEffect(() => {
    const { turns, streamingMessageId } = state();
    // Access both to establish reactive dependencies; values unused directly.
    const _len = turns.length;
    const _sid = streamingMessageId;
    if (!userScrolledUp()) {
      scrollToBottom();
    }
  });

  const handleListScroll = () => {
    if (!listRef) return;
    const atBottom = listRef.scrollHeight - listRef.scrollTop - listRef.clientHeight < 8;
    setUserScrolledUp(!atBottom);
  };

  // F-391: Stop / Esc path. The composer fires this on Stop click or Esc
  // while the lock is up. We dispatch `session_cancel` fire-and-forget and
  // locally clear the composer's stream lock so the UI becomes interactive
  // immediately (spec §4.1 treats Stop as an instant interaction).
  const handleCancel = () => {
    const id = sessionId();
    if (!id) return;
    cancelStream(id);
    sessionCancel(id).catch((err: unknown) => reportInvokeError(id, 'session_cancel', err));
  };

  const handleSend = (text: string, chips: InsertedChip[]) => {
    const id = sessionId();
    if (!id) return;
    setAwaitingResponse(id, true);
    // Re-pin on new user message
    setUserScrolledUp(false);

    const sendText = (body: string): void => {
      sessionSendMessage(id, body).catch((err) =>
        reportInvokeError(id, 'session_send_message', err),
      );
    };

    // F-142: resolve chips through the registry and prepend the provider-
    // shaped context to the user's text. The current IPC boundary
    // (`session_send_message`) takes a single `text` string; compact shape
    // above that boundary is the pragmatic v1 wire until a structured
    // `context_blocks` field lands server-side.
    if (chips.length === 0) {
      // Fast path — keep the no-chip send synchronous so callers that dispatch
      // an Enter and check `invoke` on the next tick (the existing composer
      // contract) observe the call without awaiting a promise chain.
      sendText(text);
      return;
    }
    const providerId = props.providerId ?? null;
    void resolveChips(registry(), chips, providerId).then((prefix) => {
      sendText(prefix.length > 0 ? `${prefix}\n\n${text}` : text);
    });
  };

  return (
    <section class="chat-pane" data-testid="chat-pane" aria-label="Chat pane">
      <span class="chat-pane__type-label">CHAT</span>
      {/* F-598: transcript toolbar — manual context-compaction trigger.
          Mounted at the top so the action is reachable without scrolling
          to the composer. Hidden when no session is bound. */}
      <Show when={sessionId() !== null}>
        <CompactButton sessionId={sessionId() as SessionId} />
      </Show>
      {/* Streaming indicator shown while awaiting a response */}
      <Show when={state().awaitingResponse}>
        <div
          class="chat-pane__streaming-indicator"
          data-testid="streaming-indicator"
          aria-live="polite"
        >
          <span class="streaming-cursor" aria-hidden="true" />
          <span>Awaiting response…</span>
        </div>
      </Show>

      {/* Message list */}
      <div
        class="chat-pane__messages"
        data-testid="message-list"
        data-autoscroll=""
        role="log"
        aria-live="polite"
        ref={listRef}
        onScroll={handleListScroll}
      >
        {/* F-415: fresh-session mount placeholder. Shown while the list has
            no visible turns AND we aren't already awaiting a response (the
            streaming indicator owns that state). Canonical `// noun-phrase`
            form from voice-terminology §8 / ai-patterns §"Interaction states". */}
        <Show when={visibleTurns().length === 0 && !state().awaitingResponse}>
          <p class="chat-pane__empty" data-testid="chat-pane-empty-state">
            // composer ready
          </p>
        </Show>
        <For each={displayItems()}>
          {(item) => {
            if (item.kind === 'tool_group') {
              return <ParallelReadsGroup batchId={item.batchId} calls={item.calls} />;
            }
            const turn = item.turn;
            switch (turn.type) {
              case 'user':
                return <UserBubble turn={turn} />;
              case 'assistant': {
                // F-145: when the turn belongs to a multi-variant branch
                // group, mount the branch chrome around it. Otherwise fall
                // back to the plain bubble (spec §15.2 — single-variant
                // messages render without extra chrome).
                const rootId = turn.branch_parent ?? turn.message_id;
                const group = state().branchGroups[rootId];
                if (group && liveVariantCount(group) > 1) {
                  const id = sessionId();
                  if (id !== null) {
                    return (
                      <BranchedAssistantTurn
                        turn={turn}
                        group={group}
                        turns={state().turns}
                        visibleTurns={visibleTurns()}
                        sessionId={id}
                      />
                    );
                  }
                }
                {
                  const sid = sessionId();
                  return sid !== null ? (
                    <AssistantBubble turn={turn} sessionId={sid} />
                  ) : (
                    <AssistantBubble turn={turn} />
                  );
                }
              }
              case 'tool_placeholder':
                return <ToolCallCard turn={turn} />;
              case 'error':
                return <ErrorTurn turn={turn} />;
              case 'sub_agent_banner':
                // F-136: inline banner for a spawned child agent. Nested
                // child turns (future work post-F-140) are threaded through
                // the banner's `children` prop when they arrive with a
                // matching `instance_id`; today the list is empty.
                return <SubAgentBanner turn={turn} />;
              case 'context_compacted':
                // F-598: inline summary marker. Anchors the user's eye to
                // the boundary between summarized history (now collapsed
                // into the privileged summary message) and live context.
                return (
                  <div
                    class="context-compacted-marker"
                    data-testid="context-compacted-marker"
                    data-summary-msg-id={turn.summary_msg_id}
                    data-trigger={turn.trigger}
                  >
                    <span aria-hidden="true">≡</span>
                    <span>
                      compacted{' '}
                      <span class="context-compacted-marker__count">
                        {turn.summarized_turns}
                      </span>{' '}
                      turn{turn.summarized_turns === 1 ? '' : 's'}
                      {turn.trigger === 'AutoAt98Pct' ? ' · auto' : ' · manual'}
                    </span>
                  </div>
                );
            }
          }}
        </For>
      </div>

      {/* Composer — disabled while we are awaiting the first token OR a
          stream is in flight. The store clears `awaitingResponse` on the
          first AssistantDelta but leaves `streamingMessageId` set until
          AssistantMessage(stream_finalised: true), so both must read
          falsy before the composer re-enables. */}
      <Composer
        disabled={state().awaitingResponse || state().streamingMessageId !== null}
        onSend={handleSend}
        onCancel={handleCancel}
        registry={registry()}
        loadFilePreview={loadFilePreview}
      />
    </section>
  );
};

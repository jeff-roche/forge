// F-602: per-agent cross-session memory controls on the Dashboard.
//
// One row per loaded agent. Each row shows:
//   - The agent id.
//   - The memory file path on disk (so the user can locate / back-up
//     the file outside the app).
//   - File size + last-modified + monotonic version (F-601 frontmatter).
//   - A toggle that flips `[memory.enabled.<agent>]` in workspace
//     settings. The session daemon consults the merged settings on
//     session start (F-602 wiring in `serve_with_session`).
//   - An Edit button that opens the MemoryEditor flyout. Read-only when
//     the effective enabled flag is `false`.
//   - A Clear button that wipes the file body (confirms first).
//
// The "DO NOT store secrets" warning is surfaced both at the top of the
// section and inside the editor flyout so the user sees it whether they
// edit via the Dashboard or via an agent's `memory.write` tool.

import {
  type Component,
  createResource,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from 'solid-js';
import { Button } from '@forge/design';
import {
  clearAgentMemory,
  listAgentMemory,
  type AgentMemoryEntry,
} from '../../ipc/memory';
import { setSetting } from '../../stores/settings';
import { useFocusTrap } from '../../lib/useFocusTrap';
import { MEMORY_SECRETS_WARNING, MemoryEditor } from './MemoryEditor';
import './MemorySection.css';

/** Anchor id on the Memory `<section>`. */
export const MEMORY_SECTION_ID = 'memory-section';

export interface MemorySectionProps {
  /** Workspace path passed through from the Dashboard route. */
  workspaceRoot: string;
  /** Test seam — the editor flyout uses a textarea instead of an iframe. */
  useTextareaForTest?: boolean | undefined;
}

/** Effective enabled state: the user's settings override (when set) wins
 *  over the def-level flag. Mirrors `effective_memory_enabled` in
 *  `forge_session::server`. */
export function effectiveEnabled(entry: AgentMemoryEntry): boolean {
  return entry.settings_override ?? entry.def_enabled;
}

/** Format a byte count for the meta line. */
export function formatBytes(n: number | null): string {
  if (n === null) return '— bytes';
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / (1024 * 1024)).toFixed(2)} MiB`;
}

/** Format an ISO timestamp for the meta line. */
export function formatTimestamp(iso: string | null): string {
  if (iso === null) return 'never';
  try {
    return new Date(iso).toLocaleString();
  } catch {
    return iso;
  }
}

export const MemorySection: Component<MemorySectionProps> = (props) => {
  const [entries, { refetch }] = createResource(
    () => props.workspaceRoot,
    async (root) => {
      try {
        return await listAgentMemory(root);
      } catch {
        return [] as AgentMemoryEntry[];
      }
    },
  );

  const [editing, setEditing] = createSignal<AgentMemoryEntry | null>(null);
  const [pendingClear, setPendingClear] = createSignal<AgentMemoryEntry | null>(null);
  const [error, setError] = createSignal<string | null>(null);

  const onToggle = async (entry: AgentMemoryEntry, next: boolean): Promise<void> => {
    setError(null);
    try {
      await setSetting(`memory.enabled.${entry.agent_id}`, next, 'workspace', props.workspaceRoot);
      await refetch();
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const onClear = async (entry: AgentMemoryEntry): Promise<void> => {
    setError(null);
    try {
      await clearAgentMemory(entry.agent_id);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setPendingClear(null);
      // Refresh always — a partial-failure clear may have left the file in
      // a state the row's stale `size_bytes` no longer reflects.
      await refetch();
    }
  };

  return (
    <section id={MEMORY_SECTION_ID} class="memory-section" aria-label="Agent memory">
      <header class="memory-section__header">
        <span class="memory-section__label">MEMORY</span>
      </header>

      <p class="memory-section__warn" role="note" data-testid="memory-section-warning">
        {MEMORY_SECRETS_WARNING}
      </p>

      <Show when={error()}>
        <p class="memory-section__error" role="alert" data-testid="memory-section-error">
          {error()}
        </p>
      </Show>

      <Show
        when={(entries() ?? []).length > 0}
        fallback={
          <p class="memory-section__empty" data-testid="memory-section-empty">
            No agent definitions loaded for this workspace.
          </p>
        }
      >
        <ul class="memory-section__list" role="list">
          <For each={entries() ?? []}>
            {(entry) => (
              <MemoryRow
                entry={entry}
                onToggle={(next) => void onToggle(entry, next)}
                onEdit={() => setEditing(entry)}
                onRequestClear={() => setPendingClear(entry)}
              />
            )}
          </For>
        </ul>
      </Show>

      <Show when={editing()}>
        {(entry) => (
          <MemoryEditor
            agentId={entry().agent_id}
            path={entry().path}
            readOnly={!effectiveEnabled(entry())}
            useTextareaForTest={props.useTextareaForTest}
            onClose={() => {
              setEditing(null);
              void refetch();
            }}
            onSaved={() => void refetch()}
          />
        )}
      </Show>

      <Show when={pendingClear()}>
        {(entry) => (
          <ClearConfirm
            agentId={entry().agent_id}
            onCancel={() => setPendingClear(null)}
            onConfirm={() => void onClear(entry())}
          />
        )}
      </Show>
    </section>
  );
};

interface MemoryRowProps {
  entry: AgentMemoryEntry;
  onToggle: (next: boolean) => void;
  onEdit: () => void;
  onRequestClear: () => void;
}

const MemoryRow: Component<MemoryRowProps> = (props) => {
  const enabled = () => effectiveEnabled(props.entry);
  const sourceLabel = () =>
    props.entry.settings_override === null ? 'inherit' : 'override';

  return (
    <li class="memory-section__row" data-testid={`memory-row-${props.entry.agent_id}`}>
      <div class="memory-section__row-head">
        <span class="memory-section__agent-id">{props.entry.agent_id}</span>

        <label class="memory-section__toggle">
          <input
            type="checkbox"
            data-testid={`memory-toggle-${props.entry.agent_id}`}
            checked={enabled()}
            onChange={(e) => props.onToggle(e.currentTarget.checked)}
            aria-label={`Toggle memory for ${props.entry.agent_id}`}
          />
          <span class="memory-section__toggle-label">
            {enabled() ? 'ENABLED' : 'DISABLED'} ({sourceLabel()})
          </span>
        </label>

        <div class="memory-section__actions">
          <Button
            variant="ghost"
            size="sm"
            data-testid={`memory-edit-${props.entry.agent_id}`}
            aria-label={`Edit memory for ${props.entry.agent_id}`}
            onClick={props.onEdit}
          >
            EDIT
          </Button>
          <Show when={props.entry.size_bytes !== null}>
            <Button
              variant="ghost"
              size="sm"
              data-testid={`memory-clear-${props.entry.agent_id}`}
              aria-label={`Clear memory for ${props.entry.agent_id}`}
              onClick={props.onRequestClear}
            >
              CLEAR
            </Button>
          </Show>
        </div>
      </div>

      <span class="memory-section__path" data-testid={`memory-path-${props.entry.agent_id}`}>
        {props.entry.path}
      </span>
      <span class="memory-section__meta">
        {formatBytes(props.entry.size_bytes)}
        {' · '}
        v{props.entry.version ?? '—'}
        {' · '}
        updated {formatTimestamp(props.entry.updated_at)}
      </span>
    </li>
  );
};

interface ClearConfirmProps {
  agentId: string;
  onCancel: () => void;
  onConfirm: () => void;
}

const ClearConfirm: Component<ClearConfirmProps> = (props) => {
  let dialogRef: HTMLDivElement | undefined;
  useFocusTrap(() => dialogRef);

  onMount(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        props.onCancel();
      }
    };
    window.addEventListener('keydown', handler);
    onCleanup(() => window.removeEventListener('keydown', handler));
  });

  return (
    <div
      class="memory-editor__backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) props.onCancel();
      }}
      data-testid="memory-clear-modal"
    >
      <div
        ref={dialogRef}
        class="memory-editor memory-editor--small"
        role="dialog"
        aria-modal="true"
        aria-labelledby="memory-clear-title"
      >
        <header class="memory-editor__head">
          <h3 id="memory-clear-title" class="memory-editor__title">
            CLEAR MEMORY?
          </h3>
        </header>
        <div class="memory-editor__warn">
          This wipes <strong>{props.agentId}</strong>'s memory file. The
          previous body cannot be recovered. Continue?
        </div>
        <footer class="memory-editor__foot">
          <div class="memory-editor__actions">
            <Button
              variant="ghost"
              size="sm"
              data-testid="memory-clear-cancel"
              onClick={props.onCancel}
            >
              CANCEL
            </Button>
            <Button
              variant="primary"
              size="sm"
              data-testid="memory-clear-confirm"
              onClick={props.onConfirm}
            >
              CLEAR
            </Button>
          </div>
        </footer>
      </div>
    </div>
  );
};

// F-602: Memory editor flyout — Dashboard-scoped Markdown editor for one
// agent's `~/.config/forge/memory/<agent>.md` file.
//
// The editor mounts the monaco-host iframe (same one EditorPane uses) and
// drives it via the F-121 postMessage protocol — `kind: 'open'` to load
// the body, `kind: 'save'` to request a save event back. The Save button
// + the `Cmd/Ctrl+S` shortcut both fire `kind: 'save'`; the iframe replies
// with `{ kind: 'save', value }` carrying the current buffer, which we
// persist via the F-602 `save_agent_memory` Tauri command.
//
// Test seam: when Monaco can't run (jsdom, vitest), the parent passes
// `useTextareaForTest` so the editor renders a `<textarea>` and elides the
// iframe entirely. This keeps the component testable end-to-end without
// pulling Monaco into jsdom.
//
// Security contract:
//   - Editor draft state lives ONLY in the component's local signal /
//     iframe buffer — never persisted to disk until the user clicks Save.
//   - Read-only mode (memory disabled) hides the Save button and locks the
//     textarea / iframe `readOnly` flag so editing is impossible.
//   - The "DO NOT store secrets" warning is surfaced verbatim above the
//     editor body in every mode.

import {
  type Component,
  createEffect,
  createSignal,
  onCleanup,
  onMount,
  Show,
} from 'solid-js';
import { Button } from '@forge/design';
import { useFocusTrap } from '../../lib/useFocusTrap';
import {
  readAgentMemory,
  saveAgentMemory,
  type AgentMemorySaved,
} from '../../ipc/memory';
import './MemorySection.css';

/** Default URL the iframe loads. Mirrors `EditorPane.DEFAULT_MONACO_HOST_SRC`. */
export const DEFAULT_MONACO_HOST_SRC = '/monaco-host/index.html';

/** Persistent secret-warning copy — duplicated verbatim from
 *  `docs/architecture/memory.md` so the doc and the UI stay in lockstep. */
export const MEMORY_SECRETS_WARNING =
  'DO NOT store secrets in memory — anything here is appended verbatim to every system prompt for this agent.';

export interface MemoryEditorProps {
  agentId: string;
  /** Absolute path the file lives at — surfaced so the user can locate
   *  the file on disk. */
  path: string;
  /** When `true` the editor opens read-only and hides the Save button.
   *  The body is still loaded so the user can review what's stored. */
  readOnly: boolean;
  /** Close the flyout (parent owns lifecycle). */
  onClose: () => void;
  /** Notify the parent that a save landed so it can refresh its row. */
  onSaved?: (result: AgentMemorySaved) => void;
  /** Test seam: render a textarea instead of the Monaco iframe. */
  useTextareaForTest?: boolean | undefined;
  /** Override the iframe `src` (tests pass `about:blank`). */
  src?: string | undefined;
}

type IframeMessage =
  | { kind: 'ready' }
  | { kind: 'opened'; uri: string }
  | { kind: 'closed'; uri: string }
  | { kind: 'save'; uri: string; value: string }
  | { kind: 'change'; uri: string; value: string };

const MEMORY_URI = 'memory://buffer';

export const MemoryEditor: Component<MemoryEditorProps> = (props) => {
  const [body, setBody] = createSignal('');
  const [originalBody, setOriginalBody] = createSignal('');
  const [loading, setLoading] = createSignal(true);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [status, setStatus] = createSignal<string | null>(null);

  let iframeRef: HTMLIFrameElement | undefined;
  let dialogRef: HTMLDivElement | undefined;
  useFocusTrap(() => dialogRef);

  const isDirty = () => body() !== originalBody();

  const useTextarea = () => props.useTextareaForTest === true;

  const postToIframe = (msg: unknown): void => {
    const win = iframeRef?.contentWindow;
    if (!win) return;
    try {
      const targetOrigin = new URL(props.src ?? DEFAULT_MONACO_HOST_SRC, window.location.href)
        .origin;
      if (targetOrigin === '*' || targetOrigin === 'null') return;
      win.postMessage(msg, targetOrigin);
    } catch {
      // Bad URL — silently no-op; production resolves a real origin.
    }
  };

  const sendOpen = (): void => {
    postToIframe({
      kind: 'open',
      uri: MEMORY_URI,
      languageId: 'markdown',
      value: body(),
    });
  };

  // Load the file body once on mount.
  onMount(async () => {
    try {
      const initial = await readAgentMemory(props.agentId);
      setBody(initial);
      setOriginalBody(initial);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  });

  // Wire the iframe protocol when running in production.
  if (!useTextarea()) {
    const handleMessage = (event: MessageEvent): void => {
      // Only the iframe we mounted may drive the editor.
      if (iframeRef && event.source !== iframeRef.contentWindow) return;
      const data = event.data as IframeMessage | undefined;
      if (!data || typeof data !== 'object' || typeof data.kind !== 'string') return;
      switch (data.kind) {
        case 'ready':
          sendOpen();
          break;
        case 'change':
          if (data.uri !== MEMORY_URI) return;
          setBody(data.value);
          break;
        case 'save':
          if (data.uri !== MEMORY_URI) return;
          setBody(data.value);
          void persist(data.value);
          break;
        default:
          break;
      }
    };
    onMount(() => {
      window.addEventListener('message', handleMessage);
    });
    onCleanup(() => {
      window.removeEventListener('message', handleMessage);
    });

    // Re-open the buffer when the loaded body changes (e.g. on first load).
    createEffect(() => {
      if (!loading()) sendOpen();
    });
  }

  const persist = async (value: string): Promise<void> => {
    if (props.readOnly) return;
    setSaving(true);
    setError(null);
    try {
      const result = await saveAgentMemory(props.agentId, value);
      setOriginalBody(value);
      setStatus(`saved v${result.version}`);
      props.onSaved?.(result);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  const requestSave = (): void => {
    if (props.readOnly) return;
    if (useTextarea()) {
      void persist(body());
    } else {
      postToIframe({ kind: 'save', uri: MEMORY_URI });
    }
  };

  const handleKeyDown = (e: KeyboardEvent): void => {
    if (e.key === 'Escape') {
      e.preventDefault();
      props.onClose();
      return;
    }
    if ((e.metaKey || e.ctrlKey) && e.key === 's') {
      e.preventDefault();
      requestSave();
    }
  };

  onMount(() => {
    window.addEventListener('keydown', handleKeyDown);
  });
  onCleanup(() => {
    window.removeEventListener('keydown', handleKeyDown);
  });

  return (
    <div
      class="memory-editor__backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) props.onClose();
      }}
      data-testid="memory-editor-backdrop"
    >
      <div
        ref={dialogRef}
        class="memory-editor"
        role="dialog"
        aria-modal="true"
        aria-labelledby="memory-editor-title"
        data-testid="memory-editor"
      >
        <header class="memory-editor__head">
          <h3 id="memory-editor-title" class="memory-editor__title">
            MEMORY — {props.agentId}
          </h3>
          <span class="memory-editor__path" data-testid="memory-editor-path">
            {props.path}
          </span>
          <Button
            variant="ghost"
            size="sm"
            class="memory-editor__close"
            data-testid="memory-editor-close"
            aria-label="Close memory editor"
            onClick={props.onClose}
          >
            CLOSE
          </Button>
        </header>

        <div class="memory-editor__warn" role="note" data-testid="memory-editor-warning">
          {MEMORY_SECRETS_WARNING}
        </div>

        <Show when={props.readOnly}>
          <div
            class="memory-editor__readonly-banner"
            role="status"
            data-testid="memory-editor-readonly"
          >
            Memory is disabled for this agent — editor is read-only.
          </div>
        </Show>

        <div class="memory-editor__body">
          <Show when={loading()}>
            <span class="memory-section__meta" role="status" data-testid="memory-editor-loading">
              loading…
            </span>
          </Show>
          <Show when={!loading() && useTextarea()}>
            <textarea
              class="memory-editor__textarea"
              data-testid="memory-editor-textarea"
              aria-label={`Memory body for ${props.agentId}`}
              value={body()}
              readonly={props.readOnly}
              onInput={(e) => setBody(e.currentTarget.value)}
            />
          </Show>
          <Show when={!loading() && !useTextarea()}>
            <iframe
              ref={iframeRef}
              class="memory-editor__iframe"
              src={props.src ?? DEFAULT_MONACO_HOST_SRC}
              title={`Monaco editor for ${props.agentId} memory`}
              data-testid="memory-editor-iframe"
            />
          </Show>
        </div>

        <footer class="memory-editor__foot">
          <span class="memory-editor__status" data-testid="memory-editor-status">
            {error() ? `error: ${error()}` : status() ?? (isDirty() ? 'unsaved' : 'clean')}
          </span>
          <div class="memory-editor__actions">
            <Show when={!props.readOnly}>
              <Button
                variant="primary"
                size="sm"
                data-testid="memory-editor-save"
                aria-busy={saving()}
                disabled={saving() || loading() || !isDirty()}
                onClick={requestSave}
              >
                SAVE
              </Button>
            </Show>
          </div>
        </footer>
      </div>
    </div>
  );
};

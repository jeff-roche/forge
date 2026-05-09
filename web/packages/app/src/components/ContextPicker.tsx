import {
  type Component,
  createSignal,
  createEffect,
  For,
  Show,
  onMount,
  onCleanup,
} from 'solid-js';
import type { TreeStatsDto } from '@forge/ipc';
import { Tab } from '@forge/design';
import './ContextPicker.css';

// ---------------------------------------------------------------------------
// Categories (F-141)
// ---------------------------------------------------------------------------
//
// F-141 lands the ContextPicker component shell and chip-insertion plumbing;
// populating each category with resolved results is F-142. The seven tabs
// render as empty here — keep the icon/label shape so F-142 only needs to
// add the resolver + items array per category.

export type ContextCategory =
  | 'file'
  | 'directory'
  | 'selection'
  | 'terminal'
  | 'agent'
  | 'skill'
  | 'url';

export interface CategoryDef {
  id: ContextCategory;
  label: string;
  icon: string;
}

export const CATEGORIES: readonly CategoryDef[] = [
  { id: 'file', label: 'file', icon: '[F]' },
  { id: 'directory', label: 'directory', icon: '[D]' },
  { id: 'selection', label: 'selection', icon: '[S]' },
  { id: 'terminal', label: 'terminal', icon: '[T]' },
  { id: 'agent', label: 'agent', icon: '[A]' },
  { id: 'skill', label: 'skill', icon: '[K]' },
  { id: 'url', label: 'url', icon: '[U]' },
] as const;

// ---------------------------------------------------------------------------
// Popup placement (pure — unit-testable without jsdom layout)
// ---------------------------------------------------------------------------
//
// The DoD requires the popup to "flip to above when near bottom viewport
// edge". jsdom does not compute real layout (`getBoundingClientRect()`
// returns zeros), so the placement decision is extracted to a pure function
// and tested directly. The component calls it with the caret/composer's real
// measurements at mount + on window resize.

export type PopupPlacement = 'above' | 'below';

export interface PlacementInput {
  /** Viewport-relative top of the anchor (caret or composer). */
  anchorTop: number;
  /** Viewport-relative bottom of the anchor. */
  anchorBottom: number;
  /** `window.innerHeight`. */
  viewportHeight: number;
  /** Height of the popup itself (up to 360px per spec). */
  popupHeight: number;
  /** Gap between anchor and popup in px. */
  gap?: number;
}

/**
 * Decide whether the popup renders above or below the anchor.
 *
 * Rule: prefer below when there is room for the popup below the anchor;
 * otherwise flip to above. "Room" means `popupHeight + gap` fits between
 * the anchor bottom and the viewport bottom.
 */
export function computePopupPlacement(input: PlacementInput): PopupPlacement {
  const gap = input.gap ?? 4;
  const spaceBelow = input.viewportHeight - input.anchorBottom - gap;
  if (spaceBelow >= input.popupHeight) return 'below';
  return 'above';
}

// ---------------------------------------------------------------------------
// `@`-trigger detection (pure)
// ---------------------------------------------------------------------------
//
// Detect an active `@` token at the caret. Returns the start index (of the
// `@`) and the query (text after `@` up to the caret) when the caret sits
// inside an `@token` that was introduced by the most recent space/newline
// boundary. Returns null when the caret is not in such a token.
//
// The token ends on whitespace — once the user types a space, the trigger
// is dismissed. Supports only the unicode word-ish chars `\S` (anything
// non-whitespace), which is intentionally permissive: path-like tokens
// (`src/foo.ts`) should keep the picker open.

export interface AtTriggerMatch {
  /** Index of the `@` in the full text. */
  start: number;
  /** Substring after `@` up to the caret — the search query. */
  query: string;
}

export function detectAtTrigger(text: string, caret: number): AtTriggerMatch | null {
  if (caret < 1 || caret > text.length) return null;
  // Walk backwards from the caret looking for an `@` that is either at the
  // start of the text or preceded by whitespace. Stop on whitespace — the
  // trigger is dismissed once the user types a space.
  for (let i = caret - 1; i >= 0; i--) {
    const ch = text[i];
    if (ch === undefined) return null;
    if (/\s/.test(ch)) return null;
    if (ch === '@') {
      const before = i > 0 ? text[i - 1] : undefined;
      if (before === undefined || /\s/.test(before)) {
        return { start: i, query: text.slice(i + 1, caret) };
      }
      return null;
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// Picker result (what the parent turns into a chip)
// ---------------------------------------------------------------------------

export interface PickerResult {
  category: ContextCategory;
  /** The text to display in the chip. */
  label: string;
  /** Opaque identifier the caller can use for downstream resolution. */
  value: string;
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

/**
 * Per-category async state for the results list (F-399).
 * Either a resolved list, a loading sentinel, or an error string.
 *
 * F-536: success carries optional `stats` from the underlying `tree` IPC so
 * the picker can render a "files not shown" / "tree truncated" / "N read
 * errors" notice for tree-backed categories (file, directory). Resolvers
 * without a tree backing simply omit `stats`.
 */
export type CategoryState =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'success'; items: PickerResult[]; stats?: TreeStatsDto | null };

export interface ContextPickerProps {
  /** Current `@`-query (text after the `@`). */
  query: string;
  /** Viewport-relative rect of the anchor (composer) for placement. */
  anchorRect: { top: number; bottom: number; left: number; right: number };
  /** Called when a result is chosen (Enter or click). */
  onPick: (result: PickerResult) => void;
  /** Called when the user dismisses (Esc, blur, or clicks outside). */
  onDismiss: () => void;
  /**
   * Optional category-indexed items — F-141 leaves this empty. F-142 will
   * wire a resolver that populates per category as the user types.
   *
   * Accepts either the legacy flat list shape (backward-compatible) or the
   * richer `CategoryState` shape that carries loading/error per tab (F-399).
   */
  items?: Partial<Record<ContextCategory, PickerResult[] | CategoryState>>;
}

const POPUP_MAX_HEIGHT = 360;

// Stable id prefix for option rows. The combobox root's
// `aria-activedescendant` points at `<prefix>-<index>` so assistive tech can
// announce the active option while DOM focus stays in the composer textarea
// (WAI-ARIA combobox pattern). See F-403.
const RESULT_ID_PREFIX = 'context-picker-result';
const resultId = (index: number): string => `${RESULT_ID_PREFIX}-${index}`;

export const ContextPicker: Component<ContextPickerProps> = (props) => {
  const [activeCategoryIndex, setActiveCategoryIndex] = createSignal(0);
  const [activeItemIndex, setActiveItemIndex] = createSignal(0);
  const [placement, setPlacement] = createSignal<PopupPlacement>('above');

  let rootRef: HTMLDivElement | undefined;

  const activeCategory = (): ContextCategory => {
    const def = CATEGORIES[activeCategoryIndex()];
    return def ? def.id : 'file';
  };

  /**
   * Normalise the raw prop value for the active category into a `CategoryState`.
   * Accepts both the legacy `PickerResult[]` shape (treated as success) and the
   * richer `CategoryState` shape (F-399).
   */
  const activeCategoryState = (): CategoryState => {
    const raw = props.items?.[activeCategory()];
    if (raw === undefined) return { status: 'success', items: [] };
    if (Array.isArray(raw)) return { status: 'success', items: raw };
    return raw;
  };

  const activeItems = (): PickerResult[] => {
    const state = activeCategoryState();
    return state.status === 'success' ? state.items : [];
  };

  /**
   * F-536: build the truncation/error notice string from the active
   * category's `TreeStatsDto`. Mirrors `FilesSidebar`'s wording and rules
   * verbatim so the two surfaces speak the same language:
   *
   *   - "N files not shown"  when the walk was capped with a counted overflow
   *   - "tree truncated"     when truncated but `omitted_count` is the u64::MAX
   *                          sentinel (per F-571 — "stopped counting")
   *   - "N read errors"      when the walker swallowed per-entry errors
   *
   * Returns `null` when the active category has no stats or all-zero stats —
   * the notice is suppressed and the panel renders unchanged.
   */
  const activeStatsNotice = (): string | null => {
    const state = activeCategoryState();
    if (state.status !== 'success') return null;
    const s = state.stats;
    if (!s) return null;
    const parts: string[] = [];
    if (s.truncated && s.omitted_count > 0) {
      parts.push(`${s.omitted_count} file${s.omitted_count === 1 ? '' : 's'} not shown`);
    } else if (s.truncated) {
      parts.push('tree truncated');
    }
    if (s.error_count > 0) {
      parts.push(`${s.error_count} read error${s.error_count === 1 ? '' : 's'}`);
    }
    return parts.length > 0 ? parts.join(' · ') : null;
  };

  // Recompute placement whenever anchorRect updates. In real DOM this runs
  // on mount, window resize, and scroll; in tests it runs synchronously when
  // `anchorRect` changes.
  createEffect(() => {
    const rect = props.anchorRect;
    const viewportHeight =
      typeof window !== 'undefined' ? window.innerHeight : 800;
    setPlacement(
      computePopupPlacement({
        anchorTop: rect.top,
        anchorBottom: rect.bottom,
        viewportHeight,
        popupHeight: POPUP_MAX_HEIGHT,
      }),
    );
  });

  // Reset the item cursor when the active category changes or items change.
  createEffect(() => {
    const _cat = activeCategoryIndex();
    const _items = activeItems();
    setActiveItemIndex(0);
  });

  const commitActive = () => {
    const items = activeItems();
    const item = items[activeItemIndex()];
    if (item) {
      props.onPick(item);
    }
  };

  const handleKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      props.onDismiss();
      return;
    }
    if (e.key === 'Tab') {
      // Tab switches category (shift+Tab goes backwards). Preventing default
      // is required — the textarea still has focus while the popup is open,
      // so the browser's native Tab traversal would otherwise escape into
      // surrounding UI.
      e.preventDefault();
      const dir = e.shiftKey ? -1 : 1;
      const n = CATEGORIES.length;
      setActiveCategoryIndex((i) => (i + dir + n) % n);
      return;
    }
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      const items = activeItems();
      if (items.length === 0) return;
      setActiveItemIndex((i) => (i + 1) % items.length);
      return;
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault();
      const items = activeItems();
      if (items.length === 0) return;
      setActiveItemIndex((i) => (i - 1 + items.length) % items.length);
      return;
    }
    if (e.key === 'Enter') {
      e.preventDefault();
      commitActive();
      return;
    }
  };

  onMount(() => {
    if (typeof window !== 'undefined') {
      window.addEventListener('keydown', handleKeyDown, true);
    }
  });

  onCleanup(() => {
    if (typeof window !== 'undefined') {
      window.removeEventListener('keydown', handleKeyDown, true);
    }
  });

  // Resolve the active option's DOM id for `aria-activedescendant`. Returns
  // `undefined` when the active category has no items — SR should not be
  // pointed at a non-existent node, and Solid drops `undefined` attributes.
  const activeDescendantId = (): string | undefined => {
    const items = activeItems();
    if (items.length === 0) return undefined;
    return resultId(activeItemIndex());
  };

  return (
    <div
      class="context-picker"
      classList={{
        'context-picker--above': placement() === 'above',
        'context-picker--below': placement() === 'below',
      }}
      data-testid="context-picker"
      data-placement={placement()}
      role="combobox"
      aria-expanded="true"
      aria-haspopup="listbox"
      aria-activedescendant={activeDescendantId()}
      ref={rootRef}
    >
      {/* Search field — echoes the live `@`-query from the composer. */}
      <div class="context-picker__search">
        <span class="context-picker__search-icon" aria-hidden="true">@</span>
        <span
          class="context-picker__search-query"
          data-testid="context-picker-query"
        >
          {props.query}
        </span>
      </div>

      {/* Seven category tabs (segmented list). */}
      <div
        class="context-picker__tabs"
        role="tablist"
        data-testid="context-picker-tabs"
      >
        <For each={CATEGORIES}>
          {(cat, i) => (
            <Tab
              selected={i() === activeCategoryIndex()}
              class={`context-picker__tab${i() === activeCategoryIndex() ? ' context-picker__tab--active' : ''}`}
              data-testid={`context-picker-tab-${cat.id}`}
              onMouseDown={(e) => {
                // onMouseDown so focus stays in the textarea.
                e.preventDefault();
                setActiveCategoryIndex(i());
              }}
            >
              <span class="context-picker__tab-icon" aria-hidden="true">
                {cat.icon}
              </span>
              <span class="context-picker__tab-label">{cat.label}</span>
            </Tab>
          )}
        </For>
      </div>

      {/* F-536: inline truncation / read-error notice. Mirrors the
          FilesSidebar's selector, role, and wording (see
          `[data-testid="files-sidebar-stats-notice"]`). Rendered above the
          listbox so screen readers announce it before the option list. */}
      <Show when={activeStatsNotice() !== null}>
        <div
          class="context-picker__stats-notice"
          role="status"
          data-testid="picker-truncation-notice"
        >
          {activeStatsNotice()}
        </div>
      </Show>

      {/* Results list for the active category. F-141 renders empty — F-142
          will populate via the `items` prop. F-399 adds loading/error states. */}
      <div
        class="context-picker__results"
        role="listbox"
        data-testid="context-picker-results"
      >
        <Show when={activeCategoryState().status === 'loading'}>
          <div
            class="context-picker__loading"
            data-testid="context-picker-loading"
            aria-live="polite"
          >
            Searching…
          </div>
        </Show>

        <Show when={activeCategoryState().status === 'error'}>
          <div
            class="context-picker__error"
            data-testid="context-picker-error"
            role="alert"
          >
            {(activeCategoryState() as { status: 'error'; message: string }).message}
          </div>
        </Show>

        <Show when={activeCategoryState().status === 'success'}>
          <Show
            when={activeItems().length > 0}
            fallback={
              <div
                class="context-picker__empty"
                data-testid="context-picker-empty"
              >
                No {activeCategory()} results
              </div>
            }
          >
            <For each={activeItems()}>
              {(item, i) => (
                <div
                  id={resultId(i())}
                  class="context-picker__result"
                  classList={{
                    'context-picker__result--active':
                      i() === activeItemIndex(),
                  }}
                  role="option"
                  aria-selected={i() === activeItemIndex()}
                  data-testid={`context-picker-result-${i()}`}
                  onMouseDown={(e) => {
                    e.preventDefault();
                    props.onPick(item);
                  }}
                >
                  <span class="context-picker__result-label">{item.label}</span>
                </div>
              )}
            </For>
          </Show>
        </Show>
      </div>
    </div>
  );
};

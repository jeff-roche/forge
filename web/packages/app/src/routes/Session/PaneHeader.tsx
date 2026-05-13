import type { Component, JSX } from 'solid-js';
import { Show, createEffect } from 'solid-js';
import { Button } from '@forge/design';
import type { ProviderId } from '@forge/ipc';
import { providerAccent } from './providerAccent';
import './PaneHeader.css';

/**
 * Visual pane type (`pane-header.md §PH.2` type label). `CHAT` is the default
 * so existing session-window callers do not need to pass it explicitly; new
 * TERMINAL/EDITOR panes opt in.
 */
export type PaneHeaderType = 'CHAT' | 'TERMINAL' | 'EDITOR';

/**
 * Responsive compactness level per F-119 (`layout-panes.md §3.7`,
 * `pane-header.md §PH.4`). Derived from the enclosing pane's width via
 * `usePaneWidth`. See that hook for threshold semantics.
 */
export type Compactness = 'full' | 'compact' | 'icon-only';

/**
 * Map each pane type to its icon-only glyph. Used when the header collapses
 * its type-label text under F-119's `compact` / `icon-only` thresholds.
 * Emoji keeps the glyph system-font-safe (no extra icon dependency) and
 * stable across Phase 1's token refresh cadence.
 */
const TYPE_ICON: Record<PaneHeaderType, string> = {
  CHAT: '\u{1F4AC}',
  TERMINAL: '\u2328\uFE0F',
  EDITOR: '\u270E\uFE0F',
};

/**
 * F-748: per-pane health signal surfaced as a status pill in the header.
 *
 * - `idle` (default) — no pill; the session is operating normally.
 * - `detecting` — the bridge has noticed silence on the UDS read pipe but
 *   has not yet confirmed daemon death. Rendered with a subtle pulse so
 *   the operator notices something is off before the overlay arrives.
 * - `error` — daemon-death has been confirmed; the crash-restart overlay
 *   is up. The pill anchors the visual state for screen readers / casual
 *   glances; the overlay carries the affordance.
 */
export type PaneHeaderStatus = 'idle' | 'detecting' | 'error';

export interface PaneHeaderProps {
  subject: string;
  /**
   * F-394 trailing slot. Caller-owned JSX rendered between the subject and
   * the cost-meter slot — the natural home for pane-specific badges like
   * EditorPane's dirty indicator. Non-essential chrome: removed from the
   * tree at `icon-only` alongside the provider pill and cost meter so
   * screen readers don't announce vestigial content.
   */
  trailing?: JSX.Element | undefined;
  /**
   * F-394 drag-source handler. Forwarded as `onPointerDown` on the
   * `<header>`. Consumers threaded through F-118's drag-to-dock gesture
   * (e.g. `dockApi.startDrag(leafId)`) pass it here; the header is the
   * only non-content surface on a pane and the canonical drag handle.
   */
  onHeaderPointerDown?: ((e: PointerEvent) => void) | undefined;
  /**
   * Active provider id (F-091). Drives the pill accent color via the
   * `--pane-header-provider-accent` custom property; the display text remains
   * the caller-supplied `providerLabel`.
   *
   * Only meaningful for chat panes. Non-chat panes (terminal, editor) omit
   * the provider pill by leaving `providerId`/`providerLabel` `undefined`.
   */
  providerId?: ProviderId;
  providerLabel?: string;
  /**
   * Cost meter text. Chat panes render an `in … · out … · $…` string; other
   * pane types pass a short status string (e.g. a terminal's `cwd`) or
   * `undefined` to suppress the slot entirely.
   */
  costLabel?: string | undefined;
  /**
   * Close-button label copy (`pane-header.md §PH.6`). Chat panes use
   * `CLOSE SESSION`; terminal/editor panes pass `CLOSE PANE`/`CLOSE TAB`.
   * Defaults to `CLOSE SESSION` so existing callers keep their behavior.
   */
  closeLabel?: string;
  /**
   * Accessible name for the close button — chat uses `Close session window`;
   * terminal/editor variants pass `Close pane` / `Close tab`.
   */
  closeAriaLabel?: string;
  /**
   * `CHAT` | `TERMINAL` | `EDITOR` label shown in the type slot. Defaults to
   * `CHAT` for call-site compatibility with the Phase-1 session window.
   */
  typeLabel?: PaneHeaderType;
  /**
   * Responsive compactness level, typically derived from `usePaneWidth`
   * against the enclosing pane element. Controls label collapse (`compact`
   * at <320px) and badge removal (`icon-only` at <240px). Defaults to
   * `full`, preserving pre-F-119 layout for untouched callers.
   * Per `docs/ui-specs/layout-panes.md §3.7` and `pane-header.md §2.3`.
   */
  compactness?: Compactness;
  /**
   * F-748: optional session-health pill — drives the small chip rendered
   * between the subject and the provider pill. Omitted callers see no
   * pill (idle); SessionWindow flips this to `detecting` / `error` to
   * mirror the crash-restart overlay state.
   */
  status?: PaneHeaderStatus;
  onClose: () => void;
}

/**
 * Pane header (28px) per `docs/ui-specs/pane-header.md`. Shows the type
 * label + subject, an optional provider pill (chat), an optional cost/detail
 * meter (chat cost, terminal cwd), and a close action.
 *
 * F-125: non-chat panes opt out of the provider pill by omitting `providerId`
 * / `providerLabel`. `costLabel` is also optional so the terminal variant can
 * render the cwd in the same margin-left-auto slot, and `closeLabel` lets the
 * terminal variant render `CLOSE PANE` instead of `CLOSE SESSION`.
 *
 * F-119: at narrow widths the header collapses chrome. The type label swaps
 * to a compact-safe icon glyph at `compact` (<320px) and the provider pill +
 * cost meter are removed from the tree entirely at `icon-only` (<240px) —
 * removed rather than hidden so screen readers don't announce vestigial
 * content. The close button stays at every compactness level per §PH.6.
 *
 * F-394: exposes a `trailing` slot for pane-specific badges (e.g. the
 * editor's dirty-dot) and forwards `onHeaderPointerDown` so consumers keep
 * a single header surface rather than wrapping the primitive. The sub-
 * structural `<header>` no longer claims `role="banner"` — ARIA reserves
 * the banner landmark for the one top-level banner per document.
 */
export const PaneHeader: Component<PaneHeaderProps> = (props) => {
  // The `--pane-header-provider-accent` custom property keeps the
  // `.pane-header__provider` rule generic; the rule reads
  // `var(--pane-header-provider-accent, var(--color-provider-local))`.
  // F-805: written via `setProperty` on a ref rather than a JSX `style={...}`
  // binding so `style-src-attr` can drop `'unsafe-inline'`.
  let pillRef: HTMLSpanElement | undefined;
  createEffect(() => {
    if (!pillRef) return;
    if (props.providerId) {
      pillRef.style.setProperty(
        '--pane-header-provider-accent',
        providerAccent(props.providerId),
      );
    } else {
      pillRef.style.removeProperty('--pane-header-provider-accent');
    }
  });
  const compactness = (): Compactness => props.compactness ?? 'full';
  const typeLabel = (): PaneHeaderType => props.typeLabel ?? 'CHAT';
  const isIconLabel = (): boolean => compactness() !== 'full';
  const showBadges = (): boolean => compactness() !== 'icon-only';
  return (
    <header
      class="pane-header"
      data-compactness={compactness()}
      onPointerDown={props.onHeaderPointerDown}
    >
      <span
        class="pane-header__type-label"
        data-testid="pane-header-type-label"
        data-icon-only={isIconLabel() ? 'true' : 'false'}
        aria-label={`${typeLabel().toLowerCase()} pane`}
      >
        {isIconLabel() ? TYPE_ICON[typeLabel()] : typeLabel()}
      </span>
      <span class="pane-header__subject" data-testid="pane-header-subject">
        {props.subject}
      </span>
      <Show
        when={
          showBadges() &&
          props.status !== undefined &&
          props.status !== 'idle'
        }
      >
        <span
          class="pane-header__status"
          data-testid="pane-header-status"
          data-status={props.status}
          role="status"
          aria-label={
            props.status === 'error'
              ? 'session crashed'
              : 'session unresponsive'
          }
        >
          {props.status === 'error' ? 'crashed' : 'unresponsive'}
        </span>
      </Show>
      <Show when={showBadges() && props.trailing !== undefined}>
        <span class="pane-header__trailing" data-testid="pane-header-trailing">
          {props.trailing}
        </span>
      </Show>
      <Show
        when={
          showBadges() &&
          props.providerId !== undefined &&
          props.providerLabel !== undefined
        }
      >
        <span
          ref={pillRef}
          class="pane-header__provider"
          data-testid="pane-header-provider"
        >
          {props.providerLabel}
        </span>
      </Show>
      <Show when={showBadges() && props.costLabel !== undefined}>
        <span class="pane-header__cost" data-testid="pane-header-cost">
          {props.costLabel}
        </span>
      </Show>
      <Button
        variant="ghost"
        size="sm"
        class="pane-header__close"
        aria-label={props.closeAriaLabel ?? 'Close session window'}
        onClick={props.onClose}
      >
        {props.closeLabel ?? 'CLOSE SESSION'}
      </Button>
    </header>
  );
};

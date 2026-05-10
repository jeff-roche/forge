import {
  type Component,
  type JSX,
  createEffect,
  createSignal,
  onCleanup,
} from 'solid-js';
import './SplitPane.css';

/**
 * Minimum ratio floor for either child. F-117 is ratio-only — the 320px
 * pixel minimum from `layout-panes.md` §3.7 is F-119 and intentionally
 * out of scope here. This floor just stops the divider from producing
 * a zero-width/height child or a negative value.
 */
export const MIN_RATIO = 0.1;

/**
 * Pixel snap increment for divider drag, per `layout-panes.md` §3.1:
 * "Dragging resizes in 4px snaps; Alt+drag is unsnapped."
 */
export const SNAP_PX = 4;

/** Balanced split used by the §3.1 double-click reset. */
export const RESET_RATIO = 0.5;

/**
 * Base keyboard step for arrow-key resize per the ARIA APG window-splitter
 * pattern (F-404). Expressed as a ratio so it works independently of the
 * container's pixel extent. 2% matches the issue's Remediation.
 */
export const KEYBOARD_STEP = 0.02;

/**
 * Multiplier applied to {@link KEYBOARD_STEP} when Shift is held, for a
 * coarser nudge. Matches the issue's "Shift+Arrow → 10×".
 */
export const KEYBOARD_SHIFT_MULTIPLIER = 10;

export interface SplitPaneProps {
  /** `"h"` → children stacked top/bottom; `"v"` → side by side. */
  direction: 'h' | 'v';
  /** First child's size as a fraction of the container (0..1). */
  ratio: number;
  /** Emitted on drag and on double-click reset. Caller owns the ratio. */
  onRatioChange: (next: number) => void;
  /** First child (top for `"h"`, left for `"v"`). */
  children: [JSX.Element, JSX.Element] | JSX.Element[];
}

/**
 * Two-child splitter with a draggable divider. Ratio is controlled — the
 * parent owns the value and receives updates through `onRatioChange`.
 *
 * Behavior comes from `docs/ui-specs/layout-panes.md`:
 *   - §3.1 "Gridlines are 1px in `--color-border-1`. Dragging resizes in
 *     4px snaps; Alt+drag is unsnapped. Double-clicking a gridline resets
 *     to balanced split."
 *   - §3.5 divider handle is the resize affordance — the ≥8px hit area and
 *     `col-resize` / `row-resize` cursors live here; the buttons in the tab
 *     bar are a separate concern.
 *
 * Out of scope (separate issues):
 *   F-118 drag-to-dock · F-119 320px min-width collapse · F-120 persistence
 */
export const SplitPane: Component<SplitPaneProps> = (props) => {
  let containerRef: HTMLDivElement | undefined;
  const [dragging, setDragging] = createSignal(false);
  // F-573: cache the container's bounding rect at pointerdown so the
  // pointermove hot path no longer pays a forced-layout read on every
  // event. The container can't resize mid-drag without disrupting the
  // gesture (the user would have to release the pointer to interact with
  // any other layout-mutating control), so the rect is stable for the
  // lifetime of the drag.
  let dragRect: DOMRect | null = null;
  // F-573: rAF coalesces onRatioChange so heavy children (monaco / xterm)
  // re-render at most once per frame instead of once per pointermove.
  // Mirrors the throttling in `TerminalPane.handleResize`.
  let pendingRatio: number | null = null;
  let rafHandle: number | null = null;

  const clamp = (r: number): number => {
    if (r < MIN_RATIO) return MIN_RATIO;
    if (r > 1 - MIN_RATIO) return 1 - MIN_RATIO;
    return r;
  };

  // Read the current container rect — preferring the drag-cached value when
  // a drag is in progress, falling back to a live read for the keyboard /
  // double-click paths that don't go through pointerdown.
  const currentRect = (): DOMRect | null => {
    if (dragRect !== null) return dragRect;
    if (!containerRef) return null;
    return containerRef.getBoundingClientRect();
  };

  // Translate an absolute pointer position into a clamped ratio along the
  // split axis. Reads the cached drag rect when present so jsdom tests that
  // stub `getBoundingClientRect()` on the container still observe the
  // expected dimensions on the first cache write.
  const ratioFromPointer = (clientX: number, clientY: number): number | null => {
    const rect = currentRect();
    if (rect === null) return null;
    const extent = props.direction === 'v' ? rect.width : rect.height;
    if (extent <= 0) return null;
    const offset =
      props.direction === 'v' ? clientX - rect.left : clientY - rect.top;
    return clamp(offset / extent);
  };

  // 4px snap per §3.1. Alt+drag bypasses the snap.
  const applySnap = (raw: number, altPressed: boolean): number => {
    if (altPressed) return raw;
    const rect = currentRect();
    if (rect === null) return raw;
    const extent = props.direction === 'v' ? rect.width : rect.height;
    if (extent <= 0) return raw;
    const px = raw * extent;
    const snapped = Math.round(px / SNAP_PX) * SNAP_PX;
    return clamp(snapped / extent);
  };

  const flushRatio = () => {
    rafHandle = null;
    if (pendingRatio === null) return;
    const next = pendingRatio;
    pendingRatio = null;
    props.onRatioChange(next);
  };

  const scheduleRatioChange = (next: number) => {
    pendingRatio = next;
    if (rafHandle !== null) return;
    if (typeof requestAnimationFrame === 'function') {
      rafHandle = requestAnimationFrame(flushRatio);
    } else {
      // Fallback for environments without rAF (some test harnesses).
      flushRatio();
    }
  };

  const handlePointerDown = (e: PointerEvent) => {
    // Only primary button — Tauri's non-native window chrome makes the
    // right/middle-button semantics of system panes less relevant here.
    if (e.button !== 0) return;
    const target = e.currentTarget as HTMLElement;
    target.setPointerCapture(e.pointerId);
    // Cache the rect up-front so every pointermove uses a single rect read
    // for the whole gesture instead of two per move.
    dragRect = containerRef ? containerRef.getBoundingClientRect() : null;
    setDragging(true);
    e.preventDefault();
  };

  const handlePointerMove = (e: PointerEvent) => {
    if (!dragging()) return;
    const raw = ratioFromPointer(e.clientX, e.clientY);
    if (raw === null) return;
    scheduleRatioChange(applySnap(raw, e.altKey));
  };

  const handlePointerUp = (e: PointerEvent) => {
    if (!dragging()) return;
    const target = e.currentTarget as HTMLElement;
    if (target.hasPointerCapture(e.pointerId)) {
      target.releasePointerCapture(e.pointerId);
    }
    setDragging(false);
    dragRect = null;
    // Flush any pending rAF synchronously so the released ratio is the one
    // the parent persists — no risk of the gesture ending on a stale value.
    if (rafHandle !== null && typeof cancelAnimationFrame === 'function') {
      cancelAnimationFrame(rafHandle);
      rafHandle = null;
    }
    if (pendingRatio !== null) {
      const next = pendingRatio;
      pendingRatio = null;
      props.onRatioChange(next);
    }
  };

  onCleanup(() => {
    if (rafHandle !== null && typeof cancelAnimationFrame === 'function') {
      cancelAnimationFrame(rafHandle);
    }
    rafHandle = null;
    pendingRatio = null;
    dragRect = null;
  });

  // §3.1: "Double-clicking a gridline resets to balanced split."
  const handleDoubleClick = () => {
    props.onRatioChange(RESET_RATIO);
  };

  // Keyboard resize per ARIA APG window-splitter pattern (F-404). Accepts
  // arrows on both axes in either orientation so the control is forgiving;
  // Shift coarsens the step; Home/End snap to the floor/ceiling; Enter
  // resets to balanced. `preventDefault()` suppresses page scroll for the
  // keys we consume.
  const handleKeyDown = (e: KeyboardEvent) => {
    const step = e.shiftKey ? KEYBOARD_STEP * KEYBOARD_SHIFT_MULTIPLIER : KEYBOARD_STEP;
    const current = clamp(props.ratio);
    let next: number | null = null;

    switch (e.key) {
      case 'ArrowLeft':
      case 'ArrowUp':
        next = clamp(current - step);
        break;
      case 'ArrowRight':
      case 'ArrowDown':
        next = clamp(current + step);
        break;
      case 'Home':
        next = MIN_RATIO;
        break;
      case 'End':
        next = 1 - MIN_RATIO;
        break;
      case 'Enter':
        next = RESET_RATIO;
        break;
      default:
        return;
    }

    e.preventDefault();
    props.onRatioChange(next);
  };

  const ariaValueNow = (): number => Math.round(clamp(props.ratio) * 100);
  const ariaValueMin = Math.round(MIN_RATIO * 100);
  const ariaValueMax = Math.round((1 - MIN_RATIO) * 100);

  // F-805: write the per-child sizing via `setProperty` on refs rather than
  // a JSX `style={...}` binding. The previous inline form required
  // `style-src-attr 'unsafe-inline'` in the production CSP; refs let us
  // drop that. Toggling on `direction` clears the previously-written axis
  // so a v↔h flip doesn't leave a stale `width` or `height` behind.
  let firstRef: HTMLDivElement | undefined;
  let secondRef: HTMLDivElement | undefined;
  const applyChildSize = (el: HTMLDivElement | undefined, fraction: number) => {
    if (!el) return;
    const pct = `${fraction * 100}%`;
    if (props.direction === 'v') {
      el.style.setProperty('width', pct);
      el.style.removeProperty('height');
    } else {
      el.style.setProperty('height', pct);
      el.style.removeProperty('width');
    }
  };
  createEffect(() => {
    const r = clamp(props.ratio);
    applyChildSize(firstRef, r);
    applyChildSize(secondRef, 1 - r);
  });

  const [first, second] = props.children as [JSX.Element, JSX.Element];

  return (
    <div
      class="split-pane"
      classList={{
        'split-pane--h': props.direction === 'h',
        'split-pane--v': props.direction === 'v',
      }}
      data-direction={props.direction}
      data-testid="split-pane"
      ref={containerRef}
    >
      <div
        ref={firstRef}
        class="split-pane__child split-pane__child--first"
      >
        {first}
      </div>
      <div
        class="split-pane__divider"
        data-testid="split-pane-divider"
        role="separator"
        aria-orientation={props.direction === 'v' ? 'vertical' : 'horizontal'}
        aria-valuenow={ariaValueNow()}
        aria-valuemin={ariaValueMin}
        aria-valuemax={ariaValueMax}
        tabIndex={0}
        onPointerDown={handlePointerDown}
        onPointerMove={handlePointerMove}
        onPointerUp={handlePointerUp}
        onPointerCancel={handlePointerUp}
        onDblClick={handleDoubleClick}
        onKeyDown={handleKeyDown}
      />
      <div
        ref={secondRef}
        class="split-pane__child split-pane__child--second"
      >
        {second}
      </div>
    </div>
  );
};

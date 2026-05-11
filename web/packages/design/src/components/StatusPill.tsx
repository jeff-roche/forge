import { type Component, type JSX, splitProps, Show } from 'solid-js';

/**
 * Status pill variants. Each variant binds the foreground color; the
 * background is transparent so the pill reads as inline metadata, not as a
 * chip (per `DESIGN.md §Status pill`).
 *
 * Live variants (`streaming`, `awaiting`) render a leading dot that inherits
 * the pill color. The dot is **static** for F-720 — the pulse animation
 * lands in F-740. `done` renders a static dot; `idle` omits the dot
 * entirely; `ready` / `auth` render the live-dot glow used by the providers
 * card.
 */
export type StatusPillVariant =
  | 'streaming'
  | 'awaiting'
  | 'done'
  | 'idle'
  | 'ready'
  | 'auth';

export interface StatusPillProps
  extends Omit<JSX.HTMLAttributes<HTMLSpanElement>, 'role'> {
  /** Variant — picks the color binding and dot visibility. */
  variant: StatusPillVariant;
}

/**
 * Forge status pill primitive (F-720). Compact lower-case state label used
 * in session rows, provider cards, and the chat composer. Owns DOM shape
 * and color binding only; placement (right-aligned in a row, left of a
 * label, etc.) is the caller's responsibility.
 *
 * Variant labels are not assumed — `children` carries the verbatim copy
 * (`streaming`, `awaiting approval`, `done`, `idle`, `ready`, `auth`).
 * Pass it explicitly so consumers can change the copy independently of
 * the visual variant when a callsite needs to.
 */
export const StatusPill: Component<StatusPillProps> = (props) => {
  const [own, rest] = splitProps(props, ['variant', 'class', 'children']);

  const className = (): string => {
    const parts = ['forge-status-pill', `forge-status-pill--${own.variant}`];
    if (own.class) parts.push(own.class);
    return parts.join(' ');
  };

  // `idle` is the only variant that omits the dot — every other variant
  // renders one so the eye can pick the state up at a glance even when the
  // label is read peripherally.
  const showDot = (): boolean => own.variant !== 'idle';

  return (
    <span class={className()} {...rest}>
      <Show when={showDot()}>
        <span class="forge-status-pill__dot" aria-hidden="true" />
      </Show>
      <span class="forge-status-pill__label">{own.children}</span>
    </span>
  );
};

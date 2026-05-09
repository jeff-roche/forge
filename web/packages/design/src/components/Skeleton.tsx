import { type Component, type JSX, splitProps, For } from 'solid-js';

/** Visual variant. `block` is the default rectangle; `text` renders a shorter,
 *  text-line shape (~1em height); `card` matches the multi-row card silhouette
 *  the dashboard uses for its grid surfaces. */
export type SkeletonVariant = 'block' | 'text' | 'card';

export interface SkeletonProps
  extends Omit<JSX.HTMLAttributes<HTMLDivElement>, 'role' | 'aria-busy'> {
  /** Visual treatment. Defaults to `'block'`. */
  variant?: SkeletonVariant;
  /** Optional row count for list/grid placeholders. Renders `count` siblings.
   *  Defaults to `1` (the wrapping element is the only placeholder). */
  count?: number;
  /**
   * Accessible label announced to assistive tech once. Defaults to `'Loading'`.
   * The placeholder group itself carries `role="status"` + `aria-live="polite"`
   * so screen readers register the load without spamming on every paint.
   */
  label?: string;
}

/**
 * Forge skeleton primitive (F-684). Shape-matched placeholder for loading
 * surfaces that resolve into grids, lists, or cards. Replaces plain-text
 * "loading…" copy per `docs/design/ai-patterns.md` §"Interaction states".
 *
 * Use `count > 1` to render multiple stacked rows for list / grid surfaces.
 * The wrapper sets `role="status"` + `aria-busy="true"` so the placeholder
 * pattern is announced as a load state, not an empty region.
 */
export const Skeleton: Component<SkeletonProps> = (props) => {
  const [own, rest] = splitProps(props, ['variant', 'count', 'label', 'class']);

  const variant = (): SkeletonVariant => own.variant ?? 'block';
  const count = (): number => Math.max(1, own.count ?? 1);
  const label = (): string => own.label ?? 'Loading';

  const itemClass = (): string => {
    const parts = ['forge-skeleton', `forge-skeleton--${variant()}`];
    if (own.class) parts.push(own.class);
    return parts.join(' ');
  };

  return (
    <div
      class="forge-skeleton-group"
      role="status"
      aria-busy="true"
      aria-live="polite"
      aria-label={label()}
      {...rest}
    >
      <For each={Array.from({ length: count() })}>
        {() => <div class={itemClass()} aria-hidden="true" />}
      </For>
    </div>
  );
};

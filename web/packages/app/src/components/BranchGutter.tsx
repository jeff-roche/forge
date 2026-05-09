import { type Component, createEffect } from 'solid-js';
import './BranchGutter.css';

/**
 * F-145 — branch gutter thread-line.
 *
 * Spec `docs/ui-specs/branching.md` §15.4:
 *   > a subtle indicator in the gutter shows which variant path the
 *   > conversation is currently on: a 2px vertical line in
 *   > `--color-ember-300` that threads between the variant selector
 *   > and its children.
 *
 * Rendered as a positioned stripe running the height of the container
 * it's mounted in. The `depth` prop supports nested branches — each
 * level indents the line by 4px so a descendant branch's gutter sits
 * alongside its ancestor's. The stripe uses `--color-ember-300`
 * (#ff7a30), the warm-hover secondary from the ember scale.
 */
export interface BranchGutterProps {
  /**
   * Nesting depth — 0 is the outermost branch, N is the Nth nested
   * branch under it. Indent is `depth * 4px`. The 2px line width is
   * fixed per spec.
   */
  depth: number;
}

export const BranchGutter: Component<BranchGutterProps> = (props) => {
  // Bridge `depth` into CSS via the `--branch-depth` custom property so the
  // 4px indent step is owned by `tokens.css` (`var(--sp-1)`). Set through
  // `setProperty` on a ref rather than a JSX `style={...}` binding so
  // `style-src-attr` can drop `'unsafe-inline'` (F-805).
  let ref: HTMLSpanElement | undefined;
  createEffect(() => {
    ref?.style.setProperty('--branch-depth', String(props.depth));
  });

  return (
    <span
      ref={ref}
      class="branch-gutter"
      data-testid="branch-gutter"
      data-branch-depth={String(props.depth)}
      aria-hidden="true"
    />
  );
};

import { describe, expect, it } from 'vitest';
import { render } from '@solidjs/testing-library';
import { BranchGutter } from './BranchGutter';

describe('BranchGutter', () => {
  it('renders a 2px vertical line with data-branch-depth=0 at the outer branch', () => {
    const { getByTestId } = render(() => <BranchGutter depth={0} />);
    const gutter = getByTestId('branch-gutter');
    expect(gutter.getAttribute('data-branch-depth')).toBe('0');
    // The depth bridges into CSS via the `--branch-depth` custom property;
    // the actual `left` offset is computed by tokens.css (`var(--sp-1)`).
    expect(gutter.getAttribute('style')).toContain('--branch-depth: 0');
  });

  it('bridges depth into the --branch-depth custom property for nested branches', () => {
    const { getByTestId } = render(() => <BranchGutter depth={2} />);
    const gutter = getByTestId('branch-gutter');
    expect(gutter.getAttribute('data-branch-depth')).toBe('2');
    expect(gutter.getAttribute('style')).toContain('--branch-depth: 2');
  });

  it('renders separate gutters per nesting level when mounted together', () => {
    // Simulate a nested-branches scenario: two gutters mounted alongside
    // each other, depths 0 and 1. Spec §15.4 implies the child's gutter
    // sits alongside the parent's — two distinct DOM nodes with different
    // indents.
    const { getAllByTestId } = render(() => (
      <div>
        <BranchGutter depth={0} />
        <BranchGutter depth={1} />
      </div>
    ));
    const gutters = getAllByTestId('branch-gutter');
    expect(gutters).toHaveLength(2);
    expect(gutters[0]!.getAttribute('style')).toContain('--branch-depth: 0');
    expect(gutters[1]!.getAttribute('style')).toContain('--branch-depth: 1');
  });
});

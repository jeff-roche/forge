import { describe, expect, it } from 'vitest';
import { cleanup, render } from '@solidjs/testing-library';
import { Skeleton } from './Skeleton';

describe('Skeleton', () => {
  it('renders a status region with aria-busy=true', () => {
    const { getByRole } = render(() => <Skeleton />);
    const status = getByRole('status');
    expect(status.getAttribute('aria-busy')).toBe('true');
    expect(status.getAttribute('aria-live')).toBe('polite');
    cleanup();
  });

  it('uses a default Loading label and forwards a custom one', () => {
    const { getByRole, unmount } = render(() => <Skeleton />);
    expect(getByRole('status').getAttribute('aria-label')).toBe('Loading');
    unmount();
    const custom = render(() => <Skeleton label="Loading providers" />);
    expect(custom.getByRole('status').getAttribute('aria-label')).toBe(
      'Loading providers',
    );
    custom.unmount();
  });

  it('applies the block variant by default', () => {
    const { getByRole } = render(() => <Skeleton />);
    const status = getByRole('status');
    const items = status.querySelectorAll('.forge-skeleton');
    expect(items.length).toBe(1);
    expect(items[0]?.classList.contains('forge-skeleton--block')).toBe(true);
    cleanup();
  });

  it.each(['block', 'text', 'card'] as const)(
    'wires variant=%s to a class modifier',
    (variant) => {
      const { getByRole } = render(() => <Skeleton variant={variant} />);
      const item = getByRole('status').querySelector('.forge-skeleton');
      expect(item?.classList.contains(`forge-skeleton--${variant}`)).toBe(true);
      cleanup();
    },
  );

  it('renders `count` items when count > 1', () => {
    const { getByRole } = render(() => <Skeleton count={3} variant="card" />);
    const items = getByRole('status').querySelectorAll('.forge-skeleton');
    expect(items.length).toBe(3);
    for (const item of Array.from(items)) {
      expect(item.classList.contains('forge-skeleton--card')).toBe(true);
    }
    cleanup();
  });

  it('clamps non-positive counts to 1', () => {
    const { getByRole } = render(() => <Skeleton count={0} />);
    expect(getByRole('status').querySelectorAll('.forge-skeleton').length).toBe(1);
    cleanup();
  });

  it('forwards extra attributes (class, data-*) to the wrapper', () => {
    const { getByRole } = render(() => (
      <Skeleton class="extra" data-testid="sk" />
    ));
    const status = getByRole('status');
    expect(status.classList.contains('forge-skeleton-group')).toBe(true);
    expect(status.getAttribute('data-testid')).toBe('sk');
    // The custom class is merged into each item, not the wrapper, so the
    // group still carries its baseline class.
    const items = status.querySelectorAll('.forge-skeleton');
    expect(items[0]?.classList.contains('extra')).toBe(true);
    cleanup();
  });
});

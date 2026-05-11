import { describe, expect, it, afterEach } from 'vitest';
import { cleanup, render } from '@solidjs/testing-library';
import { StatusPill, type StatusPillVariant } from './StatusPill';

describe('StatusPill', () => {
  afterEach(cleanup);

  it('renders the variant class and verbatim label children', () => {
    const { container } = render(() => (
      <StatusPill variant="streaming">streaming</StatusPill>
    ));
    const pill = container.querySelector('.forge-status-pill');
    expect(pill).not.toBeNull();
    expect(pill!.classList.contains('forge-status-pill--streaming')).toBe(true);
    expect(pill!.textContent).toBe('streaming');
  });

  it.each<[StatusPillVariant, string, boolean]>([
    ['streaming', 'streaming', true],
    ['awaiting', 'awaiting approval', true],
    ['done', 'done', true],
    ['idle', 'idle', false],
    ['ready', 'ready', true],
    ['auth', 'auth', true],
  ])('renders the %s variant (dot=%s)', (variant, label, dot) => {
    const { container } = render(() => (
      <StatusPill variant={variant}>{label}</StatusPill>
    ));
    const pill = container.querySelector(`.forge-status-pill--${variant}`);
    expect(pill).not.toBeNull();
    expect(pill!.textContent).toBe(label);
    expect(pill!.querySelector('.forge-status-pill__dot') !== null).toBe(dot);
  });

  it('forwards aria-label and other HTML attributes', () => {
    const { container } = render(() => (
      <StatusPill variant="done" aria-label="session complete">
        done
      </StatusPill>
    ));
    const pill = container.querySelector('.forge-status-pill');
    expect(pill!.getAttribute('aria-label')).toBe('session complete');
  });

  it('merges a caller-supplied class with the variant class', () => {
    const { container } = render(() => (
      <StatusPill variant="idle" class="custom-class">
        idle
      </StatusPill>
    ));
    const pill = container.querySelector('.forge-status-pill');
    expect(pill!.classList.contains('forge-status-pill--idle')).toBe(true);
    expect(pill!.classList.contains('custom-class')).toBe(true);
  });
});

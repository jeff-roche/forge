import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render } from '@solidjs/testing-library';
import { DashboardHero } from './DashboardHero';

describe('DashboardHero (F-719)', () => {
  afterEach(() => {
    cleanup();
  });

  it('renders the verbatim headline `Welcome back. Forge something.`', () => {
    const { getByRole } = render(() => <DashboardHero />);
    const heading = getByRole('heading', { level: 1 });
    // The headline is split across a `<br>` for layout; the accessible
    // text concatenates the two lines.
    expect(heading.textContent).toBe('Welcome back.Forge something.');
    // The brand word paints via an inline <em> so the ember override
    // hooks cleanly without splitting the text node.
    expect(heading.querySelector('em')?.textContent).toBe('Forge');
  });

  it('renders both CTA buttons with the verbatim labels', () => {
    const { getByRole } = render(() => <DashboardHero />);
    expect(
      getByRole('button', { name: 'Attach to session' }),
    ).toBeTruthy();
    expect(getByRole('button', { name: '+ New session' })).toBeTruthy();
  });

  it('fires `onAttach` when the Attach button is clicked', () => {
    const onAttach = vi.fn();
    const { getByRole } = render(() => <DashboardHero onAttach={onAttach} />);
    fireEvent.click(getByRole('button', { name: 'Attach to session' }));
    expect(onAttach).toHaveBeenCalledTimes(1);
  });

  it('fires `onNewSession` when the + New session button is clicked', () => {
    const onNewSession = vi.fn();
    const { getByRole } = render(() => (
      <DashboardHero onNewSession={onNewSession} />
    ));
    fireEvent.click(getByRole('button', { name: '+ New session' }));
    expect(onNewSession).toHaveBeenCalledTimes(1);
  });
});

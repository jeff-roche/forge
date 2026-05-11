import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor } from '@solidjs/testing-library';
import { DashboardHero } from './DashboardHero';
import { setInvokeForTesting } from '../../lib/tauri';

describe('DashboardHero (F-719 / F-728)', () => {
  beforeEach(() => {
    // Default hermetic stub — empty sessions and providers so the hero
    // renders its empty-state sentence. Individual tests below override
    // with fixture-shaped responses.
    setInvokeForTesting((async (cmd: string) => {
      if (cmd === 'session_list') return [];
      if (cmd === 'dashboard_list_providers') return [];
      if (cmd === 'has_credential') return false;
      return undefined;
    }) as never);
  });

  afterEach(() => {
    setInvokeForTesting(null);
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

  // F-728: the status sentence collapses to the verbatim empty-state copy
  // when the workspace has no sessions and no configured providers.
  it('renders the empty-state sentence when no sessions and no providers exist', async () => {
    const { container } = render(() => <DashboardHero />);
    await waitFor(() => {
      expect(container.querySelector('.dashboard-hero__status')?.textContent).toBe(
        'Ready to forge. Add a provider and start a session.',
      );
    });
  });

  // F-728: the status sentence reflects live workspace state — verbatim
  // example from the F-728 DoD.
  it('renders the synthesized sentence for the active state', async () => {
    setInvokeForTesting((async (cmd: string, args?: Record<string, unknown>) => {
      if (cmd === 'session_list') {
        return [
          {
            id: 's1',
            subject: 'refactor',
            state: 'active',
            persistence: 'persist',
            createdAt: '2026-04-18T00:00:00Z',
            lastEventAt: '2026-04-18T00:00:00Z',
          },
          {
            id: 's2',
            subject: 'doc-site',
            state: 'active',
            persistence: 'persist',
            createdAt: '2026-04-18T00:00:00Z',
            lastEventAt: '2026-04-18T00:00:00Z',
          },
          {
            id: 's3',
            subject: 'awaiting',
            state: 'awaiting approval',
            persistence: 'persist',
            createdAt: '2026-04-18T00:00:00Z',
            lastEventAt: '2026-04-18T00:00:00Z',
          },
        ];
      }
      if (cmd === 'dashboard_list_providers') {
        return [
          {
            id: 'anthropic',
            display_name: 'Anthropic',
            credential_required: true,
            has_credential: true,
            model_available: true,
          },
          {
            id: 'ollama',
            display_name: 'Ollama',
            credential_required: false,
            has_credential: false,
            model_available: true,
          },
          {
            id: 'openai',
            display_name: 'OpenAI',
            credential_required: true,
            has_credential: false,
            model_available: true,
          },
        ];
      }
      if (cmd === 'has_credential') {
        const id = args?.['providerId'] as string | undefined;
        return id === 'anthropic';
      }
      return undefined;
    }) as never);

    const { container } = render(() => <DashboardHero />);
    await waitFor(() => {
      expect(container.querySelector('.dashboard-hero__status')?.textContent).toBe(
        'Two sessions active. One agent paused awaiting approval. Anthropic and local Ollama connected — OpenAI awaiting credentials.',
      );
    });
  });
});

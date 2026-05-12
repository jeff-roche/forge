import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, waitFor } from '@solidjs/testing-library';
import { MemoryRouter, Route } from '@solidjs/router';
import { ProvidersPage } from './ProvidersPage';
import { setInvokeForTesting } from '../../lib/tauri';
import type { ProviderEntry } from '../../ipc/dashboard';

const SAMPLE_ROWS: ProviderEntry[] = [
  {
    id: 'anthropic',
    display_name: 'Anthropic',
    credential_required: true,
    has_credential: true,
    model_available: true,
    model: 'claude-sonnet-4',
    enabled: true,
  },
  {
    id: 'openai',
    display_name: 'OpenAI',
    credential_required: true,
    has_credential: false,
    model_available: false,
    enabled: true,
  },
  {
    id: 'custom_openai:ollama',
    display_name: 'custom_openai — ollama',
    credential_required: true,
    has_credential: false,
    model_available: true,
    model: 'llama3.2',
    endpoint: 'http://127.0.0.1:11434/v1',
    enabled: false,
  },
  {
    id: 'custom_openai:vllm',
    display_name: 'custom_openai — vllm',
    credential_required: true,
    has_credential: true,
    model_available: true,
    model: 'qwen2',
    endpoint: 'http://127.0.0.1:8000',
    enabled: true,
  },
];

const BUILTIN_IDS = ['anthropic', 'openai'];
const CUSTOM_IDS = ['custom_openai:ollama', 'custom_openai:vllm'];

function renderPage() {
  return render(() => (
    <MemoryRouter>
      <Route path="/" component={ProvidersPage} />
    </MemoryRouter>
  ));
}

describe('<ProvidersPage> (F-729)', () => {
  afterEach(() => {
    setInvokeForTesting(null);
    cleanup();
  });

  describe('header', () => {
    beforeEach(() => {
      setInvokeForTesting(
        (async (cmd: string) => {
          if (cmd === 'dashboard_list_providers') return [];
          return undefined;
        }) as never,
      );
    });

    it('renders the page title "Providers"', async () => {
      const { getByRole } = renderPage();
      const heading = getByRole('heading', { level: 1 });
      expect(heading.textContent).toBe('Providers');
    });

    it('renders the Add provider CTA', () => {
      const { getByTestId } = renderPage();
      const cta = getByTestId('add-provider-button');
      expect(cta.textContent).toContain('+ Add provider');
      // F-730: the button is enabled and opens the AddProviderForm.
      expect(cta).toHaveProperty('disabled', false);
    });
  });

  describe('loading state', () => {
    it('renders the skeleton while the IPC is pending', async () => {
      let release: (() => void) | null = null;
      const pending = new Promise<void>((r) => {
        release = r;
      });
      setInvokeForTesting(
        (async (cmd: string) => {
          if (cmd === 'dashboard_list_providers') {
            await pending;
            return [];
          }
          return undefined;
        }) as never,
      );

      const { queryByTestId } = renderPage();
      expect(queryByTestId('providers-page-loading')).not.toBeNull();
      release!();
      await waitFor(() => {
        expect(queryByTestId('providers-page-loading')).toBeNull();
      });
    });
  });

  describe('empty state', () => {
    beforeEach(() => {
      setInvokeForTesting(
        (async (cmd: string) => {
          if (cmd === 'dashboard_list_providers') return [];
          return undefined;
        }) as never,
      );
    });

    it('renders the empty-state copy when no providers are configured', async () => {
      const { findByTestId } = renderPage();
      const empty = await findByTestId('providers-page-empty');
      expect(empty.textContent).toBe('No providers configured. Add one to get started.');
    });
  });

  describe('error state', () => {
    beforeEach(() => {
      setInvokeForTesting(
        (async (cmd: string) => {
          if (cmd === 'dashboard_list_providers') {
            throw new Error('dashboard_list_providers: settings unreadable');
          }
          return undefined;
        }) as never,
      );
    });

    it('renders the verbatim error and a RETRY action', async () => {
      const { findByTestId, getByTestId } = renderPage();
      const block = await findByTestId('providers-page-error');
      expect(block.textContent).toContain('dashboard_list_providers: settings unreadable');
      expect(getByTestId('providers-page-retry')).not.toBeNull();
    });

    it('retries the IPC when RETRY is clicked', async () => {
      const fn = vi.fn(async (cmd: string) => {
        if (cmd === 'dashboard_list_providers') {
          throw new Error('boom');
        }
        return undefined;
      });
      setInvokeForTesting(fn as never);
      const { findByTestId } = renderPage();
      const retry = await findByTestId('providers-page-retry');
      const before = fn.mock.calls.filter((c) => c[0] === 'dashboard_list_providers').length;
      retry.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      await waitFor(() => {
        const after = fn.mock.calls.filter((c) => c[0] === 'dashboard_list_providers').length;
        expect(after).toBeGreaterThan(before);
      });
    });
  });

  describe('ready state', () => {
    beforeEach(() => {
      setInvokeForTesting(
        (async (cmd: string) => {
          if (cmd === 'dashboard_list_providers') return SAMPLE_ROWS;
          return undefined;
        }) as never,
      );
    });

    it('renders one row per provider returned by dashboard_list_providers', async () => {
      const { findAllByTestId } = renderPage();
      const rows = await findAllByTestId('provider-row');
      expect(rows).toHaveLength(SAMPLE_ROWS.length);
    });

    it('renders brand dot + identity + test/edit/remove buttons per row', async () => {
      const { findAllByTestId, getAllByTestId, getByTestId } = renderPage();
      // Wait for the rows to land first.
      const rows = await findAllByTestId('provider-row');
      expect(rows).toHaveLength(SAMPLE_ROWS.length);

      expect(getAllByTestId('provider-brand-dot')).toHaveLength(SAMPLE_ROWS.length);
      expect(getAllByTestId('provider-identity')).toHaveLength(SAMPLE_ROWS.length);
      // F-731 swapped the test-connection placeholder with the real button —
      // assert one button per row keyed by the provider id.
      for (const row of SAMPLE_ROWS) {
        expect(getByTestId(`test-connection-button-${row.id}`)).toBeInTheDocument();
      }
      // F-733 swapped the placeholder for the real toggle — one switch per
      // row, with aria-checked reflecting the row's `enabled` flag.
      for (const row of SAMPLE_ROWS) {
        const toggle = getByTestId(`provider-enabled-toggle-${row.id}`);
        expect(toggle).toBeInTheDocument();
        const input = toggle.querySelector('input[role="switch"]');
        expect(input?.getAttribute('aria-checked')).toBe(String(row.enabled !== false));
      }
      // Disabled rows surface a data attribute for the dim-out treatment.
      const rowsRendered = getAllByTestId('provider-row');
      const disabledOnes = rowsRendered.filter((el) => el.getAttribute('data-enabled') === 'false');
      expect(disabledOnes).toHaveLength(SAMPLE_ROWS.filter((r) => r.enabled === false).length);
      // Edit pencil renders on every row. Enabled for custom_openai:* rows
      // (where endpoint/model are editable today); disabled for built-ins
      // until per-vendor edit dialogs land.
      for (const id of CUSTOM_IDS) {
        const trigger = getByTestId(
          `edit-provider-trigger-${id}`,
        ) as HTMLButtonElement;
        expect(trigger).toBeInTheDocument();
        expect(trigger.disabled).toBe(false);
      }
      for (const id of BUILTIN_IDS) {
        const trigger = getByTestId(
          `edit-provider-trigger-${id}`,
        ) as HTMLButtonElement;
        expect(trigger).toBeInTheDocument();
        expect(trigger.disabled).toBe(true);
        expect(trigger.getAttribute('title') ?? '').toContain('No editable fields');
      }
      // F-732: Remove button renders for every row.
      for (const row of SAMPLE_ROWS) {
        expect(getByTestId(`remove-provider-trigger-${row.id}`)).toBeInTheDocument();
      }
    });
  });

  // ---------------------------------------------------------------------------
  // Active provider selector — per row "Set active" / "ACTIVE" badge
  // ---------------------------------------------------------------------------

  describe('active provider', () => {
    it('marks the active provider with the ACTIVE badge and `Set active` on the others', async () => {
      setInvokeForTesting(
        (async (cmd: string) => {
          if (cmd === 'dashboard_list_providers') return SAMPLE_ROWS;
          if (cmd === 'get_active_provider') return 'anthropic';
          return undefined;
        }) as never,
      );
      const { findByTestId, queryByTestId } = renderPage();
      // Active row gets the badge; no Set-active button.
      await findByTestId('active-badge-anthropic');
      expect(queryByTestId('set-active-anthropic')).toBeNull();
      // Non-active rows get the Set-active button; no badge.
      expect(await findByTestId('set-active-openai')).toBeInTheDocument();
      expect(await findByTestId('set-active-custom_openai:ollama')).toBeInTheDocument();
      expect(queryByTestId('active-badge-openai')).toBeNull();
    });

    it('clicking Set active fires set_active_provider with the row id', async () => {
      const calls: string[] = [];
      setInvokeForTesting(
        (async (cmd: string, args?: Record<string, unknown>) => {
          calls.push(cmd);
          if (cmd === 'dashboard_list_providers') return SAMPLE_ROWS;
          if (cmd === 'get_active_provider') return 'anthropic';
          if (cmd === 'set_active_provider') {
            // Echo the id so the test can assert the wire shape.
            calls.push(`payload:${String((args as { providerId?: string })?.providerId)}`);
            return undefined;
          }
          return undefined;
        }) as never,
      );
      const { findByTestId } = renderPage();
      const btn = await findByTestId('set-active-openai');
      btn.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      await waitFor(() => {
        expect(calls.find((c) => c === 'set_active_provider')).toBeDefined();
      });
      expect(calls).toContain('payload:openai');
    });
  });
});

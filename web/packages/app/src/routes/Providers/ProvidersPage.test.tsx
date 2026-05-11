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
    id: 'ollama',
    display_name: 'Ollama',
    credential_required: false,
    has_credential: false,
    model_available: true,
    model: 'llama3:8b',
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

const BUILTIN_IDS = ['anthropic', 'openai', 'ollama'];
const CUSTOM_IDS = ['custom_openai:vllm'];

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
      // F-732: Edit button only renders for custom_openai:* rows; built-ins
      // get an empty non-applicable slot so the row track stays aligned.
      for (const id of CUSTOM_IDS) {
        expect(getByTestId(`edit-provider-trigger-${id}`)).toBeInTheDocument();
      }
      for (const id of BUILTIN_IDS) {
        expect(getByTestId(`edit-not-applicable-${id}`)).toBeInTheDocument();
      }
      // F-732: Remove button renders for every row.
      for (const row of SAMPLE_ROWS) {
        expect(getByTestId(`remove-provider-trigger-${row.id}`)).toBeInTheDocument();
      }
    });
  });
});

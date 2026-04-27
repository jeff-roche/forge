import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { setInvokeForTesting } from '../lib/tauri';
import {
  DEFAULT_SETTINGS,
  applyLocalUpdate,
  resetSettingsStore,
  seedSettings,
  setSetting,
  settings,
} from './settings';

describe('settings store (F-151)', () => {
  beforeEach(() => {
    resetSettingsStore();
  });

  describe('defaults', () => {
    it('initializes with the documented defaults', () => {
      expect(settings.notifications.bg_agents).toBe('toast');
      expect(settings.windows.session_mode).toBe('single');
    });

    it('DEFAULT_SETTINGS matches the live store after reset', () => {
      expect(settings.notifications.bg_agents).toBe(
        DEFAULT_SETTINGS.notifications.bg_agents,
      );
      expect(settings.windows.session_mode).toBe(
        DEFAULT_SETTINGS.windows.session_mode,
      );
    });
  });

  describe('seedSettings', () => {
    it('replaces the store with the seeded value', () => {
      seedSettings({
        notifications: { bg_agents: 'both' },
        windows: { session_mode: 'split' },
        providers: { custom_openai: {} },
        catalog: { enabled: {} },
        dashboard: { container_banner_dismissed: false },
        memory: { enabled: {} },
      });
      expect(settings.notifications.bg_agents).toBe('both');
      expect(settings.windows.session_mode).toBe('split');
    });

    it('is idempotent: re-seeding overwrites the previous snapshot', () => {
      seedSettings({
        notifications: { bg_agents: 'silent' },
        windows: { session_mode: 'split' },
        providers: { custom_openai: {} },
        catalog: { enabled: {} },
        dashboard: { container_banner_dismissed: false },
        memory: { enabled: {} },
      });
      seedSettings({
        notifications: { bg_agents: 'os' },
        windows: { session_mode: 'single' },
        providers: { custom_openai: {} },
        catalog: { enabled: {} },
        dashboard: { container_banner_dismissed: false },
        memory: { enabled: {} },
      });
      expect(settings.notifications.bg_agents).toBe('os');
      expect(settings.windows.session_mode).toBe('single');
    });
  });

  describe('applyLocalUpdate', () => {
    it('updates a nested scalar by dotted key', () => {
      applyLocalUpdate('notifications.bg_agents', 'os');
      expect(settings.notifications.bg_agents).toBe('os');
      // Untouched sibling stays on its default.
      expect(settings.windows.session_mode).toBe('single');
    });

    it('creates a nested table when the section does not exist yet', () => {
      // Simulate a schema extension: set a key under a brand-new section.
      applyLocalUpdate('future_section.feature_flag', true);
      // The typed `AppSettings` shape doesn't surface this, but the store's
      // runtime object does — this guards against future-schema breakage.
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      expect((settings as any).future_section.feature_flag).toBe(true);
    });

    it('silently swallows an empty-segment key', () => {
      applyLocalUpdate('', 'whatever');
      applyLocalUpdate('notifications..bg_agents', 'whatever');
      // Store is unchanged from defaults.
      expect(settings.notifications.bg_agents).toBe('toast');
    });
  });

  describe('setSetting', () => {
    let invokeMock: ReturnType<typeof vi.fn>;

    beforeEach(() => {
      invokeMock = vi.fn();
      setInvokeForTesting(invokeMock as never);
    });

    afterEach(() => {
      setInvokeForTesting(null);
    });

    it('invokes set_setting with the full payload and mirrors into the store', async () => {
      invokeMock.mockResolvedValue(undefined);
      await setSetting('windows.session_mode', 'split', 'workspace', '/ws');
      expect(invokeMock).toHaveBeenCalledWith('set_setting', {
        key: 'windows.session_mode',
        value: 'split',
        level: 'workspace',
        workspaceRoot: '/ws',
      });
      expect(settings.windows.session_mode).toBe('split');
    });

    it('leaves the store untouched when the IPC call rejects', async () => {
      invokeMock.mockRejectedValue('invalid setting value');
      await expect(
        setSetting('notifications.bg_agents', 42, 'workspace', '/ws'),
      ).rejects.toBeDefined();
      // bg_agents is still the default — the optimistic mirror runs only on
      // a resolved invoke.
      expect(settings.notifications.bg_agents).toBe('toast');
    });

    it('preserves sibling fields across successive writes', async () => {
      invokeMock.mockResolvedValue(undefined);
      await setSetting('notifications.bg_agents', 'both', 'user', '/ws');
      await setSetting('windows.session_mode', 'split', 'user', '/ws');
      expect(settings.notifications.bg_agents).toBe('both');
      expect(settings.windows.session_mode).toBe('split');
    });
  });
});

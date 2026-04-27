// F-151: user + workspace settings store.
//
// Mirrors the shape of `AppSettings` from `@forge/ipc`. Seeded once per session
// from `get_settings` (the shell merges workspace over user at TOML-tree
// granularity); subsequent writes go through `setSetting`, which persists to
// disk AND updates the in-memory store so Solid components re-render without
// an explicit reload.
//
// The store holds the *effective* (merged) settings — the tier a given field
// came from is not surfaced here. Callers that need to display provenance
// should call `getSettings` directly and reason about both tiers client-side.

import { createStore, produce, reconcile } from 'solid-js/store';
import type { AppSettings, NotificationMode, SessionMode } from '@forge/ipc';
import { setSetting as ipcSetSetting, type SettingsLevel } from '../ipc/session';

/**
 * Default settings shape. Matches the Rust-side `AppSettings::default()` — keep
 * the two in sync so the "before seed" view and the "no settings file on disk"
 * view are indistinguishable.
 */
export const DEFAULT_SETTINGS: AppSettings = {
  notifications: { bg_agents: 'toast' satisfies NotificationMode },
  windows: { session_mode: 'single' satisfies SessionMode },
  providers: { custom_openai: {} },
  catalog: { enabled: {} },
  dashboard: { container_banner_dismissed: false },
  memory: { enabled: {} },
};

/** Deep clone the defaults so in-place store writes never mutate the
 * exported `DEFAULT_SETTINGS` constant. A shallow spread keeps nested
 * sections aliased to the constant, which `produce`'s mutation path would
 * then clobber silently — a footgun that surfaces as leaky state across
 * consecutive test cases. */
function freshDefaults(): AppSettings {
  return JSON.parse(JSON.stringify(DEFAULT_SETTINGS)) as AppSettings;
}

const [settingsStore, setSettingsStoreInternal] =
  createStore<AppSettings>(freshDefaults());

export const settings = settingsStore;

/**
 * Replace the store contents with `seeded`. Called once on session init from
 * `SessionWindow` after `getSettings(workspaceRoot)` returns. Idempotent —
 * a follow-up re-seed overwrites; no merge with the previous contents.
 */
export function seedSettings(seeded: AppSettings): void {
  setSettingsStoreInternal(reconcile(seeded));
}

/** Test helper: clear the store back to defaults. */
export function resetSettingsStore(): void {
  setSettingsStoreInternal(reconcile(freshDefaults()));
}

/**
 * Persist `(key, value)` to the requested tier and mirror the change into the
 * local store so Solid subscribers re-render without awaiting a round-trip
 * re-fetch.
 *
 * The store update is optimistic in the sense that it happens after the
 * backend confirms the write (the `await` resolves); if the IPC call rejects
 * (e.g. invalid value, type mismatch) the store is left untouched. Callers
 * should still catch + surface the error.
 *
 * `key` uses the dotted path shape the backend expects
 * (`"notifications.bg_agents"`, `"windows.session_mode"`). Deep paths longer
 * than two levels are supported by the backend; the local mirror walks the
 * same dotted path so new sections added later work without touching this
 * file.
 */
export async function setSetting(
  key: string,
  value: unknown,
  level: SettingsLevel,
  workspaceRoot: string,
): Promise<void> {
  await ipcSetSetting(key, value, level, workspaceRoot);
  applyLocalUpdate(key, value);
}

/**
 * Apply `(dotted_key, value)` to the in-memory store. Exposed separately from
 * `setSetting` so tests (and any future optimistic-update path) can exercise
 * the local-mirror logic without an IPC stub.
 */
export function applyLocalUpdate(key: string, value: unknown): void {
  const segments = key.split('.');
  if (segments.length === 0 || segments.some((s) => s.length === 0)) {
    // Matches the backend's empty-segment rejection; silently swallow so the
    // store never lands in an inconsistent state if a caller fat-fingers a
    // key. The backend will already have errored (and thrown above in
    // `setSetting`), so reaching here means the caller bypassed IPC.
    return;
  }
  setSettingsStoreInternal(
    produce((s) => {
      // Walk / create the nested tables. Each intermediate must be an object;
      // if a scalar is sitting in the way (shouldn't happen against the typed
      // `AppSettings` shape, but guards against any future schema drift), the
      // walk bails without mutating.
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      let cursor: any = s;
      for (let i = 0; i < segments.length - 1; i++) {
        const seg = segments[i]!;
        if (cursor[seg] === undefined || cursor[seg] === null) {
          cursor[seg] = {};
        }
        if (typeof cursor[seg] !== 'object') return;
        cursor = cursor[seg];
      }
      cursor[segments[segments.length - 1]!] = value;
    }),
  );
}

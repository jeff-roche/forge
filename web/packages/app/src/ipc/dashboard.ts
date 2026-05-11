// F-365: typed wrappers for Dashboard-level Tauri commands that previously
// reached through raw `invoke()`. Centralising them here makes the command
// surface discoverable and prevents arg-key drift when the Rust signatures
// change.

import { invoke } from '../lib/tauri';
import type { GitBranchOutput } from '@forge/ipc';

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

export type SessionWireState = 'active' | 'archived' | 'stopped';

export interface SessionSummary {
  id: string;
  subject: string;
  state: SessionWireState;
  persistence: 'persist' | 'ephemeral';
  createdAt: string;
  lastEventAt: string;
  /** Optional; provider chip is shown when present. */
  provider?: string;
}

/** Fetch the list of all sessions known to the shell. */
export async function sessionList(): Promise<SessionSummary[]> {
  return invoke<SessionSummary[]>('session_list');
}

/**
 * F-727: Fetch the subset of sessions the hero's `Attach to session` picker
 * can re-open — wire-state `stopped` (the daemon process is gone). The
 * shell filters server-side; the webview consumes the result directly with
 * no additional state pass.
 */
export async function listAttachableSessions(): Promise<SessionSummary[]> {
  return invoke<SessionSummary[]>('list_sessions');
}

/**
 * Reopen the Session window for `id`. The shell brings an existing window to
 * the front or spawns a new one when the window was previously closed.
 */
export async function openSession(id: string): Promise<void> {
  await invoke('open_session', { id });
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

export interface ProviderStatus {
  reachable: boolean;
  base_url: string;
  models: string[];
  last_checked: string;
  error_kind?: string;
}

/** Probe the configured AI provider and return its current status. */
export async function providerStatus(): Promise<ProviderStatus> {
  return invoke<ProviderStatus>('provider_status');
}

// ---------------------------------------------------------------------------
// Provider selection (F-586)
// ---------------------------------------------------------------------------

/**
 * One row of `dashboard_list_providers`. Stable id (slug), display name,
 * and the dashboard's three-state model:
 *   - `credential_required && !has_credential` → warning glyph
 *   - `model_available` false                  → secondary "no model" hint
 *   - `model` populated                        → secondary line shows it
 *
 * Mirrors the Rust `ProviderEntry` shape one-for-one.
 */
export interface ProviderEntry {
  id: string;
  display_name: string;
  credential_required: boolean;
  has_credential: boolean;
  model_available: boolean;
  model?: string;
  /** `base_url` of the underlying custom_openai section (custom rows only). */
  endpoint?: string;
  /** Reserved future override (always undefined today). */
  api_version?: string;
  /**
   * F-733: per-provider enable flag. The Providers page toggle is the only
   * writer; absent settings default to `true` so historical configs (pre
   * F-730) keep their built-ins live. The active-provider selector and the
   * new-session picker filter rows where `enabled === false`. Optional on
   * the TS surface so legacy IPC payloads — and test fixtures that pre-date
   * F-733 — still type-check; readers MUST treat `undefined` as enabled.
   */
  enabled?: boolean;
}

/**
 * F-733: read-site helper. The `enabled` flag is optional on the wire so
 * pre-F-733 payloads (where the field is absent) treat as enabled. Use
 * this anywhere a UI must filter disabled rows.
 */
export function isProviderEnabled(entry: ProviderEntry): boolean {
  return entry.enabled !== false;
}

/**
 * List the four built-in providers plus any user-configured custom_openai
 * entries. Wraps the `dashboard_list_providers` Tauri command — the
 * `dashboard_` prefix disambiguates from F-591's planned roster catalog
 * `list_providers` command (Tauri rejects duplicate command names).
 */
export async function listProviders(): Promise<ProviderEntry[]> {
  return invoke<ProviderEntry[]>('dashboard_list_providers');
}

/** Read the persisted `[providers.active]` setting (user-tier, global). */
export async function getActiveProvider(): Promise<string | null> {
  return invoke<string | null>('get_active_provider');
}

/**
 * Persist the active provider id and emit `provider:changed` app-wide so
 * any open session window's bridge can swap its inner Provider for the
 * next turn.
 */
export async function setActiveProvider(providerId: string): Promise<void> {
  await invoke('set_active_provider', { providerId });
}

// ---------------------------------------------------------------------------
// F-741: status-bar git branch feed
// ---------------------------------------------------------------------------

/**
 * Resolve the active branch name for `workspaceRoot` by shelling out to
 * `git rev-parse --abbrev-ref HEAD` inside the directory. Returns the
 * branch name on success; `null` when the working tree has a detached
 * HEAD. IPC rejections propagate to the caller, which paints `unknown`
 * per the status-bar contract.
 */
export async function gitBranch(workspaceRoot: string): Promise<string | null> {
  const out = await invoke<GitBranchOutput>('git_branch', {
    input: { workspace_root: workspaceRoot },
  });
  return out.branch ?? null;
}

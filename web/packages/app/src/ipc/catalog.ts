// F-592: typed wrappers around F-591's roster discovery commands.
//
// `list_skills` / `list_mcp_servers` / `list_agents` / `list_providers` each
// return `Vec<ScopedRosterEntry>` filtered by the supplied scope. The catalog
// UI only uses `RosterScope::SessionWide` today; agent- or provider-scoped
// filters are reserved for downstream tasks (per-agent skill bindings).

import type {
  McpServerInfo,
  RosterScope,
  ScopedRosterEntry,
  SessionId,
} from '@forge/ipc';
import { invoke } from '../lib/tauri';

/** Scope shorthand — the catalog always queries the session-wide universe. */
export const SESSION_WIDE_SCOPE: RosterScope = { type: 'SessionWide' };

/** Wraps `list_skills` (F-591). */
export async function listSkills(
  workspaceRoot: string,
  scope: RosterScope = SESSION_WIDE_SCOPE,
): Promise<ScopedRosterEntry[]> {
  return invoke<ScopedRosterEntry[]>('list_skills', { workspaceRoot, scope });
}

/** Wraps `list_mcp_servers` (F-591). Distinct from F-132's
 * `session_list_mcp_servers` which queries a running session's daemon. */
export async function listMcpServers(
  workspaceRoot: string,
  scope: RosterScope = SESSION_WIDE_SCOPE,
): Promise<ScopedRosterEntry[]> {
  return invoke<ScopedRosterEntry[]>('list_mcp_servers', { workspaceRoot, scope });
}

/** Wraps `list_agents` (F-591). */
export async function listAgents(
  workspaceRoot: string,
  scope: RosterScope = SESSION_WIDE_SCOPE,
): Promise<ScopedRosterEntry[]> {
  return invoke<ScopedRosterEntry[]>('list_agents', { workspaceRoot, scope });
}

/** Wraps `list_providers` (F-591). The Dashboard's provider picker uses the
 * differently-named `dashboard_list_providers` instead — see ipc/dashboard.ts. */
export async function listProvidersRoster(
  workspaceRoot: string,
  scope: RosterScope = SESSION_WIDE_SCOPE,
): Promise<ScopedRosterEntry[]> {
  return invoke<ScopedRosterEntry[]>('list_providers', { workspaceRoot, scope });
}

/** Wraps F-132's `session_list_mcp_servers` — returns runtime lifecycle
 * state (Starting / Healthy / Degraded / Failed / Disabled) for every MCP
 * server the named session has loaded. Distinct from `listMcpServers`
 * above, which reads on-disk `.mcp.json` without contacting any session. */
export async function sessionListMcpServers(
  sessionId: SessionId,
): Promise<McpServerInfo[]> {
  return invoke<McpServerInfo[]>('session_list_mcp_servers', { sessionId });
}

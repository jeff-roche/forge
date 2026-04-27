// F-602: typed wrappers for the agent memory Tauri commands.
//
// Backend: `crates/forge-shell/src/memory_ipc.rs`. Authority on the file
// path / size / version metadata stays in the Rust layer; this wrapper
// only marshals the four commands.
//
// Wire shape uses snake_case keys to match other dashboard DTOs
// (`PersistentApprovalEntry`, `ScopedRosterEntry`, etc.).

import { invoke } from '../lib/tauri';

/** One row per loaded agent. Mirrors `AgentMemoryEntry` on the Rust side. */
export interface AgentMemoryEntry {
  agent_id: string;
  /** Absolute path the memory file lives at (may not yet exist). */
  path: string;
  /** Bytes-on-disk; null when the file is absent. */
  size_bytes: number | null;
  /** RFC 3339 timestamp; null when the file is absent. */
  updated_at: string | null;
  /** Monotonic version from the F-601 frontmatter; null when absent. */
  version: number | null;
  /** Agent def's frontmatter `memory_enabled` flag. */
  def_enabled: boolean;
  /** User's `[memory.enabled.<agent>]` override, or null when unset. */
  settings_override: boolean | null;
}

export interface AgentMemorySaved {
  version: number;
  updated_at: string;
}

/**
 * Enumerate every loaded agent and return its memory file metadata. The
 * shell merges workspace + user settings to surface the
 * `[memory.enabled.<agent>]` override on each row.
 */
export async function listAgentMemory(workspaceRoot: string): Promise<AgentMemoryEntry[]> {
  return invoke<AgentMemoryEntry[]>('list_agent_memory', { workspaceRoot });
}

/**
 * Read the markdown body of one agent's memory file. Returns the empty
 * string when the file is absent (so the editor can open in a blank state
 * rather than show an error).
 */
export async function readAgentMemory(agentId: string): Promise<string> {
  if (!agentId) throw new Error('readAgentMemory: agentId must not be empty');
  return invoke<string>('read_agent_memory', { agentId });
}

/**
 * Write `body` to the agent's memory file (replace mode). Increments the
 * F-601 version counter and bumps `updated_at`. Returns the new metadata
 * so the caller can refresh its row without a follow-up list call.
 */
export async function saveAgentMemory(
  agentId: string,
  body: string,
): Promise<AgentMemorySaved> {
  if (!agentId) throw new Error('saveAgentMemory: agentId must not be empty');
  return invoke<AgentMemorySaved>('save_agent_memory', { agentId, body });
}

/**
 * Wipe the agent's memory file to an empty body. Idempotent — calling on
 * an absent file creates a fresh file with empty body.
 */
export async function clearAgentMemory(agentId: string): Promise<void> {
  if (!agentId) throw new Error('clearAgentMemory: agentId must not be empty');
  await invoke<void>('clear_agent_memory', { agentId });
}

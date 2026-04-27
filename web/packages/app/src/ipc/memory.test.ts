import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { setInvokeForTesting } from '../lib/tauri';
import {
  clearAgentMemory,
  listAgentMemory,
  readAgentMemory,
  saveAgentMemory,
} from './memory';

describe('memory ipc wrappers (F-602)', () => {
  beforeEach(() => {
    setInvokeForTesting(null);
  });

  afterEach(() => {
    setInvokeForTesting(null);
  });

  it('listAgentMemory passes workspaceRoot through', async () => {
    const mock = vi.fn(async () => []);
    setInvokeForTesting(mock as never);
    await listAgentMemory('/work/root');
    expect(mock).toHaveBeenCalledWith('list_agent_memory', { workspaceRoot: '/work/root' });
  });

  it('readAgentMemory rejects empty agentId locally', async () => {
    const mock = vi.fn();
    setInvokeForTesting(mock as never);
    await expect(readAgentMemory('')).rejects.toThrow(/agentId/);
    expect(mock).not.toHaveBeenCalled();
  });

  it('readAgentMemory passes agentId through', async () => {
    const mock = vi.fn(async () => 'remember the milk');
    setInvokeForTesting(mock as never);
    const body = await readAgentMemory('scribe');
    expect(mock).toHaveBeenCalledWith('read_agent_memory', { agentId: 'scribe' });
    expect(body).toBe('remember the milk');
  });

  it('saveAgentMemory passes agentId + body and surfaces version metadata', async () => {
    const mock = vi.fn(async () => ({ version: 7, updated_at: '2026-04-26T12:00:00Z' }));
    setInvokeForTesting(mock as never);
    const result = await saveAgentMemory('scribe', '# Notes');
    expect(mock).toHaveBeenCalledWith('save_agent_memory', {
      agentId: 'scribe',
      body: '# Notes',
    });
    expect(result.version).toBe(7);
    expect(result.updated_at).toBe('2026-04-26T12:00:00Z');
  });

  it('saveAgentMemory rejects empty agentId locally', async () => {
    const mock = vi.fn();
    setInvokeForTesting(mock as never);
    await expect(saveAgentMemory('', 'body')).rejects.toThrow(/agentId/);
    expect(mock).not.toHaveBeenCalled();
  });

  it('clearAgentMemory passes agentId through', async () => {
    const mock = vi.fn(async () => undefined);
    setInvokeForTesting(mock as never);
    await clearAgentMemory('scribe');
    expect(mock).toHaveBeenCalledWith('clear_agent_memory', { agentId: 'scribe' });
  });

  it('clearAgentMemory rejects empty agentId locally', async () => {
    const mock = vi.fn();
    setInvokeForTesting(mock as never);
    await expect(clearAgentMemory('')).rejects.toThrow(/agentId/);
    expect(mock).not.toHaveBeenCalled();
  });
});

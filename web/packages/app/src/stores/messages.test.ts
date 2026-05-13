import { beforeEach, describe, expect, it } from 'vitest';
import {
  pushEvent,
  setAwaitingResponse,
  getMessagesState,
  resetMessagesStore,
  liveVariantCount,
  activeVariantPosition,
  neighbourVariantId,
} from './messages';
import type { SessionId } from '@forge/ipc';

const SID = 'session-test' as SessionId;

describe('messages store', () => {
  beforeEach(() => {
    resetMessagesStore();
  });

  describe('UserMessage events', () => {
    it('appends a user turn', () => {
      pushEvent(SID, { kind: 'UserMessage', text: 'hello', message_id: 'msg-1' });
      const state = getMessagesState(SID);
      expect(state.turns).toHaveLength(1);
      expect(state.turns[0]).toMatchObject({ type: 'user', text: 'hello', message_id: 'msg-1' });
    });
  });

  describe('AssistantMessage events (complete)', () => {
    it('appends a non-streaming assistant turn', () => {
      pushEvent(SID, { kind: 'AssistantMessage', text: 'hi there', message_id: 'msg-2' });
      const state = getMessagesState(SID);
      expect(state.turns).toHaveLength(1);
      expect(state.turns[0]).toMatchObject({
        type: 'assistant',
        text: 'hi there',
        message_id: 'msg-2',
        isStreaming: false,
      });
    });

    it('clears streamingMessageId on AssistantMessage', () => {
      pushEvent(SID, { kind: 'AssistantDelta', delta: 'partial', message_id: 'msg-3' });
      pushEvent(SID, { kind: 'AssistantMessage', text: 'partial complete', message_id: 'msg-3' });
      const state = getMessagesState(SID);
      expect(state.streamingMessageId).toBeNull();
    });

    it('clears awaitingResponse on AssistantMessage', () => {
      setAwaitingResponse(SID, true);
      pushEvent(SID, { kind: 'AssistantMessage', text: 'response', message_id: 'msg-4' });
      expect(getMessagesState(SID).awaitingResponse).toBe(false);
    });
  });

  describe('AssistantDelta events (streaming)', () => {
    it('creates a streaming assistant turn on first delta', () => {
      pushEvent(SID, { kind: 'AssistantDelta', delta: 'Hello', message_id: 'msg-5' });
      const state = getMessagesState(SID);
      expect(state.turns).toHaveLength(1);
      expect(state.turns[0]).toMatchObject({
        type: 'assistant',
        isStreaming: true,
        message_id: 'msg-5',
      });
      expect(state.streamingMessageId).toBe('msg-5');
    });

    it('accumulates delta text across multiple deltas', () => {
      pushEvent(SID, { kind: 'AssistantDelta', delta: 'Hello', message_id: 'msg-6' });
      pushEvent(SID, { kind: 'AssistantDelta', delta: ' world', message_id: 'msg-6' });
      pushEvent(SID, { kind: 'AssistantDelta', delta: '!', message_id: 'msg-6' });
      const state = getMessagesState(SID);
      expect(state.turns[0]).toMatchObject({ text: 'Hello world!' });
    });

    it('clears awaitingResponse when first delta arrives', () => {
      setAwaitingResponse(SID, true);
      pushEvent(SID, { kind: 'AssistantDelta', delta: 'start', message_id: 'msg-7' });
      expect(getMessagesState(SID).awaitingResponse).toBe(false);
    });

    it('does not create duplicate turns for the same message_id', () => {
      pushEvent(SID, { kind: 'AssistantDelta', delta: 'chunk1', message_id: 'msg-8' });
      pushEvent(SID, { kind: 'AssistantDelta', delta: 'chunk2', message_id: 'msg-8' });
      expect(getMessagesState(SID).turns).toHaveLength(1);
    });
  });

  describe('ToolCallStarted / ToolCallCompleted events', () => {
    it('appends a tool placeholder with status in-progress on ToolCallStarted', () => {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: 'tc-1',
        tool_name: 'fs.read',
        args_json: '{"path":"/foo"}',
      });
      const state = getMessagesState(SID);
      expect(state.turns).toHaveLength(1);
      expect(state.turns[0]).toMatchObject({
        type: 'tool_placeholder',
        tool_call_id: 'tc-1',
        tool_name: 'fs.read',
        status: 'in-progress',
      });
    });

    it('preserves args_json on the turn', () => {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: 'tc-1',
        tool_name: 'fs.read',
        args_json: '{"path":"/foo"}',
      });
      const state = getMessagesState(SID);
      expect(state.turns[0]).toMatchObject({
        type: 'tool_placeholder',
        args_json: '{"path":"/foo"}',
      });
    });

    it('preserves batch_id on the turn when provided', () => {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: 'tc-1',
        tool_name: 'fs.read',
        args_json: '{}',
        batch_id: 'batch-42',
      });
      const state = getMessagesState(SID);
      expect(state.turns[0]).toMatchObject({
        type: 'tool_placeholder',
        batch_id: 'batch-42',
      });
    });

    it('records started_at as a number on ToolCallStarted', () => {
      const before = Date.now();
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: 'tc-1',
        tool_name: 'fs.read',
        args_json: '{}',
      });
      const after = Date.now();
      const state = getMessagesState(SID);
      const turn = state.turns[0] as { started_at: number };
      expect(typeof turn.started_at).toBe('number');
      expect(turn.started_at).toBeGreaterThanOrEqual(before);
      expect(turn.started_at).toBeLessThanOrEqual(after);
    });

    it('marks status completed and stores result_summary on ToolCallCompleted', () => {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: 'tc-2',
        tool_name: 'fs.write',
        args_json: '{}',
      });
      pushEvent(SID, {
        kind: 'ToolCallCompleted',
        tool_call_id: 'tc-2',
        result_summary: 'wrote 42 bytes',
      });
      const state = getMessagesState(SID);
      expect(state.turns[0]).toMatchObject({
        type: 'tool_placeholder',
        status: 'completed',
        result_summary: 'wrote 42 bytes',
      });
    });

    it('computes a non-negative duration_ms on ToolCallCompleted', () => {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: 'tc-2',
        tool_name: 'fs.write',
        args_json: '{}',
      });
      pushEvent(SID, {
        kind: 'ToolCallCompleted',
        tool_call_id: 'tc-2',
        result_summary: 'ok',
      });
      const state = getMessagesState(SID);
      const turn = state.turns[0] as { duration_ms?: number };
      expect(typeof turn.duration_ms).toBe('number');
      expect(turn.duration_ms).toBeGreaterThanOrEqual(0);
    });

    it('sets status errored and stores error on ToolCallFailed', () => {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: 'tc-3',
        tool_name: 'shell.exec',
        args_json: '{"cmd":"rm -rf /"}',
      });
      pushEvent(SID, {
        kind: 'ToolCallFailed',
        tool_call_id: 'tc-3',
        error: 'permission denied',
      });
      const state = getMessagesState(SID);
      expect(state.turns[0]).toMatchObject({
        type: 'tool_placeholder',
        status: 'errored',
        error: 'permission denied',
      });
    });

    it('computes a non-negative duration_ms on ToolCallFailed', () => {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: 'tc-3',
        tool_name: 'shell.exec',
        args_json: '{}',
      });
      pushEvent(SID, {
        kind: 'ToolCallFailed',
        tool_call_id: 'tc-3',
        error: 'timeout',
      });
      const state = getMessagesState(SID);
      const turn = state.turns[0] as { duration_ms?: number };
      expect(typeof turn.duration_ms).toBe('number');
      expect(turn.duration_ms).toBeGreaterThanOrEqual(0);
    });
  });

  describe('ToolCallApprovalRequested events', () => {
    it('transitions an existing placeholder to awaiting-approval', () => {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: 'tc-approval',
        tool_name: 'fs.edit',
        args_json: '{"path":"/src/foo.ts"}',
      });
      pushEvent(SID, {
        kind: 'ToolCallApprovalRequested',
        tool_call_id: 'tc-approval',
        tool_name: 'fs.edit',
        args_json: '{"path":"/src/foo.ts"}',
        preview: { description: 'Edit file /src/foo.ts: 2 hunks' },
      });
      const state = getMessagesState(SID);
      expect(state.turns).toHaveLength(1);
      expect(state.turns[0]).toMatchObject({
        type: 'tool_placeholder',
        tool_call_id: 'tc-approval',
        status: 'awaiting-approval',
        preview: { description: 'Edit file /src/foo.ts: 2 hunks' },
      });
    });

    it('creates a fresh placeholder when no prior ToolCallStarted', () => {
      pushEvent(SID, {
        kind: 'ToolCallApprovalRequested',
        tool_call_id: 'tc-fresh',
        tool_name: 'fs.write',
        args_json: '{"path":"/src/bar.ts"}',
        preview: { description: 'Write file /src/bar.ts' },
      });
      const state = getMessagesState(SID);
      expect(state.turns).toHaveLength(1);
      expect(state.turns[0]).toMatchObject({
        type: 'tool_placeholder',
        tool_call_id: 'tc-fresh',
        tool_name: 'fs.write',
        status: 'awaiting-approval',
        preview: { description: 'Write file /src/bar.ts' },
      });
    });

    it('attaches the preview description to the turn', () => {
      pushEvent(SID, {
        kind: 'ToolCallApprovalRequested',
        tool_call_id: 'tc-preview',
        tool_name: 'shell.exec',
        args_json: '{}',
        preview: { description: 'Run: /bin/sh -c echo hi (cwd /workspace)' },
      });
      const state = getMessagesState(SID);
      const turn = state.turns[0] as { preview?: { description: string } };
      expect(turn.preview?.description).toBe('Run: /bin/sh -c echo hi (cwd /workspace)');
    });

    it('does not duplicate turns if both ToolCallStarted and ApprovalRequested arrive', () => {
      pushEvent(SID, {
        kind: 'ToolCallStarted',
        tool_call_id: 'tc-dup',
        tool_name: 'fs.edit',
        args_json: '{}',
      });
      pushEvent(SID, {
        kind: 'ToolCallApprovalRequested',
        tool_call_id: 'tc-dup',
        tool_name: 'fs.edit',
        args_json: '{}',
        preview: { description: 'Edit' },
      });
      const state = getMessagesState(SID);
      expect(state.turns).toHaveLength(1);
    });
  });

  describe('Error events', () => {
    it('appends an error turn', () => {
      pushEvent(SID, { kind: 'Error', message: 'ECONNREFUSED 127.0.0.1:11434' });
      const state = getMessagesState(SID);
      expect(state.turns).toHaveLength(1);
      expect(state.turns[0]).toMatchObject({
        type: 'error',
        message: 'ECONNREFUSED 127.0.0.1:11434',
      });
    });

    it('clears awaitingResponse on Error', () => {
      setAwaitingResponse(SID, true);
      pushEvent(SID, { kind: 'Error', message: 'failed' });
      expect(getMessagesState(SID).awaitingResponse).toBe(false);
    });

    it('clears streamingMessageId on Error', () => {
      pushEvent(SID, { kind: 'AssistantDelta', delta: 'partial', message_id: 'msg-9' });
      pushEvent(SID, { kind: 'Error', message: 'stream cut' });
      expect(getMessagesState(SID).streamingMessageId).toBeNull();
    });
  });

  describe('awaitingResponse', () => {
    it('defaults to false', () => {
      expect(getMessagesState(SID).awaitingResponse).toBe(false);
    });

    it('can be set to true', () => {
      setAwaitingResponse(SID, true);
      expect(getMessagesState(SID).awaitingResponse).toBe(true);
    });
  });

  // -----------------------------------------------------------------------
  // F-145 — branch-group tracking
  // -----------------------------------------------------------------------
  describe('branch groups (F-145)', () => {
    it('registers a root variant on the first AssistantMessage', () => {
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'root answer',
        message_id: 'root-1',
        branch_parent: null,
        branch_variant_index: 0,
      });
      const state = getMessagesState(SID);
      expect(state.branchGroups['root-1']).toBeDefined();
      expect(state.branchGroups['root-1']!.variantIds).toEqual(['root-1']);
      expect(state.branchGroups['root-1']!.activeVariantId).toBe('root-1');
    });

    it('attaches a sibling variant under its branch_parent and flips active', () => {
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'root answer',
        message_id: 'root-1',
        branch_parent: null,
        branch_variant_index: 0,
      });
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'variant 1',
        message_id: 'var-1',
        branch_parent: 'root-1',
        branch_variant_index: 1,
      });
      const group = getMessagesState(SID).branchGroups['root-1']!;
      expect(group.variantIds).toEqual(['root-1', 'var-1']);
      expect(group.activeVariantId).toBe('var-1');
      expect(liveVariantCount(group)).toBe(2);
    });

    it('BranchSelected flips the active variant', () => {
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'root',
        message_id: 'root-1',
        branch_parent: null,
        branch_variant_index: 0,
      });
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'v1',
        message_id: 'var-1',
        branch_parent: 'root-1',
        branch_variant_index: 1,
      });
      pushEvent(SID, {
        kind: 'BranchSelected',
        parent: 'root-1',
        selected: 'root-1',
      });
      expect(getMessagesState(SID).branchGroups['root-1']!.activeVariantId).toBe('root-1');
    });

    it('BranchDeleted tombstones a variant and falls back the active id', () => {
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'root',
        message_id: 'root-1',
        branch_parent: null,
        branch_variant_index: 0,
      });
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'v1',
        message_id: 'var-1',
        branch_parent: 'root-1',
        branch_variant_index: 1,
      });
      // active is var-1 at this point. Delete var-1 — active must fall back.
      pushEvent(SID, {
        kind: 'BranchDeleted',
        parent: 'root-1',
        variant_index: 1,
      });
      const group = getMessagesState(SID).branchGroups['root-1']!;
      expect(group.variantIds[1]).toBeNull();
      expect(group.deletedIndices).toContain(1);
      expect(group.activeVariantId).toBe('root-1');
      expect(liveVariantCount(group)).toBe(1);
    });

    it('activeVariantPosition reports 1-indexed position and live count', () => {
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'r',
        message_id: 'root-1',
        branch_parent: null,
        branch_variant_index: 0,
      });
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'a',
        message_id: 'var-1',
        branch_parent: 'root-1',
        branch_variant_index: 1,
      });
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'b',
        message_id: 'var-2',
        branch_parent: 'root-1',
        branch_variant_index: 2,
      });
      const group = getMessagesState(SID).branchGroups['root-1']!;
      // active is var-2 (last sibling registered).
      expect(activeVariantPosition(group)).toEqual({ position: 3, total: 3 });
    });

    it('neighbourVariantId cycles prev / next and wraps', () => {
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'r',
        message_id: 'root-1',
        branch_parent: null,
        branch_variant_index: 0,
      });
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'a',
        message_id: 'var-1',
        branch_parent: 'root-1',
        branch_variant_index: 1,
      });
      const group = getMessagesState(SID).branchGroups['root-1']!;
      // active is var-1. next wraps to root-1; prev wraps to root-1.
      expect(neighbourVariantId(group, 'next')).toBe('root-1');
      expect(neighbourVariantId(group, 'prev')).toBe('root-1');
    });

    it('AssistantMessage without branch fields defaults to root variant', () => {
      pushEvent(SID, {
        kind: 'AssistantMessage',
        text: 'legacy',
        message_id: 'legacy-1',
      });
      const group = getMessagesState(SID).branchGroups['legacy-1']!;
      expect(group.variantIds).toEqual(['legacy-1']);
    });
  });

  describe('multi-session isolation', () => {
    it('keeps turns isolated per session', () => {
      const SID2 = 'session-other' as SessionId;
      pushEvent(SID, { kind: 'UserMessage', text: 'hello sid1', message_id: 'a' });
      pushEvent(SID2, { kind: 'UserMessage', text: 'hello sid2', message_id: 'b' });
      expect(getMessagesState(SID).turns).toHaveLength(1);
      expect(getMessagesState(SID2).turns).toHaveLength(1);
      expect(getMessagesState(SID).turns[0]!.type).toBe('user');
    });
  });

  // F-136: banner-turn lifecycle inside the store. ChatPane integration
  // lives in ChatPane.test.tsx; these cases pin the pure reducer semantics.
  describe('SubAgentSpawned + BackgroundAgentCompleted (F-136)', () => {
    it('appends a sub_agent_banner turn in running state', () => {
      pushEvent(SID, {
        kind: 'SubAgentSpawned',
        parent_instance_id: 'p-1',
        child_instance_id: 'c-1',
        from_msg: 'm-1',
        agent_name: 'writer',
      });
      const state = getMessagesState(SID);
      expect(state.turns).toHaveLength(1);
      const turn = state.turns[0]!;
      if (turn.type !== 'sub_agent_banner') {
        throw new Error(`expected sub_agent_banner turn, got ${turn.type}`);
      }
      expect(turn.child_instance_id).toBe('c-1');
      expect(turn.parent_instance_id).toBe('p-1');
      expect(turn.agent_name).toBe('writer');
      expect(turn.status).toBe('running');
      expect(typeof turn.started_at).toBe('number');
    });

    it('does not stack duplicate banners when the same SubAgentSpawned arrives twice', () => {
      pushEvent(SID, {
        kind: 'SubAgentSpawned',
        parent_instance_id: 'p',
        child_instance_id: 'c-dup',
        from_msg: 'm',
      });
      pushEvent(SID, {
        kind: 'SubAgentSpawned',
        parent_instance_id: 'p',
        child_instance_id: 'c-dup',
        from_msg: 'm',
      });
      const state = getMessagesState(SID);
      expect(state.turns).toHaveLength(1);
    });

    it('flips the matching banner to done on BackgroundAgentCompleted', () => {
      pushEvent(SID, {
        kind: 'SubAgentSpawned',
        parent_instance_id: 'p',
        child_instance_id: 'c-term',
        from_msg: 'm',
      });
      pushEvent(SID, { kind: 'BackgroundAgentCompleted', instance_id: 'c-term' });
      const turn = getMessagesState(SID).turns[0]!;
      if (turn.type !== 'sub_agent_banner') throw new Error('expected banner');
      expect(turn.status).toBe('done');
    });

    it('BackgroundAgentCompleted for an unknown instance id is a no-op', () => {
      pushEvent(SID, { kind: 'BackgroundAgentCompleted', instance_id: 'nobody' });
      expect(getMessagesState(SID).turns).toHaveLength(0);
    });

    // F-448 Phase 3: optional model / tool_count fields flow from the wire
    // event onto the banner turn. Omitted fields leave the turn fields
    // `undefined` so the component can hide chips cleanly.
    it('carries model + tool_count onto the banner turn when the event provides them', () => {
      pushEvent(SID, {
        kind: 'SubAgentSpawned',
        parent_instance_id: 'p',
        child_instance_id: 'c-enriched',
        from_msg: 'm',
        model: 'sonnet-4.5',
        tool_count: 4,
      });
      const turn = getMessagesState(SID).turns[0]!;
      if (turn.type !== 'sub_agent_banner') throw new Error('expected banner');
      expect(turn.model).toBe('sonnet-4.5');
      expect(turn.tool_count).toBe(4);
    });

    it('leaves model / tool_count undefined when the event omits them', () => {
      pushEvent(SID, {
        kind: 'SubAgentSpawned',
        parent_instance_id: 'p',
        child_instance_id: 'c-bare',
        from_msg: 'm',
      });
      const turn = getMessagesState(SID).turns[0]!;
      if (turn.type !== 'sub_agent_banner') throw new Error('expected banner');
      expect(turn.model).toBeUndefined();
      expect(turn.tool_count).toBeUndefined();
    });
  });

  describe('TurnError events (F-749)', () => {
    it('replaces an empty streaming assistant turn at the same message_id', () => {
      // Mirror the orchestrator's protocol: it emits a finalised empty
      // assistant turn immediately before the TurnError so the slot exists.
      pushEvent(SID, { kind: 'UserMessage', text: 'hi', message_id: 'msg-1' });
      pushEvent(SID, { kind: 'AssistantMessage', text: '', message_id: 'msg-1' });
      pushEvent(SID, {
        kind: 'TurnError',
        message_id: 'msg-1',
        error_kind: 'auth',
        message: 'rejected',
        retriable: false,
      });
      const state = getMessagesState(SID);
      // The transcript ends up as [user, turn_error] — no dangling empty bubble.
      expect(state.turns.map((t) => t.type)).toEqual(['user', 'turn_error']);
      const err = state.turns[1]!;
      if (err.type !== 'turn_error') throw new Error('expected turn_error');
      expect(err.error_kind).toBe('auth');
      expect(err.retriable).toBe(false);
      expect(state.awaitingResponse).toBe(false);
      expect(state.streamingMessageId).toBeNull();
    });

    it('appends a turn_error when no assistant placeholder exists', () => {
      pushEvent(SID, { kind: 'UserMessage', text: 'hi', message_id: 'msg-1' });
      pushEvent(SID, {
        kind: 'TurnError',
        message_id: 'msg-99',
        error_kind: 'network',
        message: 'transport error',
        retriable: true,
      });
      const state = getMessagesState(SID);
      expect(state.turns.map((t) => t.type)).toEqual(['user', 'turn_error']);
    });

    it('preserves retry_after_secs and raw onto the turn', () => {
      pushEvent(SID, {
        kind: 'TurnError',
        message_id: 'msg-1',
        error_kind: 'rate_limit',
        message: 'rate limit',
        retriable: true,
        retry_after_secs: 17,
        raw: 'HTTP 429: too many requests',
      });
      const turn = getMessagesState(SID).turns[0]!;
      if (turn.type !== 'turn_error') throw new Error('expected turn_error');
      expect(turn.retry_after_secs).toBe(17);
      expect(turn.raw).toBe('HTTP 429: too many requests');
    });
  });
});

// Adapter tests — fromRustEvent() maps Rust-serialized forge_core::Event
// shapes (`#[serde(tag="type", rename_all="snake_case")]`) into the TS
// messages store's SessionEvent union. See F-037 / #73.
//
// Wire shapes used as inputs here are pinned by
// `crates/forge-core/tests/event_wire_shape.rs`. That file is the "comparable
// harness" called out in F-037's DoD: each Event variant is constructed as a
// real Rust value and round-tripped through `serde_json::to_value`, with the
// resulting JSON asserted against a hard-coded expected shape. The same
// expected shapes are the inputs below. When the Rust enum changes, that
// Rust test fails first; update the failing cases, then mirror the new
// shapes into this file.

import { describe, it, expect } from 'vitest';
import { fromRustEvent } from './events';

describe('fromRustEvent — user_message', () => {
  it('renames id → message_id and strips non-rendering fields', () => {
    const rust = {
      type: 'user_message',
      id: 'mid-1',
      at: '2026-04-18T10:00:00Z',
      text: 'hello',
      context: [],
      branch_parent: null,
    };

    expect(fromRustEvent(rust)).toEqual({
      kind: 'UserMessage',
      message_id: 'mid-1',
      text: 'hello',
    });
  });
});

describe('fromRustEvent — non-rendering variants return null', () => {
  // The enum-value strings below (SessionPersistence::Persist → "Persist",
  // RosterScope::SessionWide → "SessionWide", EndReason::UserExit → "UserExit",
  // etc.) reflect serde's PascalCase default — forge_core/types.rs and
  // event.rs carry no `#[serde(rename_all)]` on these enums.
  const nullCases = [
    {
      type: 'session_started',
      at: '2026-04-18T10:00:00Z',
      workspace: '/w',
      agent: null,
      persistence: 'Persist',
    },
    { type: 'session_ended', at: '2026-04-18T10:00:00Z', reason: 'UserExit', archived: true },
    // F-136: sub_agent_spawned + background_agent_completed are now mapped
    // into renderable SessionEvents (banner mount + terminal flip); they're
    // asserted in their own positive describes below.
    // F-145: branch_selected / branch_deleted are also mapped (positive
    // tests live in the `assistant_message` describe), so neither appears
    // in this null-passthrough list.
    { type: 'background_agent_started', id: 'ba-1', agent: 'a', at: '2026-04-18T10:00:00Z' },
    {
      type: 'usage_tick',
      provider: 'p',
      model: 'm',
      tokens_in: 1,
      tokens_out: 2,
      cost_usd: 0,
      scope: 'SessionWide',
    },
    // F-598: context_compacted now maps to a real SessionEvent — see the
    // positive describe below.
    {
      type: 'tool_call_approved',
      id: 'tc-x',
      by: 'User',
      scope: 'Once',
      at: '2026-04-18T10:00:00Z',
    },
  ];

  it.each(nullCases)('returns null for type=$type', (ev) => {
    expect(fromRustEvent(ev)).toBeNull();
  });
});

describe('fromRustEvent — invalid input returns null (never throws)', () => {
  it.each([
    ['null', null],
    ['undefined', undefined],
    ['string', 'not an event'],
    ['number', 42],
    ['array', [1, 2, 3]],
    ['empty object', {}],
    ['unknown type', { type: 'something_we_do_not_know' }],
    ['missing type', { id: 'x', text: 'y' }],
  ] as Array<[string, unknown]>)('returns null for %s', (_label, input) => {
    expect(fromRustEvent(input)).toBeNull();
  });
});

describe('fromRustEvent — tool_call_rejected', () => {
  it('maps to ToolCallFailed with reason as error', () => {
    const rust = {
      type: 'tool_call_rejected',
      id: 'tc-rej-1',
      reason: 'user denied',
    };

    expect(fromRustEvent(rust)).toEqual({
      kind: 'ToolCallFailed',
      tool_call_id: 'tc-rej-1',
      error: 'user denied',
    });
  });

  it('falls back to "rejected" when reason is null', () => {
    const rust = {
      type: 'tool_call_rejected',
      id: 'tc-rej-2',
      reason: null,
    };

    expect(fromRustEvent(rust)).toEqual({
      kind: 'ToolCallFailed',
      tool_call_id: 'tc-rej-2',
      error: 'rejected',
    });
  });
});

describe('fromRustEvent — tool_call_completed', () => {
  it('stringifies result into result_summary and renames id → tool_call_id', () => {
    const rust = {
      type: 'tool_call_completed',
      id: 'tc-7',
      result: { ok: true, bytes: 42 },
      duration_ms: 12,
      at: '2026-04-18T10:00:05Z',
    };

    expect(fromRustEvent(rust)).toEqual({
      kind: 'ToolCallCompleted',
      tool_call_id: 'tc-7',
      result_summary: '{"ok":true,"bytes":42}',
      // F-447: ok/duration forwarded verbatim so the expanded card can
      // distinguish success from failure without re-parsing the summary.
      result_ok: true,
      duration_ms: 12,
    });
  });

  it('truncates long result_summary at 200 chars', () => {
    const big = 'x'.repeat(500);
    const rust = {
      type: 'tool_call_completed',
      id: 'tc-8',
      result: { content: big },
      duration_ms: 1,
      at: '2026-04-18T10:00:05Z',
    };

    const out = fromRustEvent(rust);
    expect(out).toMatchObject({ kind: 'ToolCallCompleted', tool_call_id: 'tc-8' });
    expect((out as { result_summary: string }).result_summary.length).toBe(200);
  });

  // F-447: the expanded body renders `result.preview` as the up-to-800-char
  // result preview, independent of the 200-char result_summary truncation.
  it('forwards result.preview and result.error as structured fields', () => {
    const rust = {
      type: 'tool_call_completed',
      id: 'tc-p',
      result: { ok: true, preview: 'hello from forge' },
      duration_ms: 42,
      at: '2026-04-18T10:00:05Z',
    };
    expect(fromRustEvent(rust)).toMatchObject({
      kind: 'ToolCallCompleted',
      tool_call_id: 'tc-p',
      result_ok: true,
      result_preview: 'hello from forge',
      duration_ms: 42,
    });

    const errored = {
      type: 'tool_call_completed',
      id: 'tc-e',
      result: { ok: false, error: 'ENOENT' },
      duration_ms: 7,
      at: '2026-04-18T10:00:05Z',
    };
    expect(fromRustEvent(errored)).toMatchObject({
      kind: 'ToolCallCompleted',
      tool_call_id: 'tc-e',
      result_ok: false,
      result_error: 'ENOENT',
      duration_ms: 7,
    });
  });
});

describe('fromRustEvent — tool_call_approval_requested', () => {
  it('passes through preview and renames id → tool_call_id (Rust carries no tool name/args here)', () => {
    const rust = {
      type: 'tool_call_approval_requested',
      id: 'tc-9',
      preview: { description: 'Edit file /src/foo.ts' },
    };

    expect(fromRustEvent(rust)).toEqual({
      kind: 'ToolCallApprovalRequested',
      tool_call_id: 'tc-9',
      preview: { description: 'Edit file /src/foo.ts' },
    });
  });
});

describe('fromRustEvent — tool_call_started', () => {
  it('renames id → tool_call_id, tool → tool_name, stringifies args as args_json', () => {
    const rust = {
      type: 'tool_call_started',
      id: 'tc-1',
      msg: 'mid-3',
      tool: 'fs.read',
      args: { path: 'readable.txt' },
      at: '2026-04-18T10:00:03Z',
      parallel_group: null,
    };

    expect(fromRustEvent(rust)).toEqual({
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-1',
      tool_name: 'fs.read',
      args_json: '{"path":"readable.txt"}',
    });
  });

  it('stringifies parallel_group u32 into batch_id when present', () => {
    const rust = {
      type: 'tool_call_started',
      id: 'tc-2',
      msg: 'mid-3',
      tool: 'fs.read',
      args: { path: 'a.txt' },
      at: '2026-04-18T10:00:03Z',
      parallel_group: 7,
    };

    expect(fromRustEvent(rust)).toEqual({
      kind: 'ToolCallStarted',
      tool_call_id: 'tc-2',
      tool_name: 'fs.read',
      args_json: '{"path":"a.txt"}',
      batch_id: '7',
    });
  });
});

describe('fromRustEvent — assistant_delta', () => {
  it('maps delta id → message_id and preserves delta text', () => {
    const rust = {
      type: 'assistant_delta',
      id: 'mid-3',
      at: '2026-04-18T10:00:02Z',
      delta: 'partial ',
    };

    expect(fromRustEvent(rust)).toEqual({
      kind: 'AssistantDelta',
      message_id: 'mid-3',
      delta: 'partial ',
    });
  });
});

describe('fromRustEvent — assistant_message', () => {
  it('maps the finalized stream form (stream_finalised: true) to AssistantMessage', () => {
    const rust = {
      type: 'assistant_message',
      id: 'mid-2',
      provider: 'mock',
      model: 'mock-1',
      at: '2026-04-18T10:00:01Z',
      stream_finalised: true,
      text: 'hi there',
      branch_parent: null,
      branch_variant_index: 0,
    };

    // F-145: adapter now forwards branch_parent / branch_variant_index and,
    // when present, provider / model / at onto the store event so branch
    // grouping and the metadata popover can read them.
    expect(fromRustEvent(rust)).toEqual({
      kind: 'AssistantMessage',
      message_id: 'mid-2',
      text: 'hi there',
      branch_parent: null,
      branch_variant_index: 0,
      provider: 'mock',
      model: 'mock-1',
      at: '2026-04-18T10:00:01Z',
    });
  });

  it('maps a branched sibling: branch_parent + branch_variant_index forwarded', () => {
    // F-145: when Rust emits an AssistantMessage whose branch_parent points
    // at a root, the adapter preserves that linkage so the store can build
    // a branch group around it.
    const rust = {
      type: 'assistant_message',
      id: 'variant-2',
      provider: 'mock',
      model: 'mock-1',
      at: '2026-04-20T14:26:02Z',
      stream_finalised: true,
      text: 'variant text',
      branch_parent: 'root-1',
      branch_variant_index: 2,
    };
    expect(fromRustEvent(rust)).toEqual({
      kind: 'AssistantMessage',
      message_id: 'variant-2',
      text: 'variant text',
      branch_parent: 'root-1',
      branch_variant_index: 2,
      provider: 'mock',
      model: 'mock-1',
      at: '2026-04-20T14:26:02Z',
    });
  });

  // F-145: branch-tree mutations round-trip through the adapter.
  it('maps branch_selected to BranchSelected', () => {
    expect(
      fromRustEvent({
        type: 'branch_selected',
        parent: 'root-1',
        selected: 'variant-2',
      }),
    ).toEqual({
      kind: 'BranchSelected',
      parent: 'root-1',
      selected: 'variant-2',
    });
  });

  it('maps branch_deleted to BranchDeleted', () => {
    expect(
      fromRustEvent({
        type: 'branch_deleted',
        parent: 'root-1',
        variant_index: 2,
      }),
    ).toEqual({
      kind: 'BranchDeleted',
      parent: 'root-1',
      variant_index: 2,
    });
  });

  it('drops branch_selected with non-string parent', () => {
    expect(
      fromRustEvent({ type: 'branch_selected', parent: 42, selected: 'b' }),
    ).toBeNull();
  });

  it('drops branch_deleted with non-number variant_index', () => {
    expect(
      fromRustEvent({
        type: 'branch_deleted',
        parent: 'root-1',
        variant_index: 'two',
      }),
    ).toBeNull();
  });

  it('returns null for the stream-open sentinel (stream_finalised: false)', () => {
    // Rust emits an empty AssistantMessage with stream_finalised:false at stream
    // start (orchestrator.rs:118). Forwarding it would push an empty non-streaming
    // turn into the store, and the first AssistantDelta would find that turn and
    // append to it without ever setting streamingMessageId — breaking the cursor.
    // The first delta must be the one that creates the streaming turn.
    const rust = {
      type: 'assistant_message',
      id: 'mid-open',
      provider: 'mock',
      model: 'mock-1',
      at: '2026-04-18T10:00:00Z',
      stream_finalised: false,
      text: '',
      branch_parent: null,
      branch_variant_index: 0,
    };

    expect(fromRustEvent(rust)).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// F-064 / M12 / T7 — Runtime narrowing regression tests.
//
// Before narrowing, required string/shape fields were type-asserted with
// `as string` / `as { description: string }`, so malformed daemon payloads
// (bug, version skew, compromised bridge writer) silently flowed into the
// messages store as `undefined`. Downstream, `AssistantDelta` concatenated
// the literal "undefined" into chat text, and `ApprovalPrompt` dereferenced
// `preview.description` on an undefined object and threw at render time.
//
// fromRustEvent must now reject these payloads (return null) rather than
// forwarding garbage. See docs/audits/phase-1/findings/M12.md.
// ---------------------------------------------------------------------------

describe('fromRustEvent — narrowing drops malformed required fields', () => {
  it('drops user_message missing id', () => {
    expect(fromRustEvent({ type: 'user_message', text: 'hi' })).toBeNull();
  });

  it('drops user_message missing text', () => {
    expect(fromRustEvent({ type: 'user_message', id: 'm1' })).toBeNull();
  });

  it('drops user_message with non-string text', () => {
    expect(fromRustEvent({ type: 'user_message', id: 'm1', text: 42 })).toBeNull();
  });

  it('drops tool_call_rejected missing id', () => {
    expect(fromRustEvent({ type: 'tool_call_rejected', reason: 'x' })).toBeNull();
  });

  it('drops tool_call_completed missing id', () => {
    expect(fromRustEvent({ type: 'tool_call_completed', result: {} })).toBeNull();
  });

  it('drops tool_call_approval_requested missing id', () => {
    expect(
      fromRustEvent({ type: 'tool_call_approval_requested', preview: { description: 'x' } }),
    ).toBeNull();
  });

  it('drops tool_call_approval_requested missing preview (ApprovalPrompt crash repro)', () => {
    expect(fromRustEvent({ type: 'tool_call_approval_requested', id: 'tc-x' })).toBeNull();
  });

  it('drops tool_call_approval_requested with preview missing description', () => {
    expect(
      fromRustEvent({ type: 'tool_call_approval_requested', id: 'tc-x', preview: {} }),
    ).toBeNull();
  });

  it('drops tool_call_approval_requested with preview.description non-string', () => {
    expect(
      fromRustEvent({
        type: 'tool_call_approval_requested',
        id: 'tc-x',
        preview: { description: 123 },
      }),
    ).toBeNull();
  });

  it('drops tool_call_started missing tool name', () => {
    expect(
      fromRustEvent({ type: 'tool_call_started', id: 'tc-1', args: {} }),
    ).toBeNull();
  });

  it('drops tool_call_started missing id', () => {
    expect(
      fromRustEvent({ type: 'tool_call_started', tool: 'fs.read', args: {} }),
    ).toBeNull();
  });

  it('drops assistant_delta missing delta (AssistantDelta undefined-concat repro)', () => {
    expect(fromRustEvent({ type: 'assistant_delta', id: 'm1' })).toBeNull();
  });

  it('drops assistant_delta with non-string delta', () => {
    expect(fromRustEvent({ type: 'assistant_delta', id: 'm1', delta: 42 })).toBeNull();
  });

  it('drops assistant_delta missing id', () => {
    expect(fromRustEvent({ type: 'assistant_delta', delta: 'hi' })).toBeNull();
  });

  it('drops finalised assistant_message missing text', () => {
    expect(
      fromRustEvent({ type: 'assistant_message', id: 'm1', stream_finalised: true }),
    ).toBeNull();
  });

  it('drops finalised assistant_message with non-string text', () => {
    expect(
      fromRustEvent({
        type: 'assistant_message',
        id: 'm1',
        stream_finalised: true,
        text: null,
      }),
    ).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// F-136: sub_agent_spawned + background_agent_completed — ChatPane banner
// wire-up. The Rust shape matches `forge_core::Event::SubAgentSpawned` /
// `BackgroundAgentCompleted`; event_wire_shape.rs pins it serverside.
// ---------------------------------------------------------------------------

describe('fromRustEvent — sub_agent_spawned (F-136)', () => {
  it('maps to SubAgentSpawned with parent/child/from_msg carried through', () => {
    const rust = {
      type: 'sub_agent_spawned',
      parent: 'parent-inst-1',
      child: 'child-inst-1',
      from_msg: 'msg-7',
    };
    expect(fromRustEvent(rust)).toEqual({
      kind: 'SubAgentSpawned',
      parent_instance_id: 'parent-inst-1',
      child_instance_id: 'child-inst-1',
      from_msg: 'msg-7',
    });
  });

  it('forwards an optional agent_name when the shell enriches the payload', () => {
    const rust = {
      type: 'sub_agent_spawned',
      parent: 'p',
      child: 'c',
      from_msg: 'm',
      agent_name: 'test-writer',
    };
    expect(fromRustEvent(rust)).toEqual({
      kind: 'SubAgentSpawned',
      parent_instance_id: 'p',
      child_instance_id: 'c',
      from_msg: 'm',
      agent_name: 'test-writer',
    });
  });

  it('drops payloads with a non-string parent', () => {
    expect(
      fromRustEvent({ type: 'sub_agent_spawned', parent: 7, child: 'c', from_msg: 'm' }),
    ).toBeNull();
  });

  it('drops payloads with a non-string child', () => {
    expect(
      fromRustEvent({ type: 'sub_agent_spawned', parent: 'p', child: null, from_msg: 'm' }),
    ).toBeNull();
  });

  it('drops payloads with a non-string from_msg', () => {
    expect(
      fromRustEvent({ type: 'sub_agent_spawned', parent: 'p', child: 'c' }),
    ).toBeNull();
  });

  // F-448 Phase 3: optional header-chip wire fields.
  it('forwards optional model + tool_count when the Rust event carries them', () => {
    const rust = {
      type: 'sub_agent_spawned',
      parent: 'p',
      child: 'c',
      from_msg: 'm',
      model: 'sonnet-4.5',
      tool_count: 4,
    };
    expect(fromRustEvent(rust)).toEqual({
      kind: 'SubAgentSpawned',
      parent_instance_id: 'p',
      child_instance_id: 'c',
      from_msg: 'm',
      model: 'sonnet-4.5',
      tool_count: 4,
    });
  });

  it('omits model when payload carries a non-string model', () => {
    const rust = {
      type: 'sub_agent_spawned',
      parent: 'p',
      child: 'c',
      from_msg: 'm',
      model: 123,
    };
    expect(fromRustEvent(rust)).toEqual({
      kind: 'SubAgentSpawned',
      parent_instance_id: 'p',
      child_instance_id: 'c',
      from_msg: 'm',
    });
  });

  it('omits tool_count when payload carries a non-number tool_count', () => {
    const rust = {
      type: 'sub_agent_spawned',
      parent: 'p',
      child: 'c',
      from_msg: 'm',
      tool_count: 'four',
    };
    expect(fromRustEvent(rust)).toEqual({
      kind: 'SubAgentSpawned',
      parent_instance_id: 'p',
      child_instance_id: 'c',
      from_msg: 'm',
    });
  });
});

describe('fromRustEvent — background_agent_completed (F-136)', () => {
  it('maps to BackgroundAgentCompleted keyed by instance id', () => {
    const rust = {
      type: 'background_agent_completed',
      id: 'child-inst-1',
      at: '2026-04-20T10:00:00Z',
    };
    expect(fromRustEvent(rust)).toEqual({
      kind: 'BackgroundAgentCompleted',
      instance_id: 'child-inst-1',
    });
  });

  it('drops payloads missing the id', () => {
    expect(
      fromRustEvent({ type: 'background_agent_completed', at: '2026-04-20T10:00:00Z' }),
    ).toBeNull();
  });
});

describe('fromRustEvent — context_compacted (F-598)', () => {
  it('maps required fields and trigger variants', () => {
    expect(
      fromRustEvent({
        type: 'context_compacted',
        at: '2026-04-26T10:00:00Z',
        summarized_turns: 4,
        summary_msg_id: 'sum-1',
        trigger: 'AutoAt98Pct',
      }),
    ).toEqual({
      kind: 'ContextCompacted',
      summary_msg_id: 'sum-1',
      summarized_turns: 4,
      trigger: 'AutoAt98Pct',
    });

    expect(
      fromRustEvent({
        type: 'context_compacted',
        at: '2026-04-26T10:00:00Z',
        summarized_turns: 1,
        summary_msg_id: 'sum-2',
        trigger: 'UserRequested',
      }),
    ).toEqual({
      kind: 'ContextCompacted',
      summary_msg_id: 'sum-2',
      summarized_turns: 1,
      trigger: 'UserRequested',
    });
  });

  it('drops payloads with malformed fields', () => {
    // missing summary_msg_id
    expect(
      fromRustEvent({
        type: 'context_compacted',
        at: '2026-04-26T10:00:00Z',
        summarized_turns: 4,
        trigger: 'AutoAt98Pct',
      }),
    ).toBeNull();
    // bad trigger
    expect(
      fromRustEvent({
        type: 'context_compacted',
        at: '2026-04-26T10:00:00Z',
        summarized_turns: 4,
        summary_msg_id: 'sum-3',
        trigger: 'NotARealTrigger',
      }),
    ).toBeNull();
    // non-numeric count
    expect(
      fromRustEvent({
        type: 'context_compacted',
        at: '2026-04-26T10:00:00Z',
        summarized_turns: 'four',
        summary_msg_id: 'sum-4',
        trigger: 'AutoAt98Pct',
      }),
    ).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// F-749 — turn_error
// ---------------------------------------------------------------------------

describe('fromRustEvent — turn_error', () => {
  it('maps auth wire shape onto TurnError with retriable=false', () => {
    expect(
      fromRustEvent({
        type: 'turn_error',
        id: 'mid-9',
        at: '2026-04-18T10:00:00Z',
        kind: { kind: 'auth' },
        message: 'Provider rejected the credential.',
        retriable: false,
        raw: 'HTTP 401: invalid api key',
      }),
    ).toEqual({
      kind: 'TurnError',
      message_id: 'mid-9',
      error_kind: 'auth',
      message: 'Provider rejected the credential.',
      retriable: false,
      raw: 'HTTP 401: invalid api key',
    });
  });

  it('maps rate_limit with retry_after_secs onto TurnError', () => {
    expect(
      fromRustEvent({
        type: 'turn_error',
        id: 'mid-10',
        at: '2026-04-18T10:00:00Z',
        kind: { kind: 'rate_limit', retry_after_secs: 42 },
        message: 'Provider rate limit hit.',
        retriable: true,
      }),
    ).toEqual({
      kind: 'TurnError',
      message_id: 'mid-10',
      error_kind: 'rate_limit',
      message: 'Provider rate limit hit.',
      retriable: true,
      retry_after_secs: 42,
    });
  });

  it('omits retry_after_secs when absent', () => {
    expect(
      fromRustEvent({
        type: 'turn_error',
        id: 'mid-11',
        at: '2026-04-18T10:00:00Z',
        kind: { kind: 'rate_limit' },
        message: 'Provider rate limit hit.',
        retriable: true,
      }),
    ).toEqual({
      kind: 'TurnError',
      message_id: 'mid-11',
      error_kind: 'rate_limit',
      message: 'Provider rate limit hit.',
      retriable: true,
    });
  });

  it('maps network / server / malformed_response / unknown variants', () => {
    const cases: Array<{
      variant: string;
      expected:
        | 'network'
        | 'server'
        | 'malformed_response'
        | 'unknown';
    }> = [
      { variant: 'network', expected: 'network' },
      { variant: 'server', expected: 'server' },
      { variant: 'malformed_response', expected: 'malformed_response' },
      { variant: 'unknown', expected: 'unknown' },
    ];
    for (const c of cases) {
      const out = fromRustEvent({
        type: 'turn_error',
        id: `mid-${c.variant}`,
        at: '2026-04-18T10:00:00Z',
        kind: { kind: c.variant },
        message: 'fail',
        retriable: true,
      });
      expect(out).toMatchObject({
        kind: 'TurnError',
        error_kind: c.expected,
        retriable: true,
      });
    }
  });

  it('falls back to `unknown` when the wire kind is unrecognised', () => {
    expect(
      fromRustEvent({
        type: 'turn_error',
        id: 'mid-x',
        at: '2026-04-18T10:00:00Z',
        kind: { kind: 'not_a_real_kind' },
        message: 'fail',
        retriable: true,
      }),
    ).toMatchObject({
      kind: 'TurnError',
      error_kind: 'unknown',
    });
  });

  it('drops malformed payloads (missing id / message / retriable / kind)', () => {
    // missing id
    expect(
      fromRustEvent({
        type: 'turn_error',
        at: '2026-04-18T10:00:00Z',
        kind: { kind: 'auth' },
        message: 'm',
        retriable: false,
      }),
    ).toBeNull();
    // missing message
    expect(
      fromRustEvent({
        type: 'turn_error',
        id: 'mid',
        at: '2026-04-18T10:00:00Z',
        kind: { kind: 'auth' },
        retriable: false,
      }),
    ).toBeNull();
    // missing retriable
    expect(
      fromRustEvent({
        type: 'turn_error',
        id: 'mid',
        at: '2026-04-18T10:00:00Z',
        kind: { kind: 'auth' },
        message: 'm',
      }),
    ).toBeNull();
    // missing kind tag
    expect(
      fromRustEvent({
        type: 'turn_error',
        id: 'mid',
        at: '2026-04-18T10:00:00Z',
        kind: {},
        message: 'm',
        retriable: false,
      }),
    ).toBeNull();
  });
});

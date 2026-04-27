import { beforeEach, describe, expect, it } from 'vitest';
import type { ProviderId, SessionId } from '@forge/ipc';
import {
  getSessionTelemetry,
  recordProviderModel,
  recordUsageTick,
  resetSessionTelemetryStore,
  routeTelemetryEvent,
} from './sessionTelemetry';

const SID = 'session-telemetry-test' as SessionId;

describe('sessionTelemetry store (F-395)', () => {
  beforeEach(() => {
    resetSessionTelemetryStore();
  });

  it('defaults to null provider/model/tokens/cost for a fresh session', () => {
    const t = getSessionTelemetry(SID);
    expect(t.provider).toBeNull();
    expect(t.model).toBeNull();
    expect(t.tokensIn).toBeNull();
    expect(t.tokensOut).toBeNull();
    expect(t.costUsd).toBeNull();
  });

  it('recordProviderModel sets provider and model', () => {
    recordProviderModel(SID, 'anthropic' as ProviderId, 'claude-opus-4-7');
    const t = getSessionTelemetry(SID);
    expect(t.provider).toBe('anthropic');
    expect(t.model).toBe('claude-opus-4-7');
  });

  it('recordProviderModel overwrites prior values (most-recent wins)', () => {
    recordProviderModel(SID, 'anthropic' as ProviderId, 'claude-opus-4-7');
    recordProviderModel(SID, 'openai' as ProviderId, 'gpt-5');
    const t = getSessionTelemetry(SID);
    expect(t.provider).toBe('openai');
    expect(t.model).toBe('gpt-5');
  });

  it('recordUsageTick sets tokens_in / tokens_out / cost_usd', () => {
    recordUsageTick(SID, 1234, 5678, 0.042);
    const t = getSessionTelemetry(SID);
    expect(t.tokensIn).toBe(1234);
    expect(t.tokensOut).toBe(5678);
    expect(t.costUsd).toBeCloseTo(0.042);
  });

  it('provider and usage are tracked independently', () => {
    recordProviderModel(SID, 'ollama' as ProviderId, 'qwen2.5-coder');
    recordUsageTick(SID, 100, 200, 0);
    const t = getSessionTelemetry(SID);
    expect(t.provider).toBe('ollama');
    expect(t.model).toBe('qwen2.5-coder');
    expect(t.tokensIn).toBe(100);
    expect(t.tokensOut).toBe(200);
    expect(t.costUsd).toBe(0);
  });

  describe('routeTelemetryEvent (wire-shape router)', () => {
    it('routes assistant_message with provider + model into the store', () => {
      routeTelemetryEvent(SID, {
        type: 'assistant_message',
        id: 'a-1',
        at: '2026-04-21T10:00:00Z',
        provider: 'anthropic',
        model: 'claude-opus-4-7',
        text: 'hi',
        stream_finalised: true,
        branch_parent: null,
        branch_variant_index: 0,
      });
      const t = getSessionTelemetry(SID);
      expect(t.provider).toBe('anthropic');
      expect(t.model).toBe('claude-opus-4-7');
    });

    it('ignores an assistant_message that omits provider/model (keeps last-observed pair)', () => {
      recordProviderModel(SID, 'ollama' as ProviderId, 'qwen');
      routeTelemetryEvent(SID, {
        type: 'assistant_message',
        id: 'a-2',
        text: 'hi',
        stream_finalised: true,
        branch_parent: null,
        branch_variant_index: 0,
      });
      const t = getSessionTelemetry(SID);
      // Still the prior pair — we don't clear on a metadata-less event.
      expect(t.provider).toBe('ollama');
      expect(t.model).toBe('qwen');
    });

    it('routes usage_tick with tokens + cost into the store', () => {
      routeTelemetryEvent(SID, {
        type: 'usage_tick',
        provider: 'anthropic',
        model: 'claude-opus-4-7',
        tokens_in: 1234,
        tokens_out: 5678,
        cost_usd: 0.042,
        scope: { type: 'SessionWide' },
      });
      const t = getSessionTelemetry(SID);
      expect(t.tokensIn).toBe(1234);
      expect(t.tokensOut).toBe(5678);
      expect(t.costUsd).toBeCloseTo(0.042);
    });

    it('routes usage_tick whose session_id matches the window', () => {
      // F-605: ticks tagged with this window's session id flow through.
      routeTelemetryEvent(SID, {
        type: 'usage_tick',
        session_id: SID,
        provider: 'anthropic',
        model: 'claude-opus-4-7',
        tokens_in: 100,
        tokens_out: 200,
        cost_usd: 0.01,
        scope: { type: 'SessionWide' },
      });
      const t = getSessionTelemetry(SID);
      expect(t.tokensIn).toBe(100);
      expect(t.tokensOut).toBe(200);
    });

    it('drops usage_tick whose session_id is for a different session', () => {
      // F-605: a tick from another session must not credit this window's
      // running totals.
      routeTelemetryEvent(SID, {
        type: 'usage_tick',
        session_id: 'c0ffeec0ffeec0ff',
        provider: 'anthropic',
        model: 'claude-opus-4-7',
        tokens_in: 9999,
        tokens_out: 9999,
        cost_usd: 9.99,
        scope: { type: 'SessionWide' },
      });
      const t = getSessionTelemetry(SID);
      expect(t.tokensIn).toBeNull();
      expect(t.tokensOut).toBeNull();
      expect(t.costUsd).toBeNull();
    });

    it('is a no-op for unrelated wire events', () => {
      routeTelemetryEvent(SID, {
        type: 'user_message',
        id: 'u-1',
        at: '2026-04-21T10:00:00Z',
        text: 'hi',
        context: [],
        branch_parent: null,
      });
      const t = getSessionTelemetry(SID);
      expect(t.provider).toBeNull();
      expect(t.tokensIn).toBeNull();
    });

    it('is a no-op for non-object inputs', () => {
      routeTelemetryEvent(SID, null);
      routeTelemetryEvent(SID, undefined);
      routeTelemetryEvent(SID, 'string');
      routeTelemetryEvent(SID, 42);
      const t = getSessionTelemetry(SID);
      expect(t.provider).toBeNull();
      expect(t.tokensIn).toBeNull();
    });
  });

  it('resetSessionTelemetryStore clears every session', () => {
    recordProviderModel(SID, 'anthropic' as ProviderId, 'claude-opus-4-7');
    recordUsageTick(SID, 1, 2, 3);
    resetSessionTelemetryStore();
    const t = getSessionTelemetry(SID);
    expect(t.provider).toBeNull();
    expect(t.tokensIn).toBeNull();
  });
});

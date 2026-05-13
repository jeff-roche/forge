// F-749: rendering + action behaviour for `<TurnErrorCard>`. Covers each
// `TurnErrorKind` arm — auth shows "Open provider settings" and hides
// Retry; rate_limit shows a countdown when seconds are known; the other
// kinds show Retry immediately. The "Show details" disclosure flips the
// raw section in/out. Tests inject `onOpenProviderSettings` to avoid the
// solid-router context dependency.

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { cleanup, fireEvent, render } from '@solidjs/testing-library';
import type { ChatTurn } from '../../stores/messages';
import { TurnErrorCard } from './TurnErrorCard';

type TurnErrorTurn = Extract<ChatTurn, { type: 'turn_error' }>;

function fixture(overrides: Partial<TurnErrorTurn> = {}): TurnErrorTurn {
  return {
    type: 'turn_error',
    message_id: 'mid-failed',
    error_kind: 'unknown',
    message: 'something broke',
    retriable: true,
    ...overrides,
  } as TurnErrorTurn;
}

beforeEach(() => {
  vi.useRealTimers();
});

afterEach(() => {
  cleanup();
});

describe('TurnErrorCard — auth kind', () => {
  it('renders "Open provider settings" CTA and hides Retry', () => {
    const turn = fixture({ error_kind: 'auth', retriable: false });
    const onOpen = vi.fn();
    const { getByTestId, queryByTestId } = render(() => (
      <TurnErrorCard turn={turn} onOpenProviderSettings={onOpen} />
    ));

    expect(getByTestId(`turn-error-open-providers-${turn.message_id}`)).toBeTruthy();
    expect(queryByTestId(`turn-error-retry-${turn.message_id}`)).toBeNull();
  });

  it('invokes the navigation callback when the CTA is clicked', () => {
    const turn = fixture({ error_kind: 'auth', retriable: false });
    const onOpen = vi.fn();
    const { getByTestId } = render(() => (
      <TurnErrorCard turn={turn} onOpenProviderSettings={onOpen} />
    ));

    fireEvent.click(getByTestId(`turn-error-open-providers-${turn.message_id}`));
    expect(onOpen).toHaveBeenCalledOnce();
  });

  it('renders the `auth_error` monospace label', () => {
    const turn = fixture({ error_kind: 'auth', retriable: false });
    const { getByTestId } = render(() => (
      <TurnErrorCard turn={turn} onOpenProviderSettings={vi.fn()} />
    ));

    expect(getByTestId(`turn-error-kind-${turn.message_id}`).textContent).toBe(
      'auth_error',
    );
  });
});

describe('TurnErrorCard — rate_limit kind', () => {
  it('renders Retry disabled with a countdown when retry_after_secs is known', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(2026, 0, 1, 12, 0, 0));
    const turn = fixture({
      error_kind: 'rate_limit',
      retriable: true,
      retry_after_secs: 5,
    });
    const onRetry = vi.fn();
    const { getByTestId } = render(() => (
      <TurnErrorCard turn={turn} onRetry={onRetry} />
    ));
    const btn = getByTestId(
      `turn-error-retry-${turn.message_id}`,
    ) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    expect(btn.textContent).toContain('RETRY IN 5');
    // Clicking a disabled Retry must not fire the callback.
    fireEvent.click(btn);
    expect(onRetry).not.toHaveBeenCalled();
  });

  it('flips Retry to enabled once the countdown elapses', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(2026, 0, 1, 12, 0, 0));
    const turn = fixture({
      error_kind: 'rate_limit',
      retriable: true,
      retry_after_secs: 2,
    });
    const onRetry = vi.fn();
    const { getByTestId } = render(() => (
      <TurnErrorCard turn={turn} onRetry={onRetry} />
    ));
    const btn = getByTestId(
      `turn-error-retry-${turn.message_id}`,
    ) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);

    vi.advanceTimersByTime(3000);
    expect(btn.disabled).toBe(false);
    expect(btn.textContent).toContain('RETRY');
    fireEvent.click(btn);
    expect(onRetry).toHaveBeenCalledOnce();
  });

  it('shows Retry available immediately when retry_after_secs is unknown', () => {
    const turn = fixture({
      error_kind: 'rate_limit',
      retriable: true,
    });
    const onRetry = vi.fn();
    const { getByTestId } = render(() => (
      <TurnErrorCard turn={turn} onRetry={onRetry} />
    ));
    const btn = getByTestId(
      `turn-error-retry-${turn.message_id}`,
    ) as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
    fireEvent.click(btn);
    expect(onRetry).toHaveBeenCalledOnce();
  });
});

describe('TurnErrorCard — retriable kinds (network / server / malformed / unknown)', () => {
  const cases: Array<TurnErrorTurn['error_kind']> = [
    'network',
    'server',
    'malformed_response',
    'unknown',
  ];

  it.each(cases)(
    'renders Retry available immediately for %s',
    (kind) => {
      const turn = fixture({ error_kind: kind, retriable: true });
      const onRetry = vi.fn();
      const { getByTestId, queryByTestId } = render(() => (
        <TurnErrorCard turn={turn} onRetry={onRetry} />
      ));
      const btn = getByTestId(
        `turn-error-retry-${turn.message_id}`,
      ) as HTMLButtonElement;
      expect(btn.disabled).toBe(false);
      // The auth CTA must not appear for any retriable kind.
      expect(
        queryByTestId(`turn-error-open-providers-${turn.message_id}`),
      ).toBeNull();
      fireEvent.click(btn);
      expect(onRetry).toHaveBeenCalledOnce();
    },
  );
});

describe('TurnErrorCard — Show details disclosure', () => {
  it('hides `raw` by default and reveals on click', () => {
    const turn = fixture({
      error_kind: 'server',
      retriable: true,
      raw: 'HTTP 503: service unavailable',
    });
    const { getByTestId, queryByTestId } = render(() => (
      <TurnErrorCard turn={turn} onRetry={vi.fn()} />
    ));

    expect(queryByTestId(`turn-error-raw-${turn.message_id}`)).toBeNull();
    fireEvent.click(getByTestId(`turn-error-details-toggle-${turn.message_id}`));
    expect(
      getByTestId(`turn-error-raw-${turn.message_id}`).textContent,
    ).toContain('HTTP 503');
  });

  it('shows a placeholder when raw is absent', () => {
    const turn = fixture({ error_kind: 'network', retriable: true });
    const { getByTestId } = render(() => (
      <TurnErrorCard turn={turn} onRetry={vi.fn()} />
    ));
    fireEvent.click(getByTestId(`turn-error-details-toggle-${turn.message_id}`));
    expect(
      getByTestId(`turn-error-raw-empty-${turn.message_id}`).textContent,
    ).toMatch(/No additional provider detail/);
  });
});

describe('TurnErrorCard — non-retriable verdict respected', () => {
  it('disables Retry when the orchestrator marked the error non-retriable', () => {
    // Defensive: even for a non-auth kind, `retriable: false` on the wire
    // wins. The card MUST never offer a Retry the daemon told us is
    // counterproductive.
    const turn = fixture({ error_kind: 'unknown', retriable: false });
    const onRetry = vi.fn();
    const { getByTestId } = render(() => (
      <TurnErrorCard turn={turn} onRetry={onRetry} />
    ));
    const btn = getByTestId(
      `turn-error-retry-${turn.message_id}`,
    ) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    fireEvent.click(btn);
    expect(onRetry).not.toHaveBeenCalled();
  });
});

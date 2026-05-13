import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, fireEvent } from '@solidjs/testing-library';
import { CrashRestartOverlay } from './CrashRestartOverlay';

describe('CrashRestartOverlay (F-748)', () => {
  afterEach(() => cleanup());

  it('renders the prompting state with Restart and Close affordances', () => {
    const onRestart = vi.fn();
    const onClose = vi.fn();
    const { getByTestId, queryByTestId } = render(() => (
      <CrashRestartOverlay
        state="prompting"
        onRestart={onRestart}
        onClose={onClose}
      />
    ));
    expect(getByTestId('crash-restart-overlay').dataset.state).toBe(
      'prompting',
    );
    expect(getByTestId('crash-restart-overlay-headline').textContent).toBe(
      'Session crashed',
    );
    expect(getByTestId('crash-restart-overlay-body').textContent).toContain(
      'preserved',
    );
    expect(getByTestId('crash-restart-overlay-restart')).toBeInTheDocument();
    expect(getByTestId('crash-restart-overlay-close')).toBeInTheDocument();
    // F-748: while prompting, no spinner copy is shown — the affordance
    // is the user's decision point, not an indeterminate state.
    expect(queryByTestId('crash-restart-overlay-progress')).toBeNull();
  });

  it('Restart click invokes onRestart exactly once', () => {
    const onRestart = vi.fn();
    const onClose = vi.fn();
    const { getByTestId } = render(() => (
      <CrashRestartOverlay
        state="prompting"
        onRestart={onRestart}
        onClose={onClose}
      />
    ));
    fireEvent.click(getByTestId('crash-restart-overlay-restart'));
    expect(onRestart).toHaveBeenCalledTimes(1);
    expect(onClose).not.toHaveBeenCalled();
  });

  it('Close click invokes onClose exactly once', () => {
    const onRestart = vi.fn();
    const onClose = vi.fn();
    const { getByTestId } = render(() => (
      <CrashRestartOverlay
        state="prompting"
        onRestart={onRestart}
        onClose={onClose}
      />
    ));
    fireEvent.click(getByTestId('crash-restart-overlay-close'));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(onRestart).not.toHaveBeenCalled();
  });

  it('renders the restarting state with a progress affordance instead of buttons', () => {
    const { getByTestId, queryByTestId } = render(() => (
      <CrashRestartOverlay
        state="restarting"
        onRestart={() => {}}
        onClose={() => {}}
      />
    ));
    expect(getByTestId('crash-restart-overlay-progress')).toBeInTheDocument();
    expect(getByTestId('crash-restart-overlay-progress').textContent).toMatch(
      /restarting/i,
    );
    // Spec: during restart the prompting buttons are replaced — neither
    // affordance is reachable.
    expect(queryByTestId('crash-restart-overlay-restart')).toBeNull();
    expect(queryByTestId('crash-restart-overlay-close')).toBeNull();
  });

  it('renders the restart_failed state with the failure message and retry/close affordances', () => {
    const onRestart = vi.fn();
    const onClose = vi.fn();
    const { getByTestId } = render(() => (
      <CrashRestartOverlay
        state="restart_failed"
        errorMessage="session_restart: connect UDS: connection refused"
        onRestart={onRestart}
        onClose={onClose}
      />
    ));
    expect(getByTestId('crash-restart-overlay').dataset.state).toBe(
      'restart_failed',
    );
    expect(getByTestId('crash-restart-overlay-error').textContent).toContain(
      'connection refused',
    );
    // Retry uses the same `onRestart` handler — the SessionWindow drives
    // both the initial prompt and the post-failure retry through one call.
    fireEvent.click(getByTestId('crash-restart-overlay-retry'));
    expect(onRestart).toHaveBeenCalledTimes(1);
    fireEvent.click(getByTestId('crash-restart-overlay-close'));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('Escape does NOT dismiss the overlay (session is unusable until Restart/Close)', () => {
    const onClose = vi.fn();
    const onRestart = vi.fn();
    const { getByTestId } = render(() => (
      <CrashRestartOverlay
        state="prompting"
        onRestart={onRestart}
        onClose={onClose}
      />
    ));
    fireEvent.keyDown(getByTestId('crash-restart-overlay'), { key: 'Escape' });
    // Pressing Escape must NOT trigger close — the design direction in
    // the issue body explicitly forbids silent dismissal.
    expect(onClose).not.toHaveBeenCalled();
    expect(onRestart).not.toHaveBeenCalled();
  });

  it('uses role="alertdialog" with aria-modal so screen readers park focus on it', () => {
    const { getByTestId } = render(() => (
      <CrashRestartOverlay
        state="prompting"
        onRestart={() => {}}
        onClose={() => {}}
      />
    ));
    const overlay = getByTestId('crash-restart-overlay');
    expect(overlay.getAttribute('role')).toBe('alertdialog');
    expect(overlay.getAttribute('aria-modal')).toBe('true');
    expect(overlay.getAttribute('aria-labelledby')).toBe(
      'crash-restart-overlay-headline',
    );
  });
});

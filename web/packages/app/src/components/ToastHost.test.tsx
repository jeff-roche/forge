// ToastHost integration smoke test: pushing a toast at runtime must
// render it inside the host element.

import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { cleanup, render, waitFor } from '@solidjs/testing-library';
import { ToastHost } from './ToastHost';
import { clearToastsForTesting, dismissToast, pushToast } from './toast';

beforeEach(() => {
  clearToastsForTesting();
});

afterEach(() => {
  clearToastsForTesting();
  cleanup();
});

describe('ToastHost', () => {
  it('renders a toast pushed after mount', async () => {
    const { getByTestId, queryByTestId } = render(() => <ToastHost />);
    expect(getByTestId('toast-host').children.length).toBe(0);
    const id = pushToast('info', 'hello world');
    await waitFor(() => {
      expect(queryByTestId(`toast-${id}`)).not.toBeNull();
    });
    expect(getByTestId(`toast-${id}`).textContent).toContain('hello world');
  });

  it('removes a toast when dismissed', async () => {
    const { queryByTestId } = render(() => <ToastHost />);
    const id = pushToast('info', 'goodbye');
    await waitFor(() => {
      expect(queryByTestId(`toast-${id}`)).not.toBeNull();
    });
    dismissToast(id);
    await waitFor(() => {
      expect(queryByTestId(`toast-${id}`)).toBeNull();
    });
  });

  it('tags errors with role="alert"', async () => {
    const { queryByTestId } = render(() => <ToastHost />);
    const id = pushToast('error', 'boom');
    await waitFor(() => {
      const node = queryByTestId(`toast-${id}`);
      expect(node?.getAttribute('role')).toBe('alert');
    });
  });
});

// Toast renderer for the in-memory queue declared in `./toast.ts`.
//
// Mounted once at the AppShell level so any component can call
// `pushToast(kind, message)` and have the message surface as a
// dismissible pop-up. Each toast auto-dismisses after a kind-specific
// TTL (errors hang around longer than info / warnings) and the user can
// dismiss sooner via the × button.
//
// Why this component is intentionally minimal:
// - No animation library (CSS keyframe slide-in is enough).
// - No portal — the host is `position: fixed`, so DOM position doesn't
//   matter for stacking.
// - `aria-live="polite"` on the list so screen readers announce the
//   message without preempting in-flight speech.

import { type Component, For, onCleanup, onMount } from 'solid-js';
import { type Toast, type ToastKind, dismissToast, toasts } from './toast';
import './ToastHost.css';

/**
 * Auto-dismiss TTL by kind. Errors stay longer because they often carry
 * verbatim daemon messages the user wants to copy.
 */
const TTL_MS: Record<ToastKind, number> = {
  info: 10_000,
  warning: 12_000,
  error: 15_000,
};

export const ToastHost: Component = () => (
  <ol class="toast-host" data-testid="toast-host" aria-live="polite">
    <For each={toasts()}>{(toast) => <ToastItem toast={toast} />}</For>
  </ol>
);

interface ToastItemProps {
  toast: Toast;
}

const ToastItem: Component<ToastItemProps> = (props) => {
  let timer: ReturnType<typeof setTimeout> | null = null;
  onMount(() => {
    timer = setTimeout(() => dismissToast(props.toast.id), TTL_MS[props.toast.kind]);
  });
  onCleanup(() => {
    if (timer !== null) clearTimeout(timer);
  });

  return (
    <li
      class="toast"
      data-kind={props.toast.kind}
      data-testid={`toast-${props.toast.id}`}
      role={props.toast.kind === 'error' ? 'alert' : 'status'}
    >
      <span class="toast__message">{props.toast.message}</span>
      <button
        type="button"
        class="toast__close"
        aria-label="Dismiss notification"
        data-testid={`toast-dismiss-${props.toast.id}`}
        onClick={() => dismissToast(props.toast.id)}
      >
        ×
      </button>
    </li>
  );
};

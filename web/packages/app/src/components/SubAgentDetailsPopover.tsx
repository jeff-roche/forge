import { type Component, Show, onMount } from 'solid-js';
import { useFocusTrap } from '../lib/useFocusTrap';
import type { SubAgentStatus } from '../stores/messages';
import './SubAgentDetailsPopover.css';

/**
 * F-448 Phase 3 — status details popover anchored to the SubAgentBanner's
 * state chip. Spec `docs/ui-specs/sub-agent-banner.md` §6 Interaction.
 *
 * Surfaces the child's instance id, status, started-at timestamp, last-step
 * summary, and a jump into the Agent Monitor (§9). Dismisses on outside
 * click and Escape via the menu-mode `useFocusTrap` hook.
 */

export interface SubAgentDetailsPopoverProps {
  childInstanceId: string;
  status: SubAgentStatus;
  /** ms epoch of the spawn — rendered as `HH:MM:SS`. */
  startedAt: number;
  /** Optional last-step summary line; hidden when undefined. */
  lastStepSummary?: string | undefined;
  /** Fires when the "Open in Agent Monitor" affordance is activated. */
  onOpenInMonitor: () => void;
  /** Fires on Escape or outside click. Host owns open/closed state. */
  onDismiss: () => void;
}

function formatStartedAt(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number): string => String(n).padStart(2, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

export const SubAgentDetailsPopover: Component<SubAgentDetailsPopoverProps> = (props) => {
  // Menu-mode focus trap: Escape + outside-click dismissal. F-696: per APG
  // menu pattern, focus moves to the first interactive descendant on open so
  // assistive tech can announce popover contents and Tab navigates within it.
  let rootRef: HTMLDivElement | undefined;
  useFocusTrap(() => rootRef, { trap: false, onDismiss: () => props.onDismiss() });
  onMount(() => {
    const first = rootRef?.querySelector<HTMLElement>(
      'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
    );
    first?.focus();
  });

  return (
    <div
      ref={rootRef}
      class="sub-agent-popover"
      data-testid={`sub-agent-banner-popover-${props.childInstanceId}`}
      role="menu"
      aria-label={`Sub-agent ${props.childInstanceId} details`}
    >
      <dl class="sub-agent-popover__grid">
        <dt>instance</dt>
        <dd
          class="sub-agent-popover__instance"
          data-testid={`sub-agent-banner-popover-instance-${props.childInstanceId}`}
        >
          {props.childInstanceId}
        </dd>
        <dt>status</dt>
        <dd
          class="sub-agent-popover__status"
          data-state={props.status}
          data-testid={`sub-agent-banner-popover-status-${props.childInstanceId}`}
        >
          {props.status}
        </dd>
        <dt>started</dt>
        <dd
          class="sub-agent-popover__started"
          data-testid={`sub-agent-banner-popover-started-${props.childInstanceId}`}
        >
          {formatStartedAt(props.startedAt)}
        </dd>
        <Show when={props.lastStepSummary}>
          <dt>last step</dt>
          <dd
            class="sub-agent-popover__last-step"
            data-testid={`sub-agent-banner-popover-last-step-${props.childInstanceId}`}
          >
            {props.lastStepSummary}
          </dd>
        </Show>
      </dl>
      <footer class="sub-agent-popover__footer">
        <button
          type="button"
          class="sub-agent-popover__monitor"
          data-testid={`sub-agent-banner-popover-monitor-${props.childInstanceId}`}
          onClick={() => props.onOpenInMonitor()}
        >
          OPEN IN AGENT MONITOR
        </button>
      </footer>
    </div>
  );
};

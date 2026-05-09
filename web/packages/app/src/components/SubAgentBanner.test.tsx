import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@solidjs/testing-library';
import { SubAgentBanner } from './SubAgentBanner';
import type { ChatTurn } from '../stores/messages';

// F-448 Phase 3 — header chip completion (`model`, `N tools`) + state-chip
// popover. Tests cover: chip rendering with/without fields, popover open /
// close, stopPropagation preventing the header toggle on chip click,
// Escape + outside-click dismissal.

// F-136 component tests — the DoD pins expand/collapse, nested rendering for
// sub-of-sub banners, and completion state. Tests pass a `turn` fixture +
// optional `children`/`onOpenInMonitor` overrides; no store wiring required.

function makeTurn(
  overrides: Partial<Extract<ChatTurn, { type: 'sub_agent_banner' }>> = {},
): Extract<ChatTurn, { type: 'sub_agent_banner' }> {
  return {
    type: 'sub_agent_banner',
    child_instance_id: 'child-1',
    parent_instance_id: 'parent-1',
    status: 'running',
    started_at: Date.UTC(2026, 3, 20, 14, 37, 0),
    ...overrides,
  };
}

describe('SubAgentBanner — header + default state', () => {
  it('renders the spawned glyph and agent name in the header', () => {
    const { getByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn({ agent_name: 'test-writer' })} />
    ));
    const header = getByTestId('sub-agent-banner-header-child-1');
    expect(header).toHaveTextContent('spawned');
    expect(header).toHaveTextContent('test-writer');
  });

  it('falls back to a short prefix of the child id when no agent_name is set', () => {
    const { getByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn({ child_instance_id: 'abcd1234-efgh-5678' })} />
    ));
    const header = getByTestId('sub-agent-banner-header-abcd1234-efgh-5678');
    // Short prefix (first 8 chars) is visible; the full id is not.
    expect(header).toHaveTextContent('abcd1234');
    expect(header).not.toHaveTextContent('efgh-5678');
  });

  it('renders a "delegated at HH:MM" timestamp from started_at', () => {
    // Build a fixture whose local-time HH:MM we can predict independent
    // of the runner's TZ by reading the same Date back.
    const at = new Date();
    at.setHours(9, 5, 0, 0);
    const { getByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn({ started_at: at.getTime() })} />
    ));
    const ts = getByTestId('sub-agent-banner-timestamp-child-1');
    expect(ts).toHaveTextContent(/delegated at \d{2}:\d{2}/);
  });

  it('mounts collapsed by default — summary visible, body hidden', () => {
    const { getByTestId, queryByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn()} />
    ));
    expect(getByTestId('sub-agent-banner-child-1')).toHaveAttribute(
      'data-expanded',
      'false',
    );
    expect(getByTestId('sub-agent-banner-summary-child-1')).toBeInTheDocument();
    expect(queryByTestId('sub-agent-banner-body-child-1')).not.toBeInTheDocument();
  });
});

describe('SubAgentBanner — expand/collapse', () => {
  it('toggles to expanded on header click and back to collapsed on second click', () => {
    const { getByTestId, queryByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn()} />
    ));
    const header = getByTestId('sub-agent-banner-header-child-1');

    fireEvent.click(header);
    expect(getByTestId('sub-agent-banner-child-1')).toHaveAttribute(
      'data-expanded',
      'true',
    );
    expect(getByTestId('sub-agent-banner-body-child-1')).toBeInTheDocument();
    expect(queryByTestId('sub-agent-banner-summary-child-1')).not.toBeInTheDocument();

    fireEvent.click(header);
    expect(getByTestId('sub-agent-banner-child-1')).toHaveAttribute(
      'data-expanded',
      'false',
    );
  });

  // F-696: header is a `role="button"` disclosure — must announce its
  // collapsed/expanded state via aria-expanded for assistive tech.
  it('header reports aria-expanded=false when collapsed and true when expanded', () => {
    const { getByTestId } = render(() => <SubAgentBanner turn={makeTurn()} />);
    const header = getByTestId('sub-agent-banner-header-child-1');
    expect(header.getAttribute('aria-expanded')).toBe('false');
    fireEvent.click(header);
    expect(header.getAttribute('aria-expanded')).toBe('true');
    fireEvent.click(header);
    expect(header.getAttribute('aria-expanded')).toBe('false');
  });

  it('expands when Enter is pressed on the focused header', () => {
    const { getByTestId } = render(() => <SubAgentBanner turn={makeTurn()} />);
    const header = getByTestId('sub-agent-banner-header-child-1');
    fireEvent.keyDown(header, { key: 'Enter' });
    expect(getByTestId('sub-agent-banner-child-1')).toHaveAttribute(
      'data-expanded',
      'true',
    );
  });

  it('expands when Space is pressed on the focused header', () => {
    const { getByTestId } = render(() => <SubAgentBanner turn={makeTurn()} />);
    const header = getByTestId('sub-agent-banner-header-child-1');
    fireEvent.keyDown(header, { key: ' ' });
    expect(getByTestId('sub-agent-banner-child-1')).toHaveAttribute(
      'data-expanded',
      'true',
    );
  });

  it('renders the "// no steps yet" placeholder when expanded with no children', () => {
    const { getByTestId } = render(() => <SubAgentBanner turn={makeTurn()} />);
    fireEvent.click(getByTestId('sub-agent-banner-header-child-1'));
    const empty = getByTestId('sub-agent-banner-empty-child-1');
    expect(empty).toBeInTheDocument();
    // F-411: canonical empty-state form is `// <noun phrase>` per
    // voice-terminology.md §8.
    expect(empty).toHaveTextContent('// no steps yet');
    expect(empty.textContent).not.toMatch(/No step events yet/i);
  });
});

describe('SubAgentBanner — nested rendering for sub-of-sub', () => {
  it('renders a nested SubAgentBanner when an expanded body carries a sub_agent_banner child turn', () => {
    const grandchild: Extract<ChatTurn, { type: 'sub_agent_banner' }> = {
      type: 'sub_agent_banner',
      child_instance_id: 'grandchild-1',
      parent_instance_id: 'child-1',
      status: 'running',
      started_at: Date.now(),
      agent_name: 'deep-helper',
    };
    const { getByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn()} children={[grandchild]} />
    ));
    fireEvent.click(getByTestId('sub-agent-banner-header-child-1'));

    // Nested banner is rendered inside the parent body.
    expect(getByTestId('sub-agent-banner-grandchild-1')).toBeInTheDocument();
    const nestedHeader = getByTestId('sub-agent-banner-header-grandchild-1');
    expect(nestedHeader).toHaveTextContent('deep-helper');
  });

  it('replaces nested inline children with an "OPEN IN NEW WINDOW" button past max depth (3)', () => {
    const grandchild: Extract<ChatTurn, { type: 'sub_agent_banner' }> = {
      type: 'sub_agent_banner',
      child_instance_id: 'too-deep-1',
      parent_instance_id: 'child-1',
      status: 'running',
      started_at: Date.now(),
    };
    const { getByTestId, queryByTestId } = render(() => (
      <SubAgentBanner
        turn={makeTurn()}
        children={[grandchild]}
        depth={3}
      />
    ));
    fireEvent.click(getByTestId('sub-agent-banner-header-child-1'));
    // At depth ≥ 3 the body renders the open-monitor fallback, not the
    // nested banner — its testid must be absent.
    expect(queryByTestId('sub-agent-banner-too-deep-1')).not.toBeInTheDocument();
    expect(getByTestId('sub-agent-banner-open-monitor-child-1')).toBeInTheDocument();
  });
});

describe('SubAgentBanner — completion state', () => {
  it('renders the state chip with data-state="running" while the child is live', () => {
    const { getByTestId, container } = render(() => (
      <SubAgentBanner turn={makeTurn({ status: 'running' })} />
    ));
    const chip = getByTestId('sub-agent-banner-state-child-1');
    expect(chip).toHaveAttribute('data-state', 'running');
    expect(chip).toHaveTextContent('running');
    // The outer section also mirrors the status via data-status so CSS can
    // target completed vs. running styling without prop plumbing.
    const section = container.querySelector('.sub-agent-banner');
    expect(section).toHaveAttribute('data-status', 'running');
  });

  it('renders the state chip with data-state="done" on completion', () => {
    const { getByTestId, container } = render(() => (
      <SubAgentBanner turn={makeTurn({ status: 'done' })} />
    ));
    const chip = getByTestId('sub-agent-banner-state-child-1');
    expect(chip).toHaveAttribute('data-state', 'done');
    expect(chip).toHaveTextContent('done');
    const section = container.querySelector('.sub-agent-banner');
    expect(section).toHaveAttribute('data-status', 'done');
  });

  it('summary line shows step count + last step when provided', () => {
    const { getByTestId } = render(() => (
      <SubAgentBanner
        turn={makeTurn({ step_count: 6, last_step_summary: 'wrote validate.test.ts' })}
      />
    ));
    const summary = getByTestId('sub-agent-banner-summary-child-1');
    expect(summary).toHaveTextContent('6 steps');
    expect(summary).toHaveTextContent('wrote validate.test.ts');
  });

  it('summary line falls back to "waiting for first step" before any step event', () => {
    const { getByTestId } = render(() => <SubAgentBanner turn={makeTurn()} />);
    expect(getByTestId('sub-agent-banner-summary-child-1')).toHaveTextContent(
      'waiting for first step',
    );
  });
});

describe('SubAgentBanner — double-click to Agent Monitor (F-140)', () => {
  it('calls onOpenInMonitor with the child instance id on header double-click', () => {
    const open = vi.fn();
    const { getByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn()} onOpenInMonitor={open} />
    ));
    fireEvent.dblClick(getByTestId('sub-agent-banner-header-child-1'));
    expect(open).toHaveBeenCalledWith('child-1');
  });

  it('invokes the "OPEN IN NEW WINDOW" button when depth exceeds the inline cap', () => {
    const open = vi.fn();
    const grandchild: Extract<ChatTurn, { type: 'sub_agent_banner' }> = {
      type: 'sub_agent_banner',
      child_instance_id: 'too-deep-2',
      parent_instance_id: 'child-1',
      status: 'running',
      started_at: Date.now(),
    };
    const { getByTestId } = render(() => (
      <SubAgentBanner
        turn={makeTurn()}
        children={[grandchild]}
        depth={3}
        onOpenInMonitor={open}
      />
    ));
    fireEvent.click(getByTestId('sub-agent-banner-header-child-1'));
    const openBtn = getByTestId('sub-agent-banner-open-monitor-child-1');
    // F-695: spec (`sub-agent-banner.md` §6) is authoritative — the button
    // copy must be the verbatim phrase the spec sanctions, rendered in
    // voice-terminology.md §8 display caps.
    expect(openBtn).toHaveTextContent('OPEN IN NEW WINDOW');
    expect(openBtn.textContent).not.toMatch(/OPEN MONITOR/);
    fireEvent.click(openBtn);
    expect(open).toHaveBeenCalledWith('child-1');
  });
});

describe('SubAgentBanner — header chips (F-448 Phase 3)', () => {
  it('renders the model chip when the turn carries a model', () => {
    const { getByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn({ model: 'sonnet-4.5' })} />
    ));
    const chip = getByTestId('sub-agent-banner-model-child-1');
    expect(chip).toHaveTextContent('sonnet-4.5');
  });

  it('renders the tool-count chip when the turn carries a tool_count', () => {
    const { getByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn({ tool_count: 4 })} />
    ));
    const chip = getByTestId('sub-agent-banner-tools-child-1');
    // Chip reads "N tools" in plural; singular form is accepted for 1.
    expect(chip).toHaveTextContent('4 tools');
  });

  it('singular tool-count label for tool_count === 1', () => {
    const { getByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn({ tool_count: 1 })} />
    ));
    expect(getByTestId('sub-agent-banner-tools-child-1')).toHaveTextContent('1 tool');
  });

  it('hides the model chip when model is undefined', () => {
    const { queryByTestId } = render(() => <SubAgentBanner turn={makeTurn()} />);
    expect(queryByTestId('sub-agent-banner-model-child-1')).not.toBeInTheDocument();
  });

  it('hides the tool-count chip when tool_count is undefined', () => {
    const { queryByTestId } = render(() => <SubAgentBanner turn={makeTurn()} />);
    expect(queryByTestId('sub-agent-banner-tools-child-1')).not.toBeInTheDocument();
  });

  it('chips render independently — model without tool_count is fine', () => {
    const { getByTestId, queryByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn({ model: 'haiku-3.5' })} />
    ));
    expect(getByTestId('sub-agent-banner-model-child-1')).toHaveTextContent('haiku-3.5');
    expect(queryByTestId('sub-agent-banner-tools-child-1')).not.toBeInTheDocument();
  });
});

describe('SubAgentBanner — state-chip popover (F-448 Phase 3)', () => {
  it('state chip is a <button> — not a passive <span>', () => {
    const { getByTestId } = render(() => <SubAgentBanner turn={makeTurn()} />);
    const chip = getByTestId('sub-agent-banner-state-child-1');
    expect(chip.tagName).toBe('BUTTON');
    // Button must be reachable by keyboard — Tab order.
    expect(chip.getAttribute('type')).toBe('button');
  });

  it('clicking the state chip opens the details popover', () => {
    const { getByTestId, queryByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn()} />
    ));
    expect(queryByTestId('sub-agent-banner-popover-child-1')).not.toBeInTheDocument();
    fireEvent.click(getByTestId('sub-agent-banner-state-child-1'));
    expect(getByTestId('sub-agent-banner-popover-child-1')).toBeInTheDocument();
  });

  it('state-chip click does not also toggle the header collapse (stopPropagation)', () => {
    const { getByTestId } = render(() => <SubAgentBanner turn={makeTurn()} />);
    const section = getByTestId('sub-agent-banner-child-1');
    // Starts collapsed.
    expect(section).toHaveAttribute('data-expanded', 'false');
    fireEvent.click(getByTestId('sub-agent-banner-state-child-1'));
    // Still collapsed — the chip's click must not bubble to the header.
    expect(section).toHaveAttribute('data-expanded', 'false');
  });

  it('popover renders child instance id, status, started-at, last-step summary', () => {
    const at = new Date();
    at.setHours(9, 5, 0, 0);
    const { getByTestId } = render(() => (
      <SubAgentBanner
        turn={makeTurn({
          child_instance_id: 'child-abc',
          status: 'running',
          started_at: at.getTime(),
          step_count: 2,
          last_step_summary: 'wrote foo.ts',
        })}
      />
    ));
    fireEvent.click(getByTestId('sub-agent-banner-state-child-abc'));
    const pop = getByTestId('sub-agent-banner-popover-child-abc');
    expect(pop).toHaveTextContent('child-abc');
    expect(pop).toHaveTextContent('running');
    expect(pop.textContent).toMatch(/\d{2}:\d{2}/);
    expect(pop).toHaveTextContent('wrote foo.ts');
  });

  it('popover dismisses on Escape', () => {
    const { getByTestId, queryByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn()} />
    ));
    fireEvent.click(getByTestId('sub-agent-banner-state-child-1'));
    expect(getByTestId('sub-agent-banner-popover-child-1')).toBeInTheDocument();
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(queryByTestId('sub-agent-banner-popover-child-1')).not.toBeInTheDocument();
  });

  it('popover dismisses on outside mousedown', () => {
    const { getByTestId, queryByTestId } = render(() => (
      <>
        <div data-testid="outside" />
        <SubAgentBanner turn={makeTurn()} />
      </>
    ));
    fireEvent.click(getByTestId('sub-agent-banner-state-child-1'));
    expect(getByTestId('sub-agent-banner-popover-child-1')).toBeInTheDocument();
    fireEvent.mouseDown(getByTestId('outside'));
    expect(queryByTestId('sub-agent-banner-popover-child-1')).not.toBeInTheDocument();
  });

  // F-696: APG menu pattern — opening the popover must move focus into it
  // (first interactive descendant) so AT can announce its contents and Tab
  // navigates the popover instead of skipping it.
  it('moves focus into the popover (first interactive child) on open', () => {
    const { getByTestId } = render(() => <SubAgentBanner turn={makeTurn()} />);
    fireEvent.click(getByTestId('sub-agent-banner-state-child-1'));
    const monitorBtn = getByTestId('sub-agent-banner-popover-monitor-child-1');
    expect(document.activeElement).toBe(monitorBtn);
  });

  it('"Open in Agent Monitor" link inside the popover calls onOpenInMonitor', () => {
    const open = vi.fn();
    const { getByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn()} onOpenInMonitor={open} />
    ));
    fireEvent.click(getByTestId('sub-agent-banner-state-child-1'));
    fireEvent.click(getByTestId('sub-agent-banner-popover-monitor-child-1'));
    expect(open).toHaveBeenCalledWith('child-1');
  });
});

// ---------------------------------------------------------------------------
// F-399: error-detail row
// ---------------------------------------------------------------------------

describe('SubAgentBanner — error state (F-399)', () => {
  it('renders the error-detail row when status is error and error_detail is set', () => {
    const { getByTestId } = render(() => (
      <SubAgentBanner
        turn={makeTurn({ status: 'error', error_detail: 'timeout after 30s' })}
      />
    ));
    const errorRow = getByTestId('sub-agent-banner-error-child-1');
    expect(errorRow).toBeInTheDocument();
    expect(errorRow).toHaveTextContent('timeout after 30s');
  });

  it('does not render the error-detail row when status is error but no error_detail', () => {
    const { queryByTestId } = render(() => (
      <SubAgentBanner turn={makeTurn({ status: 'error' })} />
    ));
    expect(queryByTestId('sub-agent-banner-error-child-1')).not.toBeInTheDocument();
  });

  it('does not render the error-detail row when status is done even if error_detail is set', () => {
    const { queryByTestId } = render(() => (
      <SubAgentBanner
        turn={makeTurn({ status: 'done', error_detail: 'stale error' })}
      />
    ));
    expect(queryByTestId('sub-agent-banner-error-child-1')).not.toBeInTheDocument();
  });
});

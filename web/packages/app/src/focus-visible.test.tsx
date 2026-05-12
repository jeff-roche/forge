import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render } from '@solidjs/testing-library';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import type { ProviderId } from '@forge/ipc';
import { ApprovalPrompt } from './components/ApprovalPrompt/ApprovalPrompt';
import { PaneHeader } from './routes/Session/PaneHeader';
import { setInvokeForTesting } from './lib/tauri';

// ---------------------------------------------------------------------------
// Regression for F-083: every interactive button in ApprovalPrompt and
// PaneHeader must (a) carry a `:focus-visible` rule using the ember-400
// outline pattern, and (b) be reachable via keyboard focus. JSDOM cannot
// compute `:focus-visible` styles, so we assert the rule on disk and
// confirm focusability against the live DOM.
// ---------------------------------------------------------------------------

const cssDir = resolve(__dirname);

const FOCUS_RULE = /:focus-visible\s*\{[^}]*outline:\s*2px\s+solid\s+var\(--color-ember-400\)[^}]*outline-offset:\s*2px/;

function readCss(relPath: string): string {
  return readFileSync(resolve(cssDir, relPath), 'utf-8');
}

function expectFocusVisibleRule(css: string, selector: string) {
  // Find a rule that targets `selector:focus-visible` (selector may share
  // the rule with siblings via comma; just require the selector and the
  // ember-400 outline pattern in the same rule body).
  const re = new RegExp(
    `${selector.replace(/[.\-]/g, (c) => `\\${c}`)}:focus-visible\\s*[,\\{]`,
  );
  expect(css, `expected ${selector}:focus-visible rule`).toMatch(re);
  expect(css, 'expected ember-400 outline pattern somewhere in file').toMatch(FOCUS_RULE);
}

afterEach(() => {
  cleanup();
  setInvokeForTesting(null);
});

// ---------------------------------------------------------------------------
// CSS rules on disk
// ---------------------------------------------------------------------------

describe('F-083 :focus-visible rules in CSS', () => {
  it('ApprovalPrompt.css covers .approval-prompt__btn and .approval-prompt__menu-item', () => {
    const css = readCss('components/ApprovalPrompt/ApprovalPrompt.css');
    expectFocusVisibleRule(css, '.approval-prompt__btn');
    expectFocusVisibleRule(css, '.approval-prompt__menu-item');
  });

  it('PaneHeader.css covers .pane-header__close', () => {
    const css = readCss('routes/Session/PaneHeader.css');
    expectFocusVisibleRule(css, '.pane-header__close');
  });
});

// ---------------------------------------------------------------------------
// Buttons are reachable via keyboard focus
// ---------------------------------------------------------------------------

describe('F-083 buttons are keyboard-focusable', () => {
  it('ApprovalPrompt buttons accept focus (reject, approve, dropdown, menu items)', () => {
    const container = document.createElement('div');
    container.tabIndex = 0;
    document.body.appendChild(container);

    const { getByTestId } = render(
      () => (
        <ApprovalPrompt
          toolCallId="tc-test"
          toolName="fs.edit"
          argsJson={JSON.stringify({ path: '/src/foo.ts', patch: '...' })}
          preview={{ description: 'Edit /src/foo.ts' }}
          containerRef={container}
          onApprove={vi.fn()}
          onReject={vi.fn()}
        />
      ),
      { container },
    );

    for (const id of ['reject-btn', 'approve-once-btn', 'approve-dropdown-btn']) {
      const btn = getByTestId(id) as HTMLButtonElement;
      btn.focus();
      expect(document.activeElement, `${id} should accept focus`).toBe(btn);
    }

    // Open dropdown to expose menu items, then verify each menu item is focusable.
    const toggle = getByTestId('approve-dropdown-btn') as HTMLButtonElement;
    toggle.click();
    for (const id of ['scope-once-btn', 'scope-file-btn', 'scope-pattern-btn', 'scope-tool-btn']) {
      const item = getByTestId(id) as HTMLButtonElement;
      item.focus();
      expect(document.activeElement, `${id} should accept focus`).toBe(item);
    }
  });

  it('PaneHeader close button accepts focus', () => {
    const { getByRole } = render(() => (
      <PaneHeader
        subject="hello"
        providerId={'anthropic' as ProviderId}
        providerLabel="anthropic"
        costLabel="0.00"
        onClose={vi.fn()}
      />
    ));
    const close = getByRole('button', { name: /close session window/i }) as HTMLButtonElement;
    close.focus();
    expect(document.activeElement).toBe(close);
  });
});

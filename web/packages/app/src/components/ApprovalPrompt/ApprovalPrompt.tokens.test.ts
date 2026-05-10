import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// ApprovalPrompt token adherence (F-090):
// voice-terminology.md §9 agent rule #1 — "Use design tokens, never raw hex
// or pixel values." Two sub-elements previously declared raw values that
// did not match any token in docs/design/typography.md §4:
//
//   .approval-prompt__pattern-label   font-size: 10px; letter-spacing: 0.08em;
//   .approval-prompt__dropdown-toggle font-size: 9px;   (override of parent 11px)
//
// After F-090:
//   - `.approval-prompt__pattern-label` matches `mono-xs` (9px / 0.3em /
//     uppercase) per typography.md §4.
//   - `.approval-prompt__dropdown-toggle` removes the unjustified font-size
//     override and inherits 11px from its parent button.
//
// JSDOM cannot reliably resolve var() / inheritance at the source level, so
// assert the source-string contract on the CSS rule blocks (matches the
// F-084/F-085 pattern in ApprovalPrompt.css.test.ts).

const cssPath = resolve(__dirname, 'ApprovalPrompt.css');
const css = readFileSync(cssPath, 'utf-8');

function ruleBody(selector: string): string {
  const at = css.indexOf(selector);
  if (at < 0) throw new Error(`selector not found in ApprovalPrompt.css: ${selector}`);
  const open = css.indexOf('{', at);
  const close = css.indexOf('}', open);
  if (open < 0 || close < 0) {
    throw new Error(`malformed rule block for selector: ${selector}`);
  }
  return css.slice(open + 1, close).trim();
}

function hasDecl(body: string, name: string, value: string): boolean {
  const normalised = body.replace(/\s+/g, ' ');
  return normalised.includes(`${name}: ${value};`);
}

describe('ApprovalPrompt — pattern-label matches mono-xs (F-090)', () => {
  const body = ruleBody('.approval-prompt__pattern-label');

  it('font-size resolves to mono-xs (9px) via the typography token', () => {
    // After F-823 the raw `font-size: 9px` migrated to the
    // `--type-mono-xs` typography token. Asserting on the token name keeps
    // the F-090 invariant — pattern-label uses the mono-xs scale, not a
    // raw 10px — without re-encoding the px value here.
    expect(hasDecl(body, 'font-size', 'var(--type-mono-xs)')).toBe(true);
    expect(body).not.toMatch(/font-size:\s*10px\b/);
    expect(body).not.toMatch(/font-size:\s*9px\b/);
  });

  it('letter-spacing: 0.3em (mono-xs)', () => {
    expect(hasDecl(body, 'letter-spacing', '0.3em')).toBe(true);
    expect(body).not.toMatch(/letter-spacing:\s*0\.08em\b/);
  });

  it('text-transform: uppercase (mono-xs convention)', () => {
    expect(hasDecl(body, 'text-transform', 'uppercase')).toBe(true);
  });
});

describe('ApprovalPrompt — dropdown-toggle has no font-size override (F-090)', () => {
  const body = ruleBody('.approval-prompt__dropdown-toggle');

  it('does not override font-size — inherits 11px from .approval-prompt__btn', () => {
    expect(body).not.toMatch(/font-size:/);
  });
});

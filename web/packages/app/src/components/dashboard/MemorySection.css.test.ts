import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// F-685 — MemorySection error state must carry the standard error treatment
// from `docs/design/ai-patterns.md` §"Interaction states":
//   "Error: ember-400 border on the affected surface PLUS a persistent toast
//    carrying the verbatim technical identifier."
//
// The runtime suite (MemorySection.test.tsx) covers the toast invocation and
// the role/testid contract. This file pins the CSS rule itself so the border
// cannot regress silently — jsdom does not compute styles from imported
// stylesheets, so we read the source CSS and assert the declarations
// directly. Mirrors the pattern in ApprovalPrompt.css.test.ts.

const cssPath = resolve(__dirname, 'MemorySection.css');
const css = readFileSync(cssPath, 'utf-8');

function ruleBody(selector: string): string {
  const at = css.indexOf(selector);
  if (at < 0) throw new Error(`selector not found in MemorySection.css: ${selector}`);
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

describe('MemorySection error surface (F-685)', () => {
  const body = ruleBody('.memory-section__error');

  it('carries the ember-400 left border per ai-patterns Interaction states', () => {
    expect(hasDecl(body, 'border-left', '3px solid var(--color-ember-400)')).toBe(true);
  });

  it('reads as an error surface — padded and tinted, not a stray paragraph', () => {
    // Padding + background tint are what make the border read as "the
    // affected surface" rather than a floating accent stripe.
    expect(body).toMatch(/padding\s*:/);
    expect(body).toMatch(/background\s*:/);
  });
});

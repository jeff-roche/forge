import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// WhitelistedPill token adherence (F-090):
// voice-terminology.md §9 agent rule #1 — "Use design tokens, never raw hex
// or pixel values." `.whitelisted-pill` previously declared
//   padding: 2px var(--sp-2);   // 2px is not in the spacing scale
//   font-size: 10px;            // 10px is not in the mono scale
// After F-090, padding must use `var(--sp-1) var(--sp-2)` (4px vertical) and
// font-size must match `mono-xs` (9px) per the badge convention in
// docs/design/typography.md §4. JSDOM cannot reliably resolve var() at the
// source level, so assert the source-string contract on the CSS rule block
// (matches the F-084/F-085 pattern).

const cssPath = resolve(__dirname, 'WhitelistedPill.css');
const css = readFileSync(cssPath, 'utf-8');

function ruleBody(selector: string): string {
  const at = css.indexOf(selector);
  if (at < 0) throw new Error(`selector not found in WhitelistedPill.css: ${selector}`);
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

describe('WhitelistedPill — design-token adherence (F-090)', () => {
  // Anchor on `.whitelisted-pill {` to disambiguate from the wrapper class
  // (`.whitelisted-pill-wrapper`) and from descendant/state selectors
  // (`.whitelisted-pill:hover`, `.whitelisted-pill__dot`, etc.) that all
  // share the same prefix.
  const body = ruleBody('.whitelisted-pill {');

  it('padding uses spacing tokens (no raw 2px)', () => {
    expect(hasDecl(body, 'padding', 'var(--sp-1) var(--sp-2)')).toBe(true);
    expect(body).not.toMatch(/padding:\s*2px\b/);
  });

  it('font-size resolves to mono-xs (9px) via the typography token, not raw 10px', () => {
    // F-823 migrated the raw `font-size: 9px` to the `--type-mono-xs`
    // typography token. The F-090 invariant is preserved (mono-xs scale,
    // not 10px); we just assert on the token name now.
    expect(hasDecl(body, 'font-size', 'var(--type-mono-xs)')).toBe(true);
    expect(body).not.toMatch(/font-size:\s*10px\b/);
    expect(body).not.toMatch(/font-size:\s*9px\b/);
  });
});

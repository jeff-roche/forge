import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// F-720 SessionsPanel token adherence. voice-terminology.md §9 agent rule
// #1 — "Use design tokens, never raw hex or pixel values." JSDOM can't
// resolve var() at the source level, so assert the source-string contract
// directly. Mirrors the F-090 / F-084 / F-085 css-source-string pattern.

const cssPath = resolve(__dirname, 'SessionsPanel.css');
const css = readFileSync(cssPath, 'utf-8');

function ruleBody(selector: string): string {
  const at = css.indexOf(selector);
  if (at < 0) throw new Error(`selector not found in SessionsPanel.css: ${selector}`);
  const open = css.indexOf('{', at);
  const close = css.indexOf('}', open);
  if (open < 0 || close < 0) {
    throw new Error(`malformed rule block for selector: ${selector}`);
  }
  return css.slice(open + 1, close).trim();
}

describe('SessionsPanel — F-720 token adherence', () => {
  it('card chrome uses surface/border/radius tokens', () => {
    const body = ruleBody('.sessions ');
    expect(body).toMatch(/background:\s*var\(--color-surface-1\)/);
    expect(body).toMatch(/border:\s*1px solid var\(--color-border-1\)/);
    expect(body).toMatch(/border-radius:\s*var\(--r-sm\)/);
    expect(body).toMatch(/padding:\s*var\(--sp-5\)/);
  });

  it('header label uses the mono-xs uppercase token treatment', () => {
    const body = ruleBody('.sessions__label');
    expect(body).toMatch(/font-family:\s*var\(--font-mono\)/);
    expect(body).toMatch(/font-size:\s*var\(--type-mono-xs\)/);
    expect(body).toMatch(/letter-spacing:\s*0\.2em/);
    expect(body).toMatch(/color:\s*var\(--color-text-tertiary\)/);
  });

  it('chip filter container is wrapped in surface-3 chrome', () => {
    const body = ruleBody('.sessions__chips');
    expect(body).toMatch(/background:\s*var\(--color-surface-3\)/);
  });

  it('selected chip lifts to surface-2 and text-primary', () => {
    const body = ruleBody('.sessions__chip--selected');
    expect(body).toMatch(/background:\s*var\(--color-surface-2\)/);
    expect(body).toMatch(/color:\s*var\(--color-text-primary\)/);
  });

  it('rows separate with a 1px border-bottom in border-1', () => {
    const body = ruleBody('.sessions__row');
    expect(body).toMatch(/border-bottom:\s*1px solid var\(--color-border-1\)/);
  });

  it('row hover lifts to surface-2', () => {
    const body = ruleBody('.sessions__row:hover');
    expect(body).toMatch(/background:\s*var\(--color-surface-2\)/);
  });

  it('row focus-visible uses the ember outline ring', () => {
    const body = ruleBody('.sessions__row:focus-visible');
    expect(body).toMatch(/outline:\s*2px solid var\(--color-ember-400\)/);
  });

  it('contains no raw hex literal anywhere in the file', () => {
    // Permits hex inside `url(...)` data references; the SessionsPanel
    // doesn't use any, so the simple test is sufficient.
    expect(css).not.toMatch(/#[0-9a-fA-F]{3,8}\b/);
  });

  it('does not reintroduce the dropped session-card BEM block', () => {
    // F-720 swapped the card-grid layout for a row list — references to
    // the old `.session-card*` selectors would indicate a half-migrated
    // file.
    expect(css).not.toMatch(/\.session-card[\s_-]/);
  });
});

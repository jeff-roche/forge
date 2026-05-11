import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// F-719: pin the 12-column grid contract from DESIGN.md §Layout grid.
// jsdom doesn't compute styles from imported stylesheets, so we read the
// source CSS and assert the declarations directly (matching the F-085 /
// F-090 pattern used elsewhere in this tree).

const cssPath = resolve(__dirname, 'Dashboard.css');
const css = readFileSync(cssPath, 'utf-8');

function ruleBody(selector: string): string {
  const at = css.indexOf(selector + ' {');
  if (at < 0) throw new Error(`selector not found in Dashboard.css: ${selector}`);
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

describe('Dashboard.css — 12-column grid (F-719)', () => {
  it('.dashboard__grid uses display: grid', () => {
    expect(hasDecl(ruleBody('.dashboard__grid'), 'display', 'grid')).toBe(true);
  });

  it('.dashboard__grid uses a repeat(12, 1fr) track', () => {
    expect(
      hasDecl(ruleBody('.dashboard__grid'), 'grid-template-columns', 'repeat(12, 1fr)'),
    ).toBe(true);
  });

  it('.dashboard__grid uses var(--sp-6) as the gap token', () => {
    expect(hasDecl(ruleBody('.dashboard__grid'), 'gap', 'var(--sp-6)')).toBe(true);
  });

  it.each([4, 6, 8, 12])('.dashboard__slot--col-%i resolves to grid-column: span %i', (n) => {
    expect(
      hasDecl(ruleBody(`.dashboard__slot--col-${n}`), 'grid-column', `span ${n}`),
    ).toBe(true);
  });
});

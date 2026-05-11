import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// F-740 — UsageCard spark live-dot pulse.
//
// `DESIGN.md §Spark chart` calls for the latest data point to read as the
// current day. F-740 layers the shared `forge-pulse` keyframe on top so
// the dot reads as *live* even when the series is flat. The keyframe
// itself lives in @forge/design (asserted in StatusPill.css.test.ts); the
// app-side test pins the selector + reduced-motion suppression so the
// animation hook cannot regress silently.

const cssPath = resolve(__dirname, 'UsageCard.css');
const css = readFileSync(cssPath, 'utf-8');

function ruleBody(selector: string): string {
  const at = css.indexOf(selector);
  if (at < 0) throw new Error(`selector not found in UsageCard.css: ${selector}`);
  const open = css.indexOf('{', at);
  const close = css.indexOf('}', open);
  if (open < 0 || close < 0) {
    throw new Error(`malformed rule block for selector: ${selector}`);
  }
  return css.slice(open + 1, close).trim();
}

describe('UsageCard spark live-dot pulse (F-740)', () => {
  it('binds the live dot to the shared forge-pulse keyframe', () => {
    const body = ruleBody('.usage-card__spark-live-dot');
    expect(body).toMatch(/animation:\s*forge-pulse\s+1\.6s\s+ease-in-out\s+infinite/);
  });

  it('suppresses the pulse under prefers-reduced-motion', () => {
    expect(css).toMatch(
      /@media\s*\(prefers-reduced-motion:\s*reduce\)\s*\{[\s\S]*?\.usage-card__spark-live-dot[\s\S]*?animation:\s*none/,
    );
  });
});

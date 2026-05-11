import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// F-740 — `forge-pulse` keyframe + selector wiring.
//
// DESIGN.md §Status pill specifies a shared 1.6s, 1 → 0.4 → 1 opacity pulse
// applied to the `streaming` and `awaiting` variant dots. Static dot for
// `done`, no dot at all for `idle`, and the glow-only treatment for
// `ready` / `auth` — none of those non-live variants animate.
//
// jsdom does not resolve @keyframes from imported stylesheets, so we read
// the source CSS and assert on declarations directly. Mirrors the pattern
// in MemorySection.css.test.ts and ApprovalPrompt.css.test.ts.

const cssPath = resolve(__dirname, 'forge-button.css');
const css = readFileSync(cssPath, 'utf-8');

function ruleBody(selector: string): string {
  // Match `selector {` (with optional whitespace) so we skip selector-name
  // mentions inside comments. The lookahead pins the `{` immediately after
  // the selector, not somewhere downstream (e.g. an unrelated @keyframes
  // opener that happens to follow the selector mention).
  const re = new RegExp(
    `${selector.replace(/[.*+?^${}()|[\\]\\\\]/g, '\\$&')}\\s*\\{`,
  );
  const m = re.exec(css);
  if (!m) throw new Error(`selector not found in forge-button.css: ${selector}`);
  const open = m.index + m[0].length - 1;
  const close = css.indexOf('}', open);
  if (close < 0) {
    throw new Error(`malformed rule block for selector: ${selector}`);
  }
  return css.slice(open + 1, close).trim();
}

describe('forge-pulse primitive (F-740)', () => {
  it('declares the @keyframes block with 1 → 0.4 → 1 opacity per DESIGN.md', () => {
    expect(css).toMatch(/@keyframes\s+forge-pulse\s*\{[^}]*opacity:\s*1[^}]*\}/);
    expect(css).toMatch(/@keyframes\s+forge-pulse[\s\S]*?50%[\s\S]*?opacity:\s*0\.4/);
  });

  it('exposes the `.forge-pulse-dot` utility bound to the keyframe', () => {
    const body = ruleBody('.forge-pulse-dot');
    expect(body).toMatch(/animation:\s*forge-pulse\s+1\.6s\s+ease-in-out\s+infinite/);
  });

  it('suppresses the utility under prefers-reduced-motion', () => {
    // Match the media query wrapping a `.forge-pulse-dot { animation: none }`
    // block — the keyframe-level suppression DESIGN.md asks for.
    expect(css).toMatch(
      /@media\s*\(prefers-reduced-motion:\s*reduce\)\s*\{[\s\S]*?\.forge-pulse-dot[\s\S]*?animation:\s*none/,
    );
  });
});

describe('StatusPill pulse wiring (F-740)', () => {
  it('binds the streaming dot to the forge-pulse keyframe', () => {
    const selector =
      '.forge-status-pill--streaming .forge-status-pill__dot,\n  .forge-status-pill--awaiting .forge-status-pill__dot';
    // The combined selector may render with either ordering / whitespace; we
    // search for the canonical block in source.
    expect(css).toContain('.forge-status-pill--streaming .forge-status-pill__dot');
    expect(css).toContain('.forge-status-pill--awaiting .forge-status-pill__dot');
    // Sanity: there's an `animation: forge-pulse` declaration adjacent.
    const at = css.indexOf('.forge-status-pill--streaming .forge-status-pill__dot');
    const close = css.indexOf('}', at);
    const body = css.slice(at, close);
    expect(body).toMatch(/animation:\s*forge-pulse\s+1\.6s\s+ease-in-out\s+infinite/);
    // Touch the selector argument so the helper-typed binding isn't unused.
    void selector;
  });

  it('suppresses the StatusPill pulse under prefers-reduced-motion', () => {
    // Reduce-motion block must reference both live variants.
    const reduceBlocks = css.match(
      /@media\s*\(prefers-reduced-motion:\s*reduce\)\s*\{[\s\S]*?\}\s*\}/g,
    );
    expect(reduceBlocks).not.toBeNull();
    const joined = (reduceBlocks ?? []).join('\n');
    expect(joined).toContain('.forge-status-pill--streaming .forge-status-pill__dot');
    expect(joined).toContain('.forge-status-pill--awaiting .forge-status-pill__dot');
    expect(joined).toMatch(/animation:\s*none/);
  });

  it('does NOT bind the pulse keyframe to the static variants', () => {
    // `done` / `idle` / `ready` / `auth` must not appear in any animation
    // declaration anywhere in the stylesheet.
    for (const variant of ['done', 'idle', 'ready', 'auth']) {
      const re = new RegExp(
        `\\.forge-status-pill--${variant}\\s+\\.forge-status-pill__dot[^}]*animation`,
      );
      expect(css).not.toMatch(re);
    }
  });
});

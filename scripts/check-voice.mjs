#!/usr/bin/env node
// Voice-terminology drift check (F-699 follow-up #820, item 6).
//
// Greps user-facing TSX/MD/MDX strings for vocabulary that violates
// `docs/design/voice-terminology.md`. The canonical vocabulary table lives
// there; this script is a tripwire, not the source of truth.
//
// Initial mode: INFORMATIONAL. The script always exits 0. Findings print
// to stdout in `file:line:col — <offender> (<reason>)` form so a CI run
// can surface them without blocking merges. Promote to a gating check
// (exit non-zero on findings) once the corpus is clean.
//
// Scope:
//   - `.tsx`, `.ts`, `.md`, `.mdx` under `web/packages/app/src` and `docs/`.
//   - Skip generated artifacts (`web/packages/ipc/src/generated/`),
//     `node_modules`, `dist`, and the voice-terminology doc itself
//     (it lists the forbidden words verbatim by design).
//
// Heuristic precision is deliberately low — the rule prefers false
// positives that prompt a human review over false negatives that let
// drift sneak in. Exclusions live below per offender.

import { readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..');

const scanRoots = [
  resolve(repoRoot, 'web/packages/app/src'),
  resolve(repoRoot, 'web/packages/design/src'),
  resolve(repoRoot, 'docs/design'),
  resolve(repoRoot, 'docs/frontend'),
  resolve(repoRoot, 'docs/ui-specs'),
  resolve(repoRoot, 'docs/product'),
];

const skipPathSegments = [
  'node_modules',
  'dist',
  'generated',
];

const skipFileBasenames = new Set([
  // The vocabulary doc itself lists every forbidden word verbatim. Scanning
  // it would yield only self-references.
  'voice-terminology.md',
]);

/** Recursively yield absolute paths of source files under `dir`. */
function* walk(dir) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    if (entry.name.startsWith('.')) continue;
    if (skipPathSegments.includes(entry.name)) continue;
    const full = resolve(dir, entry.name);
    if (entry.isDirectory()) {
      yield* walk(full);
    } else if (entry.isFile()) {
      if (skipFileBasenames.has(entry.name)) continue;
      const ext = entry.name.match(/\.([a-z]+)$/i)?.[1];
      if (ext && ['tsx', 'ts', 'md', 'mdx'].includes(ext.toLowerCase())) {
        yield full;
      }
    }
  }
}

/**
 * Vocabulary rules. Each entry has:
 *   - `pattern` — RegExp (must use `g` flag)
 *   - `reason`  — human-readable rationale shown next to the offender
 *   - `allow`   — optional predicate `(line, file) => boolean`; truthy
 *                 means the match is intentional and should be skipped
 *
 * The patterns intentionally match prose, not identifiers — matches
 * inside imports, IPC type names, or symbol literals get filtered by the
 * `allow` predicates per rule.
 */
const rules = [
  {
    name: 'model-as-ui-noun',
    // "AI model" / "the model" — UI vocabulary forbids these (use
    // "AI provider" / "provider"). Identifier surfaces (`provider.model`,
    // `model_id`, `modelId`) are data, not UI copy, and stay allowed.
    pattern: /\b(?:AI model|the model|LLM)\b/g,
    reason: 'voice-terminology: use "AI provider" / "provider" (not "AI model" / "LLM" / "the model")',
    allow: () => false,
  },
  {
    name: 'conversation-as-ui-noun',
    // "conversation" — Forge UI vocabulary uses "session" or "chat". The
    // word is fine inside long-form design docs that *describe* the
    // conversational UI; we narrow to TSX strings (UI copy) only.
    pattern: /\bconversations?\b/gi,
    reason: 'voice-terminology: use "session" or "chat" (not "conversation") in UI surfaces',
    allow: (_line, file) => !file.endsWith('.tsx'),
  },
  {
    name: 'plugin-server',
    // "plugin server" / "tool server" — voice doc reserves "MCP server".
    pattern: /\b(?:plugin server|tool server)\b/gi,
    reason: 'voice-terminology: use "MCP server" (not "plugin server" / "tool server")',
    allow: () => false,
  },
  {
    name: 'helper-or-assistant-for-subagent',
    // "helper" / "child AI" / "assistant" as a synonym for sub-agent.
    // "assistant" is broad and frequently legitimate (e.g. ARIA labels,
    // accessibility text, agent-role enum). We restrict to phrases that
    // clearly substitute for "sub-agent".
    pattern: /\b(?:child AI|helper agent|AI assistant)\b/gi,
    reason: 'voice-terminology: use "sub-agent" (not "child AI" / "helper agent" / "AI assistant")',
    allow: () => false,
  },
];

/** 1-based (line, col) for an offset in `source`. */
function locate(source, offset) {
  let line = 1;
  let col = 1;
  for (let i = 0; i < offset; i += 1) {
    if (source[i] === '\n') {
      line += 1;
      col = 1;
    } else {
      col += 1;
    }
  }
  return { line, col };
}

const findings = [];

for (const root of scanRoots) {
  let exists = false;
  try {
    exists = statSync(root).isDirectory();
  } catch {
    exists = false;
  }
  if (!exists) continue;
  for (const file of walk(root)) {
    const rel = relative(repoRoot, file).split('\\').join('/');
    const source = readFileSync(file, 'utf-8');
    for (const rule of rules) {
      rule.pattern.lastIndex = 0;
      let m;
      while ((m = rule.pattern.exec(source)) !== null) {
        const { line, col } = locate(source, m.index);
        const lineText = source.split('\n')[line - 1] ?? '';
        if (rule.allow(lineText, rel)) continue;
        findings.push({ file: rel, line, col, match: m[0], reason: rule.reason, rule: rule.name });
      }
    }
  }
}

if (findings.length === 0) {
  console.log('ok: no voice-terminology drift detected');
  process.exit(0);
}

// Informational mode: print and exit 0. The CI step that calls us is
// `continue-on-error: true` until the corpus is clean; flip both this
// exit and the CI step in a follow-up once the count drops to 0.
console.log(`voice-terminology findings (${findings.length} — informational, non-gating):`);
for (const f of findings) {
  console.log(`  ${f.file}:${f.line}:${f.col} — "${f.match}" (${f.rule}: ${f.reason})`);
}
process.exit(0);

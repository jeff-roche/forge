#!/usr/bin/env node
// Tests for check-raw-buttons.mjs.
//
// The rule is drafted DISABLED (F-398): it is not wired into `just check-web`
// and CI does not call it. These tests prove the rule catches raw `<button>`
// on a TSX fixture and honors the allowlist of row/card-as-button sites.
// They run only when explicitly invoked: `node scripts/check-raw-buttons.test.mjs`.

import { mkdtempSync, writeFileSync, mkdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';
import { dirname } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const rulePath = resolve(here, 'check-raw-buttons.mjs');

/** Run a fixture through the rule's exported `scanTsxSources` and return findings. */
async function runRule({ files, allowlist }) {
  const { scanTsxSources } = await import(pathToFileURL(rulePath).href);
  const root = mkdtempSync(resolve(tmpdir(), 'forge-raw-button-'));
  try {
    for (const [relPath, body] of Object.entries(files)) {
      const abs = resolve(root, relPath);
      mkdirSync(dirname(abs), { recursive: true });
      writeFileSync(abs, body, 'utf-8');
    }
    return scanTsxSources({ root, allowlist: allowlist ?? [] });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

const cases = [];
function test(name, fn) { cases.push({ name, fn }); }

test('flags a raw <button> in a TSX file', async () => {
  const findings = await runRule({
    files: {
      'web/packages/app/src/components/Foo.tsx':
        'export const Foo = () => <button type="button">CLICK</button>;\n',
    },
  });
  if (findings.length !== 1) throw new Error(`expected 1 finding, got ${findings.length}: ${JSON.stringify(findings)}`);
  if (!findings[0].file.endsWith('Foo.tsx')) throw new Error(`wrong file: ${findings[0].file}`);
  if (findings[0].line !== 1) throw new Error(`wrong line: ${findings[0].line}`);
});

test('ignores a Button primitive import from @forge/design', async () => {
  const findings = await runRule({
    files: {
      'web/packages/app/src/components/Foo.tsx':
        "import { Button } from '@forge/design';\nexport const Foo = () => <Button variant=\"primary\">CLICK</Button>;\n",
    },
  });
  if (findings.length !== 0) throw new Error(`expected 0 findings, got ${findings.length}: ${JSON.stringify(findings)}`);
});

test('skips files on the allowlist (row/card-as-button sites)', async () => {
  const findings = await runRule({
    files: {
      'web/packages/app/src/routes/AgentMonitor.tsx':
        'export const Row = () => <button type="button" class="row">...</button>;\n',
    },
    allowlist: ['web/packages/app/src/routes/AgentMonitor.tsx'],
  });
  if (findings.length !== 0) throw new Error(`expected allowlist to suppress, got ${findings.length}: ${JSON.stringify(findings)}`);
});

test('flags multi-line <button> blocks with attrs spanning lines', async () => {
  const findings = await runRule({
    files: {
      'web/packages/app/src/components/Multi.tsx':
        'export const Multi = () => (\n  <button\n    type="button"\n    class="x"\n  >GO</button>\n);\n',
    },
  });
  if (findings.length !== 1) throw new Error(`expected 1 finding, got ${findings.length}`);
  if (findings[0].line !== 2) throw new Error(`expected line 2 (opening <button), got ${findings[0].line}`);
});

test('does not false-positive on a string containing the word button', async () => {
  const findings = await runRule({
    files: {
      'web/packages/app/src/components/Label.tsx':
        'export const Label = () => <span title="this is a button label">x</span>;\n',
    },
  });
  if (findings.length !== 0) throw new Error(`expected 0 findings, got ${findings.length}: ${JSON.stringify(findings)}`);
});

test('skips .test.tsx files (tests legitimately reference raw <button>)', async () => {
  const findings = await runRule({
    files: {
      'web/packages/app/src/components/Foo.test.tsx':
        "it('was a raw button', () => { render(() => <button>x</button>); });\n",
    },
  });
  if (findings.length !== 0) throw new Error(`expected 0 findings, got ${findings.length}: ${JSON.stringify(findings)}`);
});

test('skips // line comments that mention <button>', async () => {
  const findings = await runRule({
    files: {
      'web/packages/app/src/components/Foo.tsx':
        '// state chip is a <button> that wraps the popover\nexport const Foo = () => <span>x</span>;\n',
    },
  });
  if (findings.length !== 0) throw new Error(`expected 0 findings, got ${findings.length}: ${JSON.stringify(findings)}`);
});

let failed = 0;
for (const { name, fn } of cases) {
  try {
    await fn();
    console.log(`ok: ${name}`);
  } catch (err) {
    failed += 1;
    console.error(`FAIL: ${name}`);
    console.error(`  ${err.message}`);
  }
}

if (failed > 0) {
  console.error(`\n${failed} test(s) failed`);
  process.exit(1);
}
console.log(`\n${cases.length} tests passed`);

#!/usr/bin/env node
// Wraps `@google/design.md lint` and drops contrast warnings at or above the
// Forge floor. The locked exception is the status bar (#fff on Ember 400 =
// 3.37:1); the brand color drives that ratio across primary buttons too.
// Anything stricter than the floor still warns; anything else (parse errors,
// unknown sections, duplicates) flows through untouched.

import { spawnSync } from "node:child_process";

const FLOOR = 3.37;
const RATIO_RE =
  /contrast ratio (\d+\.\d+):1, below WCAG AA minimum of \d+(?:\.\d+)?:1\./;

const result = spawnSync(
  "npx",
  ["--yes", "@google/design.md", "lint", "--format=json", "DESIGN.md"],
  { encoding: "utf8" },
);

if (result.error) {
  console.error(result.error.message);
  process.exit(2);
}

let report;
try {
  report = JSON.parse(result.stdout);
} catch {
  process.stdout.write(result.stdout);
  process.stderr.write(result.stderr);
  process.exit(result.status ?? 1);
}

const kept = [];
let suppressed = 0;
for (const f of report.findings) {
  const m = f.message?.match(RATIO_RE);
  if (m && f.severity === "warning" && parseFloat(m[1]) >= FLOOR) {
    suppressed += 1;
    continue;
  }
  kept.push(f);
}

const errors = kept.filter((f) => f.severity === "error").length;
const warnings = kept.filter((f) => f.severity === "warning").length;
const infos = kept.filter((f) => f.severity === "info").length;

console.log(
  JSON.stringify(
    {
      findings: kept,
      summary: { errors, warnings, infos },
      forge: { contrast_floor: FLOOR, contrast_warnings_suppressed: suppressed },
    },
    null,
    2,
  ),
);

process.exit(errors > 0 ? 1 : 0);

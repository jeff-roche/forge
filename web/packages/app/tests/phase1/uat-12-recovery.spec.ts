// UAT-12 — Recovery from provider or daemon loss.
// Plan: docs/testing/phase1-uat.md §UAT-12

import { test } from '@playwright/test';

test.describe('UAT-12 — recovery', () => {
  test.skip('Variant B — forged crash mid-stream (requires tauri-driver)', async () => {
    // 1. Start session with MockProvider.
    // 2. kill forged while stream is mid-flight.
    // 3. Assert Session window surfaces a disconnect indicator.
    // 4. Return to Dashboard; state reads stopped.
  });
});

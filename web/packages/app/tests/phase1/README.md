# Phase 1 Playwright UATs

Automated verification harness for the Phase 1 UAT plan in
`docs/testing/phase1-uat.md`.

## Layout

```
tests/phase1/
  fixtures/
    tauri-mock.ts        # installs mock window.__TAURI_INTERNALS__ before app boot
    events.ts            # factories for UserMessage / AssistantDelta / ToolCall* payloads
  uat-01a-*.spec.ts      # outcome gate (skipped — needs forged bridge fixture)
  uat-02-*.spec.ts       # Dashboard sessions list — runs under mocked IPC
  uat-04-*.spec.ts       # Session window lifecycle — runs under mocked IPC
  uat-05-*.spec.ts       # Chat pane streaming & composer
  uat-06-*.spec.ts       # Tool call card
  uat-07-*.spec.ts       # Four-scope approval
  uat-08-*.spec.ts       # fs.write / fs.edit (skipped — needs forged bridge)
  uat-11-*.spec.ts       # Multi-session isolation (skipped)
  uat-12-*.spec.ts       # Recovery variants (skipped)
```

UAT-09, UAT-10, and UAT-13 are disk-state cases driven by the bash harness at
`docs/testing/phase1-uat.sh` — not by Playwright.

## Mocking model

Playwright runs the Solid app against the Vite dev server (`baseURL
http://127.0.0.1:5173`). Before each page load, `installTauriMock(page)`
injects a `window.__TAURI_INTERNALS__` polyfill that:

- dispatches `invoke(cmd, args)` to test-registered handlers;
- records every `invoke` call for assertions;
- routes `plugin:event|listen` through `transformCallback` so emitted events
  reach `@tauri-apps/api/event`'s `listen()`.

In specs, register responses with `tauri.onInvoke('session_list', async () => […])`
and push events with `tauri.emit('session:event', payload)`. See
`uat-04-session-window-lifecycle.spec.ts` and `uat-05-chat-pane-streaming.spec.ts`
for worked examples.

## Running

```bash
# From web/packages/app/
pnpm exec playwright install      # first-time only
pnpm run test:e2e                 # headless
pnpm run test:e2e:ui              # Playwright UI for debugging
pnpm run test:e2e -- uat-02       # filter to a single UAT
```

## What's skipped and why

Any spec marked `test.skip(...)` carries a reason string:

| Reason | Fix |
|---|---|
| `requires forged bridge fixture` | Build a companion fixture that spawns `forged` with `FORGE_MOCK_SEQUENCE_FILE`, captures the socket path, and forwards Tauri `invoke` calls through a real UDS. |
| `requires tauri-driver` | Set up `tauri-driver` + `webdriverio` for real-shell tests; those specs should move to a sibling `tests/phase1-shell/` suite. |
| `selector pending` | Tester must add a `data-testid` or finalize a selector convention for the relevant UI element before the assertion becomes stable. |

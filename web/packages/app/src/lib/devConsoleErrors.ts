// Catch-all listeners for unhandled JS errors and promise rejections.
// In production we want crashes to surface in error-monitoring tools the
// shell may add later; in dev we just want them in the devtools console
// where the user is already looking. The two-listener pair below covers
// every escape hatch from the per-callsite `.catch()` handlers that
// Forge code already installs:
//
//   - `window.error`           : uncaught synchronous exceptions, missing
//                                 resources, and module-load failures.
//   - `unhandledrejection`     : promises that reject with no `.catch()`.
//
// Both fire BEFORE the browser's default "Uncaught" devtools message, so
// the supplemental log lets us prefix consistently and include extra
// context (event source, filename, line) that the default message drops.
//
// Called once from `index.tsx`; idempotent via a module-level flag in
// case HMR re-evaluates the module.

let installed = false;

export function installDevConsoleErrorListeners(): void {
  if (installed) return;
  if (!import.meta.env.DEV) return;
  installed = true;

  window.addEventListener('error', (event) => {
    const where =
      event.filename && event.lineno
        ? `${event.filename}:${event.lineno}:${event.colno ?? 0}`
        : '<unknown>';
    // Use the Error object when available so the devtools console renders
    // a clickable stack trace; fall back to the bare message otherwise.
    const detail = event.error ?? event.message ?? '<no detail>';
    // eslint-disable-next-line no-console
    console.error(`[uncaught ${where}]`, detail);
  });

  window.addEventListener('unhandledrejection', (event) => {
    // eslint-disable-next-line no-console
    console.error('[unhandled promise rejection]', event.reason);
  });
}

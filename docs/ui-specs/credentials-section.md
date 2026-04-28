# Credentials Section

> Dashboard section ([F-588](https://github.com/forge-ide/forge/issues/606)) — per-provider credential management with rotation confirmation and a security-conscious form contract.

---

## Purpose

Let the user store, rotate, and remove provider credentials without leaving the Dashboard. The section is the authoritative surface for credential state — every other view (the providers grid, the first-run banner) reflects `has_credential` as a hint and links here when remediation is needed.

## Where

`<CredentialsSection>` mounts inside the Dashboard root, anchored at id `credentials-section` so the first-run banner can deep-link to it. Component path: `web/packages/app/src/components/dashboard/CredentialsSection.tsx`.

## Size

Fills the dashboard column width. Single column of rows — one row per provider in `CREDENTIAL_PROVIDERS`. No internal scrolling; the row count is small and bounded.

## Structure

```
┌─ CREDENTIALS ──────────────────────────────────────────────┐
│ ✓ Anthropic   ANTHROPIC_API_KEY                  [LOGOUT]  │
│ Replace key  [••••••••••••]                      [ROTATE]  │
│                                                            │
│ ⚠ OpenAI      OPENAI_API_KEY                               │
│ Add key      [           ]                       [STORE]   │
└────────────────────────────────────────────────────────────┘
```

### Row anatomy

- **Indicator + label row.** `✓` (stored) or `⚠` (missing) icon, provider display name, env-var hint (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY` …), and a `LOGOUT` button when a credential is present.
- **Form row.** `<input type="password">` plus a primary action whose label flips between `STORE` (no credential yet) and `ROTATE` (replacing one).
- **Error row.** A `role="alert"` line under the form when the most recent submit / logout call rejected.

### Rotation confirmation

Submitting the form when a credential is already stored opens a modal:

```
┌────────────────────────────────────────────┐
│ REPLACE STORED KEY?                        │
├────────────────────────────────────────────┤
│ A credential for <Provider> is already     │
│ stored. Replacing it overwrites the        │
│ keyring entry — the previous key cannot    │
│ be recovered.                              │
├────────────────────────────────────────────┤
│                          [CANCEL] [REPLACE]│
└────────────────────────────────────────────┘
```

The modal is a focus-trapped `role="dialog" aria-modal="true"` with a window-level `Escape` handler. Logout is reversible (re-enter the key) and stays single-step — no modal.

## States

Per row, four states cleanly separated:

- **Loading.** First read of `has_credential` — the row paints with the indicator's neutral fallback (treated as missing). Probe failure does not crash the row; it degrades to "no stored credential" so the user can still type a key and recover.
- **Empty / no credential.** `⚠` indicator (color-warn), `Add key` form label, `STORE` action.
- **Stored.** `✓` indicator (color-ok), `Replace key` form label, `ROTATE` action, plus the `LOGOUT` button. Replacing requires the rotation-confirm modal.
- **Pending.** While a `loginProvider` / `logoutProvider` IPC is in flight, both buttons report `aria-busy=true`, the input is `disabled`, and the action's label stays the same — no spinner copy.
- **Error.** `role="alert"` line under the form with the verbatim IPC rejection message. The line clears on the next user action.

## Copy

- Section label: `CREDENTIALS`
- Indicator labels (aria-label only): `Credential stored for <Provider>` / `No credential for <Provider>`
- Form labels: `Replace key` / `Add key`
- Buttons: `STORE`, `ROTATE`, `LOGOUT` (uppercase, mono, matches the project's destructive / write-style action discipline).
- Rotation modal title: `REPLACE STORED KEY?` (uppercase question — same voice as `CLEAR MEMORY?` in `memory-section.md`).
- Rotation body: verbatim — "A credential for <strong>{providerLabel}</strong> is already stored. Replacing it overwrites the keyring entry — the previous key cannot be recovered."
- Rotation buttons: `CANCEL`, `REPLACE`.

## Color & typography

- Stored indicator: `--color-ok`. Missing indicator: `--color-warn`.
- Env-var hint: `--font-mono`, `--color-text-tertiary` — the hint is reference, not interactive.
- Action buttons follow `@forge/design` `Button` variants: `ghost` for `LOGOUT` and `CANCEL`, `primary` (ember accent) for `STORE` / `ROTATE` / `REPLACE`.

## Security contract

The section is the canonical implementation of Forge's credential UX rules. Reviewers should treat any drift as a security issue:

- The typed value lives only in the row's local `draft()` signal and the `<input type="password">`'s browser-DOM state.
- The signal is cleared the moment the IPC call resolves — success or rejection — and on rotation cancel. No ambient terminal state can hold the secret.
- No `aria-label`, log line, or rendered DOM string ever echoes the key.
- The DOM never contains a rendered key — only the password input ever holds the typed value.

## Keyboard

- Tab — moves focus from indicator → input → action button → next row.
- Enter inside the input — submits the form (same path as clicking `STORE` / `ROTATE`).
- Escape inside the rotation modal — cancels (matches `WAI-ARIA APG Dialog` pattern; the listener is window-level so the focus trap can't swallow the keystroke).

## Cross-spec references

- [`providers-section.md`](./providers-section.md) — the neighbouring section whose card "credential" hint is sourced from `has_credential`.
- [`dashboard.md`](./dashboard.md) — first-run `<CredentialBanner>` lives at the top of the dashboard and deep-links to `#credentials-section`.
- `docs/architecture/credentials.md` — backend keyring contract (referenced for the rotation-overwrite invariant).
- `docs/design/component-principles.md` — four-state rule.

## Doesn't do

- Does not surface keyless providers (e.g. Ollama). Adding them would surface a "missing key" indicator for a provider that does not need one.
- Does not let the user *view* a stored key. The IPC contract is one-way once stored — only `has_credential` is queryable.
- Does not export / back up keys. The user's keyring is the system of record.
- Does not own provider activation — that's [`providers-section.md`](./providers-section.md).

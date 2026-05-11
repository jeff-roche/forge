# V1 UI completeness

## Purpose

Forge V1 ships when an operator can complete every workflow below from the UI alone — no terminal, no hand-edited TOML, no shelling into `forge_cli`. This document is the acceptance gate: each row pins a user-facing workflow to the ui-spec section that delivers it, so reviewers can verify the end-to-end promise without re-reading every spec.

## Acceptance gate

| Workflow | Trigger | Spec section | Acceptance evidence |
| --- | --- | --- | --- |
| Open workspace | Dashboard hero `+ New session` with no cached workspace, or `Browse` inside the new-session form | [new-session-flow.md §Empty-workspace branch](../ui-specs/new-session-flow.md#empty-workspace-branch) | Native directory picker confirms; selected absolute path prefills `workspace_root` and caches for subsequent spawns. |
| Spawn new session | Dashboard hero `+ New session` | [new-session-flow.md](../ui-specs/new-session-flow.md), [dashboard.md §Hero block](../ui-specs/dashboard.md#hero-block) | `session_start` IPC returns a `session_id`; Session window opens in `v-fresh` state. |
| Attach to existing session | Dashboard hero `Attach to session` (F-727) | [dashboard.md §Hero block](../ui-specs/dashboard.md#hero-block) | Attach picker enumerates detached sessions; selection hands off to the existing Session window. |
| Add provider | Providers route `+ Add provider` | [providers-page.md §Add provider](../ui-specs/providers-page.md#add-provider) | `add_provider` IPC (F-730) persists the new entry; provider appears in the list and on the dashboard Providers card without restart. |
| Edit provider | Providers row `Edit` | [providers-page.md §Edit/Remove](../ui-specs/providers-page.md#editremove) | `update_provider` IPC (F-731) writes back; row reflects the change. |
| Remove provider | Providers row `Remove` (with destructive-action confirmation) | [providers-page.md §Edit/Remove](../ui-specs/providers-page.md#editremove) | `remove_provider` IPC (F-732) succeeds; row disappears; dependent sessions surface the absence verbatim. |
| Enter and test credential | Providers row credential field + `Test connection` | [providers-page.md §Credential entry](../ui-specs/providers-page.md#credential-entry), [§Test connection](../ui-specs/providers-page.md#test-connection) | Write-only credential IPC (per [credentials-section.md](../ui-specs/credentials-section.md)) lands the secret; `test_provider_connection` (F-733) returns `ready` or the verbatim error. |
| Add MCP server | Catalog `+ Add MCP server` | [catalog.md §Add MCP server](../ui-specs/catalog.md#add-mcp-server) | Modal accepts stdio or http variant; validation passes; new entry appears in the MCP tab and in the Enabled card. |
| Toggle skill enabled | Catalog row toggle (Skills tab) | [catalog.md §Enablement toggles](../ui-specs/catalog.md#enablement-toggles) | Toggle round-trips through the catalog enablement IPC; dashboard Enabled card reflects the new state. |
| Toggle MCP server enabled | Catalog row toggle (MCP tab) | [catalog.md §Enablement toggles](../ui-specs/catalog.md#enablement-toggles) | Same contract as skills; dashboard Enabled card mirrors. |
| Toggle agent enabled | Catalog row toggle (Agents tab) or dashboard Enabled card row | [catalog.md §Enablement toggles](../ui-specs/catalog.md#enablement-toggles), [dashboard.md §Enabled card](../ui-specs/dashboard.md#enabled-card) | Toggle from either surface (F-724) persists; the other surface re-renders the same truth on the next `WORKSPACE_CONFIG_CHANGED_EVENT`. |
| View usage (7D / 30D / MTD) | Dashboard Usage card selector | [dashboard.md §Usage card](../ui-specs/dashboard.md#usage-card), [usage.md](../ui-specs/usage.md) | `usage_summary` IPC (F-722) returns the windowed totals; KPI tiles and spark chart render. |
| Manage containers (stop / logs / prune) | Dashboard Containers card row actions, or Containers route | [dashboard.md §Containers card](../ui-specs/dashboard.md#containers-card), [containers-section.md](../ui-specs/containers-section.md) | Per-row `Stop` / `Logs` and header `Prune` invoke the existing podman IPC; verbatim runtime errors surface inline. |
| First-run dashboard (no state) | Cold launch with no providers, no sessions, no enabled assets | [dashboard.md](../ui-specs/dashboard.md), [dashboard.md §Hero block](../ui-specs/dashboard.md#hero-block) (empty state) | Hero status sentence reads the empty-state copy; each card paints its empty placeholder pointing at the remediation route (F-737). |

## Out of scope (V1)

Mirrored from the Phase 3.1 milestone description:

- **Skill authoring UI.** Editing `.md` skill files remains a CLI / editor workflow.
- **Agent definition authoring UI.** Same — authoring agents is not in the V1 UI surface.
- **Workspace creation flow.** Opening Forge in a directory is the workspace-select gesture; there is no in-product "new workspace" dialog.

## Cross-spec references

- [dashboard.md](../ui-specs/dashboard.md)
- [app-shell.md](../ui-specs/app-shell.md)
- [new-session-flow.md](../ui-specs/new-session-flow.md)
- [providers-page.md](../ui-specs/providers-page.md)
- [credentials-section.md](../ui-specs/credentials-section.md)
- [catalog.md](../ui-specs/catalog.md)
- [containers-section.md](../ui-specs/containers-section.md)
- [usage.md](../ui-specs/usage.md)
- [forge-mocks.html](../forge-mocks.html)

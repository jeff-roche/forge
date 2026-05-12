# Persistence

> Extracted from IMPLEMENTATION.md §7 — filesystem-only persistence, full file layout, metadata schemas, usage aggregation, archive/reactivation

---

## 7. Persistence layer — filesystem only

Forge has no embedded database. All persistent state is on disk as plain files.

### 7.1 Filesystem layout

```
~/.config/forge/
  config.toml              # user-global settings (TOML)
  workspaces.toml          # known-workspaces registry
  credentials.toml         # {provider_id → keychain_ref}
  approvals.toml           # persistent approval whitelist (user tier) — §7.7
  settings.toml            # user preferences (notifications, windows) — §7.8
  memory/
    <agent-name>.md        # opt-in cross-session memory

~/.agents/<name>.md              # user-global agent definitions
~/.agent-skills/<name>/          # user-global skills (agentskills.io folders)
  SKILL.md
  scripts/
  references/
~/.mcp.json                      # user-global MCP servers

<workspace>/
  AGENTS.md                      # shared workspace instructions
  .mcp.json                      # workspace MCP servers
  .agents/<name>.md              # workspace agent definitions
  .agent-skills/<name>/SKILL.md  # workspace skills
  .forge/                  # internal, self-gitignored
    .gitignore             # contents: *
    approvals.toml         # persistent approval whitelist (workspace tier) — §7.7
    settings.toml          # workspace setting overrides — §7.8
    sessions/
      <id>/
        meta.toml
        events.jsonl
        snapshots/
      archived/<id>/       # same layout as above, moved on archive
    layouts.json           # window/pane layout (see `session-layout.md`)
    languages.toml         # user-added LSP servers
    cache/                 # fetch results, etc.
```

### 7.2 `workspaces.toml` (user-global registry)

```toml
[[workspace]]
id = "sha1-hex"
path = "/home/alice/code/acme-api"
name = "acme-api"
last_opened = 2026-04-15T14:22:00Z
pinned = false

[[workspace]]
id = "sha1-hex2"
path = "/home/alice/code/docs-v2"
...
```

### 7.3 `meta.toml` (per session)

```toml
id = "a3f1b2c4"
workspace_id = "sha1-hex"
name = "refactor-payment-service"
agent = "refactor-bot"                 # optional; omitted for bare-provider sessions
provider_id = "anthropic"               # for bare-provider sessions
model = "sonnet-4.5"                    # for bare-provider sessions
state = "active"                        # active | archived | ended
persistence = "persist"                 # persist | ephemeral
started_at = 2026-04-15T14:22:00Z
ended_at = ... (optional)
tokens_in = 48200
tokens_out = 12100
cost_usd = 0.23
pid = 48211
socket_path = "/tmp/forge-1000/sessions/a3f1b2c4.sock"
```

### 7.4 Usage aggregation

Each session's event log has `UsageTick` events — the authoritative source. Queries for "spend across all workspaces last 7 days" work by:

1. Scanning known workspaces (from `workspaces.toml`)
2. Reading each session's `events.jsonl` (skipping schema header)
3. Filtering `UsageTick` events within the date range
4. Aggregating

For volume performance, phase 3 may introduce an additive monthly aggregate file (`~/.config/forge/usage-<YYYY-MM>.jsonl`) written on session end. Decision deferred until we have real volume signal.

### 7.5 Archive on session end

When a `persist` session ends:
1. Final events written
2. `meta.toml` updated to `state = "archived"`, `ended_at = <now>`
3. The session directory is moved from `.forge/sessions/<id>/` to `.forge/sessions/archived/<id>/`
4. Socket path removed from runtime dir

When an `ephemeral` session ends:
1. Process terminates
2. The session directory is fully removed from disk
3. No trace persists

### 7.6 Reactivating an archived session

`forge session attach <id>` on an archived session:
1. Looks up `meta.toml` in `archived/<id>/`
2. Moves the directory back to `sessions/<id>/`
3. Spawns `forged` with `--reactivate <id>` flag
4. Session replays event log to restore in-memory state
5. Ready for new activity; state becomes `active` again

### 7.7 `approvals.toml` schema (F-036)

Persistent approval whitelist. Two tiers — user (`~/.config/forge/approvals.toml`) and workspace (`<workspace>/.forge/approvals.toml`). Both optional; a missing file is treated as empty (first-run is the common case, not an error). Source of truth: `crates/forge-core/src/approvals.rs`.

```toml
[[entries]]
scope_key = "file:fs.write:/src/foo.ts"  # deterministic key derived from tool + target
tool_name = "fs.write"                    # the tool the key was derived from
label     = "this file"                   # short UI label (e.g. "this file", "this tool", "pattern /src/*")

[[entries]]
scope_key = "tool:shell.exec"
tool_name = "shell.exec"
label     = "this tool"
```

**Validation.** Strict: `#[serde(deny_unknown_fields)]` on every record. Unknown or misspelled keys are rejected at load — the approval trust boundary does not accept forward-compat garbage.

**Atomic writes.** Saves go to `<path>.tmp` then `rename` into place. Rename is atomic on POSIX for same-filesystem targets, so a partial `.tmp` never becomes the visible config.

**Merge precedence.** On `scope_key` collision between user and workspace entries, **workspace wins**. The shell surfaces both tiers to the frontend so UI can show provenance, but a single winning entry per key seeds the in-memory auto-approve whitelist. Unlike settings, this is a coarse whole-entry override — there are no sub-fields to deep-merge.

### 7.8 `settings.toml` schema (F-151)

Structured user preferences. Two tiers — user (`~/.config/forge/settings.toml`) and workspace (`<workspace>/.forge/settings.toml`). Both optional; a missing file yields defaults. Source of truth: `crates/forge-core/src/settings.rs`.

```toml
[notifications]
bg_agents = "toast"     # one of: "toast" | "os" | "both" | "silent" (default "toast")

[windows]
session_mode = "single" # one of: "single" | "split" (default "single")
```

**Forward-compat.** The top-level struct does **not** carry `deny_unknown_fields`. A newer release may add sections; older builds reading a newer file ignore unknown keys rather than refuse to load. This is the intentional inverse of `approvals.toml`.

**`#[serde(default)]` everywhere.** Every field and nested struct is defaulted, so a file containing only `[notifications]` still loads — the missing `[windows]` section falls back to defaults.

**Atomic writes.** Same `<path>.tmp` + rename pattern as approvals.

**Deep merge: workspace overrides user at field granularity.** Workspace does **not** wholesale replace user. The merge runs on the raw `toml::Value` tree parsed from each file (empty tree when absent) and overlays workspace keys onto user keys at every depth before deserializing into `AppSettings`. Concretely:

- `user.windows.session_mode = "split"`
- `workspace.notifications.bg_agents = "os"` (workspace file contains **only** `[notifications]`)
- → merged: `notifications.bg_agents = "os"`, `windows.session_mode = "split"`

The reason this runs on raw TOML and not on `AppSettings` structs: `#[serde(default)]` erases the distinction between "workspace explicitly set `windows.session_mode = single`" and "workspace did not mention windows at all" — both deserialize identically. A struct-level merge would clobber a user's non-default value with the workspace's implicit default.

**`set_setting` write path.** Writes mutate the **raw TOML tree** (`apply_setting_update`) then atomic-save the resulting text — never a re-serialized `AppSettings`. Re-serializing would promote every in-memory default into the persisted file and silently destroy the absent-means-pick-up-default semantic the merge layer depends on. Validation happens by deserializing the updated tree into `AppSettings` before write: type mismatches (e.g. `bg_agents = 42`) are rejected at the IPC boundary; unknown keys pass through untouched for forward-compat.

**Array merge.** Arrays overwrite wholesale rather than concatenate. The current schema has no additive lists; if one is added later, this section must be revisited.

> The third user-visible persistence file, `<workspace>/.forge/layouts.json` (F-150), is documented separately in [`session-layout.md`](./session-layout.md).

//! F-734: `add_mcp_server` IPC.
//!
//! The Catalog page's `+ Add MCP server` modal is the only caller. Writes a
//! single entry into the universal `.mcp.json` document — workspace-scoped
//! (`<workspace>/.mcp.json`) or user-scoped (`~/.mcp.json`), per the
//! F-127 schema authority in `crates/forge-mcp/src/lib.rs`.
//!
//! # Module anchor
//!
//! This file is the canonical home for MCP-related Tauri commands. F-735
//! (catalog scope toggles) and F-736 (catalog chips) do not add IPCs, so
//! they should not need to touch this module. New MCP-writing commands —
//! a future delete or edit flow — belong here.
//!
//! # Authorization
//!
//! Dashboard-scoped: the catalog window mounts on the dashboard label.
//! Other windows are rejected by `require_window_label` ahead of any I/O.
//!
//! # Concurrency
//!
//! A process-wide async mutex serialises the read-modify-write triple
//! against the chosen `.mcp.json`. The user/workspace files are separate
//! locks today — name collisions are scope-local, so cross-scope races
//! cannot corrupt either file.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
#[cfg(feature = "webview")]
use tauri::{Runtime, State, Webview};
#[cfg(feature = "webview")]
use tokio::sync::Mutex;
use ts_rs::TS;

use forge_mcp::{McpServerSpec, ServerKind};

/// F-673: command-named error prefix. Every outer error path returned from
/// this command must begin with this constant.
pub const ADD_MCP_SERVER_ERROR: &str = "add_mcp_server: ";

/// Slug pattern for MCP server names. Same shape as the universal `.mcp.json`
/// example configs (`github`, `postgres-schemata`, etc.) plus underscores —
/// matches the catalog spec's `^[a-z0-9][a-z0-9_-]*$`.
fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Process-wide guard that serialises `add_mcp_server`'s read-modify-write
/// against `.mcp.json`. Held only across the read → merge → write triple so
/// the worst-case latency is one disk write.
#[cfg(feature = "webview")]
fn mcp_write_guard() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

/// `add_mcp_server` IPC input. The `workspace_root` field discriminates the
/// target file — `Some(path)` writes `<path>/.mcp.json`, `None` writes
/// `~/.mcp.json`.
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct AddMcpServerInput {
    /// Target workspace root. `None` writes to the user-scope `~/.mcp.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    /// Unique server name within the chosen scope. Matches
    /// `^[a-z0-9][a-z0-9_-]*$`.
    pub name: String,
    /// Transport-discriminated config. Mirrors the universal `.mcp.json`
    /// schema in `crates/forge-mcp/src/lib.rs`.
    pub config: McpServerConfig,
}

/// Transport-discriminated MCP server config. Mirrors `forge_mcp::ServerKind`
/// but with the JSON-friendly tag layout the webview emits.
#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

impl McpServerConfig {
    /// Convert the wire shape into the authoritative `McpServerSpec`. Funnels
    /// through `forge-mcp`'s validation (`StrictFields::Reject` via the
    /// universal-schema loader) so the schema authority for `.mcp.json`
    /// stays single-rooted.
    pub fn into_spec(self) -> McpServerSpec {
        match self {
            McpServerConfig::Stdio { command, args, env } => McpServerSpec {
                kind: ServerKind::Stdio {
                    command,
                    args,
                    env: env.into_iter().collect(),
                },
            },
            McpServerConfig::Http { url, headers } => McpServerSpec {
                kind: ServerKind::Http {
                    url,
                    headers: headers.into_iter().collect(),
                },
            },
        }
    }
}

/// Pure validation exposed for unit tests. Errors carry the `add_mcp_server:`
/// prefix so callers can pattern-match on a single shape.
pub fn validate_input(input: &AddMcpServerInput) -> Result<(), String> {
    if input.name.trim().is_empty() {
        return Err(format!("{ADD_MCP_SERVER_ERROR}name is required"));
    }
    if !is_valid_name(&input.name) {
        return Err(format!("{ADD_MCP_SERVER_ERROR}name must be a slug"));
    }
    match &input.config {
        McpServerConfig::Stdio { command, .. } => {
            if command.trim().is_empty() {
                return Err(format!("{ADD_MCP_SERVER_ERROR}stdio requires command"));
            }
        }
        McpServerConfig::Http { url, .. } => {
            let parsed = url::Url::parse(url)
                .map_err(|e| format!("{ADD_MCP_SERVER_ERROR}invalid URL: {e}"))?;
            match parsed.scheme() {
                "http" | "https" => {}
                other => {
                    return Err(format!(
                        "{ADD_MCP_SERVER_ERROR}invalid URL: scheme {other} is not http/https"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Scope label for error messages — matches the `+ Add MCP server` form's
/// `Workspace` / `User` radio copy.
fn scope_label(workspace_root: Option<&str>) -> &'static str {
    if workspace_root.is_some() {
        "workspace"
    } else {
        "user"
    }
}

/// Resolve the `.mcp.json` path for a given scope. `workspace_root = Some(_)`
/// writes `<workspace>/.mcp.json`; `None` writes `~/.mcp.json`. Reused by
/// the unit tests so the resolution rule has a single source of truth.
pub fn resolve_mcp_json_path(workspace_root: Option<&Path>, user_home: &Path) -> PathBuf {
    match workspace_root {
        Some(ws) => ws.join(".mcp.json"),
        None => user_home.join(".mcp.json"),
    }
}

/// Pure merger: load the existing `.mcp.json` body (empty string when the
/// file is missing), reject a duplicate name, append the new entry, and
/// emit the rendered universal-schema body. Exposed for unit tests.
pub fn merge_entry(
    existing: &str,
    name: &str,
    spec: McpServerSpec,
    scope: &str,
) -> Result<String, String> {
    let mut servers: BTreeMap<String, McpServerSpec> = parse_existing(existing)?;
    if servers.contains_key(name) {
        return Err(format!(
            "{ADD_MCP_SERVER_ERROR}server {name} already configured in {scope}"
        ));
    }
    servers.insert(name.to_string(), spec);
    forge_mcp::render_universal(&servers)
        .map_err(|e| format!("{ADD_MCP_SERVER_ERROR}schema validation failed: {e}"))
}

/// Parse an existing `.mcp.json` body into the canonical map. Treats an
/// empty body as the empty map. Surfaces parse errors through the
/// `schema validation failed:` channel — the wire shape is the same one
/// the loader emits, so a corrupt file at the time of an add cannot be
/// silently overwritten.
fn parse_existing(body: &str) -> Result<BTreeMap<String, McpServerSpec>, String> {
    if body.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct File {
        #[serde(rename = "mcpServers", default)]
        servers: BTreeMap<String, RawEntry>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawEntry {
        #[serde(rename = "type")]
        kind: Option<String>,
        command: Option<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        url: Option<String>,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    }

    let parsed: File = serde_json::from_str(body)
        .map_err(|e| format!("{ADD_MCP_SERVER_ERROR}schema validation failed: {e}"))?;

    let mut out = BTreeMap::new();
    for (name, raw) in parsed.servers {
        let spec = match raw.kind.as_deref() {
            Some("stdio") | None if raw.command.is_some() => McpServerSpec {
                kind: ServerKind::Stdio {
                    command: raw.command.unwrap(),
                    args: raw.args,
                    env: raw.env,
                },
            },
            Some("http") | None if raw.url.is_some() => McpServerSpec {
                kind: ServerKind::Http {
                    url: raw.url.unwrap(),
                    headers: raw.headers,
                },
            },
            _ => {
                return Err(format!(
                    "{ADD_MCP_SERVER_ERROR}schema validation failed: \
                     entry {name} missing transport fields"
                ));
            }
        };
        out.insert(name, spec);
    }
    Ok(out)
}

/// F-734: write a new MCP server entry into the chosen `.mcp.json`.
#[cfg(feature = "webview")]
#[tauri::command]
pub async fn add_mcp_server<R: Runtime>(
    input: AddMcpServerInput,
    webview: Webview<R>,
    state: State<'_, crate::ipc::BridgeState>,
) -> Result<(), String> {
    crate::ipc::require_window_label(&webview, "dashboard", "add_mcp_server")
        .map_err(|e| format!("{ADD_MCP_SERVER_ERROR}{e}"))?;
    validate_input(&input)?;

    let _write_lock = mcp_write_guard().lock().await;

    let workspace_path: Option<PathBuf> = match input.workspace_root.as_deref() {
        Some(raw) if !raw.trim().is_empty() => Some(
            crate::ipc::resolve_workspace_root_for_command(webview.label(), raw, &state)
                .await
                .map_err(|e| format!("{ADD_MCP_SERVER_ERROR}{e}"))?,
        ),
        _ => None,
    };

    let user_home = dirs::home_dir()
        .ok_or_else(|| format!("{ADD_MCP_SERVER_ERROR}could not resolve user home directory"))?;
    let target_path = resolve_mcp_json_path(workspace_path.as_deref(), &user_home);
    let scope = scope_label(input.workspace_root.as_deref());

    let existing = tokio::fs::read_to_string(&target_path)
        .await
        .unwrap_or_default();

    let spec = input.config.clone().into_spec();
    let rendered = merge_entry(&existing, &input.name, spec, scope)?;

    if let Some(parent) = target_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            format!("{ADD_MCP_SERVER_ERROR}could not prepare .mcp.json parent: {e}")
        })?;
    }
    tokio::fs::write(&target_path, rendered)
        .await
        .map_err(|e| format!("{ADD_MCP_SERVER_ERROR}could not write .mcp.json: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn stdio_input(name: &str) -> AddMcpServerInput {
        AddMcpServerInput {
            workspace_root: None,
            name: name.to_string(),
            config: McpServerConfig::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@scope/server".to_string()],
                env: HashMap::new(),
            },
        }
    }

    fn http_input(name: &str, url: &str) -> AddMcpServerInput {
        AddMcpServerInput {
            workspace_root: None,
            name: name.to_string(),
            config: McpServerConfig::Http {
                url: url.to_string(),
                headers: HashMap::new(),
            },
        }
    }

    #[test]
    fn validates_stdio_success() {
        validate_input(&stdio_input("github")).expect("stdio entry");
    }

    #[test]
    fn validates_http_success() {
        validate_input(&http_input("remote", "https://mcp.example.com/api")).expect("http entry");
    }

    #[test]
    fn rejects_missing_name() {
        let mut input = stdio_input("");
        input.name = String::new();
        let err = validate_input(&input).unwrap_err();
        assert!(err.starts_with(ADD_MCP_SERVER_ERROR), "{err}");
        assert!(err.contains("name is required"), "{err}");
    }

    #[test]
    fn rejects_whitespace_name() {
        let mut input = stdio_input("name");
        input.name = "   ".to_string();
        let err = validate_input(&input).unwrap_err();
        assert!(err.contains("name is required"), "{err}");
    }

    #[test]
    fn rejects_non_slug_name() {
        let mut input = stdio_input("Bad Name");
        input.name = "Bad Name".to_string();
        let err = validate_input(&input).unwrap_err();
        assert!(err.contains("name must be a slug"), "{err}");
    }

    #[test]
    fn rejects_name_starting_with_dash() {
        let mut input = stdio_input("-leading");
        input.name = "-leading".to_string();
        let err = validate_input(&input).unwrap_err();
        assert!(err.contains("name must be a slug"), "{err}");
    }

    #[test]
    fn accepts_alphanumeric_with_dash_and_underscore() {
        for ok in ["a", "a1", "scope_name", "scope-name", "0abc"] {
            let mut input = stdio_input(ok);
            input.name = ok.to_string();
            validate_input(&input).unwrap_or_else(|e| panic!("expected ok for {ok}: {e}"));
        }
    }

    #[test]
    fn rejects_stdio_without_command() {
        let mut input = stdio_input("github");
        input.config = McpServerConfig::Stdio {
            command: String::new(),
            args: vec![],
            env: HashMap::new(),
        };
        let err = validate_input(&input).unwrap_err();
        assert!(err.contains("stdio requires command"), "{err}");
    }

    #[test]
    fn rejects_http_with_invalid_url() {
        let err = validate_input(&http_input("bad", "not a url")).unwrap_err();
        assert!(err.contains("invalid URL"), "{err}");
    }

    #[test]
    fn rejects_http_with_non_http_scheme() {
        let err = validate_input(&http_input("bad", "ftp://example.com")).unwrap_err();
        assert!(err.contains("invalid URL"), "{err}");
        assert!(err.contains("ftp"), "{err}");
    }

    #[test]
    fn merge_entry_writes_stdio_into_empty_file() {
        let spec = McpServerConfig::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "@scope/server".into()],
            env: HashMap::new(),
        }
        .into_spec();
        let body = merge_entry("", "github", spec, "workspace").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            parsed["mcpServers"]["github"]["command"].as_str(),
            Some("npx")
        );
    }

    #[test]
    fn merge_entry_writes_http_into_empty_file() {
        let spec = McpServerConfig::Http {
            url: "https://mcp.example.com/api".into(),
            headers: HashMap::new(),
        }
        .into_spec();
        let body = merge_entry("", "remote", spec, "user").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            parsed["mcpServers"]["remote"]["url"].as_str(),
            Some("https://mcp.example.com/api")
        );
    }

    #[test]
    fn merge_entry_preserves_existing_entries() {
        let initial = r#"{
            "mcpServers": {
                "github": { "command": "npx", "args": ["-y", "@a/b"] }
            }
        }"#;
        let spec = McpServerConfig::Http {
            url: "https://mcp.example.com/api".into(),
            headers: HashMap::new(),
        }
        .into_spec();
        let body = merge_entry(initial, "remote", spec, "user").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parsed["mcpServers"]["github"].is_object());
        assert!(parsed["mcpServers"]["remote"].is_object());
    }

    #[test]
    fn merge_entry_rejects_duplicate_name() {
        let initial = r#"{ "mcpServers": { "github": { "command": "npx" } } }"#;
        let spec = McpServerConfig::Stdio {
            command: "other".into(),
            args: vec![],
            env: HashMap::new(),
        }
        .into_spec();
        let err = merge_entry(initial, "github", spec, "workspace").unwrap_err();
        assert!(err.starts_with(ADD_MCP_SERVER_ERROR), "{err}");
        assert!(err.contains("already configured in workspace"), "{err}");
    }

    #[test]
    fn merge_entry_rejects_corrupt_existing_file() {
        let spec = McpServerConfig::Stdio {
            command: "x".into(),
            args: vec![],
            env: HashMap::new(),
        }
        .into_spec();
        let err = merge_entry("{ not json", "x", spec, "user").unwrap_err();
        assert!(err.contains("schema validation failed"), "{err}");
    }

    #[test]
    fn resolve_path_workspace_branch() {
        let ws = Path::new("/tmp/workspace");
        let home = Path::new("/home/u");
        let path = resolve_mcp_json_path(Some(ws), home);
        assert_eq!(path, Path::new("/tmp/workspace/.mcp.json"));
    }

    #[test]
    fn resolve_path_user_branch() {
        let home = Path::new("/home/u");
        let path = resolve_mcp_json_path(None, home);
        assert_eq!(path, Path::new("/home/u/.mcp.json"));
    }

    #[test]
    fn round_trip_through_forge_mcp_loader() {
        // The merge output round-trips through forge_mcp's own loader —
        // single schema authority, no parallel parser.
        let tmp = TempDir::new().unwrap();
        let spec = McpServerConfig::Stdio {
            command: "npx".into(),
            args: vec!["-y".into()],
            env: HashMap::new(),
        }
        .into_spec();
        let body = merge_entry("", "github", spec, "workspace").unwrap();
        std::fs::write(tmp.path().join(".mcp.json"), body).unwrap();
        let loaded = forge_mcp::config::load_workspace(tmp.path()).unwrap();
        assert!(loaded.contains_key("github"));
    }

    /// F-673 prefix invariant — pinned so a future rename of
    /// `ADD_MCP_SERVER_ERROR` doesn't silently drift.
    #[test]
    fn add_mcp_server_error_prefix_is_command_named() {
        assert_eq!(ADD_MCP_SERVER_ERROR, "add_mcp_server: ");
    }
}

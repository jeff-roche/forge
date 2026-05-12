//! Roster types — discoverable session resources (providers, skills, MCP
//! servers, agents) plus a uniform [`RosterScope`] discriminator.
//!
//! F-591 introduces these alongside four `list_*(scope)` IPC commands in
//! `forge-shell::ipc` so the catalog UI (F-592) can enumerate everything an
//! agent can reach without knowing how each kind is loaded. This module
//! holds only the wire shapes; the loaders live in their canonical crates
//! (`forge_agents::skill_loader`, `forge_agents::load_agents`,
//! `forge_mcp::config`, and the hardcoded provider list in `forge-shell`).
//!
//! Serialization uses `#[serde(tag = "type")]` to match the project-wide
//! tagged-union convention documented in `docs/architecture/event-conventions.md`,
//! so the generated TS shape is `{ type: "Skill", id: ... } | …` — directly
//! consumable by the eventual webview store without a custom decoder.
//!
//! See `docs/architecture/crate-architecture.md` §3.1 for the canonical type
//! sketch this implementation realises.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::{AgentId, ProviderId};
use crate::skill::SkillId;

/// Identifier for an MCP server entry in a roster.
///
/// MCP servers are named in `.mcp.json` (e.g. `"github"`, `"sentry"`). The
/// name is the natural identifier; we wrap it in a newtype so future
/// validation (length cap, slug shape) has a single home and so the wire
/// shape stays distinct from a free `String` in TS.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(
    export,
    export_to = "../../../web/packages/ipc/src/generated/",
    type = "string"
)]
#[serde(transparent)]
pub struct McpId(String);

impl McpId {
    /// Construct an [`McpId`] from any string-shaped value.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for McpId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for McpId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for McpId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<McpId> for String {
    fn from(id: McpId) -> Self {
        id.0
    }
}

/// Scope filter for roster queries.
///
/// `SessionWide` returns everything visible at the session level;
/// `Agent(id)` narrows to entries an agent has bound (its own skills, its
/// own MCP servers, etc.); `Provider(id)` narrows to entries tied to one
/// provider (today only the provider entry itself).
///
/// Wire shape uses `#[serde(tag = "type")]` so the TS binding is
/// `{ type: "SessionWide" } | { type: "Agent", id: AgentId } | { type: "Provider", id: ProviderId }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type")]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub enum RosterScope {
    SessionWide,
    Agent { id: AgentId },
    Provider { id: ProviderId },
}

/// Transport an MCP server uses. Mirrors the two `ServerKind` variants in
/// `forge_mcp` (stdio child process vs. HTTP endpoint). Surfaced on
/// [`RosterEntry::Mcp`] so the catalog UI can render transport chips and
/// filter by transport without re-reading `.mcp.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub enum McpTransport {
    Stdio,
    Http,
}

/// One discoverable entry in a roster.
///
/// Each variant carries the minimum payload needed to identify and label the
/// entry in the catalog UI. Loaders elsewhere produce the heavyweight shape
/// (`Skill { id, prompt, ... }`); this enum stays narrow so the IPC payload
/// for a 200-entry catalog does not balloon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type")]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub enum RosterEntry {
    Provider {
        id: ProviderId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    Skill {
        id: SkillId,
    },
    Mcp {
        id: McpId,
        /// Transport this server uses (stdio or http). Absent when the
        /// catalog hasn't resolved the underlying `.mcp.json` entry yet
        /// (e.g. fixture-only roster entries in tests).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transport: Option<McpTransport>,
    },
    Agent {
        id: AgentId,
        background: bool,
    },
}

/// File-system tier an entry was loaded from. Drives the catalog's
/// `Workspace` / `User` filter chips and any UI that wants to indicate
/// origin alongside the scope. `None` for provider rows and other entries
/// that have no on-disk tier (built-in providers, in-memory overlays).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub enum RosterTier {
    Workspace,
    User,
}

/// One [`RosterEntry`] paired with the [`RosterScope`] it loaded under.
///
/// The catalog UI uses the scope to group entries (e.g. show user-scoped
/// skills above workspace-scoped). Filter logic in the `list_*` commands
/// emits the scope each result lives under, never the requested filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../web/packages/ipc/src/generated/")]
pub struct ScopedRosterEntry {
    pub entry: RosterEntry,
    pub scope: RosterScope,
    /// File-system tier (workspace or user) this entry was loaded from.
    /// Absent for entries that have no on-disk tier (e.g. built-in
    /// providers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<RosterTier>,
}

impl ScopedRosterEntry {
    /// Construct a new scoped roster entry with no on-disk tier.
    pub fn new(entry: RosterEntry, scope: RosterScope) -> Self {
        Self {
            entry,
            scope,
            tier: None,
        }
    }

    /// Construct a scoped roster entry tagged with the file-system tier it
    /// was loaded from.
    pub fn with_tier(entry: RosterEntry, scope: RosterScope, tier: RosterTier) -> Self {
        Self {
            entry,
            scope,
            tier: Some(tier),
        }
    }

    /// Return `true` when this entry should be returned for `filter`.
    ///
    /// `SessionWide` matches everything; `Agent(id)` matches only entries
    /// scoped to that exact agent (a session-wide entry is **not** matched
    /// — agent-scoped queries are narrow on purpose); `Provider(id)`
    /// matches only entries scoped to that provider.
    pub fn matches(&self, filter: &RosterScope) -> bool {
        match filter {
            RosterScope::SessionWide => true,
            RosterScope::Agent { id: agent_filter } => match &self.scope {
                RosterScope::Agent { id } => id == agent_filter,
                _ => false,
            },
            RosterScope::Provider {
                id: provider_filter,
            } => match &self.scope {
                RosterScope::Provider { id } => id == provider_filter,
                _ => false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_id(s: &str) -> AgentId {
        AgentId::from_string(s.to_string())
    }

    fn provider_id(s: &str) -> ProviderId {
        ProviderId::from_string(s.to_string())
    }

    fn skill_id(s: &str) -> SkillId {
        SkillId::new(s).unwrap()
    }

    #[test]
    fn mcp_id_serializes_as_plain_string() {
        let id = McpId::new("github");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"github\"");
        let back: McpId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn roster_scope_session_wide_wire_shape() {
        let json = serde_json::to_string(&RosterScope::SessionWide).unwrap();
        assert_eq!(json, "{\"type\":\"SessionWide\"}");
        let back: RosterScope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, RosterScope::SessionWide);
    }

    #[test]
    fn roster_scope_agent_carries_id() {
        let scope = RosterScope::Agent {
            id: agent_id("planner"),
        };
        let json = serde_json::to_string(&scope).unwrap();
        assert_eq!(json, "{\"type\":\"Agent\",\"id\":\"planner\"}");
        let back: RosterScope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, scope);
    }

    #[test]
    fn roster_scope_provider_carries_id() {
        let scope = RosterScope::Provider {
            id: provider_id("anthropic"),
        };
        let json = serde_json::to_string(&scope).unwrap();
        assert_eq!(json, "{\"type\":\"Provider\",\"id\":\"anthropic\"}");
        let back: RosterScope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, scope);
    }

    #[test]
    fn roster_entry_provider_omits_model_when_absent() {
        let entry = RosterEntry::Provider {
            id: provider_id("anthropic"),
            model: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(json, "{\"type\":\"Provider\",\"id\":\"anthropic\"}");
        let back: RosterEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn roster_entry_provider_includes_model_when_set() {
        let entry = RosterEntry::Provider {
            id: provider_id("anthropic"),
            model: Some("claude-3-5".to_string()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            json,
            "{\"type\":\"Provider\",\"id\":\"anthropic\",\"model\":\"claude-3-5\"}"
        );
    }

    #[test]
    fn roster_entry_skill_wire_shape() {
        let entry = RosterEntry::Skill {
            id: skill_id("planner"),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(json, "{\"type\":\"Skill\",\"id\":\"planner\"}");
        let back: RosterEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn roster_entry_mcp_wire_shape() {
        let entry = RosterEntry::Mcp {
            id: McpId::new("github"),
            transport: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(json, "{\"type\":\"Mcp\",\"id\":\"github\"}");
        let back: RosterEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn roster_entry_mcp_with_transport_wire_shape() {
        let entry = RosterEntry::Mcp {
            id: McpId::new("remote"),
            transport: Some(McpTransport::Http),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(json, "{\"type\":\"Mcp\",\"id\":\"remote\",\"transport\":\"http\"}");
        let back: RosterEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn roster_entry_agent_wire_shape() {
        let entry = RosterEntry::Agent {
            id: agent_id("reviewer"),
            background: true,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            json,
            "{\"type\":\"Agent\",\"id\":\"reviewer\",\"background\":true}"
        );
        let back: RosterEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn scoped_roster_entry_wire_shape() {
        let scoped = ScopedRosterEntry::new(
            RosterEntry::Skill {
                id: skill_id("planner"),
            },
            RosterScope::SessionWide,
        );
        let json = serde_json::to_string(&scoped).unwrap();
        assert_eq!(
            json,
            "{\"entry\":{\"type\":\"Skill\",\"id\":\"planner\"},\"scope\":{\"type\":\"SessionWide\"}}"
        );
        let back: ScopedRosterEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, scoped);
    }

    #[test]
    fn matches_session_wide_returns_everything() {
        let entries = [
            ScopedRosterEntry::new(
                RosterEntry::Skill { id: skill_id("a") },
                RosterScope::SessionWide,
            ),
            ScopedRosterEntry::new(
                RosterEntry::Skill { id: skill_id("b") },
                RosterScope::Agent {
                    id: agent_id("planner"),
                },
            ),
            ScopedRosterEntry::new(
                RosterEntry::Provider {
                    id: provider_id("anthropic"),
                    model: None,
                },
                RosterScope::Provider {
                    id: provider_id("anthropic"),
                },
            ),
        ];
        for e in &entries {
            assert!(e.matches(&RosterScope::SessionWide));
        }
    }

    #[test]
    fn matches_agent_filter_excludes_session_and_other_agents() {
        let target = agent_id("planner");
        let other = agent_id("reviewer");
        let session_wide = ScopedRosterEntry::new(
            RosterEntry::Skill { id: skill_id("a") },
            RosterScope::SessionWide,
        );
        let bound_to_target = ScopedRosterEntry::new(
            RosterEntry::Skill { id: skill_id("b") },
            RosterScope::Agent { id: target.clone() },
        );
        let bound_to_other = ScopedRosterEntry::new(
            RosterEntry::Skill { id: skill_id("c") },
            RosterScope::Agent { id: other.clone() },
        );

        let filter = RosterScope::Agent { id: target };
        assert!(!session_wide.matches(&filter));
        assert!(bound_to_target.matches(&filter));
        assert!(!bound_to_other.matches(&filter));
    }

    #[test]
    fn matches_provider_filter_excludes_other_providers() {
        let target = provider_id("anthropic");
        let other = provider_id("openai");
        let bound_to_target = ScopedRosterEntry::new(
            RosterEntry::Provider {
                id: target.clone(),
                model: None,
            },
            RosterScope::Provider { id: target.clone() },
        );
        let bound_to_other = ScopedRosterEntry::new(
            RosterEntry::Provider {
                id: other.clone(),
                model: None,
            },
            RosterScope::Provider { id: other.clone() },
        );

        let filter = RosterScope::Provider { id: target };
        assert!(bound_to_target.matches(&filter));
        assert!(!bound_to_other.matches(&filter));
    }
}

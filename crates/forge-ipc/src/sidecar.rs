//! F-608: sidecar wire protocol.
//!
//! `forged` (the daemon) and `forged-agent` (the per-instance sidecar process)
//! exchange [`SidecarMessage`] frames over a per-instance Unix domain socket.
//! Framing reuses the existing length-prefixed JSON helpers in this crate
//! ([`crate::write_frame`] / [`crate::read_frame_into`]) — same `[u32 BE
//! length][JSON body]` shape, same 4 MiB cap, same `serde(tag = "t")`
//! discrimination — so the daemon ↔ shell wire and the daemon ↔ sidecar wire
//! share one mental model and one piece of framing code.
//!
//! Variants follow `docs/architecture/agent-sidecar.md` §2. This module
//! defines the types only: transport, supervisor, and spawning land in
//! later F-608 steps.

use chrono::{DateTime, Utc};
use forge_core::{AgentInstanceId, Event, MessageId, ProviderId};
use serde::{Deserialize, Serialize};

/// Wire protocol version negotiated in [`SidecarMessage::Hello`].
///
/// Bumped when the discriminator set or any payload field changes shape.
/// The sidecar refuses a `Hello` whose `proto` it does not recognize and
/// exits non-zero so the supervisor's restart-then-escalate path can
/// surface the mismatch as a `SessionFailed`.
pub const SIDECAR_PROTO_VERSION: u32 = 1;

/// Schema version reported by the sidecar in [`SidecarMessage::HelloAck`].
///
/// Tracks the same shape contract as [`crate::SCHEMA_VERSION`] does for
/// the daemon ↔ shell wire — bumped whenever an existing field's
/// serialization changes (renames, type widening, etc.). The supervisor
/// logs the value but does not gate startup on it today; that becomes a
/// hard check once we ship a non-`1` schema.
pub const SIDECAR_SCHEMA_VERSION: u32 = 1;

/// Bidirectional message exchanged between `forged` and `forged-agent`.
///
/// The discriminator is `t` (matching [`crate::IpcMessage`]) so the wire
/// format stays uniform across Forge IPC. Daemon → sidecar variants are
/// commands; sidecar → daemon variants are events / replies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum SidecarMessage {
    // ── daemon → sidecar ──────────────────────────────────────────────────
    /// First frame after the sidecar connects. Carries everything the
    /// child needs to initialize its provider loop without re-reading
    /// daemon-owned state from disk.
    Hello(SidecarHello),

    /// Run one turn of the request loop. The sidecar streams events back
    /// as [`SidecarMessage::Event`] frames and emits no reply; the
    /// daemon observes turn completion via the `Event::TurnCompleted`
    /// payload.
    RunTurn(SidecarRunTurn),

    /// Push a per-provider credential to the sidecar (§5: daemon owns
    /// the keyring; sidecar receives just-in-time). `secret_handle` is
    /// an opaque string for step 1 — real `secrecy::SecretString`
    /// plumbing lands with step 6.
    Credentials(SidecarCredentials),

    /// Approve a pending tool call. Mirrors [`crate::ToolCallApproved`]
    /// shape so the sidecar can hand it through unchanged.
    ToolCallApproved(SidecarToolCallApproved),

    /// Reject a pending tool call. Mirrors [`crate::ToolCallRejected`].
    ToolCallRejected(SidecarToolCallRejected),

    /// Proxied from the shell (F-598): compact the active transcript.
    CompactTranscript(SidecarCompactTranscript),

    /// Cooperative shutdown signal. The sidecar drains its outbox,
    /// closes the write half, and exits within `grace_ms` or the
    /// supervisor escalates to SIGTERM.
    Shutdown(SidecarShutdown),

    // ── sidecar → daemon ──────────────────────────────────────────────────
    /// Acknowledges [`SidecarMessage::Hello`] and reports the child's
    /// PID. The supervisor pins this `pid` into `ResourceMonitor::track`
    /// (closing F-451) and uses `started_at` to time-stamp restart
    /// counters.
    HelloAck(SidecarHelloAck),

    /// One `forge_core::Event` produced inside the sidecar's run loop.
    /// `seq` is monotonic per sidecar process; the daemon writes the
    /// `event` payload to the session event log unchanged. No
    /// translation layer (§2: same wire shape on both legs).
    Event(SidecarEvent),

    /// The provider loop hit a tool call that needs human approval. The
    /// daemon forwards it to the shell and replies with
    /// [`SidecarMessage::ToolCallApproved`] or
    /// [`SidecarMessage::ToolCallRejected`].
    ToolCallApprovalRequest(SidecarToolCallApprovalRequest),

    /// 1 Hz liveness ping. The supervisor's heartbeat watchdog times
    /// out at 5 s of silence and escalates to a restart (§3).
    Heartbeat(SidecarHeartbeat),

    /// Best-effort panic dump written from the sidecar's panic hook
    /// before the process exits. May be lost on a hard segfault — the
    /// supervisor falls back to EOF + non-zero exit detection in that
    /// case.
    Crashed(SidecarCrashed),
}

// ── daemon → sidecar payloads ────────────────────────────────────────────

/// Initial daemon → sidecar handshake payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarHello {
    pub proto: u32,
    pub instance_id: AgentInstanceId,
    pub agent_def: SidecarAgentDef,
    pub allowed_paths: Vec<String>,
    pub workspace_path: String,
    pub provider_spec: SidecarProviderSpec,
    pub sandbox_level: SidecarSandboxLevel,
    /// Optional OTLP / tracing collector endpoint. `None` skips export.
    pub telemetry_endpoint: Option<String>,
}

/// Wire-friendly subset of `forge_agents::AgentDef` carried over the
/// sidecar handshake. The full type is not `Serialize` (it owns parser
/// state); the sidecar only needs the post-parse fields the run loop
/// reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarAgentDef {
    pub name: String,
    pub description: Option<String>,
    pub body: String,
    pub allowed_paths: Vec<String>,
    pub isolation: String,
    pub memory_enabled: bool,
}

/// Wire-friendly description of which provider the sidecar should
/// instantiate for this turn loop. Step 1 keeps the shape opaque (a
/// `kind` discriminator + a `model` hint); step 3+ swaps in a richer
/// type once the provider crate exposes a serializable spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarProviderSpec {
    /// Provider identifier — e.g. `"ollama"`, `"anthropic"`, `"openai"`.
    pub kind: String,
    /// Model id passed to the provider. Free-form; provider-specific.
    pub model: String,
    /// Optional base URL override (e.g. self-hosted Ollama).
    pub base_url: Option<String>,
}

/// Wire-friendly mirror of `forge_session::sandbox::SandboxLevel`. The
/// host type carries `Arc<dyn ContainerRuntime>` for `Level2` and is
/// not serializable; the sidecar runs *outside* the L2 container (§6)
/// so it only needs to know which mode the daemon is in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum SidecarSandboxLevel {
    Level1,
    Level2 {
        /// Container image reference the daemon's `Level2Session` is
        /// running. Informational on the sidecar side.
        image: String,
    },
}

/// `RunTurn` payload — see `crates/forge-session/src/orchestrator.rs:165`
/// for the field-level mapping to today's in-process `run_turn`
/// arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarRunTurn {
    pub turn_id: String,
    pub msg_id: MessageId,
    pub text: String,
    /// Concatenated `AGENTS.md` / agent body / memory the daemon
    /// assembled.
    pub agents_md: String,
    pub branch_parent: Option<MessageId>,
    pub branch_variant_index: Option<u32>,
    /// Soft cap for the assembled prompt + tool transcript, in bytes.
    pub byte_budget: u64,
}

/// Push a credential for `provider_id`. Step 1 stores it as an opaque
/// string handle; step 6 swaps in `Vec<u8>` + `secrecy::SecretString`
/// and adds the zeroize-on-drop discipline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarCredentials {
    pub provider_id: ProviderId,
    pub secret_handle: String,
}

/// Approve a pending tool call (mirrors [`crate::ToolCallApproved`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarToolCallApproved {
    pub id: String,
    pub scope: String,
}

/// Reject a pending tool call (mirrors [`crate::ToolCallRejected`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarToolCallRejected {
    pub id: String,
    pub reason: Option<String>,
}

/// F-598: compact the active transcript. Empty payload — proxied
/// straight from [`crate::CompactTranscript`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SidecarCompactTranscript {}

/// Cooperative shutdown grace window before the supervisor escalates
/// to SIGTERM. See §3 lifecycle diagram.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarShutdown {
    pub grace_ms: u64,
}

// ── sidecar → daemon payloads ────────────────────────────────────────────

/// `HelloAck` reply. `pid` closes F-451 (real PID into
/// `ResourceMonitor`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarHelloAck {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub schema_version: u32,
}

/// One `forge_core::Event` lifted out of the sidecar's run loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarEvent {
    pub seq: u64,
    pub event: Event,
}

/// Tool-call approval request raised from inside the sidecar. The
/// daemon forwards `name` + `args` to the shell unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarToolCallApprovalRequest {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// 1 Hz liveness ping. `pending_turns` lets the supervisor see whether
/// silence is "idle" (0) or "stuck mid-turn" (>0) for triage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarHeartbeat {
    pub at: DateTime<Utc>,
    pub pending_turns: u32,
}

/// Panic dump emitted by the sidecar's panic hook before exit. Best-
/// effort: a hard segfault never produces this frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarCrashed {
    pub panic_message: String,
    pub backtrace: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use forge_core::{AgentInstanceId, MessageId, ProviderId};

    /// Round-trip every variant through `serde_json::to_vec` /
    /// `from_slice` and assert the shape is preserved. The acceptance
    /// criterion in `docs/architecture/agent-sidecar.md` Step 1 calls
    /// this out by name as `sidecar_message_roundtrip`.
    #[test]
    fn sidecar_message_roundtrip() {
        let at = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();

        let cases = vec![
            SidecarMessage::Hello(SidecarHello {
                proto: SIDECAR_PROTO_VERSION,
                instance_id: AgentInstanceId::from_string("inst-1".into()),
                agent_def: SidecarAgentDef {
                    name: "researcher".into(),
                    description: Some("does research".into()),
                    body: "## Prompt\nbe helpful".into(),
                    allowed_paths: vec!["/workspace".into()],
                    isolation: "process".into(),
                    memory_enabled: false,
                },
                allowed_paths: vec!["/workspace".into(), "/tmp/forge".into()],
                workspace_path: "/workspace".into(),
                provider_spec: SidecarProviderSpec {
                    kind: "ollama".into(),
                    model: "llama3.1:8b".into(),
                    base_url: Some("http://localhost:11434".into()),
                },
                sandbox_level: SidecarSandboxLevel::Level1,
                telemetry_endpoint: None,
            }),
            SidecarMessage::Hello(SidecarHello {
                proto: SIDECAR_PROTO_VERSION,
                instance_id: AgentInstanceId::from_string("inst-2".into()),
                agent_def: SidecarAgentDef {
                    name: "builder".into(),
                    description: None,
                    body: String::new(),
                    allowed_paths: vec![],
                    isolation: "trusted".into(),
                    memory_enabled: true,
                },
                allowed_paths: vec![],
                workspace_path: "/workspace".into(),
                provider_spec: SidecarProviderSpec {
                    kind: "anthropic".into(),
                    model: "claude-opus".into(),
                    base_url: None,
                },
                sandbox_level: SidecarSandboxLevel::Level2 {
                    image: "registry.example.com/forge/sandbox:1".into(),
                },
                telemetry_endpoint: Some("http://otel:4317".into()),
            }),
            SidecarMessage::RunTurn(SidecarRunTurn {
                turn_id: "turn-1".into(),
                msg_id: MessageId::from_string("msg-1".into()),
                text: "hello".into(),
                agents_md: "# Agents".into(),
                branch_parent: Some(MessageId::from_string("msg-0".into())),
                branch_variant_index: Some(2),
                byte_budget: 65_536,
            }),
            SidecarMessage::Credentials(SidecarCredentials {
                provider_id: ProviderId::from_string("prov-1".into()),
                secret_handle: "handle-abc".into(),
            }),
            SidecarMessage::ToolCallApproved(SidecarToolCallApproved {
                id: "tool-1".into(),
                scope: "Once".into(),
            }),
            SidecarMessage::ToolCallRejected(SidecarToolCallRejected {
                id: "tool-2".into(),
                reason: Some("denied by user".into()),
            }),
            SidecarMessage::CompactTranscript(SidecarCompactTranscript::default()),
            SidecarMessage::Shutdown(SidecarShutdown { grace_ms: 2000 }),
            SidecarMessage::HelloAck(SidecarHelloAck {
                pid: 4242,
                started_at: at,
                schema_version: SIDECAR_SCHEMA_VERSION,
            }),
            SidecarMessage::Event(SidecarEvent {
                seq: 7,
                event: Event::SessionStarted {
                    at,
                    workspace: "/workspace".into(),
                    agent: None,
                    persistence: forge_core::types::SessionPersistence::Persist,
                },
            }),
            SidecarMessage::ToolCallApprovalRequest(SidecarToolCallApprovalRequest {
                id: "tool-3".into(),
                name: "shell.exec".into(),
                args: serde_json::json!({"cmd": "ls", "args": ["-la"]}),
            }),
            SidecarMessage::Heartbeat(SidecarHeartbeat {
                at,
                pending_turns: 1,
            }),
            SidecarMessage::Crashed(SidecarCrashed {
                panic_message: "index out of bounds".into(),
                backtrace: Some("frame 0: foo\nframe 1: bar".into()),
            }),
        ];

        for sent in cases {
            let bytes = serde_json::to_vec(&sent).expect("serialize");
            let got: SidecarMessage = serde_json::from_slice(&bytes).expect("deserialize");
            // `SidecarMessage` doesn't derive `PartialEq` (its payloads
            // include `serde_json::Value` and `forge_core::Event` whose
            // `PartialEq` surface we don't want to depend on here), so
            // compare canonical JSON instead — same discipline used in
            // [`crate`]'s `round_trips_hello_*` tests.
            let sent_json = serde_json::to_string(&sent).unwrap();
            let got_json = serde_json::to_string(&got).unwrap();
            assert_eq!(sent_json, got_json, "round-trip mismatch");
        }
    }

    /// The discriminator field is `t` so the sidecar wire is uniform
    /// with [`crate::IpcMessage`]. Pin the on-wire shape so a future
    /// rename cannot drift past CI silently.
    #[test]
    fn sidecar_message_uses_t_discriminator() {
        let msg = SidecarMessage::Shutdown(SidecarShutdown { grace_ms: 500 });
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json.get("t").and_then(|v| v.as_str()), Some("Shutdown"));
        assert_eq!(json.get("grace_ms").and_then(|v| v.as_u64()), Some(500));
    }

    /// Reuse of [`crate::write_frame`] / [`crate::read_frame_into`] is
    /// the contract called out in the architecture doc Step 1. Drive a
    /// `SidecarMessage` through the same helpers `IpcMessage` uses to
    /// prove the framing helpers are message-type-agnostic.
    #[tokio::test]
    async fn sidecar_message_round_trips_over_unix_stream() {
        use tokio::net::UnixStream;

        let (mut a, mut b) = UnixStream::pair().unwrap();
        let sent = SidecarMessage::Heartbeat(SidecarHeartbeat {
            at: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap(),
            pending_turns: 0,
        });

        crate::write_frame(&mut a, &sent).await.unwrap();
        let mut buf = Vec::new();
        let got: SidecarMessage = crate::read_frame_into(&mut b, &mut buf).await.unwrap();

        let sent_json = serde_json::to_string(&sent).unwrap();
        let got_json = serde_json::to_string(&got).unwrap();
        assert_eq!(sent_json, got_json);
    }
}

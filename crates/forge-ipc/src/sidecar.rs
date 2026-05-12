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
//!
//! ## Future work: centralized construction helpers
//!
//! Several payload structs in this module ([`SidecarHello`],
//! [`SidecarRunTurn`], [`SidecarCredentials`]) are built across multiple
//! call sites — supervisor side, agent-host side, and tests — with
//! the same field invariants every time (non-empty `instance_id`,
//! valid `proto`, redacted secret bytes). A small builder or factory
//! that centralizes those checks (e.g.
//! `SidecarHello::builder().proto(P).instance_id(id).build()`) would
//! let us validate once instead of per-site, and would surface
//! shape-drift at construction time rather than at serialize time.
//!
//! Tracked under F-682 quality-hygiene; deliberately deferred from this
//! pass because a proper builder is a real refactor (every call site
//! migrates) and is out of scope for a doc-only cleanup. Pick this up
//! the next time a new field is added to one of those payloads —
//! that's a natural touch point for the migration.

use chrono::{DateTime, Utc};
use forge_core::{AgentInstanceId, Event, MessageId, ProviderId};
use serde::{Deserialize, Serialize};

/// Opaque byte container for [`SidecarCredentials::secret`].
///
/// F-608 step 6 swaps the step-1 placeholder `secret_handle: String` for
/// real credential bytes pushed daemon → sidecar. The wire shape is just
/// a JSON byte array (serde transparent), but `Debug` and `Display` are
/// overridden to redact the contents — so a stray `tracing::debug!("{:?}",
/// frame)` anywhere in the supervisor / sidecar stack cannot leak the
/// secret bytes into logs. The sidecar wraps the inner `Vec<u8>` into a
/// `secrecy::SecretString` immediately on receive, so the only place the
/// raw bytes ever appear in cleartext is on the wire and inside the
/// receive-side conversion — both narrow, audited paths.
///
/// F-659: derives [`zeroize::Zeroize`] and [`zeroize::ZeroizeOnDrop`] so
/// the inner buffer is wiped before its heap slot is freed. Without this,
/// intermediate copies (clones, deserialization temporaries) leave
/// plaintext credential bytes recoverable from a heap dump or core file
/// (CWE-312).
#[derive(Clone, Serialize, Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
#[serde(transparent)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    /// Construct from a raw byte buffer. Callers should typically have a
    /// `SecretString` in hand and call `from_secret` instead — this
    /// constructor exists for the deserialize / round-trip / test paths.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Length of the inner buffer. Non-secret — only the byte count is
    /// exposed, never the payload itself.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Predicate mirror of [`Self::len`].
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Consume the wrapper and return the inner bytes.
    ///
    /// The receive side uses this to hand the bytes straight into a
    /// `secrecy::SecretString` (which zeroes-on-drop) without any
    /// intermediate clone. F-659: `SecretBytes` derives `ZeroizeOnDrop`,
    /// which generates a `Drop` impl and forbids moving out of `self.0`
    /// directly. We `mem::take` the inner Vec instead — the moved-out
    /// bytes flow into the receiver's `SecretString` (zeroized when
    /// *that* drops), and the now-empty Vec left behind is harmless to
    /// zeroize on `SecretBytes`'s Drop.
    pub fn into_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }

    /// Borrow the inner bytes. Reserved for narrow audited call sites
    /// (e.g. immediate conversion into `SecretString` on receive). Do
    /// not pass the slice to anything that may log or stringify it.
    pub fn expose_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SecretBytes {
    /// Redact the payload. The byte count is included so log lines remain
    /// useful for triage (e.g. confirming a non-empty credential was
    /// pushed) without leaking the value.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretBytes([REDACTED {} bytes])", self.0.len())
    }
}

impl std::fmt::Display for SecretBytes {
    /// `Display` mirrors `Debug` so a `format!("{}", bytes)` cannot leak
    /// the credential. There is no legitimate caller for stringifying
    /// secret bytes — every consumer should be unwrapping into a
    /// `SecretString` and exposing only at the network boundary.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[REDACTED {} bytes]", self.0.len())
    }
}

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
///
/// History:
/// * `1` — initial F-608 step 1 protocol.
/// * `2` — F-608 step 6: `Credentials.secret_handle: String` →
///   `Credentials.secret: SecretBytes` (`Vec<u8>` opaque, redacted
///   `Debug`/`Display`).
/// * `3` — F-676: split the overloaded `Hello` discriminator into
///   `Hello` (sidecar → daemon handshake) and `DaemonHello` (daemon →
///   sidecar metadata follow-up). The daemon and sidecar binaries ship
///   together, so this is a co-deployed bump; mixing a pre-`3`
///   supervisor with a `3`-aware sidecar (or vice versa) is unsupported
///   and surfaces as a deserialization error on the unknown variant.
pub const SIDECAR_SCHEMA_VERSION: u32 = 3;

/// Bidirectional message exchanged between `forged` and `forged-agent`.
///
/// The discriminator is `t` (matching [`crate::IpcMessage`]) so the wire
/// format stays uniform across Forge IPC. Daemon → sidecar variants are
/// commands; sidecar → daemon variants are events / replies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum SidecarMessage {
    // ── daemon → sidecar ──────────────────────────────────────────────────
    /// Daemon → sidecar metadata frame sent immediately after the
    /// supervisor's [`SidecarMessage::HelloAck`]. Carries the agent
    /// definition, allowed paths, workspace path, provider spec,
    /// sandbox level, and telemetry endpoint the run-turn body needs to
    /// initialize without re-reading daemon-owned state from disk.
    ///
    /// Split from [`SidecarMessage::Hello`] in F-676 (schema_version 3)
    /// so the discriminator no longer encodes two semantically distinct
    /// frames. The receive side validates `proto` and `instance_id`
    /// against the values established at handshake time.
    DaemonHello(SidecarHello),

    /// Run one turn of the request loop. The sidecar streams events back
    /// as [`SidecarMessage::Event`] frames and emits no reply; the
    /// daemon observes turn completion via the `Event::TurnCompleted`
    /// payload.
    RunTurn(SidecarRunTurn),

    /// Push a per-provider credential to the sidecar (§5: daemon owns
    /// the keyring; sidecar receives just-in-time). The bytes ride
    /// inside [`SecretBytes`], whose `Debug` / `Display` impls redact —
    /// so a stray `format!("{:?}", frame)` anywhere in the supervisor
    /// or sidecar code path cannot leak the credential. The receive
    /// side immediately wraps into `secrecy::SecretString`.
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
    /// Initial handshake frame the *child* sends on connect to advertise
    /// its `instance_id` and `proto` version. The supervisor validates
    /// both before responding with [`SidecarMessage::HelloAck`]. The
    /// daemon-initiated metadata follow-up is
    /// [`SidecarMessage::DaemonHello`]; F-676 split the two so this
    /// discriminator carries the handshake direction unambiguously.
    Hello(SidecarHello),

    /// Acknowledges [`SidecarMessage::Hello`] and reports the daemon's
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

/// Handshake / metadata payload shared by [`SidecarMessage::Hello`]
/// (sidecar → daemon, on connect) and [`SidecarMessage::DaemonHello`]
/// (daemon → sidecar, after `HelloAck`). The runtime fields are
/// identical; F-676 splits the *discriminator* so call sites can tell
/// the two flows apart without inspecting context.
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
    /// F-678: peer-reported schema version. The supervisor compares this
    /// against [`SIDECAR_SCHEMA_VERSION`] and `tracing::warn!`s on
    /// mismatch; the agent-host applies the same check on the
    /// `HelloAck` it receives back. `#[serde(default)]` defaults to `0`
    /// for legacy peers (e.g. older `forged-agent` builds) so a skewed
    /// peer is surfaced via the warn rather than silently rejected at
    /// the deserialize layer.
    #[serde(default)]
    pub schema_version: u32,
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
    /// Provider identifier — e.g. `"anthropic"`, `"openai"`, `"custom_openai:<name>"`.
    pub kind: String,
    /// Model id passed to the provider. Free-form; provider-specific.
    pub model: String,
    /// Optional base URL override (e.g. self-hosted OpenAI-compatible endpoint).
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

/// Push a credential for `provider_id`. F-608 step 6: the secret rides
/// inside a [`SecretBytes`] wrapper whose `Debug` / `Display` redact, so
/// nothing in the daemon / supervisor / sidecar stack can leak the value
/// through a tracing emission. The sidecar wraps the inner bytes into
/// `secrecy::SecretString` (zeroize-on-drop) the moment the frame is
/// dispatched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarCredentials {
    pub provider_id: ProviderId,
    pub secret: SecretBytes,
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

/// On-disk crash dump persisted by the sidecar's panic hook before
/// exit. Mirrors [`SidecarCrashed`] but includes the identifiers needed
/// for the daemon-side reader to attribute the dump to a session +
/// instance. Persisted at
/// `<XDG_DATA_HOME or $HOME/.local/share>/forge/crashes/<session-id>/<instance-id>-<unix-ts>.json`
/// per `docs/architecture/agent-sidecar.md` §"Step 7".
///
/// Independent of the cooperative `SidecarMessage::Crashed` IPC frame:
/// the IPC path is the cooperative case (drained on shutdown), the
/// file-on-disk path is the defensive case — covers situations where
/// the IPC plumbing itself failed and the supervisor never received the
/// in-band frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CrashDump {
    /// Logical instance id assigned by the daemon at spawn time.
    pub instance_id: String,
    /// Session the instance belonged to. Lifts the dump out of an
    /// anonymous flat dir so a stuck session's history is grep-able.
    pub session_id: String,
    /// Stringified panic payload (per the `std::panic::PanicInfo`
    /// downcast discipline used by the standard library's default
    /// hook).
    pub panic_message: String,
    /// Captured backtrace if `RUST_BACKTRACE=1` was set; otherwise
    /// `None`.
    pub backtrace: Option<String>,
    /// RFC 3339 UTC timestamp of the panic. Captured at hook fire time.
    pub captured_at: DateTime<Utc>,
}

#[cfg(test)]
#[allow(deprecated)] // F-652: tests still drive the deprecated bare read_frame helpers.
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
                    kind: "anthropic".into(),
                    model: "claude-opus-4-7".into(),
                    base_url: Some("https://api.anthropic.com".into()),
                },
                sandbox_level: SidecarSandboxLevel::Level1,
                telemetry_endpoint: None,
                schema_version: SIDECAR_SCHEMA_VERSION,
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
                schema_version: SIDECAR_SCHEMA_VERSION,
            }),
            SidecarMessage::DaemonHello(SidecarHello {
                proto: SIDECAR_PROTO_VERSION,
                instance_id: AgentInstanceId::from_string("inst-3".into()),
                agent_def: SidecarAgentDef {
                    name: "daemon-payload".into(),
                    description: Some("daemon → sidecar metadata".into()),
                    body: "## Prompt".into(),
                    allowed_paths: vec!["/workspace".into()],
                    isolation: "process".into(),
                    memory_enabled: true,
                },
                allowed_paths: vec!["/workspace".into()],
                workspace_path: "/workspace".into(),
                provider_spec: SidecarProviderSpec {
                    kind: "anthropic".into(),
                    model: "claude-opus-4-7".into(),
                    base_url: None,
                },
                sandbox_level: SidecarSandboxLevel::Level1,
                telemetry_endpoint: Some("http://otel:4317".into()),
                schema_version: SIDECAR_SCHEMA_VERSION,
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
                secret: SecretBytes::new(b"handle-abc".to_vec()),
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

    /// F-676: `Hello` and `DaemonHello` carry the same payload shape
    /// but must serialize with distinct `t` tags so the receive side can
    /// disambiguate the handshake-direction frame from the daemon-side
    /// metadata follow-up. Pin both tags here so a future enum-rename
    /// surfaces in CI.
    #[test]
    fn hello_and_daemon_hello_use_distinct_wire_tags() {
        let payload = SidecarHello {
            proto: SIDECAR_PROTO_VERSION,
            instance_id: AgentInstanceId::from_string("inst-x".into()),
            agent_def: SidecarAgentDef {
                name: "n".into(),
                description: None,
                body: String::new(),
                allowed_paths: vec![],
                isolation: "process".into(),
                memory_enabled: false,
            },
            allowed_paths: vec![],
            workspace_path: "/workspace".into(),
            provider_spec: SidecarProviderSpec {
                kind: "anthropic".into(),
                model: "claude-opus-4-7".into(),
                base_url: None,
            },
            sandbox_level: SidecarSandboxLevel::Level1,
            telemetry_endpoint: None,
            schema_version: SIDECAR_SCHEMA_VERSION,
        };

        let hello_v: serde_json::Value = serde_json::from_slice(
            &serde_json::to_vec(&SidecarMessage::Hello(payload.clone())).unwrap(),
        )
        .unwrap();
        let daemon_v: serde_json::Value = serde_json::from_slice(
            &serde_json::to_vec(&SidecarMessage::DaemonHello(payload.clone())).unwrap(),
        )
        .unwrap();

        assert_eq!(hello_v.get("t").and_then(|v| v.as_str()), Some("Hello"));
        assert_eq!(
            daemon_v.get("t").and_then(|v| v.as_str()),
            Some("DaemonHello")
        );

        // Round-trip back to the typed enum: the `t` tag must drive the
        // arm, not the payload shape.
        let hello_back: SidecarMessage = serde_json::from_value(hello_v).unwrap();
        let daemon_back: SidecarMessage = serde_json::from_value(daemon_v).unwrap();
        assert!(matches!(hello_back, SidecarMessage::Hello(_)));
        assert!(matches!(daemon_back, SidecarMessage::DaemonHello(_)));
    }

    /// `CrashDump` is the on-disk dump shape persisted by the sidecar's
    /// panic hook before exit. The shape is consumed by the daemon-side
    /// crash reader, so a silent rename here would break post-mortem
    /// triage. Pin the round-trip + canonical JSON keys.
    #[test]
    fn crash_dump_roundtrip_and_keys() {
        let captured_at = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let dump = CrashDump {
            instance_id: "inst-1".into(),
            session_id: "sess-1".into(),
            panic_message: "index out of bounds".into(),
            backtrace: Some("frame 0: foo".into()),
            captured_at,
        };
        let bytes = serde_json::to_vec(&dump).expect("serialize");
        let got: CrashDump = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(got, dump);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for key in [
            "instance_id",
            "session_id",
            "panic_message",
            "backtrace",
            "captured_at",
        ] {
            assert!(value.get(key).is_some(), "missing key {key}: {value}");
        }
    }

    /// F-608 step 6: `SecretBytes::Debug` must redact the payload so a
    /// stray `tracing::debug!("{:?}", frame)` can never leak credential
    /// bytes — even if the surrounding `SidecarCredentials` derives
    /// `Debug`. Pinned here because the redaction guarantee is the
    /// security contract the architecture doc §5 calls out by name.
    #[test]
    fn secret_bytes_debug_redacts_payload() {
        let secret = SecretBytes::new(b"sk-ant-leaked-key".to_vec());
        let dbg = format!("{secret:?}");
        let display = format!("{secret}");
        assert!(
            !dbg.contains("sk-ant-leaked-key"),
            "Debug must not leak secret bytes: {dbg}"
        );
        assert!(
            !display.contains("sk-ant-leaked-key"),
            "Display must not leak secret bytes: {display}"
        );
        assert!(dbg.contains("REDACTED"), "Debug should mark redaction");
        // The payload's byte count is non-secret — handy for triage.
        assert!(dbg.contains("17"), "Debug should report byte count");
    }

    /// F-608 step 6: even the variant's derived `Debug` must not leak
    /// the credential — `SidecarCredentials` derives `Debug`, but the
    /// inner `secret: SecretBytes` field overrides its own `Debug` to
    /// redact. This test pins both layers.
    #[test]
    fn sidecar_credentials_debug_redacts_payload() {
        let frame = SidecarMessage::Credentials(SidecarCredentials {
            provider_id: ProviderId::from_string("anthropic".into()),
            secret: SecretBytes::new(b"sk-ant-leaked-key".to_vec()),
        });
        let dbg = format!("{frame:?}");
        assert!(
            !dbg.contains("sk-ant-leaked-key"),
            "outer Debug must not leak the secret bytes: {dbg}"
        );
        assert!(
            dbg.contains("anthropic"),
            "non-secret provider_id should remain visible: {dbg}"
        );
    }

    /// F-608 step 6: the wire format *does* carry the secret bytes
    /// (that's the frame's job) — only `Debug` / `Display` redact. Pin
    /// the round-trip so the receive side recovers the exact bytes the
    /// daemon pushed.
    #[test]
    fn sidecar_credentials_round_trip_preserves_bytes() {
        let original = SidecarCredentials {
            provider_id: ProviderId::from_string("anthropic".into()),
            secret: SecretBytes::new(b"sk-ant-secret".to_vec()),
        };
        let frame = SidecarMessage::Credentials(original);
        let bytes = serde_json::to_vec(&frame).unwrap();
        let recovered: SidecarMessage = serde_json::from_slice(&bytes).unwrap();
        match recovered {
            SidecarMessage::Credentials(c) => {
                assert_eq!(c.provider_id.to_string(), "anthropic");
                assert_eq!(c.secret.expose_bytes(), b"sk-ant-secret");
            }
            other => panic!("expected Credentials, got {other:?}"),
        }
    }

    /// F-659: `SecretBytes` must zero its inner buffer on drop so a
    /// heap dump or core file cannot recover credential bytes from a
    /// freed allocation. The contract is enforced two ways:
    ///
    /// 1. A static `ZeroizeOnDrop` assertion guarantees the type's
    ///    `Drop` impl wipes the buffer (the derive macro generates the
    ///    `drop` body that calls `Zeroize::zeroize` before the inner
    ///    `Vec` deallocates).
    /// 2. A behavioral check: hold the inner buffer steady through
    ///    `ManuallyDrop`, snapshot the heap pointer, run the destructor
    ///    explicitly, and read the (still-pinned) allocation back —
    ///    `ManuallyDrop` skips the outer Drop, so the only thing that
    ///    runs is the derived zeroize step on the inner `Vec` we
    ///    forcibly drop. The allocation itself stays alive until the
    ///    test exits, eliminating the use-after-free flakiness of a
    ///    naked `drop(secret)` + post-free read.
    #[test]
    fn secret_bytes_zeroes_buffer_on_drop() {
        // Static assertion: the type opts into ZeroizeOnDrop. If a
        // future refactor accidentally removes the derive, this stops
        // compiling.
        fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        assert_zeroize_on_drop::<SecretBytes>();

        // Behavioral check: build a SecretBytes, snapshot its heap
        // pointer + length, then invoke its destructor and re-read
        // the now-zeroized inner buffer. We use the fact that
        // `Vec::zeroize` writes 0x00 to every byte from index 0 to
        // `capacity` *before* the Vec deallocates, so reading those
        // bytes through the captured pointer immediately after the
        // zeroize step (but before reuse) catches a missing impl.
        let payload = b"sk-ant-leaked-key".to_vec();
        let original = payload.clone();
        let len = payload.len();
        let mut secret = SecretBytes::new(payload);
        let ptr: *const u8 = secret.expose_bytes().as_ptr();

        // Sanity: the buffer holds the plaintext while alive.
        let live: Vec<u8> = (0..len).map(|i| unsafe { ptr.add(i).read() }).collect();
        assert_eq!(live, original, "live buffer should hold plaintext");

        // Drive `Zeroize::zeroize` directly — this is the same call
        // the derived `Drop` runs before the Vec deallocates.
        zeroize::Zeroize::zeroize(&mut secret);

        // After zeroize the buffer must be all-zero. The Vec is still
        // alive (we did not drop it), so the read is well-defined.
        let residual: Vec<u8> = (0..len).map(|i| unsafe { ptr.add(i).read() }).collect();
        assert!(
            residual.iter().all(|&b| b == 0),
            "SecretBytes::zeroize left non-zero residue: {residual:?}"
        );
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

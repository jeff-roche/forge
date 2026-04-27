//! F-608 step 3: transport-agnostic event emission seam.
//!
//! `run_turn` and `run_request_loop` (in `forge-session`) historically
//! held a concrete `Arc<Session>` and called `session.emit(Event)` on
//! every event. With the agent sidecar landing in F-608, the same
//! orchestrator body needs to run in two contexts:
//!
//! * **In-process (daemon)** — events flow into the local
//!   `forge_session::session::Session`'s durable event log + broadcast.
//! * **Sidecar (`forged-agent`)** — events leave the process as
//!   `SidecarMessage::Event(SidecarEvent { seq, event })` frames over
//!   the per-instance Unix domain socket; the daemon writes them to the
//!   session log on the receiving end.
//!
//! [`EventSink`] is the single trait every emitter sees. The trait
//! lives in `forge-core` (rather than `forge-session`) so the sidecar
//! crate can implement it without taking a dependency on the daemon's
//! persistence / IPC server / MCP machinery — the architecture doc
//! Open Questions §1 calls out keeping the sidecar's compile graph
//! minimal as the explicit non-goal that motivates this placement.

use async_trait::async_trait;

use crate::event::Event;

/// One-way emission surface for an [`Event`].
///
/// Implementors are responsible for whatever durability + fan-out their
/// transport requires:
///
/// * The daemon's `Session` flushes the event to its on-disk
///   `EventLog` and broadcasts it to subscribed IPC clients.
/// * The sidecar's `IpcEventSink` allocates a monotonic `seq`, frames
///   the event as a `SidecarMessage::Event`, and writes it on the UDS.
///
/// `Send + Sync` is required because both the orchestrator's stream
/// loop and the parallel-tool dispatch path hand sinks across `tokio`
/// task boundaries (`Arc<dyn EventSink>` clones into spawned tasks).
///
/// The error type is [`anyhow::Error`] to match the orchestrator's
/// existing error-propagation shape: today every `session.emit(...)?`
/// flows through `anyhow::Result<()>`. Concrete sink implementations
/// may map their internal typed errors (e.g.
/// `forge_session::SessionError`, `tokio::io::Error`) into
/// `anyhow::Error` at the boundary.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Append `event` to whatever durable channel this sink represents.
    ///
    /// Failures are surfaced to the caller — the orchestrator
    /// propagates them out of `run_turn` so a misbehaving sink does
    /// not silently lose events. The IPC sink in `forged-agent`
    /// converts a write failure into a hard turn error so the
    /// supervisor can observe the connection drop and recycle the
    /// sidecar.
    async fn emit(&self, event: Event) -> anyhow::Result<()>;
}

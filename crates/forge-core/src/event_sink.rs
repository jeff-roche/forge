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
    ///
    /// # Error contract (sidecar semantics)
    ///
    /// Implementors classify their internal failures into one of two
    /// shapes before mapping into [`anyhow::Error`]:
    ///
    /// * **Fatal** — the underlying transport / log is unrecoverable
    ///   for the *current turn*. Examples: the sidecar's UDS write
    ///   half is closed, the daemon's `EventLog` returned an I/O
    ///   error from a full disk, the broadcast channel has zero live
    ///   receivers AND the persistence write also failed. The caller
    ///   (`run_turn`) treats the error as terminal: the turn aborts
    ///   and (in the sidecar case) the supervisor recycles the
    ///   process so a clean handshake can re-establish the channel.
    ///
    ///   Fatal errors propagate unchanged through `emit(...)?` — do
    ///   *not* swallow them and `Ok(())` to keep the turn alive.
    ///   Silent loss of an event corrupts the post-mortem record and
    ///   defeats the durable-log invariant.
    ///
    /// * **Recoverable** — a transient sub-failure that does *not*
    ///   invalidate the channel. Examples: a broadcast receiver lag
    ///   when at least one subscriber is still attached, a
    ///   best-effort observer that doesn't gate the turn (telemetry
    ///   exporters, mirror sinks). Implementors handle these
    ///   internally — log via `tracing` and return `Ok(())`. They
    ///   must *not* propagate via the trait, because every caller
    ///   treats a returned error as fatal per the rule above.
    ///
    /// In short: if a sink returns `Err`, the turn ends. Anything a
    /// sink can absorb without ending the turn must be absorbed
    /// inside the sink, with a `tracing::warn!` (or lower) for
    /// visibility, before returning `Ok(())`.
    async fn emit(&self, event: Event) -> anyhow::Result<()>;
}

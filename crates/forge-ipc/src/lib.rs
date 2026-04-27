use anyhow::bail;
pub use forge_core::RerunVariant;
// F-155: the MCP state + response shapes flow verbatim over UDS, so
// re-export here alongside `RerunVariant` for callers that prefer a single
// IPC import path.
pub use forge_core::{McpStateEvent, ServerState};
pub use forge_mcp::McpServerInfo;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTO_VERSION: u32 = 1;
pub const SCHEMA_VERSION: u32 = 1;

/// Hard cap on a single IPC frame body, enforced on both the write side
/// (pre-send in [`write_frame`]) and the read side (pre-allocation in
/// [`read_frame`], before the body buffer is sized). 4 MiB is generous
/// for every `IpcMessage` variant the session emits today while still
/// capping the blast radius of a malicious peer claiming a huge
/// length prefix.
const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum IpcMessage {
    Hello(Hello),
    HelloAck(HelloAck),
    Subscribe(Subscribe),
    Event(IpcEvent),
    SendUserMessage(SendUserMessage),
    ToolCallApproved(ToolCallApproved),
    ToolCallRejected(ToolCallRejected),
    /// F-143: client → session request to re-run an assistant message.
    RerunMessage(RerunMessage),
    /// F-144: client → session request to activate a specific branch variant.
    SelectBranch(SelectBranch),
    /// F-145: client → session request to tombstone a branch variant.
    DeleteBranch(DeleteBranch),
    /// F-598: client → session request to compact the transcript.
    /// The daemon dispatches to `forge_session::compaction::compact` and
    /// (on success) emits `Event::ContextCompacted` through the event
    /// stream — there is no direct response payload here.
    CompactTranscript(CompactTranscript),
    /// F-155: client → session request for the daemon's MCP server list.
    /// Response arrives as [`IpcMessage::McpServersList`].
    ListMcpServers(ListMcpServers),
    /// F-155: daemon → client response carrying the snapshot the daemon's
    /// authoritative `McpManager::list` returned.
    McpServersList(McpServersList),
    /// F-155: client → session request to toggle an MCP server on/off. The
    /// daemon acts on its single authoritative `McpManager` so a running
    /// session's tool dispatch is affected. Response arrives as
    /// [`IpcMessage::McpToggleResult`] — `error` is `Some` when the name
    /// is unknown or the lifecycle transition failed.
    ToggleMcpServer(ToggleMcpServer),
    /// F-155: daemon → client response for a [`IpcMessage::ToggleMcpServer`].
    McpToggleResult(McpToggleResult),
    /// F-155: client → session request to import a third-party MCP config
    /// into the workspace `.mcp.json`. `apply=false` runs a dry import —
    /// the daemon computes the new server set and returns it without
    /// rewriting the file. Response arrives as
    /// [`IpcMessage::McpImportResult`].
    ImportMcpConfig(ImportMcpConfig),
    /// F-155: daemon → client response for a [`IpcMessage::ImportMcpConfig`].
    McpImportResult(McpImportResult),
    /// F-603: client → session request to pause the orchestrator at the
    /// next inter-step checkpoint. The daemon completes any in-flight
    /// step (tool call, model stream) before the pause takes effect, then
    /// emits `Event::SessionPaused`. Pausing an already-paused session is
    /// a no-op and emits no second event.
    PauseSession(PauseSession),
    /// F-603: client → session request to resume a paused orchestrator.
    /// The daemon emits `Event::SessionResumed` and the next
    /// `StepStarted` opens. Resuming an already-running session is a
    /// no-op and emits no second event.
    ResumeSession(ResumeSession),
}

/// Client → session: re-run the assistant message with `msg_id` using the
/// given `variant`. All three variants (`Replace`, `Branch`, `Fresh`) are
/// wired as of F-144.
#[derive(Debug, Serialize, Deserialize)]
pub struct RerunMessage {
    /// Target assistant message to re-run. Wire shape is the canonical
    /// `MessageId` string to stay symmetric with `ToolCallApproved.id`.
    pub msg_id: String,
    pub variant: RerunVariant,
}

/// Client → session: activate a specific branch variant for replay / UI.
///
/// `parent` is the branch-point message id (the root variant's own id). The
/// daemon resolves `variant_index` against the event log: `0` refers to the
/// root itself; `N >= 1` refers to the `AssistantMessage` with
/// `branch_parent == Some(parent)` and `branch_variant_index == N`. On a
/// successful resolve, the daemon emits `Event::BranchSelected { parent,
/// selected }` where `selected` is the resolved MessageId.
#[derive(Debug, Serialize, Deserialize)]
pub struct SelectBranch {
    pub parent_id: String,
    pub variant_index: u32,
}

/// Client → session: tombstone a specific branch variant (F-145).
///
/// `parent_id` is the branch-point message id; `variant_index` identifies the
/// sibling to delete (0 = the root variant, N >= 1 = the Nth branch sibling).
/// The daemon resolves the target against the event log and emits
/// `Event::BranchDeleted { parent, variant_index }`. Deleting `variant_index
/// == 0` is **not** rejected here — the orchestrator may decide policy
/// (e.g. refuse when it would orphan every sibling).
#[derive(Debug, Serialize, Deserialize)]
pub struct DeleteBranch {
    pub parent_id: String,
    pub variant_index: u32,
}

/// Client → session: send a user message to start a new turn.
#[derive(Debug, Serialize, Deserialize)]
pub struct SendUserMessage {
    pub text: String,
}

/// F-598: client → session: ask the daemon to compact the active session's
/// transcript via the privileged summary path. No fields today — the
/// daemon resolves the session from the connection. On success, a
/// `ContextCompacted` event arrives through the event stream; on failure
/// the daemon logs and the stream stays unchanged.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct CompactTranscript {}

/// F-603: client → session: pause the orchestrator at the next inter-step
/// checkpoint. No fields today — the daemon resolves the session from the
/// connection. The daemon flips an in-memory `Paused` flag; the
/// orchestrator's request loop checks it between steps and parks the turn
/// (in-flight tool calls run to completion first). A `SessionPaused` event
/// fires on the `Running → Paused` transition; redundant pauses log
/// `debug!` and emit nothing.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct PauseSession {}

/// F-603: client → session: resume a paused orchestrator. No fields today
/// — the daemon resolves the session from the connection. The daemon
/// clears the `Paused` flag; any orchestrator awaiting the pause
/// checkpoint wakes and re-enters its run loop. A `SessionResumed` event
/// fires on the `Paused → Running` transition; redundant resumes log
/// `debug!` and emit nothing.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct ResumeSession {}

/// F-155: client → session: list the daemon's managed MCP servers.
///
/// No fields today — the daemon reports its own merged view built from
/// `<workspace>/.mcp.json` + `~/.mcp.json` at session start. A future
/// revision may add a selector (e.g. filter by server name) but the
/// frontend doesn't need one.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct ListMcpServers {}

/// F-155: daemon → client response carrying the snapshot returned by
/// `McpManager::list()`.
#[derive(Debug, Serialize, Deserialize)]
pub struct McpServersList {
    pub servers: Vec<McpServerInfo>,
}

/// F-155: client → session request to toggle an MCP server on or off.
///
/// `enabled` is the *target* state: `true` starts the server if it is not
/// already running; `false` disables it (parks in `ServerState::Disabled`
/// so the canonical "server disabled" error surfaces for in-flight /
/// subsequent tool calls).
#[derive(Debug, Serialize, Deserialize)]
pub struct ToggleMcpServer {
    pub name: String,
    pub enabled: bool,
}

/// F-155: daemon → client response for a `ToggleMcpServer`. `error` is
/// `None` on success; when `Some`, the toggle was rejected (unknown
/// server, lifecycle transition failed) and `enabled_after` reports the
/// *pre-toggle* state so the UI can reconcile without round-tripping
/// `ListMcpServers`.
#[derive(Debug, Serialize, Deserialize)]
pub struct McpToggleResult {
    pub name: String,
    pub enabled_after: bool,
    pub error: Option<String>,
}

/// F-155: client → session request to import a third-party MCP config
/// into the workspace's universal `.mcp.json`. `source` is the slug
/// accepted by `forge_mcp::import::ImportSource::from_slug`. When `apply`
/// is `false` the daemon runs a dry import — it returns the set of
/// names that *would* be imported and leaves the on-disk config
/// untouched; `true` rewrites the workspace file and rebuilds the
/// manager.
#[derive(Debug, Serialize, Deserialize)]
pub struct ImportMcpConfig {
    pub source: String,
    pub apply: bool,
}

/// F-155: daemon → client response for an `ImportMcpConfig`.
///
/// On success `imported` lists the server names that were applied (or
/// would be applied under `apply=false`). `destination_path` is the
/// absolute path of the rewritten workspace `.mcp.json`; empty when the
/// import was a dry-run. `error` is `Some` when the import failed (bad
/// slug, source file unreadable, write failed, etc.).
#[derive(Debug, Serialize, Deserialize)]
pub struct McpImportResult {
    pub source: String,
    pub imported: Vec<String>,
    pub destination_path: String,
    pub error: Option<String>,
}

/// Client → session: approve a pending tool call.
#[derive(Debug, Serialize, Deserialize)]
pub struct ToolCallApproved {
    /// The ToolCallId to approve.
    pub id: String,
    /// Approval scope: "Once" | "ThisFile" | "ThisPattern" | "ThisTool".
    pub scope: String,
}

/// Client → session: reject a pending tool call.
#[derive(Debug, Serialize, Deserialize)]
pub struct ToolCallRejected {
    pub id: String,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Subscribe {
    pub since: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcEvent {
    pub seq: u64,
    // F-112: typed `Event` (not `serde_json::Value`).
    //
    // Previously the emission path was `Event -> serde_json::Value -> bytes`,
    // which forced serde to walk the dynamic tagged-union `Value` tree once
    // to construct it and a second time to write it out. Carrying the typed
    // `Event` directly collapses the pipeline to a single static traversal:
    // `Event -> bytes`. The wire shape is identical (serde flattens nested
    // structs the same way regardless of intermediate `Value`), so IPC
    // peers and the TS adapter see no change.
    pub event: forge_core::Event,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u32,
    pub client: ClientInfo,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClientInfo {
    pub kind: String,
    pub pid: u32,
    pub user: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HelloAck {
    pub session_id: String,
    pub workspace: String,
    pub started_at: String,
    pub event_seq: u64,
    pub schema_version: u32,
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &IpcMessage,
) -> anyhow::Result<()> {
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_FRAME_SIZE {
        bail!("frame too large: {} bytes", body.len());
    }
    writer.write_u32(body.len() as u32).await?;
    writer.write_all(&body).await?;
    Ok(())
}

/// Read a single IPC frame, allocating a fresh body buffer per call.
///
/// Convenience wrapper over [`read_frame_into`] for the cold paths
/// (handshake, tests, CLI tools) where the per-frame allocation is
/// negligible. Hot pumping loops (assistant-token streaming) should
/// hoist a `Vec<u8>` outside the loop and call [`read_frame_into`]
/// directly to amortize the allocation across frames (F-565).
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<IpcMessage> {
    let mut buf = Vec::new();
    read_frame_into(reader, &mut buf).await
}

/// F-565: read a single IPC frame into the caller-owned `buf`, reusing
/// its capacity across calls. The buffer is resized (without zero-fill
/// of already-allocated capacity) to the frame body length, then filled
/// with `read_exact`. Decoded `IpcMessage` borrows nothing from `buf`,
/// so the caller may immediately reuse it for the next frame.
///
/// Replaces a `vec![0u8; len]` per call — at streaming-token rates that
/// is one heap alloc + one zero-fill per token, both removed here.
pub async fn read_frame_into<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> anyhow::Result<IpcMessage> {
    let len = reader.read_u32().await? as usize;
    if len > MAX_FRAME_SIZE {
        bail!("frame too large: {} bytes", len);
    }
    // `resize` only zero-fills the *grown* slice; bytes within existing
    // capacity are reused without re-zeroing, and `read_exact` overwrites
    // every byte before `from_slice` observes them.
    buf.resize(len, 0);
    reader.read_exact(&mut buf[..len]).await?;
    let msg = serde_json::from_slice(&buf[..len])?;
    Ok(msg)
}

/// F-354: read a frame subject to `deadline`. Prevents a silent or
/// stalled peer from hanging the caller forever — without this, a local
/// same-uid attacker could open a UDS connection, send zero bytes, and
/// tie up a daemon task until shutdown (CWE-400 / CWE-770).
///
/// Returns an error if `deadline` elapses before either the length
/// prefix or the frame body is fully read. The underlying reader is
/// left in an indeterminate state; callers should drop the connection
/// on error rather than retrying.
pub async fn read_frame_with_deadline<R: AsyncRead + Unpin>(
    reader: &mut R,
    deadline: Duration,
) -> anyhow::Result<IpcMessage> {
    let mut buf = Vec::new();
    read_frame_into_with_deadline(reader, &mut buf, deadline).await
}

/// F-565: deadline-bounded variant of [`read_frame_into`]. See
/// [`read_frame_with_deadline`] for the deadline semantics.
pub async fn read_frame_into_with_deadline<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    deadline: Duration,
) -> anyhow::Result<IpcMessage> {
    match tokio::time::timeout(deadline, read_frame_into(reader, buf)).await {
        Ok(inner) => inner,
        Err(_) => bail!("ipc read timed out after {:?}", deadline),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixStream;

    fn hello_msg() -> IpcMessage {
        IpcMessage::Hello(Hello {
            proto: PROTO_VERSION,
            client: ClientInfo {
                kind: "test".to_string(),
                pid: 1234,
                user: "alice".to_string(),
            },
        })
    }

    fn hello_ack_msg() -> IpcMessage {
        IpcMessage::HelloAck(HelloAck {
            session_id: "sess-abc".to_string(),
            workspace: "/tmp/ws".to_string(),
            started_at: "2024-01-01T00:00:00Z".to_string(),
            event_seq: 7,
            schema_version: SCHEMA_VERSION,
        })
    }

    #[tokio::test]
    async fn round_trips_hello_over_unix_stream() {
        let (mut a, mut b) = UnixStream::pair().unwrap();

        let sent = hello_msg();
        write_frame(&mut a, &sent).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();

        let sent_json = serde_json::to_string(&sent).unwrap();
        let got_json = serde_json::to_string(&got).unwrap();
        assert_eq!(sent_json, got_json);
    }

    #[tokio::test]
    async fn round_trips_hello_ack_over_unix_stream() {
        let (mut a, mut b) = UnixStream::pair().unwrap();

        let sent = hello_ack_msg();
        write_frame(&mut a, &sent).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();

        let sent_json = serde_json::to_string(&sent).unwrap();
        let got_json = serde_json::to_string(&got).unwrap();
        assert_eq!(sent_json, got_json);
    }

    /// F-354: a silent peer must not stall `read_frame_with_deadline` past
    /// the supplied deadline. Without the deadline wrapper, `read_u32`
    /// blocks forever.
    #[tokio::test]
    async fn read_frame_with_deadline_fires_on_silent_peer() {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut reader = a;

        let started = std::time::Instant::now();
        let result = read_frame_with_deadline(&mut reader, Duration::from_millis(100)).await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "expected deadline error, got {:?}", result);
        assert!(
            elapsed < Duration::from_millis(500),
            "deadline did not fire promptly: {:?}",
            elapsed
        );
    }

    /// F-354: the deadline wrapper must succeed when the peer writes a
    /// frame within the deadline.
    #[tokio::test]
    async fn read_frame_with_deadline_succeeds_before_deadline() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let sent = hello_msg();
        tokio::spawn(async move {
            write_frame(&mut a, &sent).await.unwrap();
        });

        let got = read_frame_with_deadline(&mut b, Duration::from_secs(5))
            .await
            .expect("frame should arrive before deadline");
        matches!(got, IpcMessage::Hello(_));
    }
}

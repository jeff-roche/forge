//! Tauri ↔ session UDS bridge.
//!
//! The Tauri commands in [`crate::ipc`] are thin wrappers over this module.
//! This split keeps the bridge logic testable without a live Tauri runtime:
//! tests spawn a real `forge-session` daemon, drive the bridge via
//! [`SessionBridge`], and capture forwarded events through a generic
//! [`EventSink`].
//!
//! **Locking discipline (F-109):** no function in this file may hold the
//! `SessionConnections` map lock (`inner`) across an `.await` point. The
//! map lock serializes every Tauri command's lookup; holding it across a
//! UDS write stalls concurrent commands on unrelated sessions. The pattern
//! is always: acquire → capture the per-connection handle (writer `Arc`,
//! reader `Option`) → drop → await → re-acquire briefly to install state.
//! The `clippy::await_holding_lock` deny below is structural documentation:
//! it catches `std::sync::Mutex` regressions even though it does not cover
//! `tokio::sync::Mutex` (tokio's guards are deliberately excluded from that
//! lint). Reviewers and the accompanying concurrency regression test are
//! the enforcement for the tokio case.
#![deny(clippy::await_holding_lock)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use forge_core::{ApprovalScope, RerunVariant};
use forge_ipc::{
    read_frame, read_frame_into, write_frame, ClientInfo, CompactTranscript, DeleteBranch, Hello,
    HelloAck, ImportMcpConfig, InterruptSession, IpcMessage, ListMcpServers, McpImportResult,
    McpServersList, McpToggleResult, PauseSession, RefineHandoff, RerunMessage, ResumeSession,
    SelectBranch, SendUserMessage, Subscribe, ToggleMcpServer, ToolCallApproved, ToolCallRejected,
    PROTO_VERSION,
};
use serde::Serialize;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Payload emitted to the webview for every session event forwarded by the
/// background reader task. `event` is the daemon's typed [`forge_core::Event`]
/// — Tauri serializes the full payload once when it calls `AppHandle::emit`,
/// so nothing on this path touches `serde_json::Value`.
///
/// F-112: prior to this change `event: serde_json::Value` forced a second
/// serialization hop (Event → Value → bytes). Carrying typed `Event` here
/// keeps the whole emit path at a single static traversal without altering
/// the wire shape observed by the webview (`#[serde(flatten)]`-equivalent —
/// serde walks the nested struct the same way regardless).
#[derive(Debug, Clone, Serialize)]
pub struct SessionEventPayload {
    pub session_id: String,
    pub seq: u64,
    pub event: forge_core::Event,
}

/// Sink for forwarded session events. In production the Tauri command layer
/// implements this by calling `AppHandle::emit("session:event", payload)`;
/// tests use an in-memory channel to assert end-to-end delivery.
pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, payload: SessionEventPayload);
}

/// A single open session connection: a writer half used for send/approve/reject
/// plus an optional background reader task that pumps events into the sink.
struct Connection {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    reader: Option<OwnedReadHalf>,
    reader_task: Option<JoinHandle<()>>,
    /// F-155: shared with the event-pump task so responses to MCP
    /// request frames (`McpServersList`, `McpToggleResult`,
    /// `McpImportResult`) can be routed back to the Tauri command
    /// awaiting them. Every other frame the pump sees still flows to
    /// the event sink unchanged.
    mcp_replies: Arc<McpReplySlots>,
}

/// Per-kind reply slot queues used by `pump_events` to correlate
/// daemon→client request-response frames (today: MCP responses + the
/// F-604 refine handoff) with the Tauri commands that issued them.
///
/// Each command flow is: acquire the per-kind lock, register a
/// one-shot receiver, send the request frame, await the receiver. The
/// pump pops the oldest waiter from the queue when it observes a
/// matching response frame. Serialising by kind (rather than by any
/// correlation id) avoids adding request-id wire bytes for a low-traffic
/// command while still tolerating pipelined requests of different kinds.
#[derive(Default)]
pub(crate) struct McpReplySlots {
    list: Mutex<std::collections::VecDeque<tokio::sync::oneshot::Sender<McpServersList>>>,
    toggle: Mutex<std::collections::VecDeque<tokio::sync::oneshot::Sender<McpToggleResult>>>,
    import: Mutex<std::collections::VecDeque<tokio::sync::oneshot::Sender<McpImportResult>>>,
    /// F-604: refine-handoff response from `IpcMessage::InterruptSession`.
    interrupt: Mutex<std::collections::VecDeque<tokio::sync::oneshot::Sender<RefineHandoff>>>,
}

/// Session-id keyed registry of active bridge connections.
///
/// **F-122 workspace-root cache.** `workspace_roots` stores the canonical
/// workspace path the daemon returned in `HelloAck.workspace` for each
/// session. The editor-pane filesystem commands (`read_file`, `write_file`,
/// `tree`) look up this value server-side instead of trusting a webview
/// parameter — a compromised or buggy webview cannot widen its sandbox by
/// claiming `workspace = /` because the server always consults the cached
/// value. Populated in [`SessionBridge::hello`] after the `HelloAck`
/// returns.
///
/// TODO: once a `session_disconnect` command lands, drop the matching
/// `workspace_roots` entry alongside the `inner` entry so a recycled
/// `session_id` can't reuse a stale cache.
#[derive(Clone, Default)]
pub struct SessionConnections {
    inner: Arc<Mutex<HashMap<String, Connection>>>,
    workspace_roots: Arc<Mutex<HashMap<String, PathBuf>>>,
}

impl SessionConnections {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }

    pub async fn contains(&self, session_id: &str) -> bool {
        self.inner.lock().await.contains_key(session_id)
    }

    /// Return the cached canonical workspace root for `session_id`, or `None`
    /// when `session_hello` has not yet populated the cache. The `PathBuf`
    /// is cloned — the map lock is released before the caller awaits
    /// anything.
    pub async fn workspace_root(&self, session_id: &str) -> Option<PathBuf> {
        self.workspace_roots.lock().await.get(session_id).cloned()
    }

    /// F-122 test seam: prime the workspace-root cache so integration tests
    /// can exercise `read_file` / `write_file` / `tree` without running a
    /// live `session_hello` handshake. Gated behind `webview-test` so
    /// production builds cannot reach it. Mirrors the existing
    /// `test_socket_override` / `test_user_config_dir_override` pattern on
    /// [`crate::ipc::BridgeState`] — tests use tempdir-rooted workspaces and
    /// bypass the UDS.
    #[cfg(feature = "webview-test")]
    #[doc(hidden)]
    pub async fn prime_workspace_root_for_test(
        &self,
        session_id: impl Into<String>,
        root: PathBuf,
    ) {
        self.workspace_roots
            .lock()
            .await
            .insert(session_id.into(), root);
    }
}

/// Resolve the default socket path for a session id, following the same
/// rules as `forge-session::socket_path::resolve_socket_path`. Exposed so
/// the Tauri command layer and tests agree on the convention.
///
/// Returns `Err` when no per-user runtime directory can be established:
///   - Linux without `XDG_RUNTIME_DIR`: errors (F-044 / H8 — no `/tmp`
///     fallback, socket there would be world-connectable).
///   - macOS without `XDG_RUNTIME_DIR`: falls back to
///     `$HOME/Library/Application Support/Forge/run` at `0o700` (F-339).
///     The per-user `0o700` invariant is preserved, just via a natively-
///     appropriate path rather than relaxed into a shared directory.
///
/// Delegates the runtime-dir policy to [`forge_core::runtime_dir::runtime_dir`]
/// so every callsite (daemon, CLI, shell) agrees on the same rules.
pub fn default_socket_path(session_id: &str) -> Result<PathBuf> {
    Ok(forge_core::runtime_dir::runtime_dir()?
        .join("forge/sessions")
        .join(format!("{session_id}.sock")))
}

/// Top-level bridge operations invoked by Tauri commands. Keyed on
/// [`SessionConnections`]; safe to clone and share across command handlers.
#[derive(Clone)]
pub struct SessionBridge {
    connections: SessionConnections,
}

impl SessionBridge {
    pub fn new(connections: SessionConnections) -> Self {
        Self { connections }
    }

    pub fn connections(&self) -> &SessionConnections {
        &self.connections
    }

    /// Open (or reuse) a UDS connection for `session_id` and perform the
    /// framed `Hello`/`HelloAck` handshake. Returns the daemon's ack.
    ///
    /// If `socket_path` is `None`, [`default_socket_path`] is used.
    pub async fn hello(&self, session_id: &str, socket_path: Option<&Path>) -> Result<HelloAck> {
        {
            let map = self.connections.inner.lock().await;
            if map.contains_key(session_id) {
                return Err(anyhow!(
                    "session_hello: already connected to session {session_id}"
                ));
            }
        }

        let path: PathBuf = match socket_path {
            Some(p) => p.to_path_buf(),
            None => default_socket_path(session_id)?,
        };
        let stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("connect UDS {}", path.display()))?;
        let (mut reader, mut writer) = stream.into_split();

        let hello = IpcMessage::Hello(Hello {
            proto: PROTO_VERSION,
            client: ClientInfo {
                kind: "shell".to_string(),
                pid: std::process::id(),
                user: std::env::var("USER").unwrap_or_else(|_| "forge".to_string()),
            },
        });
        write_frame(&mut writer, &hello).await?;

        let ack = read_frame(&mut reader)
            .await
            .context("read HelloAck frame")?;
        let IpcMessage::HelloAck(ack) = ack else {
            return Err(anyhow!("expected HelloAck, got unexpected frame"));
        };

        let conn = Connection {
            writer: Arc::new(Mutex::new(writer)),
            reader: Some(reader),
            reader_task: None,
            mcp_replies: Arc::new(McpReplySlots::default()),
        };
        self.connections
            .inner
            .lock()
            .await
            .insert(session_id.to_string(), conn);

        // F-122 security fix: cache the daemon-reported workspace root so the
        // editor-pane filesystem commands (`read_file`, `write_file`, `tree`)
        // can look it up server-side rather than trusting a webview-supplied
        // parameter. We canonicalize at cache time so the authoritative
        // value is already normalized; a lying webview that later claims a
        // different path literally cannot widen its sandbox — the command
        // layer never reads the param.
        //
        // Defense in depth: refuse to cache an empty workspace path. An
        // empty string would canonicalize to "" → the `forge-fs` allowlist
        // glob `"/**"` would match every path on the host. The daemon
        // never emits an empty `HelloAck.workspace` today (`forge-session`
        // canonicalizes a non-empty path at load time), but a defective or
        // compromised daemon would otherwise silently widen the sandbox.
        //
        // Canonicalization failure on a non-empty path (e.g. the daemon
        // reported a path that doesn't exist) is non-fatal to `hello`; we
        // fall back to the raw string because `forge-fs` will canonicalize
        // on its own hot path and reject the read/write there.
        if !ack.workspace.is_empty() {
            let cached = std::fs::canonicalize(&ack.workspace)
                .unwrap_or_else(|_| PathBuf::from(&ack.workspace));
            self.connections
                .workspace_roots
                .lock()
                .await
                .insert(session_id.to_string(), cached);
        }

        Ok(ack)
    }

    /// Send `Subscribe { since }` to the daemon and spawn a background task
    /// that reads event frames and delivers them to `sink`. Idempotent per
    /// connection: a second call is a no-op (task already running or in
    /// flight).
    ///
    /// **F-109:** this function must not hold the `SessionConnections` map
    /// lock across any `.await`. The sequence is:
    ///
    /// 1. Lock the map, validate, clone the writer `Arc`, take the reader
    ///    half (which also acts as a "subscription-in-flight" reservation),
    ///    drop the lock.
    /// 2. Await `write_frame` on the per-connection writer mutex.
    /// 3. Re-acquire the map lock briefly to install the reader task, or —
    ///    if the write failed — restore the reader so a retry is possible.
    ///
    /// A concurrent second call that lands between step 1 and step 3 sees
    /// `reader: None` and treats the subscription as already in flight; it
    /// returns `Ok(())` rather than failing with "reader already consumed".
    pub async fn subscribe(
        &self,
        session_id: &str,
        since: u64,
        sink: Arc<dyn EventSink>,
    ) -> Result<()> {
        // Step 1: capture per-connection handles under the map lock, then
        // drop the lock before any `.await`.
        let (writer, reader, mcp_replies) = {
            let mut map = self.connections.inner.lock().await;
            let conn = map.get_mut(session_id).ok_or_else(|| {
                anyhow!("session_subscribe: no active connection for {session_id}")
            })?;

            // Idempotent in two states: reader_task already spawned, or an
            // in-flight subscribe has reserved the reader by taking it.
            if conn.reader_task.is_some() || conn.reader.is_none() {
                return Ok(());
            }

            let writer = Arc::clone(&conn.writer);
            // Reserve the reader under the map lock so a racing call sees
            // `reader: None` and bails early rather than duplicating the
            // subscribe frame or fighting for the reader half.
            let reader = conn.reader.take().expect("reader present per check above");
            let mcp_replies = Arc::clone(&conn.mcp_replies);
            (writer, reader, mcp_replies)
        };

        // Step 2: await the Subscribe frame with the map lock released.
        let sub = IpcMessage::Subscribe(Subscribe { since });
        let write_result = {
            let mut writer_guard = writer.lock().await;
            write_frame(&mut *writer_guard, &sub).await
        };

        // Step 3: re-acquire the map lock briefly to either install the
        // reader task or restore the reader on failure.
        let mut map = self.connections.inner.lock().await;
        let conn = map.get_mut(session_id).ok_or_else(|| {
            anyhow!("session_subscribe: connection for {session_id} disappeared mid-subscribe")
        })?;

        if let Err(err) = write_result {
            // Put the reader back so a subsequent subscribe can try again.
            conn.reader = Some(reader);
            return Err(err);
        }

        let session_id_owned = session_id.to_string();
        let task = tokio::spawn(async move {
            pump_events(reader, session_id_owned, sink, mcp_replies).await;
        });
        conn.reader_task = Some(task);

        Ok(())
    }

    pub async fn send_message(&self, session_id: &str, text: String) -> Result<()> {
        let writer = self.writer_for(session_id).await?;
        let mut writer = writer.lock().await;
        let frame = IpcMessage::SendUserMessage(SendUserMessage { text });
        write_frame(&mut *writer, &frame).await
    }

    pub async fn approve_tool(
        &self,
        session_id: &str,
        id: String,
        scope: ApprovalScope,
    ) -> Result<()> {
        let writer = self.writer_for(session_id).await?;
        let mut writer = writer.lock().await;
        // F-069 / L5 (T7): the Tauri command layer is the trust boundary — it
        // accepts a typed `ApprovalScope` only. The wire shape in
        // `forge_ipc::ToolCallApproved` still carries `scope: String` so the
        // forge-session daemon's deserializer keeps working unchanged while
        // F-053 (M7) — the complementary server-side fix — is still pending.
        let scope = scope_to_wire(&scope);
        let frame = IpcMessage::ToolCallApproved(ToolCallApproved { id, scope });
        write_frame(&mut *writer, &frame).await
    }

    pub async fn reject_tool(
        &self,
        session_id: &str,
        id: String,
        reason: Option<String>,
    ) -> Result<()> {
        let writer = self.writer_for(session_id).await?;
        let mut writer = writer.lock().await;
        let frame = IpcMessage::ToolCallRejected(ToolCallRejected { id, reason });
        write_frame(&mut *writer, &frame).await
    }

    /// F-143 / F-144: forward a `rerun_message` request to the session
    /// daemon. All three variants (Replace, Branch, Fresh) are dispatched
    /// server-side.
    pub async fn rerun_message(
        &self,
        session_id: &str,
        msg_id: String,
        variant: RerunVariant,
    ) -> Result<()> {
        let writer = self.writer_for(session_id).await?;
        let mut writer = writer.lock().await;
        let frame = IpcMessage::RerunMessage(RerunMessage { msg_id, variant });
        write_frame(&mut *writer, &frame).await
    }

    /// F-144: forward a `select_branch` request to the session daemon.
    /// Triggers the daemon to emit `Event::BranchSelected { parent,
    /// selected }` after resolving `variant_index` against the event log.
    /// Unknown variants are surfaced daemon-side as log lines; the Tauri
    /// command returns `Ok(())` once the frame is written (the emission
    /// arrives through the event stream).
    pub async fn select_branch(
        &self,
        session_id: &str,
        parent_id: String,
        variant_index: u32,
    ) -> Result<()> {
        let writer = self.writer_for(session_id).await?;
        let mut writer = writer.lock().await;
        let frame = IpcMessage::SelectBranch(SelectBranch {
            parent_id,
            variant_index,
        });
        write_frame(&mut *writer, &frame).await
    }

    /// F-145: forward a `delete_branch` request to the session daemon.
    /// Triggers the daemon to emit `Event::BranchDeleted { parent,
    /// variant_index }` once the target resolves. Unknown variants are
    /// surfaced daemon-side as log lines; the Tauri command returns
    /// `Ok(())` once the frame is written (the emission — or the rejection
    /// of a root-deletion with live siblings — arrives through the event
    /// stream).
    pub async fn delete_branch(
        &self,
        session_id: &str,
        parent_id: String,
        variant_index: u32,
    ) -> Result<()> {
        let writer = self.writer_for(session_id).await?;
        let mut writer = writer.lock().await;
        let frame = IpcMessage::DeleteBranch(DeleteBranch {
            parent_id,
            variant_index,
        });
        write_frame(&mut *writer, &frame).await
    }

    /// F-598: forward a manual `compact_transcript` request to the session
    /// daemon. The daemon emits `Event::ContextCompacted` through the
    /// event stream once the privileged summary call lands; the Tauri
    /// command returns `Ok(())` once the frame is written, so callers
    /// observe completion via the same event subscription that already
    /// drives the transcript view.
    pub async fn compact_transcript(&self, session_id: &str) -> Result<()> {
        let writer = self.writer_for(session_id).await?;
        let mut writer = writer.lock().await;
        let frame = IpcMessage::CompactTranscript(CompactTranscript::default());
        write_frame(&mut *writer, &frame).await
    }

    /// F-603: forward a `pause_session` request to the session daemon. The
    /// daemon emits `Event::SessionPaused` on the `Running → Paused`
    /// transition; redundant pauses are silently coalesced server-side
    /// (DoD: pause-while-paused is a no-op with a `debug!` log, not an
    /// error). The Tauri command returns `Ok(())` once the frame is
    /// written — the webview observes the actual transition through the
    /// session event stream.
    pub async fn pause_session(&self, session_id: &str) -> Result<()> {
        let writer = self.writer_for(session_id).await?;
        let mut writer = writer.lock().await;
        let frame = IpcMessage::PauseSession(PauseSession::default());
        write_frame(&mut *writer, &frame).await
    }

    /// F-603: forward a `resume_session` request to the session daemon.
    /// Mirror of [`Self::pause_session`] — the daemon emits
    /// `Event::SessionResumed` on the `Paused → Running` transition and
    /// silently coalesces redundant resumes server-side.
    pub async fn resume_session(&self, session_id: &str) -> Result<()> {
        let writer = self.writer_for(session_id).await?;
        let mut writer = writer.lock().await;
        let frame = IpcMessage::ResumeSession(ResumeSession::default());
        write_frame(&mut *writer, &frame).await
    }

    /// F-604: interrupt the in-flight assistant turn and fetch the
    /// refine handoff. Distinct from [`Self::pause_session`] (resumable
    /// — same turn keeps going) and from `session_cancel` (terminal —
    /// ends the session). The daemon captures the partial assistant
    /// text at the next chunk boundary, emits
    /// `Event::SessionInterrupted` on the event stream, and replies
    /// with the same payload via [`IpcMessage::RefineHandoff`] which
    /// `pump_events` routes back here through the per-kind reply slot.
    /// An interrupt with no in-flight turn replies with an empty
    /// handoff (`partial_text: ""`, `captured_at_*: ""`) — a legal
    /// shape that the calling Tauri command surfaces to the webview as
    /// "nothing was in flight to refine."
    pub async fn interrupt_session(&self, session_id: &str) -> Result<RefineHandoff> {
        let (writer, mcp_replies) = self.mcp_handles_for(session_id).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        mcp_replies.interrupt.lock().await.push_back(tx);

        let frame = IpcMessage::InterruptSession(InterruptSession::default());
        let send_result = {
            let mut guard = writer.lock().await;
            write_frame(&mut *guard, &frame).await
        };
        if let Err(e) = send_result {
            // Drop the registered slot so a future call doesn't consume
            // a stale reply. Pop the front matches the slot we just
            // pushed (FIFO across pipelined callers).
            let _ = mcp_replies.interrupt.lock().await.pop_front();
            return Err(e);
        }
        rx.await
            .map_err(|_| anyhow!("interrupt_session: reply channel closed"))
    }

    /// F-155: list the session daemon's MCP servers. The daemon returns
    /// its authoritative `McpManager::list()` snapshot — the shell no
    /// longer maintains its own manager.
    pub async fn list_mcp_servers(
        &self,
        session_id: &str,
    ) -> Result<Vec<forge_ipc::McpServerInfo>> {
        let (writer, mcp_replies) = self.mcp_handles_for(session_id).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        mcp_replies.list.lock().await.push_back(tx);

        let frame = IpcMessage::ListMcpServers(ListMcpServers::default());
        let send_result = {
            let mut guard = writer.lock().await;
            write_frame(&mut *guard, &frame).await
        };
        if let Err(e) = send_result {
            // Drop the registered slot so a future call doesn't consume a
            // stale reply. Take the first slot we registered — there may
            // be a racing caller, but the pop returns exactly one.
            let _ = mcp_replies.list.lock().await.pop_front();
            return Err(e);
        }
        let reply = rx
            .await
            .map_err(|_| anyhow!("list_mcp_servers: reply channel closed"))?;
        Ok(reply.servers)
    }

    /// F-155: toggle an MCP server on the session daemon. `enabled` is
    /// the target state — `true` starts (or no-ops), `false` parks the
    /// server in `ServerState::Disabled` so in-flight + subsequent tool
    /// calls surface the canonical "server disabled" error.
    pub async fn toggle_mcp_server(
        &self,
        session_id: &str,
        name: String,
        enabled: bool,
    ) -> Result<McpToggleResult> {
        let (writer, mcp_replies) = self.mcp_handles_for(session_id).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        mcp_replies.toggle.lock().await.push_back(tx);

        let frame = IpcMessage::ToggleMcpServer(ToggleMcpServer {
            name: name.clone(),
            enabled,
        });
        let send_result = {
            let mut guard = writer.lock().await;
            write_frame(&mut *guard, &frame).await
        };
        if let Err(e) = send_result {
            let _ = mcp_replies.toggle.lock().await.pop_front();
            return Err(e);
        }
        let reply = rx
            .await
            .map_err(|_| anyhow!("toggle_mcp_server: reply channel closed"))?;
        Ok(reply)
    }

    /// F-155: import a third-party MCP config through the session
    /// daemon. `apply=false` is a dry-run that reports the merged server
    /// list without rewriting `<workspace>/.mcp.json`.
    pub async fn import_mcp_config(
        &self,
        session_id: &str,
        source: String,
        apply: bool,
    ) -> Result<McpImportResult> {
        let (writer, mcp_replies) = self.mcp_handles_for(session_id).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        mcp_replies.import.lock().await.push_back(tx);

        let frame = IpcMessage::ImportMcpConfig(ImportMcpConfig { source, apply });
        let send_result = {
            let mut guard = writer.lock().await;
            write_frame(&mut *guard, &frame).await
        };
        if let Err(e) = send_result {
            let _ = mcp_replies.import.lock().await.pop_front();
            return Err(e);
        }
        let reply = rx
            .await
            .map_err(|_| anyhow!("import_mcp_config: reply channel closed"))?;
        Ok(reply)
    }

    /// F-155: capture the writer handle and MCP reply-slots arc for
    /// `session_id` under a single map-lock acquisition. Mirrors the
    /// `writer_for` pattern but also hands out the reply slots — which
    /// `pump_events` uses to deliver MCP responses.
    async fn mcp_handles_for(
        &self,
        session_id: &str,
    ) -> Result<(Arc<Mutex<OwnedWriteHalf>>, Arc<McpReplySlots>)> {
        let map = self.connections.inner.lock().await;
        let conn = map
            .get(session_id)
            .ok_or_else(|| anyhow!("no active connection for session {session_id}"))?;
        Ok((Arc::clone(&conn.writer), Arc::clone(&conn.mcp_replies)))
    }

    async fn writer_for(&self, session_id: &str) -> Result<Arc<Mutex<OwnedWriteHalf>>> {
        let map = self.connections.inner.lock().await;
        let conn = map
            .get(session_id)
            .ok_or_else(|| anyhow!("no active connection for session {session_id}"))?;
        Ok(Arc::clone(&conn.writer))
    }

    /// Test-only accessor for a session's writer mutex. Used by concurrency
    /// tests (F-109) to externally hold the writer so a `subscribe()` frame
    /// write stalls deterministically, then verify that unrelated sessions'
    /// commands are not blocked behind the `SessionConnections` map lock.
    ///
    /// Not used in production paths.
    #[doc(hidden)]
    pub async fn writer_arc_for_testing(
        &self,
        session_id: &str,
    ) -> Option<Arc<Mutex<OwnedWriteHalf>>> {
        let map = self.connections.inner.lock().await;
        map.get(session_id).map(|c| Arc::clone(&c.writer))
    }
}

/// F-069 / L5 (T7): map a typed [`ApprovalScope`] to the short PascalCase
/// string that `forge_ipc::ToolCallApproved.scope` (and the existing daemon
/// parser via F-053) expects. Kept explicit — not derived through
/// `serde_json::to_value` — so the wire strings stay grep-able and any
/// future variant addition forces a compile error here.
fn scope_to_wire(scope: &ApprovalScope) -> String {
    match scope {
        ApprovalScope::Once => "Once",
        ApprovalScope::ThisFile => "ThisFile",
        ApprovalScope::ThisPattern => "ThisPattern",
        ApprovalScope::ThisTool => "ThisTool",
    }
    .to_string()
}

async fn pump_events(
    mut reader: OwnedReadHalf,
    session_id: String,
    sink: Arc<dyn EventSink>,
    mcp_replies: Arc<McpReplySlots>,
) {
    // F-565: hoist a per-pump-loop body buffer so each assistant-token
    // frame reuses the same allocation instead of `vec![0u8; len]` per
    // call. 4 KiB matches the typical realistic delta envelope; larger
    // frames grow the buffer in place once and the capacity is retained.
    let mut frame_buf: Vec<u8> = Vec::with_capacity(4096);
    loop {
        match read_frame_into(&mut reader, &mut frame_buf).await {
            Ok(IpcMessage::Event(event)) => {
                sink.emit(SessionEventPayload {
                    session_id: session_id.clone(),
                    seq: event.seq,
                    event: event.event,
                });
            }
            // F-155: route MCP responses into the per-kind reply slot
            // queue. The command awaiting the response pops its own
            // oneshot before calling us — a missing slot means the
            // command gave up (e.g. timed out), so we drop the reply
            // silently rather than pile up stale data.
            Ok(IpcMessage::McpServersList(list)) => {
                if let Some(slot) = mcp_replies.list.lock().await.pop_front() {
                    let _ = slot.send(list);
                }
            }
            Ok(IpcMessage::McpToggleResult(res)) => {
                if let Some(slot) = mcp_replies.toggle.lock().await.pop_front() {
                    let _ = slot.send(res);
                }
            }
            Ok(IpcMessage::McpImportResult(res)) => {
                if let Some(slot) = mcp_replies.import.lock().await.pop_front() {
                    let _ = slot.send(res);
                }
            }
            // F-604: route the refine-handoff response into the
            // matching reply slot. Same drop-on-no-waiter semantics as
            // the MCP arms — a cancelled command leaves no slot, the
            // reply lands without a receiver, and we silently discard.
            Ok(IpcMessage::RefineHandoff(handoff)) => {
                if let Some(slot) = mcp_replies.interrupt.lock().await.pop_front() {
                    let _ = slot.send(handoff);
                }
            }
            Ok(_) => {
                // Non-event, non-response frames (e.g. late HelloAck)
                // are ignored; only session events flow to the webview.
            }
            Err(_) => {
                // Peer closed or unrecoverable framing error. Task exits.
                break;
            }
        }
    }
}

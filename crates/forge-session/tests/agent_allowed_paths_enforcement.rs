//! F-663: `AgentDef.allowed_paths` is enforced at tool dispatch.
//!
//! Pre-F-663, the orchestrator threaded the *session*'s `allowed_paths`
//! (workspace-derived) through `ToolCtx` and ignored the active agent's
//! `def.allowed_paths`. An agent could read or write any path the session
//! allowed, regardless of the scope it had declared. This contradicted the
//! documented model on `AgentDef.allowed_paths`.
//!
//! What this test pins:
//! - When the active agent's `def_allowed_paths` is **non-empty**, the
//!   effective scope handed to fs.* tools narrows to the agent's declared
//!   list — even when the session would otherwise permit a broader path.
//! - When the active agent's `def_allowed_paths` is **empty**, the session
//!   scope is preserved (back-compat for agents that don't declare).
//!
//! Reuses the F-649 canonicalization machinery in `forge-fs::enforce_allowed`.

use forge_agents::{AgentDef, Isolation, Orchestrator as AgentOrchestrator};
use forge_core::Event;
use forge_providers::MockProvider;
use forge_session::orchestrator::run_turn;
use forge_session::session::Session;
use forge_session::tools::AgentRuntime;
use std::io::Write;
use std::sync::Arc;
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::Mutex;

fn agent_def(name: &str, allowed_paths: Vec<String>) -> AgentDef {
    AgentDef {
        name: name.to_string(),
        description: None,
        body: String::new(),
        allowed_paths,
        isolation: Isolation::Process,
        memory_enabled: false,
    }
}

async fn drain_completed_result(
    rx: &mut tokio::sync::broadcast::Receiver<(u64, Event)>,
) -> Option<serde_json::Value> {
    while let Ok((_seq, event)) = rx.try_recv() {
        if let Event::ToolCallCompleted { result, .. } = event {
            return Some(result);
        }
    }
    None
}

/// F-663: an agent with a narrow `def.allowed_paths` cannot read a file the
/// **session** would otherwise permit. The path lookup must surface the
/// `forge-fs` `PathDenied` error rather than returning the file contents.
#[tokio::test]
async fn agent_def_allowed_paths_narrows_session_scope() {
    let workspace = TempDir::new().unwrap();
    // Session paths permit *anything under the workspace*.
    let session_log = workspace.path().join("events.jsonl");
    let session = Arc::new(Session::create(session_log).await.unwrap());

    // Two files: one inside an "agent-allowed" subdir, one only in the
    // session-allowed scope (the workspace root, but outside the agent's
    // declared scope).
    let agent_dir = workspace.path().join("agent-only");
    std::fs::create_dir(&agent_dir).unwrap();
    let mut allowed_file = NamedTempFile::new_in(&agent_dir).unwrap();
    allowed_file.write_all(b"in-scope content").unwrap();
    let allowed_path = std::fs::canonicalize(allowed_file.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    let mut denied_file = NamedTempFile::new_in(workspace.path()).unwrap();
    denied_file.write_all(b"out-of-scope content").unwrap();
    let denied_path = std::fs::canonicalize(denied_file.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    // Agent declares a glob covering ONLY the agent-only subdir.
    let agent_glob = format!(
        "{}/**",
        std::fs::canonicalize(&agent_dir).unwrap().display()
    );

    let orchestrator = Arc::new(AgentOrchestrator::new());
    let agent_defs = Arc::new(vec![]);
    let root_instance = orchestrator
        .spawn(
            agent_def("session-root", vec![agent_glob.clone()]),
            forge_agents::SpawnContext::user(),
        )
        .await
        .unwrap();
    let runtime = AgentRuntime {
        orchestrator: Arc::clone(&orchestrator),
        agent_defs: Arc::clone(&agent_defs),
        parent_instance_id: root_instance.id.clone(),
        def_allowed_paths: vec![agent_glob.clone()],
    };

    // Session-scope glob covers the entire workspace (broader than agent).
    let session_allowed = vec![format!(
        "{}/**",
        std::fs::canonicalize(workspace.path()).unwrap().display()
    )];

    // Mock provider scripts an fs.read on the *denied* path. With F-663
    // wired, the agent's narrow def_allowed_paths must override the broader
    // session glob and force a PathDenied.
    let script = format!(
        "{}\n{}\n",
        serde_json::json!({
            "tool_call": {"name": "fs.read", "args": {"path": denied_path.clone()}}
        }),
        serde_json::json!({"done": "tool_use"}),
    );
    let provider = Arc::new(MockProvider::from_responses(vec![script]).unwrap());

    let mut rx = session.event_tx.subscribe();
    let pending_approvals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    run_turn(
        Arc::clone(&session),
        session.as_ref(),
        provider,
        "read denied".to_string(),
        pending_approvals,
        session_allowed.clone(),
        true, // auto_approve
        Some(workspace.path().to_path_buf()),
        None,
        None,
        None,
        Some(runtime),
        None,
        None,
        None,
    )
    .await
    .expect("run_turn should complete");

    let result = drain_completed_result(&mut rx)
        .await
        .expect("ToolCallCompleted must be emitted");
    let err_msg = result["error"].as_str().unwrap_or_default();
    assert!(
        err_msg.contains("not allowed by allowed_paths"),
        "agent's narrow scope must reject a session-allowed path; got: {result}"
    );
    // And the in-scope path is NOT what was requested — sanity guard.
    assert!(
        !err_msg.contains(&allowed_path),
        "error must reference the denied path, not the allowed one; got: {result}"
    );

    // Compile-only sanity: the in-scope read would be permitted by the
    // glob — assert via the underlying helper.
    let probe = forge_fs::read_file(&allowed_path, &[agent_glob], &forge_fs::Limits::default());
    assert!(
        probe.is_ok(),
        "in-scope path must read cleanly under the agent's glob; got: {probe:?}"
    );
}

/// F-663: when the active agent declares no `allowed_paths`, the session
/// scope is preserved. This pins the back-compat path so existing agents
/// (the synthesized session root in `build_agent_runtime`, fixtures with
/// `allowed_paths: vec![]`) keep working.
#[tokio::test]
async fn agent_def_with_empty_allowed_paths_falls_back_to_session_scope() {
    let workspace = TempDir::new().unwrap();
    let session_log = workspace.path().join("events.jsonl");
    let session = Arc::new(Session::create(session_log).await.unwrap());

    let mut file = NamedTempFile::new_in(workspace.path()).unwrap();
    file.write_all(b"session-allowed").unwrap();
    let path = std::fs::canonicalize(file.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    let orchestrator = Arc::new(AgentOrchestrator::new());
    let agent_defs = Arc::new(vec![]);
    let root_instance = orchestrator
        .spawn(
            agent_def("session-root", vec![]),
            forge_agents::SpawnContext::user(),
        )
        .await
        .unwrap();
    let runtime = AgentRuntime {
        orchestrator: Arc::clone(&orchestrator),
        agent_defs: Arc::clone(&agent_defs),
        parent_instance_id: root_instance.id.clone(),
        def_allowed_paths: vec![], // empty = back-compat
    };

    let session_allowed = vec![format!(
        "{}/**",
        std::fs::canonicalize(workspace.path()).unwrap().display()
    )];

    let script = format!(
        "{}\n{}\n",
        serde_json::json!({
            "tool_call": {"name": "fs.read", "args": {"path": path.clone()}}
        }),
        serde_json::json!({"done": "tool_use"}),
    );
    let provider = Arc::new(MockProvider::from_responses(vec![script]).unwrap());

    let mut rx = session.event_tx.subscribe();
    let pending_approvals = Arc::new(Mutex::new(std::collections::HashMap::new()));
    run_turn(
        Arc::clone(&session),
        session.as_ref(),
        provider,
        "read session-allowed".to_string(),
        pending_approvals,
        session_allowed,
        true,
        Some(workspace.path().to_path_buf()),
        None,
        None,
        None,
        Some(runtime),
        None,
        None,
        None,
    )
    .await
    .expect("run_turn should complete");

    let result = drain_completed_result(&mut rx)
        .await
        .expect("ToolCallCompleted must be emitted");
    assert!(
        result.get("error").is_none(),
        "with empty agent allowed_paths, session scope must permit the read; got: {result}"
    );
    assert_eq!(
        result["content"].as_str().unwrap_or_default(),
        "session-allowed",
        "expected file content; got: {result}"
    );
}

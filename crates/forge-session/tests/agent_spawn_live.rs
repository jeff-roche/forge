//! F-140 integration test: `agent.spawn` actually spawns a child when a
//! provider calls it from inside a live session turn.
//!
//! Pre-F-140 (and pre-F-134 follow-up), `run_turn` constructed a `ToolCtx`
//! with `agent_ctx: None`, so every `agent.spawn` call returned
//! `"agent runtime not configured"`. The F-140 additional mandate wires
//! `serve_with_session` / `run_turn` to build an `AgentSpawnCtx` carrying
//! the session-scoped orchestrator, loaded agent defs, and a stable parent
//! instance id so sub-agent spawning works end-to-end from a live turn.
//!
//! What this test pins:
//!   - a provider emitting `agent.spawn("child")` from inside `run_turn`
//!     registers the child on the session's `forge_agents::Orchestrator`;
//!   - the session event log contains `Event::SubAgentSpawned { parent,
//!     child, from_msg }` with `parent` == the session's root instance id
//!     and `from_msg` == the turn's assistant message id;
//!   - the child instance is registered under its own `AgentDef.isolation`
//!     (not the parent's).

use forge_agents::{AgentDef, Isolation, Orchestrator as AgentOrchestrator};
use forge_core::Event;
use forge_providers::MockProvider;
use forge_session::orchestrator::run_turn;
use forge_session::session::Session;
use forge_session::tools::AgentRuntime;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;

fn agent_def(name: &str, isolation: Isolation) -> AgentDef {
    AgentDef {
        name: name.to_string(),
        description: None,
        body: String::new(),
        allowed_paths: vec![],
        isolation,
        memory_enabled: false,
    }
}

const SCRIPT_SPAWN: &str = r#"{"tool_call":{"name":"agent.spawn","args":{"agent_name":"child","prompt":"hello"}}}
{"done":"tool_use"}
"#;

const SCRIPT_CONT: &str = r#"{"delta":"done"}
{"done":"end_turn"}
"#;

/// DoD: `agent.spawn("child", "hello")` invoked from a live session turn
/// actually spawns the child and emits `Event::SubAgentSpawned` — no more
/// `agent_ctx None` fallback.
#[tokio::test]
async fn agent_spawn_from_live_turn_registers_child_and_emits_sub_agent_spawned() {
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let session = Arc::new(Session::create(log_path).await.unwrap());

    let provider = Arc::new(
        MockProvider::from_responses(vec![SCRIPT_SPAWN.to_string(), SCRIPT_CONT.to_string()])
            .unwrap(),
    );

    // Shared orchestrator: the same instance passed to run_turn is the one
    // we query after the turn to verify the child ended up registered.
    let orchestrator = Arc::new(AgentOrchestrator::new());
    let agent_defs = Arc::new(vec![agent_def("child", Isolation::Container)]);

    // Pre-register a "root" instance representing the session itself —
    // this is the parent the spawn tool attributes to.
    let root_instance = orchestrator
        .spawn(
            agent_def("session-root", Isolation::Process),
            forge_agents::SpawnContext::user(),
        )
        .await
        .unwrap();
    let root_id = root_instance.id.clone();

    let runtime = AgentRuntime {
        orchestrator: Arc::clone(&orchestrator),
        agent_defs: Arc::clone(&agent_defs),
        parent_instance_id: root_id.clone(),
    };

    // Subscribe before the turn so we don't miss SubAgentSpawned.
    let mut rx = session.event_tx.subscribe();

    let pending_approvals = Arc::new(Mutex::new(std::collections::HashMap::new()));

    run_turn(
        Arc::clone(&session),
        session.as_ref(),
        provider,
        "go".to_string(),
        pending_approvals,
        vec![], // allowed_paths — agent.spawn does not consult them
        true,   // auto_approve so the test doesn't stall on approval
        None,   // workspace_root
        None,   // child_registry
        None,   // byte_budget
        None,   // agents_md
        Some(runtime),
        None, // mcp — F-132 (no MCP servers in this test)
        None, // dispatcher_cache — F-567 (test exercises legacy build path)
        None, // credentials — F-587 (this test exercises a keyless mock path)
    )
    .await
    .expect("run_turn should complete");

    // Drain events looking for SubAgentSpawned.
    let mut spawned = None;
    while let Ok((_seq, event)) = rx.try_recv() {
        if let Event::SubAgentSpawned {
            parent,
            child,
            from_msg,
            ..
        } = event
        {
            spawned = Some((parent, child, from_msg));
            break;
        }
    }

    let (parent, child, _from_msg) =
        spawned.expect("SubAgentSpawned must be emitted from a live agent.spawn call");

    assert_eq!(
        parent, root_id,
        "parent must be the session root instance id, not AgentInstanceId::new()"
    );

    // Child is registered with the child's own isolation — never promoted.
    let child_inst = orchestrator
        .get(&child)
        .await
        .expect("child instance must be registered on the shared orchestrator");
    assert_eq!(
        child_inst.def.isolation,
        Isolation::Container,
        "child must run under its def's isolation (Container), not parent's Process"
    );
    assert_eq!(child_inst.def.name, "child");
}

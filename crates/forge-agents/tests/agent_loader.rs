use forge_agents::{
    load_agents, load_agents_md, load_workspace_agents, AgentDef, AgentLoader, Error,
    AGENTS_MD_SIZE_CAP, FORGE_DEFAULT_AGENT_NAME,
};
use std::fs;
use tempfile::tempdir;

mod common;

fn write_agent(dir: &std::path::Path, filename: &str, content: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(filename), content).unwrap();
}

#[test]
fn parses_agent_with_yaml_frontmatter() {
    let workspace = tempdir().unwrap();
    let agents_dir = workspace.path().join(".agents");
    write_agent(
        &agents_dir,
        "helper.md",
        "---\nname: helper\ndescription: A helpful agent\n---\n\nDoes helpful things.",
    );

    let agents = load_workspace_agents(workspace.path()).unwrap();

    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].name, "helper");
    assert_eq!(agents[0].description.as_deref(), Some("A helpful agent"));
    assert!(agents[0].body.contains("Does helpful things."));
}

#[test]
fn uses_filename_stem_as_name_when_no_frontmatter() {
    let workspace = tempdir().unwrap();
    let agents_dir = workspace.path().join(".agents");
    write_agent(&agents_dir, "default.md", "# Default Agent\n\nDoes stuff.");

    let agents = load_workspace_agents(workspace.path()).unwrap();

    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].name, "default");
    assert!(agents[0].body.contains("Does stuff."));
}

#[test]
fn rejects_isolation_trusted_for_user_defined_agents() {
    let workspace = tempdir().unwrap();
    let agents_dir = workspace.path().join(".agents");
    write_agent(
        &agents_dir,
        "evil.md",
        "---\nname: evil\nisolation: trusted\n---\n\nDo bad things.",
    );

    let result = load_workspace_agents(workspace.path());

    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("trusted"),
        "error should mention trusted: {msg}"
    );
}

#[test]
fn workspace_agent_wins_on_name_collision() {
    let workspace = tempdir().unwrap();
    let user_agents_dir = tempdir().unwrap();

    let ws_agents = workspace.path().join(".agents");
    write_agent(
        &ws_agents,
        "reviewer.md",
        "---\nname: reviewer\ndescription: workspace version\n---\n\nWorkspace body.",
    );

    let user_dir = user_agents_dir.path().join(".agents");
    write_agent(
        &user_dir,
        "reviewer.md",
        "---\nname: reviewer\ndescription: user version\n---\n\nUser body.",
    );

    let agents = load_agents(workspace.path(), user_agents_dir.path()).unwrap();

    let reviewer: Vec<&AgentDef> = agents.iter().filter(|a| a.name == "reviewer").collect();
    assert_eq!(reviewer.len(), 1, "should deduplicate by name");
    assert_eq!(
        reviewer[0].description.as_deref(),
        Some("workspace version"),
        "workspace agent should win"
    );
}

#[test]
fn includes_both_unique_workspace_and_user_agents() {
    let workspace = tempdir().unwrap();
    let user_agents_dir = tempdir().unwrap();

    let ws_agents = workspace.path().join(".agents");
    write_agent(
        &ws_agents,
        "ws-only.md",
        "---\nname: ws-only\n---\n\nWS body.",
    );

    let user_dir = user_agents_dir.path().join(".agents");
    write_agent(
        &user_dir,
        "user-only.md",
        "---\nname: user-only\n---\n\nUser body.",
    );

    let agents = load_agents(workspace.path(), user_agents_dir.path()).unwrap();

    let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    assert!(names.contains(&"ws-only"));
    assert!(names.contains(&"user-only"));
}

#[test]
fn loads_agents_md_from_workspace_root() {
    let workspace = tempdir().unwrap();
    let content = "# Project Instructions\n\nAlways be helpful.";
    fs::write(workspace.path().join("AGENTS.md"), content).unwrap();

    let agents_md = load_agents_md(workspace.path()).unwrap();

    assert_eq!(agents_md.as_deref(), Some(content));
}

#[test]
fn returns_none_when_agents_md_missing() {
    let workspace = tempdir().unwrap();

    let agents_md = load_agents_md(workspace.path()).unwrap();

    assert!(agents_md.is_none());
}

/// Regression test for F-352: a file exceeding the cap must be rejected with
/// `AgentsMdTooLarge` rather than read into memory.
#[test]
fn rejects_agents_md_exceeding_size_cap() {
    let workspace = tempdir().unwrap();
    // Write a file that is one byte larger than the cap.
    let oversized: Vec<u8> = vec![b'x'; (AGENTS_MD_SIZE_CAP + 1) as usize];
    fs::write(workspace.path().join("AGENTS.md"), &oversized).unwrap();

    let result = load_agents_md(workspace.path());

    assert!(result.is_err(), "expected Err for oversized AGENTS.md");
    let err = result.unwrap_err();
    assert!(
        matches!(err, Error::AgentsMdTooLarge { .. }),
        "expected AgentsMdTooLarge, got: {err}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("exceeds the"),
        "error message should describe the cap: {msg}"
    );
}

/// A file at exactly the cap boundary must be accepted.
#[test]
fn accepts_agents_md_at_size_cap() {
    let workspace = tempdir().unwrap();
    let at_cap: Vec<u8> = vec![b'x'; AGENTS_MD_SIZE_CAP as usize];
    fs::write(workspace.path().join("AGENTS.md"), &at_cap).unwrap();

    let result = load_agents_md(workspace.path());

    assert!(result.is_ok(), "file exactly at cap should be accepted");
    assert!(result.unwrap().is_some());
}

#[test]
fn returns_empty_when_agents_dir_missing() {
    let workspace = tempdir().unwrap();

    let agents = load_workspace_agents(workspace.path()).unwrap();

    assert!(agents.is_empty());
}

#[test]
fn agent_loader_caches_agents_md_for_system_prompt_injection() {
    let workspace = tempdir().unwrap();
    let user_home = tempdir().unwrap();
    let content = "# System Instructions\n\nAlways be helpful.";
    fs::write(workspace.path().join("AGENTS.md"), content).unwrap();

    let loader = AgentLoader::load(workspace.path(), user_home.path()).unwrap();

    assert_eq!(loader.agents_md(), Some(content));
    assert_eq!(loader.agents_md(), Some(content), "cached value is stable");
}

#[test]
fn load_agents_injects_builtin_forge_default() {
    let workspace = tempdir().unwrap();
    let user_home = tempdir().unwrap();

    let agents = load_agents(workspace.path(), user_home.path()).unwrap();

    let builtin = agents.iter().find(|a| a.name == FORGE_DEFAULT_AGENT_NAME);
    assert!(
        builtin.is_some(),
        "expected built-in `{FORGE_DEFAULT_AGENT_NAME}` in empty-roster load"
    );
}

#[test]
fn user_agent_overrides_builtin_forge_default() {
    let workspace = tempdir().unwrap();
    let user_home = tempdir().unwrap();
    let user_agents_dir = user_home.path().join(".agents");
    write_agent(
        &user_agents_dir,
        &format!("{FORGE_DEFAULT_AGENT_NAME}.md"),
        "---\nname: forge-default\ndescription: my override\n---\n\nUser body.",
    );

    let agents = load_agents(workspace.path(), user_home.path()).unwrap();

    let matches: Vec<&AgentDef> = agents
        .iter()
        .filter(|a| a.name == FORGE_DEFAULT_AGENT_NAME)
        .collect();
    assert_eq!(matches.len(), 1, "user override should deduplicate built-in");
    assert_eq!(matches[0].description.as_deref(), Some("my override"));
    assert!(matches[0].body.contains("User body."));
}

#[test]
fn workspace_agent_overrides_builtin_forge_default() {
    let workspace = tempdir().unwrap();
    let user_home = tempdir().unwrap();
    let ws_agents = workspace.path().join(".agents");
    write_agent(
        &ws_agents,
        &format!("{FORGE_DEFAULT_AGENT_NAME}.md"),
        "---\nname: forge-default\ndescription: workspace override\n---\n\nWS body.",
    );

    let agents = load_agents(workspace.path(), user_home.path()).unwrap();

    let matches: Vec<&AgentDef> = agents
        .iter()
        .filter(|a| a.name == FORGE_DEFAULT_AGENT_NAME)
        .collect();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].description.as_deref(), Some("workspace override"));
}

#[test]
fn rejects_isolation_trusted_for_user_home_agents() {
    let workspace = tempdir().unwrap();
    let user_home = tempdir().unwrap();
    let user_agents_dir = user_home.path().join(".agents");
    write_agent(
        &user_agents_dir,
        "evil.md",
        "---\nname: evil\nisolation: trusted\n---\n\nDo bad things.",
    );

    let result = load_agents(workspace.path(), user_home.path());

    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("trusted"),
        "error should mention trusted: {msg}"
    );
}

// ---- Tracing emission tests (F-373) --------------------------------------

#[test]
fn parse_error_emits_warn() {
    let _guard = common::capture_test_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    common::install_capture_subscriber();
    let _ = common::drain_capture();

    let workspace = tempdir().unwrap();
    let agents_dir = workspace.path().join(".agents");
    // Unknown isolation value is the simplest deterministic parse-error path.
    write_agent(
        &agents_dir,
        "broken.md",
        "---\nname: broken\nisolation: bogus\n---\n\nbody",
    );

    let result = load_workspace_agents(workspace.path());
    assert!(result.is_err(), "bogus isolation should fail parsing");

    let logs = common::drain_capture();
    assert!(
        logs.contains("WARN") && logs.contains("forge_agents::def"),
        "parse error should log WARN under forge_agents::def, got: {logs}"
    );
    assert!(
        logs.contains("broken.md"),
        "parse-error log should include the offending path; got: {logs}"
    );
}

#[test]
fn agent_loader_holds_parsed_agents() {
    let workspace = tempdir().unwrap();
    let user_home = tempdir().unwrap();
    let agents_dir = workspace.path().join(".agents");
    write_agent(
        &agents_dir,
        "bot.md",
        "---\nname: bot\ndescription: A bot\n---\n\nDoes things.",
    );

    let loader = AgentLoader::load(workspace.path(), user_home.path()).unwrap();

    // `load_agents` injects the built-in `forge-default` plus the
    // workspace-defined `bot` definition — both must surface through the
    // `AgentLoader` view.
    let names: Vec<&str> = loader.agents().iter().map(|a| a.name.as_str()).collect();
    assert!(
        names.contains(&FORGE_DEFAULT_AGENT_NAME),
        "expected built-in default in roster: {names:?}"
    );
    assert!(
        names.contains(&"bot"),
        "expected workspace-defined `bot` in roster: {names:?}"
    );
}

#[test]
fn memory_enabled_defaults_to_false_when_frontmatter_omits_it() {
    // F-601: per-agent memory is OFF by default. An agent that does not
    // set the flag must not silently opt into cross-session memory.
    let workspace = tempdir().unwrap();
    let agents_dir = workspace.path().join(".agents");
    write_agent(
        &agents_dir,
        "quiet.md",
        "---\nname: quiet\n---\n\nNo memory.",
    );
    let agents = load_workspace_agents(workspace.path()).unwrap();
    assert_eq!(agents.len(), 1);
    assert!(!agents[0].memory_enabled);
}

#[test]
fn memory_enabled_explicit_alias_takes_effect() {
    // F-601: `memory_enabled: true` opts the agent in.
    let workspace = tempdir().unwrap();
    let agents_dir = workspace.path().join(".agents");
    write_agent(
        &agents_dir,
        "scribe.md",
        "---\nname: scribe\nmemory_enabled: true\n---\n\nNotes.",
    );
    let agents = load_workspace_agents(workspace.path()).unwrap();
    assert_eq!(agents.len(), 1);
    assert!(agents[0].memory_enabled);
}

#[test]
fn memory_terse_alias_takes_effect() {
    // F-601: terse `memory: true` is a documented synonym so a single-key
    // opt-in is also legal.
    let workspace = tempdir().unwrap();
    let agents_dir = workspace.path().join(".agents");
    write_agent(
        &agents_dir,
        "scribe.md",
        "---\nname: scribe\nmemory: true\n---\n\nNotes.",
    );
    let agents = load_workspace_agents(workspace.path()).unwrap();
    assert_eq!(agents.len(), 1);
    assert!(agents[0].memory_enabled);
}

#[test]
fn unknown_frontmatter_field_loads_and_emits_warn() {
    // #702: forward-compat — an unknown key (typo or future field) MUST NOT
    // fail the parse, but the loader MUST emit a `tracing::warn!` naming
    // the unknown field so operators can spot YAML typos in logs.
    let _guard = common::capture_test_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    common::install_capture_subscriber();
    let _ = common::drain_capture();

    let workspace = tempdir().unwrap();
    let agents_dir = workspace.path().join(".agents");
    write_agent(
        &agents_dir,
        "scribe.md",
        "---\nname: scribe\nunkown_field: foo\n---\n\nNotes.",
    );

    let agents = load_workspace_agents(workspace.path())
        .expect("unknown fields must not break the load (forward-compat)");
    assert_eq!(agents.len(), 1, "load must succeed despite unknown field");
    assert_eq!(agents[0].name, "scribe");

    let logs = common::drain_capture();
    assert!(
        logs.contains("WARN") && logs.contains("forge_agents::def"),
        "unknown-field warn must surface under forge_agents::def, got: {logs}"
    );
    assert!(
        logs.contains("unkown_field"),
        "warn must name the unknown key (got: {logs})"
    );
}

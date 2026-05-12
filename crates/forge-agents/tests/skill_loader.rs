use forge_agents::{load_skills, load_user_skills, load_workspace_skills, parse_skill_file};
use std::fs;
use tempfile::tempdir;

mod common;

fn write_skill(scope_root: &std::path::Path, id: &str, body: &str) {
    let dir = scope_root.join(".agent-skills").join(id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), body).unwrap();
}

#[test]
fn parses_skill_with_yaml_frontmatter() {
    let workspace = tempdir().unwrap();
    write_skill(
        workspace.path(),
        "planner",
        "---\nname: Planner\nversion: 1.2.0\ndescription: Plans things\ntools:\n  - shell\n  - fs.read\n---\n\nPrompt body for planner.",
    );

    let skills = load_workspace_skills(workspace.path()).unwrap();

    assert_eq!(skills.len(), 1);
    let s = &skills[0];
    assert_eq!(s.id.as_str(), "planner");
    assert_eq!(s.name, "Planner");
    assert_eq!(s.version.as_deref(), Some("1.2.0"));
    assert_eq!(s.description.as_deref(), Some("Plans things"));
    assert_eq!(s.tools, vec!["shell".to_string(), "fs.read".to_string()]);
    assert!(s.prompt.contains("Prompt body for planner."));
    assert!(s.source_path.ends_with("SKILL.md"));
}

#[test]
fn defaults_name_to_folder_when_frontmatter_missing() {
    let workspace = tempdir().unwrap();
    write_skill(workspace.path(), "default-skill", "# Just a body");

    let skills = load_workspace_skills(workspace.path()).unwrap();

    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id.as_str(), "default-skill");
    assert_eq!(skills[0].name, "default-skill");
    assert!(skills[0].prompt.contains("Just a body"));
}

#[test]
fn ignores_unknown_frontmatter_fields() {
    let workspace = tempdir().unwrap();
    write_skill(
        workspace.path(),
        "future",
        "---\nname: Future\nfuture_field: something\n---\n\nBody.",
    );

    let skills = load_workspace_skills(workspace.path()).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "Future");
}

#[test]
fn skips_folders_without_skill_md() {
    let workspace = tempdir().unwrap();
    let empty = workspace.path().join(".agent-skills").join("empty");
    fs::create_dir_all(&empty).unwrap();
    write_skill(workspace.path(), "real", "---\nname: Real\n---\nbody");

    let skills = load_workspace_skills(workspace.path()).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id.as_str(), "real");
}

#[test]
fn returns_empty_when_skills_dir_missing() {
    let workspace = tempdir().unwrap();
    let skills = load_workspace_skills(workspace.path()).unwrap();
    assert!(skills.is_empty());
}

#[test]
fn workspace_shadows_user_on_id_collision() {
    let workspace = tempdir().unwrap();
    let user = tempdir().unwrap();

    write_skill(
        workspace.path(),
        "shared",
        "---\nname: Shared\ndescription: workspace version\n---\nws body",
    );
    write_skill(
        user.path(),
        "shared",
        "---\nname: Shared\ndescription: user version\n---\nuser body",
    );

    let skills = load_skills(workspace.path(), user.path()).unwrap();

    let shared: Vec<_> = skills
        .iter()
        .filter(|s| s.id.as_str() == "shared")
        .collect();
    assert_eq!(shared.len(), 1, "should deduplicate by id");
    assert_eq!(shared[0].description.as_deref(), Some("workspace version"));
}

#[test]
fn merges_unique_workspace_and_user_skills() {
    let workspace = tempdir().unwrap();
    let user = tempdir().unwrap();

    write_skill(workspace.path(), "ws-only", "---\nname: ws\n---\nws");
    write_skill(user.path(), "user-only", "---\nname: user\n---\nuser");

    let skills = load_skills(workspace.path(), user.path()).unwrap();

    let ids: Vec<_> = skills.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"ws-only"));
    assert!(ids.contains(&"user-only"));
}

#[test]
fn output_is_sorted_by_id_for_determinism() {
    let workspace = tempdir().unwrap();
    // Write in non-alphabetical order; loader must still emit sorted.
    for id in ["zeta", "alpha", "mu"] {
        write_skill(
            workspace.path(),
            id,
            &format!("---\nname: {id}\n---\nbody {id}"),
        );
    }

    let skills = load_workspace_skills(workspace.path()).unwrap();
    let ids: Vec<&str> = skills.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["alpha", "mu", "zeta"]);
}

#[test]
fn merged_output_is_sorted_by_id() {
    let workspace = tempdir().unwrap();
    let user = tempdir().unwrap();

    write_skill(workspace.path(), "zeta", "---\nname: z\n---\nbody");
    write_skill(user.path(), "alpha", "---\nname: a\n---\nbody");
    write_skill(user.path(), "mu", "---\nname: m\n---\nbody");

    let skills = load_skills(workspace.path(), user.path()).unwrap();
    let ids: Vec<&str> = skills.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["alpha", "mu", "zeta"]);
}

#[test]
fn returns_empty_when_user_dir_missing() {
    let user = tempdir().unwrap();
    let skills = load_user_skills(user.path()).unwrap();
    assert!(skills.is_empty());
}

#[test]
fn parse_skill_file_directly() {
    let workspace = tempdir().unwrap();
    write_skill(workspace.path(), "direct", "---\nname: Direct\n---\nbody");
    let path = workspace
        .path()
        .join(".agent-skills")
        .join("direct")
        .join("SKILL.md");

    let skill = parse_skill_file(&path).unwrap();
    assert_eq!(skill.id.as_str(), "direct");
    assert_eq!(skill.name, "Direct");
    assert_eq!(skill.source_path, path);
}

#[test]
fn rejects_skill_folder_with_invalid_id() {
    // The leading-dot rejection in `SkillId::new` filters this out at parse
    // time, regardless of `read_dir` behavior (which on Linux *does* yield
    // dotfile entries — earlier comments in this file got that wrong). We
    // exercise `parse_skill_file` directly so the test isolates the id
    // validation step from filesystem traversal.
    let dir = tempdir().unwrap();
    let bad = dir.path().join(".dotted");
    fs::create_dir_all(&bad).unwrap();
    let path = bad.join("SKILL.md");
    fs::write(&path, "body").unwrap();

    let err = parse_skill_file(&path).unwrap_err();
    assert!(
        err.to_string().contains("invalid skill id"),
        "should reject leading-dot folder names, got: {err}"
    );
}

#[test]
fn malformed_yaml_frontmatter_with_type_mismatch_returns_error() {
    // B1: a value-typed mismatch (sequence into the `name: Option<String>`
    // slot) propagates as a `gray_matter` error already. This guard pins
    // that the loader surfaces the error rather than swallowing it.
    let workspace = tempdir().unwrap();
    write_skill(
        workspace.path(),
        "type-mismatch",
        "---\nname:\n  - not\n  - a\n  - string\n---\nbody",
    );

    let result = load_workspace_skills(workspace.path());
    assert!(
        result.is_err(),
        "type-mismatched frontmatter must fail loudly: {result:?}"
    );
}

#[test]
fn frontmatter_present_but_undeserializable_returns_error() {
    // B1: the silent-fail case. When `matter` is non-empty but the YAML
    // parses to `Pod::Null` (e.g. a frontmatter block containing only a
    // comment), `gray_matter` does *not* return `Err` — `parsed.data`
    // simply lands as `None` because `deserialize_option` on `Pod::Null`
    // visits `none`. The previous loader code fell through to
    // `Frontmatter::default()`. The contract documented in
    // `docs/architecture/skills.md` §5 says malformed frontmatter is
    // rejected; this test enforces that contract.
    let workspace = tempdir().unwrap();
    write_skill(
        workspace.path(),
        "comment-only",
        "---\n# only a comment, no fields\n---\nbody",
    );

    let result = load_workspace_skills(workspace.path());
    assert!(
        result.is_err(),
        "frontmatter that fails to deserialize must surface an error, got: {result:?}",
    );
}

#[test]
fn body_only_skill_still_loads() {
    // B1 regression guard: the contract change must not break the
    // legitimate body-only case (no `---` delimiters at all).
    let workspace = tempdir().unwrap();
    write_skill(
        workspace.path(),
        "body-only",
        "Just a body, no frontmatter.",
    );

    let skills = load_workspace_skills(workspace.path()).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id.as_str(), "body-only");
    assert_eq!(skills[0].name, "body-only");
    assert!(skills[0].prompt.contains("Just a body"));
}

#[test]
fn unknown_frontmatter_field_loads_and_emits_warn() {
    // #702: forward-compat preserves silent acceptance — a YAML typo like
    // `unkown_field: foo` MUST NOT fail the load (future agentskills.io
    // revisions may add fields we don't recognize yet) — but the loader
    // MUST emit a `tracing::warn!` naming the unknown key so an operator
    // can spot the typo in logs.
    let _guard = common::capture_test_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    common::install_capture_subscriber();
    let _ = common::drain_capture();

    let workspace = tempdir().unwrap();
    write_skill(
        workspace.path(),
        "future",
        "---\nname: Future\nunkown_field: foo\n---\n\nBody.",
    );

    let skills = load_workspace_skills(workspace.path())
        .expect("unknown fields must not break the load (forward-compat)");
    assert_eq!(skills.len(), 1, "load must succeed despite unknown field");
    assert_eq!(skills[0].name, "Future");

    let logs = common::drain_capture();
    assert!(
        logs.contains("WARN") && logs.contains("forge_agents::skill_loader"),
        "unknown-field warn must surface under forge_agents::skill_loader, got: {logs}"
    );
    assert!(
        logs.contains("unkown_field"),
        "warn must name the unknown key (got: {logs})"
    );
}

#[test]
fn dir_entry_error_does_not_abort_load() {
    // B2: the loader must keep going past unreadable entries rather than
    // silently dropping them or aborting the whole scope. We can't cheaply
    // simulate a `DirEntry` IO error from userspace, but we *can* verify
    // the adjacent contract: a valid skill in the same scope still loads
    // even when other entries are unusual (a regular file at a position
    // where the loader expected a directory).
    let workspace = tempdir().unwrap();
    let skills_dir = workspace.path().join(".agent-skills");
    fs::create_dir_all(&skills_dir).unwrap();

    // Stray regular file in `.agent-skills/` — must not be parsed as a skill.
    fs::write(skills_dir.join("not-a-skill.txt"), "stray").unwrap();
    write_skill(workspace.path(), "real", "---\nname: Real\n---\nbody");

    let skills = load_workspace_skills(workspace.path()).unwrap();
    assert_eq!(skills.len(), 1, "stray file must not produce a skill");
    assert_eq!(skills[0].id.as_str(), "real");
}

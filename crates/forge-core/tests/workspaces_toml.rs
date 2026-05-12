use chrono::Utc;
use forge_core::{
    workspaces::{read_workspaces, write_workspaces, WorkspaceEntry},
    WorkspaceId,
};
use std::path::PathBuf;
use tempfile::TempDir;

#[tokio::test]
async fn workspaces_toml_round_trip_single() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("workspaces.toml");

    let entries = vec![WorkspaceEntry {
        id: WorkspaceId::new(),
        path: PathBuf::from("/home/alice/code/acme-api"),
        name: "acme-api".to_string(),
        last_opened: Utc::now().with_nanosecond(0).unwrap(),
        pinned: false,
    }];

    write_workspaces(&path, &entries).await.unwrap();
    let loaded = read_workspaces(&path).await.unwrap();

    assert_eq!(entries.len(), loaded.len());
    assert_eq!(entries[0].id, loaded[0].id);
    assert_eq!(entries[0].path, loaded[0].path);
    assert_eq!(entries[0].name, loaded[0].name);
    assert_eq!(entries[0].last_opened, loaded[0].last_opened);
    assert_eq!(entries[0].pinned, loaded[0].pinned);
}

#[tokio::test]
async fn workspaces_toml_round_trip_multiple() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("workspaces.toml");

    let entries = vec![
        WorkspaceEntry {
            id: WorkspaceId::new(),
            path: PathBuf::from("/home/alice/code/acme-api"),
            name: "acme-api".to_string(),
            last_opened: Utc::now().with_nanosecond(0).unwrap(),
            pinned: true,
        },
        WorkspaceEntry {
            id: WorkspaceId::new(),
            path: PathBuf::from("/home/alice/code/docs-v2"),
            name: "docs-v2".to_string(),
            last_opened: Utc::now().with_nanosecond(0).unwrap(),
            pinned: false,
        },
    ];

    write_workspaces(&path, &entries).await.unwrap();
    let loaded = read_workspaces(&path).await.unwrap();

    assert_eq!(2, loaded.len());
    assert_eq!(entries[0].name, loaded[0].name);
    assert_eq!(entries[1].name, loaded[1].name);
    assert_eq!(entries[0].pinned, loaded[0].pinned);
    assert_eq!(entries[1].pinned, loaded[1].pinned);
}

#[tokio::test]
async fn workspaces_toml_creates_parent_dirs() {
    let dir = TempDir::new().unwrap();
    let path = dir
        .path()
        .join(".config")
        .join("forge")
        .join("workspaces.toml");

    write_workspaces(&path, &[]).await.unwrap();
    assert!(path.exists());
}

#[tokio::test]
async fn workspaces_toml_rejects_unknown_field_on_entry() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("workspaces.toml");

    // Hand-written TOML: a valid entry plus a forward-looking field (e.g. a
    // trust-bearing `trusted` flag) that this daemon does not recognize.
    // Must error rather than silently drop the field — audit L1 (F-065).
    let toml_contents = r#"
[[workspaces]]
id = "01J9X0000000000000000WENTR"
path = "/home/alice/code/acme-api"
name = "acme-api"
last_opened = "2025-01-01T00:00:00Z"
pinned = false
trusted = true
"#;
    tokio::fs::write(&path, toml_contents).await.unwrap();

    let err = read_workspaces(&path)
        .await
        .expect_err("unknown field on WorkspaceEntry must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("trusted") || msg.contains("unknown field"),
        "expected unknown-field error, got: {msg}"
    );
}

#[tokio::test]
async fn workspaces_toml_rejects_unknown_field_on_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("workspaces.toml");

    // Top-level unknown key on WorkspacesFile (e.g. a future `version` field
    // that an older daemon should not silently ignore).
    let toml_contents = "workspaces = []\nversion = 2\n";
    tokio::fs::write(&path, toml_contents).await.unwrap();

    let err = read_workspaces(&path)
        .await
        .expect_err("unknown field on WorkspacesFile must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("version") || msg.contains("unknown field"),
        "expected unknown-field error, got: {msg}"
    );
}

#[tokio::test]
async fn workspaces_toml_empty_list() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("workspaces.toml");

    write_workspaces(&path, &[]).await.unwrap();
    let loaded = read_workspaces(&path).await.unwrap();

    assert!(loaded.is_empty());
}

/// First-run path: the workspaces registry is created lazily on the first
/// session spawn. A reader hitting the registry before any writer has must
/// see an empty list rather than a `NotFound` I/O error, so the new-session
/// flow can succeed on a brand-new install.
#[tokio::test]
async fn workspaces_toml_missing_file_reads_as_empty() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("workspaces.toml");

    assert!(!path.exists(), "precondition: file must not exist");
    let loaded = read_workspaces(&path).await.unwrap();
    assert!(loaded.is_empty());
}

#[tokio::test]
async fn register_workspace_if_missing_creates_entry_on_first_call() {
    use forge_core::workspaces::register_workspace_if_missing;

    let dir = TempDir::new().unwrap();
    let registry = dir.path().join("workspaces.toml");
    let workspace = TempDir::new().unwrap();
    let workspace_path = workspace.path().canonicalize().unwrap();

    register_workspace_if_missing(&registry, &workspace_path)
        .await
        .unwrap();
    let loaded = read_workspaces(&registry).await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].path, workspace_path);
    assert_eq!(
        loaded[0].name,
        workspace_path.file_name().unwrap().to_string_lossy(),
        "name should default to the basename"
    );
    assert!(!loaded[0].pinned, "fresh entries are not pinned");
}

#[tokio::test]
async fn register_workspace_if_missing_is_idempotent() {
    use forge_core::workspaces::register_workspace_if_missing;

    let dir = TempDir::new().unwrap();
    let registry = dir.path().join("workspaces.toml");
    let workspace = TempDir::new().unwrap();
    let workspace_path = workspace.path().canonicalize().unwrap();

    register_workspace_if_missing(&registry, &workspace_path)
        .await
        .unwrap();
    let first = read_workspaces(&registry).await.unwrap();
    let first_id = first[0].id.clone();

    // Second call must NOT append a duplicate or rewrite the existing id.
    register_workspace_if_missing(&registry, &workspace_path)
        .await
        .unwrap();
    let second = read_workspaces(&registry).await.unwrap();
    assert_eq!(second.len(), 1, "duplicate entry must not be appended");
    assert_eq!(second[0].id, first_id, "existing id must be preserved");
}

#[tokio::test]
async fn register_workspace_if_missing_appends_to_existing_entries() {
    use forge_core::workspaces::register_workspace_if_missing;

    let dir = TempDir::new().unwrap();
    let registry = dir.path().join("workspaces.toml");
    let workspace_a = TempDir::new().unwrap();
    let workspace_b = TempDir::new().unwrap();
    let path_a = workspace_a.path().canonicalize().unwrap();
    let path_b = workspace_b.path().canonicalize().unwrap();

    register_workspace_if_missing(&registry, &path_a).await.unwrap();
    register_workspace_if_missing(&registry, &path_b).await.unwrap();
    let loaded = read_workspaces(&registry).await.unwrap();
    assert_eq!(loaded.len(), 2);
    let paths: Vec<_> = loaded.iter().map(|e| e.path.clone()).collect();
    assert!(paths.contains(&path_a));
    assert!(paths.contains(&path_b));
}

trait WithNanosecond {
    fn with_nanosecond(self, ns: u32) -> Option<Self>
    where
        Self: Sized;
}

impl WithNanosecond for chrono::DateTime<chrono::Utc> {
    fn with_nanosecond(self, ns: u32) -> Option<Self> {
        chrono::Timelike::with_nanosecond(&self, ns)
    }
}

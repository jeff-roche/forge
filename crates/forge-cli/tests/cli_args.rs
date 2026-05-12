/// Integration tests for CLI argument parsing.
/// These import the production `Cli` struct directly so any structural change
/// to the real command hierarchy will break these tests.
use clap::Parser;
use forge_cli::{
    Cli, Commands, ImportSourceFlag, McpCommands, RunCommands, SessionCommands, SessionNewKind,
    SkillCommands, SkillScopeFlag,
};
use forge_mcp::import::ImportSource;
use std::path::PathBuf;

#[test]
fn parse_session_new_agent_with_workspace() {
    let cli = Cli::try_parse_from([
        "forge",
        "session",
        "new",
        "agent",
        "code-review",
        "--workspace",
        "/tmp/myproject",
    ])
    .expect("should parse");
    let Commands::Session {
        cmd:
            SessionCommands::New {
                kind:
                    SessionNewKind::Agent {
                        name, workspace, ..
                    },
            },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(name, "code-review");
    assert_eq!(workspace, Some(PathBuf::from("/tmp/myproject")));
}

#[test]
fn parse_session_new_agent_without_workspace() {
    let cli =
        Cli::try_parse_from(["forge", "session", "new", "agent", "helper"]).expect("should parse");
    let Commands::Session {
        cmd:
            SessionCommands::New {
                kind:
                    SessionNewKind::Agent {
                        name, workspace, ..
                    },
            },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(name, "helper");
    assert!(workspace.is_none());
}

#[test]
fn parse_session_new_agent_with_provider_flag() {
    let cli = Cli::try_parse_from([
        "forge",
        "session",
        "new",
        "agent",
        "code-review",
        "--provider",
        "mock",
    ])
    .expect("should parse");
    let Commands::Session {
        cmd:
            SessionCommands::New {
                kind: SessionNewKind::Agent { provider, .. },
            },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(provider, Some("mock".to_string()));
}

#[test]
fn parse_session_new_provider() {
    let cli = Cli::try_parse_from([
        "forge",
        "session",
        "new",
        "provider",
        "anthropic/claude-opus-4",
        "--workspace",
        "/home/user/proj",
    ])
    .expect("should parse");
    let Commands::Session {
        cmd:
            SessionCommands::New {
                kind: SessionNewKind::Provider { spec, workspace },
            },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(spec, "anthropic/claude-opus-4");
    assert_eq!(workspace, Some(PathBuf::from("/home/user/proj")));
}

#[test]
fn parse_session_list() {
    let cli = Cli::try_parse_from(["forge", "session", "list"]).expect("should parse");
    assert!(matches!(
        cli.command,
        Commands::Session {
            cmd: SessionCommands::List
        }
    ));
}

#[test]
fn parse_session_tail() {
    let cli = Cli::try_parse_from(["forge", "session", "tail", "abc123def4560000"])
        .expect("should parse");
    let Commands::Session {
        cmd: SessionCommands::Tail { id },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(id, "abc123def4560000");
}

#[test]
fn parse_session_kill() {
    let cli = Cli::try_parse_from(["forge", "session", "kill", "deadbeefcafe0000"])
        .expect("should parse");
    let Commands::Session {
        cmd: SessionCommands::Kill { id },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(id, "deadbeefcafe0000");
}

/// F-057 (T12a): clap must reject a path-traversal session id during
/// argument parsing — before any command body runs — so the filesystem
/// is never touched with an attacker-controlled path component.
#[test]
fn parse_session_kill_rejects_path_traversal() {
    let result = Cli::try_parse_from(["forge", "session", "kill", "../../tmp/x"]);
    assert!(
        result.is_err(),
        "path-traversal session id must be rejected at parse time"
    );
}

#[test]
fn parse_session_tail_rejects_path_traversal() {
    let result = Cli::try_parse_from(["forge", "session", "tail", "../../tmp/x"]);
    assert!(
        result.is_err(),
        "path-traversal session id must be rejected at parse time"
    );
}

#[test]
fn parse_session_kill_rejects_absolute_path() {
    let result = Cli::try_parse_from(["forge", "session", "kill", "/etc/passwd"]);
    assert!(result.is_err(), "absolute path id must be rejected");
}

#[test]
fn parse_session_kill_rejects_uppercase_hex() {
    // SessionId::new() emits lowercase hex only; uppercase cannot have
    // come from the canonical generator.
    let result = Cli::try_parse_from(["forge", "session", "kill", "DEADBEEFCAFEBABE"]);
    assert!(result.is_err(), "uppercase hex id must be rejected");
}

#[test]
fn parse_session_kill_rejects_wrong_length() {
    // 15 chars — one short of the canonical 16.
    let result = Cli::try_parse_from(["forge", "session", "kill", "deadbeefcafebab"]);
    assert!(result.is_err(), "15-char id must be rejected");
}

#[test]
fn parse_session_kill_rejects_non_hex() {
    // 16 chars but contains 'g'.
    let result = Cli::try_parse_from(["forge", "session", "kill", "deadbeefcafebabg"]);
    assert!(result.is_err(), "non-hex id must be rejected");
}

#[test]
fn parse_run_agent_with_default_input() {
    let cli = Cli::try_parse_from(["forge", "run", "agent", "code-review"]).expect("should parse");
    let Commands::Run {
        cmd: RunCommands::Agent { name, input },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(name, "code-review");
    assert_eq!(input, "-");
}

#[test]
fn parse_run_agent_with_file_input() {
    let cli = Cli::try_parse_from([
        "forge",
        "run",
        "agent",
        "helper",
        "--input",
        "/tmp/prompt.txt",
    ])
    .expect("should parse");
    let Commands::Run {
        cmd: RunCommands::Agent { name, input },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(name, "helper");
    assert_eq!(input, "/tmp/prompt.txt");
}

#[test]
fn unknown_command_returns_error() {
    let result = Cli::try_parse_from(["forge", "bogus"]);
    assert!(result.is_err(), "bogus command should fail to parse");
}

#[test]
fn parse_mcp_import_defaults_to_auto_dry_run() {
    let cli = Cli::try_parse_from(["forge", "mcp", "import"]).expect("should parse");
    let Commands::Mcp {
        cmd:
            McpCommands::Import {
                source,
                apply,
                workspace,
            },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(source, ImportSourceFlag::Auto);
    assert!(!apply);
    assert_eq!(workspace, None);
}

#[test]
fn parse_mcp_import_with_explicit_source_and_apply() {
    let cli = Cli::try_parse_from([
        "forge",
        "mcp",
        "import",
        "--source=cursor",
        "--apply",
        "--workspace",
        "/tmp/project",
    ])
    .expect("should parse");
    let Commands::Mcp {
        cmd:
            McpCommands::Import {
                source,
                apply,
                workspace,
            },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(source, ImportSourceFlag::Source(ImportSource::Cursor));
    assert!(apply);
    assert_eq!(workspace, Some(PathBuf::from("/tmp/project")));
}

#[test]
fn parse_mcp_import_rejects_bogus_source() {
    let result = Cli::try_parse_from(["forge", "mcp", "import", "--source=bogus"]);
    assert!(result.is_err(), "bogus source should fail to parse");
}

#[test]
fn parse_skill_install_defaults_to_user_target() {
    let cli =
        Cli::try_parse_from(["forge", "skill", "install", "./fixtures/sample"]).expect("parse");
    let Commands::Skill {
        cmd: SkillCommands::Install { source, target },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(source, "./fixtures/sample");
    assert_eq!(target, SkillScopeFlag::User);
}

#[test]
fn parse_skill_install_with_explicit_workspace_target() {
    let cli = Cli::try_parse_from([
        "forge",
        "skill",
        "install",
        "https://example.com/x.git",
        "--target",
        "workspace",
    ])
    .expect("parse");
    let Commands::Skill {
        cmd: SkillCommands::Install { source, target },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(source, "https://example.com/x.git");
    assert_eq!(target, SkillScopeFlag::Workspace);
}

#[test]
fn parse_skill_install_rejects_unknown_target() {
    let result = Cli::try_parse_from(["forge", "skill", "install", "./x", "--target", "bogus"]);
    assert!(result.is_err(), "bogus target must be rejected");
}

#[test]
fn parse_skill_list() {
    let cli = Cli::try_parse_from(["forge", "skill", "list"]).expect("parse");
    assert!(matches!(
        cli.command,
        Commands::Skill {
            cmd: SkillCommands::List { workspace: None }
        }
    ));
}

#[test]
fn parse_skill_remove_requires_id() {
    let result = Cli::try_parse_from(["forge", "skill", "remove"]);
    assert!(result.is_err(), "missing id must fail to parse");
}

#[test]
fn parse_skill_remove_with_scope() {
    let cli = Cli::try_parse_from(["forge", "skill", "remove", "planner", "--scope", "user"])
        .expect("parse");
    let Commands::Skill {
        cmd:
            SkillCommands::Remove {
                id,
                scope,
                workspace,
            },
    } = cli.command
    else {
        panic!("wrong command shape");
    };
    assert_eq!(id, "planner");
    assert_eq!(scope, Some(SkillScopeFlag::User));
    assert_eq!(workspace, None);
}

#[test]
fn parse_mcp_import_accepts_all_six_source_slugs() {
    for slug in ["vscode", "cursor", "claude", "continue", "kiro", "codex"] {
        let cli = Cli::try_parse_from(["forge", "mcp", "import", &format!("--source={slug}")])
            .unwrap_or_else(|e| panic!("failed to parse source={slug}: {e}"));
        let Commands::Mcp {
            cmd: McpCommands::Import { source, .. },
        } = cli.command
        else {
            panic!("wrong command shape for {slug}");
        };
        let ImportSourceFlag::Source(got) = source else {
            panic!("expected Source variant for {slug}");
        };
        assert_eq!(got.slug(), slug);
    }
}

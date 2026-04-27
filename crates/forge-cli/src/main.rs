use anyhow::Result;
use clap::Parser;
use forge_cli::{
    skill::{self as skill_mod, Resolver, SkillScope},
    Cli, Commands, ImportSourceFlag, McpCommands, RunCommands, SessionCommands, SessionNewKind,
    SkillCommands, SkillScopeFlag,
};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Session { cmd } => match cmd {
            SessionCommands::New { kind } => session_new(kind).await,
            SessionCommands::List => session_list().await,
            SessionCommands::Tail { id } => session_tail(&id).await,
            SessionCommands::Kill { id } => session_kill(&id).await,
        },
        Commands::Run { cmd } => match cmd {
            RunCommands::Agent { name, input } => run_agent(&name, &input).await,
        },
        Commands::Mcp { cmd } => match cmd {
            McpCommands::Import {
                source,
                apply,
                workspace,
            } => mcp_import(source, apply, workspace).await,
        },
        Commands::Skill { cmd } => match cmd {
            SkillCommands::Install { source, target } => skill_install(source, target).await,
            SkillCommands::List { workspace } => skill_list(workspace).await,
            SkillCommands::Remove {
                id,
                scope,
                workspace,
            } => skill_remove(id, scope, workspace).await,
        },
    }
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not resolve user home directory"))
}

async fn skill_install(source: String, target: SkillScopeFlag) -> Result<()> {
    let workspace_root = std::env::current_dir().unwrap_or_default();
    let home = home_dir()?;
    let scope: SkillScope = target.into();

    let resolved = if skill_mod::looks_like_git_url(&source) {
        let cache_root = skill_mod::default_cache_root(&home);
        let runner = skill_mod::StdCommandRunner;
        let resolver = skill_mod::GitResolver::new(source.clone(), cache_root, &runner);
        resolver.resolve()?
    } else {
        let resolver = skill_mod::LocalPathResolver::new(&source, &workspace_root);
        resolver.resolve()?
    };

    let installed = skill_mod::install_resolved(&resolved, scope, &workspace_root, &home)?;
    println!(
        "installed skill {} ({}) at {}",
        resolved.skill.id,
        scope,
        installed.display()
    );
    Ok(())
}

async fn skill_list(workspace: Option<PathBuf>) -> Result<()> {
    let workspace_root = workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let home = home_dir()?;
    let rows = skill_mod::list_installed(&workspace_root, &home)?;
    let mut stdout = std::io::stdout();
    skill_mod::render_list(&rows, &mut stdout)?;
    Ok(())
}

async fn skill_remove(
    id: String,
    scope: Option<SkillScopeFlag>,
    workspace: Option<PathBuf>,
) -> Result<()> {
    let workspace_root = workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let home = home_dir()?;

    let resolved_scope: SkillScope = match scope {
        Some(s) => s.into(),
        None => {
            // Pick a default by checking which scopes contain the id.
            let rows = skill_mod::list_installed(&workspace_root, &home)?;
            let workspace_has = rows
                .iter()
                .any(|r| r.id == id && r.scope == SkillScope::Workspace);
            let user_has = rows
                .iter()
                .any(|r| r.id == id && r.scope == SkillScope::User);
            match (workspace_has, user_has) {
                (true, _) => SkillScope::Workspace,
                (false, true) => SkillScope::User,
                (false, false) => {
                    anyhow::bail!("no installed skill with id {id:?}");
                }
            }
        }
    };

    let removed = skill_mod::remove_skill(&id, resolved_scope, &workspace_root, &home)?;
    if removed {
        println!("removed skill {id} from {resolved_scope} scope");
    } else {
        anyhow::bail!("skill {id:?} not installed in {resolved_scope} scope");
    }
    Ok(())
}

async fn mcp_import(
    source: ImportSourceFlag,
    apply: bool,
    workspace: Option<PathBuf>,
) -> Result<()> {
    let workspace_root = workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not resolve user home directory"))?;
    let source = match source {
        ImportSourceFlag::Auto => None,
        ImportSourceFlag::Source(s) => Some(s),
    };
    let args = forge_cli::mcp::ImportArgs {
        workspace_root,
        home,
        source,
        apply,
    };
    let mut stdout = std::io::stdout();
    let code = forge_cli::mcp::run(&args, &mut stdout)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

async fn session_new(kind: SessionNewKind) -> Result<()> {
    let workspace = match &kind {
        SessionNewKind::Agent { workspace, .. } | SessionNewKind::Provider { workspace, .. } => {
            workspace
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
        }
    };

    let session_id = forge_core::SessionId::new();
    let sock = forge_cli::socket::socket_path(&session_id.to_string())?;
    let pid_file = forge_cli::socket::pid_path(&session_id.to_string())?;

    if let Some(parent) = sock.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let forged = find_forged_binary()?;
    let mut cmd = std::process::Command::new(&forged);
    // F-049: forged owns the pid-file lifecycle. The CLI tells the daemon
    // where to write it (atomic O_EXCL, removed on clean exit); the CLI
    // no longer touches the file itself. This eliminates the window where
    // the CLI had recorded a pid before forged had fully started, and
    // guarantees removal on daemon-initiated exit.
    cmd.env("FORGE_SESSION_ID", session_id.to_string())
        .env("FORGE_SOCKET_PATH", sock.to_str().unwrap_or(""))
        .env("FORGE_WORKSPACE", workspace.to_str().unwrap_or(""))
        .env("FORGE_PID_FILE", pid_file.to_str().unwrap_or(""));

    match &kind {
        SessionNewKind::Agent { name, provider, .. } => {
            cmd.arg("--agent").arg(name);
            if let Some(spec) = provider {
                cmd.arg("--provider").arg(spec);
            }
        }
        SessionNewKind::Provider { spec, .. } => {
            cmd.arg("--provider").arg(spec);
        }
    }

    // Spawn forged as a detached process. Using std::process::Command means
    // the child is not killed when this handle is dropped; forged lives on
    // independently and is adopted by init once `forge` exits.
    let child = cmd.spawn()?;
    // Explicitly leak the handle — we want forged to run independently.
    std::mem::forget(child);

    // Wait for socket to appear (which confirms forged is up and the pid
    // file is already written — see forge-session/src/main.rs).
    wait_for_socket(&sock).await?;

    println!("session {} started at {}", session_id, sock.display());
    Ok(())
}

async fn session_list() -> Result<()> {
    use forge_ipc::{read_frame, write_frame, ClientInfo, Hello, IpcMessage, PROTO_VERSION};
    use tokio::net::UnixStream;

    let dir = forge_cli::socket::sessions_socket_dir()?;
    let mut read_dir = match tokio::fs::read_dir(&dir).await {
        Ok(d) => d,
        Err(_) => {
            println!("no active sessions");
            return Ok(());
        }
    };

    let mut found = false;
    while let Some(entry) = read_dir.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sock") {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        match UnixStream::connect(&path).await {
            Ok(mut stream) => {
                let hello = IpcMessage::Hello(Hello {
                    proto: PROTO_VERSION,
                    client: ClientInfo {
                        kind: "forge-cli".into(),
                        pid: std::process::id(),
                        user: whoami(),
                    },
                });
                if write_frame(&mut stream, &hello).await.is_ok() {
                    if let Ok(IpcMessage::HelloAck(ack)) = read_frame(&mut stream).await {
                        println!(
                            "{id}  active  workspace={}  started={}",
                            ack.workspace, ack.started_at
                        );
                        found = true;
                    }
                }
            }
            Err(_) => {
                println!("{id}  stale");
                found = true;
            }
        }
    }

    if !found {
        println!("no active sessions");
    }
    Ok(())
}

async fn session_tail(id: &str) -> Result<()> {
    use forge_core::Event;
    use forge_ipc::{
        read_frame, write_frame, ClientInfo, Hello, IpcMessage, Subscribe, PROTO_VERSION,
    };
    use tokio::net::UnixStream;

    let sock = forge_cli::socket::socket_path(id)?;
    let mut stream = UnixStream::connect(&sock)
        .await
        .map_err(|e| anyhow::anyhow!("cannot connect to session {id}: {e}"))?;

    write_frame(
        &mut stream,
        &IpcMessage::Hello(Hello {
            proto: PROTO_VERSION,
            client: ClientInfo {
                kind: "forge-cli".into(),
                pid: std::process::id(),
                user: whoami(),
            },
        }),
    )
    .await?;
    let _ack: IpcMessage = read_frame(&mut stream).await?;

    write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 })).await?;

    loop {
        // `read_frame` returns `Err` on clean EOF (`read_u32` fails) as
        // well as on malformed bodies. For a tail command either is a
        // reason to stop — surface the distinction via a log only if we
        // ever add structured error reporting here.
        match read_frame(&mut stream).await {
            Ok(IpcMessage::Event(ipc_event)) => {
                // F-112: IpcEvent.event is typed — no Value decode.
                let event = ipc_event.event;
                if let Some(line) = forge_cli::display::format_event(&event) {
                    println!("{line}");
                }
                if matches!(event, Event::SessionEnded { .. }) {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

async fn session_kill(id: &str) -> Result<()> {
    let pid_file = forge_cli::socket::pid_path(id)?;
    // F-049: race-free kill via start-time verification + pidfd_send_signal.
    // `kill_session_from_pid_file` reads the two-line record (pid + start-time),
    // re-reads `/proc/<pid>/stat` to confirm the PID hasn't been recycled,
    // and signals via `pidfd_open`/`pidfd_send_signal` (so even a process
    // exiting between start-time check and delivery cannot cause SIGTERM
    // to be delivered to a reused PID).
    //
    // Pid-file removal is owned by `forged` itself (see F-049 pid_file
    // module); we do not remove it here.
    let (pid, _start_time) = forge_cli::socket::kill_session_from_pid_file(&pid_file)
        .map_err(|e| anyhow::anyhow!("cannot kill session {id}: {e}"))?;
    println!("sent SIGTERM to session {id} (pid {pid})");
    Ok(())
}

async fn run_agent(name: &str, input_source: &str) -> Result<()> {
    use forge_core::Event;
    use forge_ipc::{
        read_frame, write_frame, ClientInfo, Hello, IpcMessage, SendUserMessage, Subscribe,
        PROTO_VERSION,
    };
    use tokio::net::UnixStream;

    let text = if input_source == "-" {
        use tokio::io::AsyncReadExt;
        let mut buf = String::new();
        tokio::io::stdin().read_to_string(&mut buf).await?;
        buf
    } else {
        tokio::fs::read_to_string(input_source).await?
    };

    let session_id = forge_core::SessionId::new();
    let sock = forge_cli::socket::socket_path(&session_id.to_string())?;

    if let Some(parent) = sock.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // F-049: ephemeral `forge run agent` waits on the child handle
    // directly (see `child.wait()` below), so there is no external
    // `session_kill` consumer for its pid file. Skip writing one rather
    // than leave a legacy single-line pid file that cannot pass the
    // two-line validation in `session_kill`.
    let forged = find_forged_binary()?;
    let mut child = tokio::process::Command::new(&forged)
        .arg("--agent")
        .arg(name)
        .arg("--auto-approve-unsafe")
        .arg("--ephemeral")
        .env("FORGE_SESSION_ID", session_id.to_string())
        .env("FORGE_SOCKET_PATH", sock.to_str().unwrap_or(""))
        .spawn()?;

    wait_for_socket(&sock).await?;

    let mut stream = UnixStream::connect(&sock).await?;

    write_frame(
        &mut stream,
        &IpcMessage::Hello(Hello {
            proto: PROTO_VERSION,
            client: ClientInfo {
                kind: "forge-cli".into(),
                pid: std::process::id(),
                user: whoami(),
            },
        }),
    )
    .await?;
    let _ack: IpcMessage = read_frame(&mut stream)
        .await
        .map_err(|e| anyhow::anyhow!("handshake failed: {e}"))?;

    write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 })).await?;
    write_frame(
        &mut stream,
        &IpcMessage::SendUserMessage(SendUserMessage { text }),
    )
    .await?;

    // Stream events until the session ends. An `Err` here means either
    // clean EOF (the daemon closed the connection) or a malformed frame
    // — in both cases we stop tailing and let the child exit code
    // determine the outcome.
    let mut event_exit_code = 0i32;
    loop {
        match read_frame(&mut stream).await {
            Ok(IpcMessage::Event(ipc_event)) => {
                // F-112: IpcEvent.event is typed — no Value decode.
                let event = ipc_event.event;
                if let Some(line) = forge_cli::display::format_event(&event) {
                    println!("{line}");
                }
                if let Event::SessionEnded { reason, .. } = &event {
                    if matches!(reason, forge_core::EndReason::Error(_)) {
                        event_exit_code = 1;
                    }
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    // Await the forged process; prefer its OS exit code, fall back to event-derived code.
    let process_exit_code = child
        .wait()
        .await
        .ok()
        .and_then(|s| s.code())
        .unwrap_or(event_exit_code);
    let exit_code = if process_exit_code != 0 {
        process_exit_code
    } else {
        event_exit_code
    };

    std::process::exit(exit_code);
}

/// Locate the `forged` binary relative to the current executable, then fall back to PATH.
fn find_forged_binary() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("forged");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    Ok(PathBuf::from("forged"))
}

/// Wait until a Unix socket file appears (max 5 seconds, polling every 50ms).
async fn wait_for_socket(path: &std::path::Path) -> Result<()> {
    for _ in 0..100 {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("timed out waiting for socket at {}", path.display())
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

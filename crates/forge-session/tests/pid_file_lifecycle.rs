//! Integration test: `forged` owns its pid file lifecycle (F-049, F-338).
//!
//! Persistent-mode `forged` must:
//!   - write its pid file atomically via `O_EXCL` (refuse to overwrite an
//!     existing file),
//!   - write the two-line `<pid>\n<start_time>\n` format so `session_kill`
//!     can detect pid reuse,
//!   - remove the pid file on clean SIGTERM exit.
//!
//! These tests spawn `forged` with an explicit `FORGE_PID_FILE` pointing
//! into a `TempDir` so they don't collide with live sessions on the host.
//!
//! Cross-platform: the F-338 rework replaced the Linux-only
//! `/proc/self/stat` probe with `crate::starttime::read_self_starttime`,
//! so the full lifecycle runs on macOS too. The Linux-only assertion
//! that used to compare the recorded value to `/proc/<pid>/stat` field
//! 22 stays gated — macOS can't round-trip through `/proc`. macOS
//! asserts the same invariant via the pid-file shape + starttime range.
//! Windows isn't exercised (no CI runner yet, and the pid-file test
//! spawner drives a persistent daemon whose full UDS handshake is
//! Unix-only anyway).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use forge_cli::socket::parse_pid_file_record;
use forge_ipc::{ClientInfo, Hello, IpcMessage, Subscribe, PROTO_VERSION};
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::UnixStream;

const FORGED: &str = env!("CARGO_BIN_EXE_forged");

async fn connect_with_retry(path: &std::path::Path) -> UnixStream {
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(path).await {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    UnixStream::connect(path).await.expect("forged socket")
}

async fn wait_for_file(path: &std::path::Path) {
    // ~2 s budget so a regression never hangs CI; see F-049 tests.
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("file never appeared: {}", path.display());
}

async fn handshake(sock: &std::path::Path) {
    let mut stream = connect_with_retry(sock).await;
    forge_ipc::write_frame(
        &mut stream,
        &IpcMessage::Hello(Hello {
            proto: PROTO_VERSION,
            client: ClientInfo {
                kind: "test".into(),
                pid: std::process::id(),
                user: "tester".into(),
            },
        }),
    )
    .await
    .unwrap();
    let _ack: IpcMessage = forge_ipc::read_frame(&mut stream).await.unwrap();
    forge_ipc::write_frame(&mut stream, &IpcMessage::Subscribe(Subscribe { since: 0 }))
        .await
        .unwrap();
}

#[tokio::test]
async fn forged_writes_two_line_pid_file_with_self_starttime() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let session_id = "pid-lifecycle-writes";
    let sock_path = dir.path().join("session.sock");
    let pid_file = dir.path().join("session.pid");

    let mut child = tokio::process::Command::new(FORGED)
        .env("FORGE_SESSION_ID", session_id)
        .env("FORGE_SOCKET_PATH", &sock_path)
        .env("FORGE_WORKSPACE", &workspace)
        .env("FORGE_PID_FILE", &pid_file)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn forged");

    wait_for_file(&pid_file).await;
    let raw = std::fs::read_to_string(&pid_file).expect("read pid file");
    let (recorded_pid, recorded_st) =
        parse_pid_file_record(&raw).expect("two-line pid-file format");

    // Recorded pid is forged's pid (the direct child we spawned).
    let forged_pid = child.id().expect("child pid") as libc::pid_t;
    assert_eq!(recorded_pid, forged_pid);

    // F-338: the Linux probe exposes `parse_proc_stat_starttime` so
    // the recorded value can be round-tripped against a fresh read of
    // `/proc/<forged_pid>/stat`. macOS has no equivalent text-parsing
    // entry point (libproc is FFI), so on that platform we only assert
    // the value is non-zero and shaped like microseconds-since-epoch
    // (what the macOS branch of `read_self_starttime` produces). Both
    // branches enforce the same end-to-end invariant: the pid file
    // contains a value that uniquely identifies the live `forged`.
    #[cfg(target_os = "linux")]
    {
        use forge_cli::socket::parse_proc_stat_starttime;
        let stat = std::fs::read_to_string(format!("/proc/{forged_pid}/stat")).expect("proc stat");
        let current_st = parse_proc_stat_starttime(&stat).expect("parse starttime");
        assert_eq!(recorded_st, current_st);
    }
    #[cfg(target_os = "macos")]
    {
        assert!(
            recorded_st > 1_000_000_000_000_000,
            "macOS starttime is microseconds-since-epoch; got implausibly small value {recorded_st}"
        );
    }

    // Clean up: SIGTERM forged so the test doesn't leak it.
    // SAFETY: pid is the live child we spawned.
    unsafe {
        libc::kill(forged_pid, libc::SIGTERM);
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

#[tokio::test]
async fn forged_removes_pid_file_on_sigterm() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let session_id = "pid-lifecycle-removes";
    let sock_path = dir.path().join("session.sock");
    let pid_file = dir.path().join("session.pid");

    let mut child = tokio::process::Command::new(FORGED)
        .env("FORGE_SESSION_ID", session_id)
        .env("FORGE_SOCKET_PATH", &sock_path)
        .env("FORGE_WORKSPACE", &workspace)
        .env("FORGE_PID_FILE", &pid_file)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn forged");

    wait_for_file(&pid_file).await;
    // Handshake so we know the server is up before signalling.
    handshake(&sock_path).await;

    let forged_pid = child.id().expect("child pid") as libc::pid_t;
    // SAFETY: live child.
    unsafe {
        let rc = libc::kill(forged_pid, libc::SIGTERM);
        assert_eq!(rc, 0, "kill SIGTERM: {}", std::io::Error::last_os_error());
    }
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("forged did not exit")
        .expect("wait");
    assert!(status.success(), "forged exited non-zero: {status:?}");

    assert!(
        !pid_file.exists(),
        "pid file must be removed on clean SIGTERM: {}",
        pid_file.display()
    );
}

#[tokio::test]
async fn forged_refuses_to_start_when_pid_file_already_exists() {
    // Atomic write via O_EXCL: a leftover pid file from a crashed prior
    // run must not be silently clobbered. If this invariant slips, two
    // concurrent `forged` instances could race on the same pid file and
    // the later one's pid would overwrite the earlier one's.
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let session_id = "pid-lifecycle-oexcl";
    let sock_path = dir.path().join("session.sock");
    let pid_file = dir.path().join("session.pid");

    // Pre-populate the pid file with stale contents.
    std::fs::write(&pid_file, "99999\n0\n").unwrap();

    let out = tokio::process::Command::new(FORGED)
        .env("FORGE_SESSION_ID", session_id)
        .env("FORGE_SOCKET_PATH", &sock_path)
        .env("FORGE_WORKSPACE", &workspace)
        .env("FORGE_PID_FILE", &pid_file)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("run forged");

    assert!(
        !out.status.success(),
        "forged must refuse to start when pid file exists"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lower = stderr.to_lowercase();
    assert!(
        lower.contains("pid")
            && (lower.contains("exists") || lower.contains("o_excl") || lower.contains("already")),
        "expected pid-file-exists error, got stderr: {stderr}"
    );

    // Pre-existing file must still be there (we did not overwrite it).
    let after = std::fs::read_to_string(&pid_file).unwrap();
    assert_eq!(after, "99999\n0\n");
}

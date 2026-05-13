//! F-044 (H8) regression: post-bind UDS socket permissions.
//!
//! Phase 1's IPC trust boundary is the Unix domain socket — anyone who can
//! connect to it can drive the session, approve tool calls, and exfiltrate
//! events. Before F-044 the socket was created with `0777 & ~umask` (typically
//! `0755`, world-connectable). This test spawns `forged` with an explicit
//! `FORGE_SOCKET_PATH` and asserts that after the listener has bound, the
//! socket file's mode is exactly `0o600` — owner read/write, no one else.
//!
//! The test is Linux-gated because the audit target is Linux/systemd and CI
//! only runs Linux runners. macOS/Windows get the same `set_permissions` call
//! in `server.rs` but their semantics around Unix-socket permissions differ
//! and are intentionally out of scope for this finding.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use tempfile::TempDir;
use tokio::net::UnixStream;

const FORGED: &str = env!("CARGO_BIN_EXE_forged");

async fn wait_for_socket(path: &std::path::Path) {
    for _ in 0..100 {
        if let Ok(stream) = UnixStream::connect(path).await {
            drop(stream);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!(
        "forged did not create socket {} within deadline",
        path.display()
    );
}

#[tokio::test]
async fn bound_socket_has_mode_0600() {
    let dir = TempDir::new().expect("tempdir");
    let sock_path = dir.path().join("session.sock");

    // Relax the caller's umask so any accidental reliance on the caller's
    // umask to mask bits would be exposed by this test. If `server.rs` still
    // depended on umask, the bound socket would come out `0o666`; with the
    // F-044 chmod, it must be `0o600` regardless.
    // SAFETY: umask(2) is async-signal-safe; single-threaded test process at
    // this point (tokio runtime hasn't spawned workers yet for this task).
    let _prev = unsafe { libc::umask(0) };

    let mut child = tokio::process::Command::new(FORGED)
        .arg("--auto-approve-unsafe")
        .env("FORGE_SESSION_ID", "permtest00000001")
        .env("FORGE_SOCKET_PATH", &sock_path)
        // F-743: forged requires an explicit provider spec; this test
        // exercises UDS socket permission bits and never drives a turn.
        .env("FORGE_PROVIDER", "mock")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn forged");

    wait_for_socket(&sock_path).await;

    let meta = std::fs::metadata(&sock_path).expect("stat socket");
    let mode = meta.permissions().mode() & 0o777;

    let _ = child.kill().await;
    let _ = child.wait().await;

    assert_eq!(
        mode, 0o600,
        "socket mode must be 0o600 after bind (got {mode:o}) — anyone else on the host can \
         otherwise connect and drive this session"
    );
}

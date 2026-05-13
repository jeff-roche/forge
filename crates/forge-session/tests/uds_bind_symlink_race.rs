//! F-056 (T6) regression: UDS pre-bind `remove_file` is a TOCTOU race.
//!
//! Before F-056, `serve_with_session` did:
//!
//! ```text
//! if path.exists() { remove_file(path).await?; }
//! listener.bind(path)
//! ```
//!
//! An attacker with write access to the parent directory could pre-create the
//! socket path as a symlink (or a regular file) and watch the daemon blindly
//! unlink whatever was there. Under H8's shared-`/tmp/forge-0/` fallback this
//! was a full exploit; F-044 removed that fallback, but the TOCTOU still
//! matters as defense-in-depth for any future path configuration (e.g.
//! operators pointing `FORGE_SOCKET_PATH` into a shared location themselves).
//!
//! The fix: try `bind` first. On `EADDRINUSE`, probe with a short
//! `UnixStream::connect` — only unlink-and-retry when the probe confirms the
//! entry is an orphan socket. Critically, we never unlink a symlink or a
//! regular file; both are signs of an attack or misconfiguration.
//!
//! These tests spawn `forged` with an explicit `FORGE_SOCKET_PATH` pointing at
//! a symlink and assert that the daemon fails to start without mutating the
//! symlink or its target.
//!
//! Linux-gated to match the rest of the UDS regression suite — the threat
//! model is Linux/systemd and CI only runs Linux runners.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;

const FORGED: &str = env!("CARGO_BIN_EXE_forged");

/// Waits up to ~2s for `forged` to exit on its own (we expect it to bail out
/// fast when the bind path is a symlink). Returns the exit status if the
/// process exited, or kills it and returns `None` if it's still running — a
/// `None` here is the regression signal: the old buggy code would have happily
/// unlinked the symlink and proceeded to serve.
async fn wait_for_exit(child: &mut tokio::process::Child) -> Option<std::process::ExitStatus> {
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            None
        }
    }
}

#[tokio::test]
async fn symlink_at_bind_path_is_not_unlinked_and_rebound() {
    let dir = TempDir::new().expect("tempdir");
    let target = dir.path().join("attacker-target.txt");
    let sock_path = dir.path().join("session.sock");

    // Victim's bind path is a symlink pointing at a regular file the attacker
    // already owned. Under the old code, `remove_file(&sock_path)` would have
    // unlinked the symlink, and `bind(&sock_path)` would then have created a
    // real socket at that name — quietly letting the daemon start. The fix
    // must refuse, leaving both the symlink and the target untouched.
    std::fs::write(&target, b"do not touch").expect("write target");
    std::os::unix::fs::symlink(&target, &sock_path).expect("create symlink");

    let mut child = tokio::process::Command::new(FORGED)
        .arg("--auto-approve-unsafe")
        .env("FORGE_SESSION_ID", "f056symlink00001")
        .env("FORGE_SOCKET_PATH", &sock_path)
        // F-743: forged requires an explicit provider spec; this test
        // exercises UDS bind safety and never drives a turn.
        .env("FORGE_PROVIDER", "mock")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn forged");

    let status = wait_for_exit(&mut child).await;

    // After forged has exited (or we killed it), the symlink MUST still be a
    // symlink and the target MUST still exist with its original contents. Any
    // path that went through the old `remove_file + bind` sequence would have
    // replaced `session.sock` with a real socket and left `target` orphaned.
    let meta = std::fs::symlink_metadata(&sock_path).expect("stat sock_path");
    assert!(
        meta.file_type().is_symlink(),
        "F-056: daemon must not replace a symlink at the bind path with a fresh socket"
    );
    assert!(
        target.exists(),
        "F-056: attacker target file must still exist — daemon should have refused to bind"
    );
    let contents = std::fs::read(&target).expect("read target");
    assert_eq!(
        contents, b"do not touch",
        "F-056: attacker target contents must be untouched"
    );

    // Forged must have exited non-zero, not continued to serve. `None` means
    // it was still running after 2s, which is the exact failure mode the old
    // code exhibited.
    let status = status.expect("forged should exit fast when bind path is a symlink");
    assert!(
        !status.success(),
        "F-056: forged must exit non-zero when it refuses to bind onto a symlink"
    );
}

#[tokio::test]
async fn dangling_symlink_at_bind_path_is_not_unlinked_and_rebound() {
    // Variant: the symlink points at a nonexistent target. Bind still fails
    // (EADDRINUSE-ish or ENOENT via symlink resolution), the liveness probe
    // still fails, but `symlink_metadata` still reports a symlink — so the
    // fix must still refuse to unlink.
    let dir = TempDir::new().expect("tempdir");
    let sock_path = dir.path().join("session.sock");
    let nowhere = dir.path().join("does-not-exist");

    std::os::unix::fs::symlink(&nowhere, &sock_path).expect("create dangling symlink");

    let mut child = tokio::process::Command::new(FORGED)
        .arg("--auto-approve-unsafe")
        .env("FORGE_SESSION_ID", "f056dangling0001")
        .env("FORGE_SOCKET_PATH", &sock_path)
        // F-743: forged requires an explicit provider spec; this test
        // exercises UDS bind safety and never drives a turn.
        .env("FORGE_PROVIDER", "mock")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn forged");

    let status = wait_for_exit(&mut child).await;

    let meta = std::fs::symlink_metadata(&sock_path).expect("stat sock_path");
    assert!(
        meta.file_type().is_symlink(),
        "F-056: daemon must not replace a dangling symlink with a fresh socket"
    );

    let status = status.expect("forged should exit fast when bind path is a dangling symlink");
    assert!(
        !status.success(),
        "F-056: forged must exit non-zero when it refuses to bind onto a dangling symlink"
    );
}

#[tokio::test]
async fn regular_file_at_bind_path_is_not_unlinked_and_rebound() {
    // Third variant: an attacker (or stale leftover) plants a regular file at
    // the bind path. The old code would unlink it; the fix must refuse
    // because a regular file is never a legitimate orphan socket.
    let dir = TempDir::new().expect("tempdir");
    let sock_path = dir.path().join("session.sock");

    std::fs::write(&sock_path, b"attacker-planted").expect("write regular file");
    // Give it a unique mode so we can assert the inode is untouched.
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o644))
        .expect("chmod regular file");

    let mut child = tokio::process::Command::new(FORGED)
        .arg("--auto-approve-unsafe")
        .env("FORGE_SESSION_ID", "f056regfile00001")
        .env("FORGE_SOCKET_PATH", &sock_path)
        // F-743: forged requires an explicit provider spec; this test
        // exercises UDS bind safety and never drives a turn.
        .env("FORGE_PROVIDER", "mock")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn forged");

    let status = wait_for_exit(&mut child).await;

    let meta = std::fs::symlink_metadata(&sock_path).expect("stat sock_path");
    assert!(
        meta.file_type().is_file(),
        "F-056: daemon must not replace a regular file with a fresh socket"
    );
    let contents = std::fs::read(&sock_path).expect("read regular file");
    assert_eq!(
        contents, b"attacker-planted",
        "F-056: attacker-planted file contents must be untouched"
    );

    let status = status.expect("forged should exit fast when bind path is a regular file");
    assert!(
        !status.success(),
        "F-056: forged must exit non-zero when it refuses to bind onto a regular file"
    );
}

//! Integration test: drives a real `podman` against a real image.
//!
//! Marked `#[ignore]` because it requires rootless `podman` on `PATH`. CI's
//! default `cargo test` skips it cleanly; run locally with:
//!
//! ```sh
//! cargo test -p forge-oci -- --ignored
//! ```
//!
//! When run, the test fails loudly on a misconfigured host instead of
//! masking the issue with an "auto-skip" early return.
//!
//! `cfg(target_os = "linux")` keeps the test off non-Linux hosts entirely
//! (rootless semantics differ; F-595 is Linux-first).

#![cfg(target_os = "linux")]

use forge_oci::{ContainerRuntime, ImageRef, PodmanRuntime};

/// End-to-end flag-injection regression test for `create`.
///
/// Proves empirically that `podman create <image> --privileged sh` does NOT
/// apply `--privileged` as a podman runtime flag. Podman's positional grammar
/// (`podman create [options] IMAGE [COMMAND [ARG...]]`) terminates flag
/// parsing at IMAGE, so caller-supplied argv after the image is the
/// in-container command. This test pins that behaviour: if a future podman
/// version regresses and starts treating post-IMAGE flags as runtime options,
/// `HostConfig.Privileged` would flip to `true` and this test would fail.
///
/// We do this end-to-end because the safety property lives in podman's parser,
/// not in our argv-shaping. A unit test against the mock runner can only
/// assert the positional ordering — only `podman inspect` can tell us
/// `--privileged` was rejected as a flag.
#[tokio::test]
#[ignore = "requires rootless podman on PATH (run with --ignored)"]
async fn create_does_not_apply_caller_flags_as_runtime_flags() {
    let runtime = PodmanRuntime::new();
    runtime.detect().await.expect("podman detect");
    let image = ImageRef::parse("docker.io/library/alpine:3.19").expect("valid image ref");
    runtime.pull(&image).await.expect("pull alpine");

    // Caller argv begins with `--privileged`. If podman wrongly treated this
    // as its own `--privileged` flag, the resulting container would have
    // `HostConfig.Privileged = true`, which we explicitly check against.
    let handle = runtime
        .create(&image, &["--privileged", "sh", "-c", "true"])
        .await
        .expect("create container");

    let inspect = std::process::Command::new("podman")
        .args([
            "inspect",
            "--format",
            "{{.HostConfig.Privileged}}|{{json .Config.Cmd}}",
            &handle.id,
        ])
        .output()
        .expect("podman inspect spawn");
    assert!(
        inspect.status.success(),
        "podman inspect failed: {}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let out = String::from_utf8_lossy(&inspect.stdout).trim().to_string();
    let (privileged, cmd) = out.split_once('|').expect("inspect format");

    assert_eq!(
        privileged, "false",
        "FLAG INJECTION: caller's `--privileged` was applied as a podman runtime flag (cmd={cmd})",
    );
    // Caller's literal tokens should appear verbatim as the container Cmd.
    assert!(
        cmd.contains("--privileged"),
        "expected `--privileged` to appear in container Cmd, got {cmd}"
    );

    runtime.remove(&handle).await.expect("remove container");
}

/// End-to-end flag-injection regression test for `exec`.
///
/// Proves `podman exec <CID> --user root id` does NOT run as user root inside
/// the container — podman parses `--user root id` as the in-container command,
/// so `crun` tries to exec `--user` as the program (and fails). The exit
/// status MUST be non-zero, proving the flag was not honoured. If a future
/// podman version regressed and silently honoured `--user`, we'd see
/// `uid=0(root)` in stdout instead.
#[tokio::test]
#[ignore = "requires rootless podman on PATH (run with --ignored)"]
async fn exec_does_not_apply_caller_flags_as_runtime_flags() {
    let runtime = PodmanRuntime::new();
    runtime.detect().await.expect("podman detect");
    let image = ImageRef::parse("docker.io/library/alpine:3.19").expect("valid image ref");
    runtime.pull(&image).await.expect("pull alpine");

    let handle = runtime
        .create(&image, &["sleep", "30"])
        .await
        .expect("create container");
    runtime.start(&handle).await.expect("start container");

    let result = runtime
        .exec(&handle, &["--user", "root", "id"])
        .await
        .expect("exec returns even when in-container command fails");

    assert_ne!(
        result.exit_code,
        Some(0),
        "FLAG INJECTION: caller's `--user root id` exec succeeded — \
         podman applied `--user` as a runtime flag (stdout={:?})",
        result.stdout
    );
    assert!(
        !result.stdout.contains("uid=0(root)"),
        "FLAG INJECTION: exec ran as root via caller-supplied `--user` (stdout={:?})",
        result.stdout
    );

    runtime.remove(&handle).await.expect("remove container");
}

#[tokio::test]
#[ignore = "requires rootless podman on PATH (run with --ignored)"]
async fn podman_full_lifecycle_against_alpine() {
    let runtime = PodmanRuntime::new();

    runtime
        .detect()
        .await
        .expect("podman detect: rootless podman must be configured");

    let image = ImageRef::parse("docker.io/library/alpine:3.19").expect("valid image ref");

    runtime.pull(&image).await.expect("pull alpine");

    // Long-lived foreground process so `exec` has something to attach to.
    // `sleep 60` is plenty for the test to do its work and tear down.
    let handle = runtime
        .create(&image, &["sleep", "60"])
        .await
        .expect("create container");

    runtime.start(&handle).await.expect("start container");

    let result = runtime
        .exec(&handle, &["echo", "hello"])
        .await
        .expect("exec echo");
    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.exit_code, Some(0));

    let stats = runtime.stats(&handle).await.expect("stats");
    // Alpine `sleep` is tiny; just assert we got *some* signal back.
    assert!(
        stats.pids.unwrap_or(0) >= 1,
        "expected at least one PID, got {stats:?}"
    );

    runtime.remove(&handle).await.expect("remove container");

    // After remove, `inspect` must fail — proving cleanup actually happened.
    let inspect = std::process::Command::new("podman")
        .args(["inspect", &handle.id])
        .output()
        .expect("podman inspect spawn");
    assert!(
        !inspect.status.success(),
        "expected inspect to fail after remove; stdout={:?}",
        String::from_utf8_lossy(&inspect.stdout)
    );
}

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

use forge_oci::{ContainerLimits, ContainerRuntime, ImageRef, PodmanRuntime, SecurityOpts};

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
        .create(
            &image,
            &["--privileged", "sh", "-c", "true"],
            &SecurityOpts::permissive(),
        )
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
        .create(&image, &["sleep", "30"], &SecurityOpts::permissive())
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
        .create(&image, &["sleep", "60"], &SecurityOpts::permissive())
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

/// End-to-end proof that the F-642 hardened defaults actually land on
/// the resulting container.
///
/// The unit-level tests in `crates/forge-oci/src/podman.rs` pin the
/// shape of the rendered argv. This test pins the *behaviour* — by
/// asking `podman inspect` whether the security flags took effect, we
/// catch any silent podman-side rejection (e.g. unsupported flag, kernel
/// missing the seccomp profile, podman renaming a JSON field) that
/// argv-shaping assertions would miss.
///
/// `cargo test -p forge-oci -- --ignored` to run.
#[tokio::test]
#[ignore = "requires rootless podman on PATH (run with --ignored)"]
async fn create_with_hardened_defaults_applies_every_flag() {
    let runtime = PodmanRuntime::new();
    runtime.detect().await.expect("podman detect");
    let image = ImageRef::parse("docker.io/library/alpine:3.19").expect("valid image ref");
    runtime.pull(&image).await.expect("pull alpine");

    let handle = runtime
        .create(&image, &["sleep", "30"], &SecurityOpts::hardened_default())
        .await
        .expect("create container with hardened defaults");

    // Each format string asks podman a separate behavioural question.
    // Combined into one inspect invocation to keep the test fast.
    let inspect = std::process::Command::new("podman")
        .args([
            "inspect",
            "--format",
            "{{.HostConfig.SecurityOpt}}|{{.HostConfig.CapDrop}}|{{.HostConfig.ReadonlyRootfs}}|{{.HostConfig.NetworkMode}}|{{.HostConfig.UsernsMode}}",
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
    let parts: Vec<&str> = out.splitn(5, '|').collect();
    assert_eq!(
        parts.len(),
        5,
        "expected 5 inspect fields, got {parts:?} from {out:?}"
    );
    let (security_opt, cap_drop, readonly, network, userns) =
        (parts[0], parts[1], parts[2], parts[3], parts[4]);

    assert!(
        security_opt.contains("no-new-privileges"),
        "no-new-privileges missing from SecurityOpt: {security_opt}"
    );
    assert!(
        cap_drop.contains("ALL") || cap_drop.contains("CAP_"),
        "cap-drop ALL not visible in CapDrop: {cap_drop}"
    );
    assert!(
        readonly == "true",
        "ReadonlyRootfs must be true, got {readonly:?}"
    );
    assert!(
        network.contains("none"),
        "NetworkMode must be none, got {network:?}"
    );
    assert!(
        userns.contains("keep-id"),
        "UsernsMode must be keep-id, got {userns:?}"
    );

    runtime.remove(&handle).await.expect("remove container");
}

/// F-654: end-to-end proof that the F-654 conservative cgroup caps
/// land on the resulting container.
///
/// Argv-shaping is pinned by the unit tests in
/// `crates/forge-oci/src/lib.rs` and `crates/forge-oci/src/podman.rs`.
/// This test pins the *behaviour* — by asking `podman inspect` what
/// the container's cgroup caps actually are, we catch any silent
/// podman-side rejection (e.g. unsupported flag, kernel missing the
/// pids controller, podman renaming a JSON field) that argv assertions
/// would miss.
///
/// `cargo test -p forge-oci -- --ignored` to run.
#[tokio::test]
#[ignore = "requires rootless podman on PATH (run with --ignored)"]
async fn create_with_conservative_limits_applies_every_cgroup_cap() {
    let runtime = PodmanRuntime::new();
    runtime.detect().await.expect("podman detect");
    let image = ImageRef::parse("docker.io/library/alpine:3.19").expect("valid image ref");
    runtime.pull(&image).await.expect("pull alpine");

    // Use a distinct, easy-to-verify limit set so the inspect output
    // is unambiguous (the conservative-default values are the same
    // for cpus and memory bytes, which would let a swapped accessor
    // pass silently).
    let limits = ContainerLimits {
        cpus: Some(1.0),
        memory_bytes: Some(256 * 1024 * 1024),
        memory_swap_bytes: Some(256 * 1024 * 1024),
        pids_max: Some(128),
    };
    let opts = SecurityOpts {
        limits,
        ..SecurityOpts::permissive()
    };

    let handle = runtime
        .create(&image, &["sleep", "30"], &opts)
        .await
        .expect("create container with limits");

    // One inspect, four cgroup fields.
    //   - NanoCpus is podman's bytes-per-second representation of
    //     `--cpus`: 1.0 cpu = 1_000_000_000 nanoseconds-of-cpu.
    //   - Memory / MemorySwap are bytes.
    //   - PidsLimit is the cap.
    let inspect = std::process::Command::new("podman")
        .args([
            "inspect",
            "--format",
            "{{.HostConfig.NanoCpus}}|{{.HostConfig.Memory}}|{{.HostConfig.MemorySwap}}|{{.HostConfig.PidsLimit}}",
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
    let parts: Vec<&str> = out.splitn(4, '|').collect();
    assert_eq!(
        parts.len(),
        4,
        "expected 4 inspect fields, got {parts:?} from {out:?}"
    );
    let (nano_cpus, memory, memory_swap, pids_limit) = (parts[0], parts[1], parts[2], parts[3]);

    assert_eq!(
        nano_cpus, "1000000000",
        "NanoCpus must reflect --cpus 1.0 (got {nano_cpus:?})"
    );
    assert_eq!(
        memory,
        (256 * 1024 * 1024_u64).to_string(),
        "Memory must reflect --memory (got {memory:?})"
    );
    assert_eq!(
        memory_swap,
        (256 * 1024 * 1024_u64).to_string(),
        "MemorySwap must reflect --memory-swap == memory (no swap; got {memory_swap:?})"
    );
    assert_eq!(
        pids_limit, "128",
        "PidsLimit must reflect --pids-limit (got {pids_limit:?})"
    );

    runtime.remove(&handle).await.expect("remove container");
}

/// F-654: behavioural proof that `--pids-limit` actually bounds a
/// fork-bomb. Without the cap the container would inherit the user
/// slice's default and a `:(){ :|:& };:` style workload would
/// saturate the host until OOM.
///
/// Strategy: run a small fork-bomb inside a container with
/// `pids_max = 16`, capture exec stderr, then assert the kernel
/// surfaced a "Resource temporarily unavailable" / "fork: retry"
/// signal — that's the cgroup `pids.max` rejection. The exec call
/// also returns within the foreground sleep window, proving the
/// container did not run unbounded.
#[tokio::test]
#[ignore = "requires rootless podman on PATH (run with --ignored)"]
async fn pids_limit_bounds_a_fork_bomb_inside_the_container() {
    let runtime = PodmanRuntime::new();
    runtime.detect().await.expect("podman detect");
    let image = ImageRef::parse("docker.io/library/alpine:3.19").expect("valid image ref");
    runtime.pull(&image).await.expect("pull alpine");

    let limits = ContainerLimits {
        cpus: Some(1.0),
        memory_bytes: Some(256 * 1024 * 1024),
        memory_swap_bytes: Some(256 * 1024 * 1024),
        pids_max: Some(16),
    };
    let opts = SecurityOpts {
        limits,
        ..SecurityOpts::permissive()
    };

    let handle = runtime
        .create(&image, &["sleep", "30"], &opts)
        .await
        .expect("create container with pid cap");
    runtime.start(&handle).await.expect("start container");

    // The shell loop forks repeatedly; each `&` backgrounds a
    // detached subshell. With `pids_max=16` the kernel rejects
    // additional `clone()` calls and the loop's stderr captures the
    // refusal. We bound iterations to keep the test fast and exit
    // 0 on its own so exec returns promptly even when no caps fire
    // (in which case the assertion below catches the regression).
    let result = runtime
        .exec(
            &handle,
            &[
                "sh",
                "-c",
                "i=0; while [ $i -lt 200 ]; do (sleep 5) & i=$((i+1)); done; wait 2>/dev/null; true",
            ],
        )
        .await
        .expect("exec returns even when forks fail");

    // The shell prints `sh: can't fork: Resource temporarily
    // unavailable` (or similar) once the cgroup refuses additional
    // tasks. Either pattern is enough: any text in stderr proves the
    // cap fired, since a successful run produces no stderr at all.
    assert!(
        !result.stderr.is_empty(),
        "expected fork-failure noise in stderr (cap should have fired); \
         stdout={:?} stderr={:?}",
        result.stdout,
        result.stderr
    );
    assert!(
        result.stderr.to_lowercase().contains("fork")
            || result.stderr.to_lowercase().contains("resource")
            || result.stderr.to_lowercase().contains("retry"),
        "expected stderr to mention fork/Resource/retry, got: {:?}",
        result.stderr
    );

    runtime.remove(&handle).await.expect("remove container");
}

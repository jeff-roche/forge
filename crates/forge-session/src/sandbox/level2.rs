//! Level 2 isolation: container-backed step execution.
//!
//! Promotes a step from the Level-1 seccomp/setrlimit/cgroup sandbox to a
//! pre-warmed rootless container managed by [`forge_oci::ContainerRuntime`].
//! Per-session lifecycle is owned by [`Level2Session`]: pull the image once,
//! create + start the container, share the handle across every
//! `SandboxedCommand` in the turn, and `stop` + `remove` the container on
//! teardown. Per-step execution flows through `runtime.exec(handle, argv)`,
//! mirroring `podman exec`.
//!
//! # Auto-fallback
//!
//! [`detect_or_fall_back`] probes the runtime via the F-595 `detect`
//! contract. The three documented "container unavailable" variants —
//! [`OciError::RuntimeMissing`], [`OciError::RuntimeBroken`], and
//! [`OciError::RootlessUnavailable`] — are folded into [`Level2Unavailable`]
//! and surfaced via `tracing::warn` so callers can transparently fall back to
//! Level 1 instead of failing the session.
//!
//! # Resource limits
//!
//! Per-step caps are captured in [`ContainerLimits`] (re-exported from
//! `forge_oci`) and applied to `podman create` via
//! [`forge_oci::SecurityOpts::limits`]. F-654 closed the loop:
//! [`Level2Session::create`] now plumbs the configured limits through
//! to the runtime — when a caller passes `ContainerLimits::default()`
//! (every field `None`), the session falls back to
//! [`ContainerLimits::conservative_default`] (2 cpus, 4 GiB, 1024
//! pids, no swap) so a fork-bomb or memory-exhaust workload inside
//! the sandbox cannot starve the host. See
//! `docs/architecture/isolation-model.md` §8.3 for the unit conventions.
//!
//! # Deviation from the F-596 DoD
//!
//! The F-596 spec wrote the variant as
//! `SandboxLevel::Level2 { runtime: Box<dyn ContainerRuntime> }`. We
//! deliberately use `Arc<dyn ContainerRuntime>` instead: a single session
//! spawns many `SandboxedCommand` instances per turn that all need to
//! share the same pre-warmed container, and `Box` cannot be cloned across
//! those handles. The `Arc` carries the same dyn-trait surface area and is
//! cheaper than re-detecting / re-warming per step.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use forge_oci::{ContainerHandle, ContainerRuntime, ImageRef, OciError, SecurityOpts};

/// Per-step resource limits enforced via the container's cgroup v2
/// leaf. Re-exported from [`forge_oci`] so call sites in the session
/// crate keep the original `level2::ContainerLimits` import path.
///
/// See [`forge_oci::ContainerLimits`] for the field-level units and
/// [`forge_oci::ContainerLimits::conservative_default`] for the F-654
/// strict preset (2 cpus, 4 GiB, 1024 pids, no swap).
pub use forge_oci::ContainerLimits;

/// Binary used by [`Level2Session::drop`]'s panic-safety net to reap
/// a leaked container. Hardcoded because the only `ContainerRuntime`
/// impl in the workspace today is `PodmanRuntime`; if a second
/// implementation is added the safety net would need a small
/// abstraction (e.g. each impl supplies its own teardown argv).
///
/// TODO(F-682, issue #718): refactor into a per-impl shutdown command
/// when the second container runtime lands. Tracker explicitly defers
/// the change until then — adding the abstraction now would be
/// speculative scaffolding with one consumer.
const DROP_CLEANUP_BINARY: &str = "podman";

/// Render [`ContainerLimits`] into the argv fragment that would be
/// inserted between `podman create` and the IMAGE positional.
///
/// Thin wrapper over [`forge_oci::ContainerLimits::to_create_flags`]
/// kept on this module path because earlier call sites and tests
/// reach for `level2::limits_to_create_flags`. The actual shaping
/// lives in `forge-oci` so the limit flags travel through the same
/// `SecurityOpts` rendering as the security flags.
pub fn limits_to_create_flags(limits: ContainerLimits) -> Vec<String> {
    limits.to_create_flags()
}

/// Resolve the limits actually applied to the container.
///
/// `ContainerLimits::default()` (every field `None`) means the caller
/// did not specify caps; in that case we substitute the F-654
/// conservative preset so a fork-bomb / OOM workload cannot starve
/// the host. Any non-default input is honoured verbatim — operators
/// who explicitly opt for tighter or looser caps get exactly what
/// they asked for, including a single field that they wanted unset.
fn effective_limits(requested: ContainerLimits) -> ContainerLimits {
    if requested == ContainerLimits::default() {
        ContainerLimits::conservative_default()
    } else {
        requested
    }
}

/// Reasons Level 2 cannot be used on this host. Mapped 1:1 from the
/// F-595 [`OciError`] variants that signal "container runtime unreachable"
/// — every other [`OciError`] is treated as a hard failure.
#[derive(Debug, thiserror::Error)]
pub enum Level2Unavailable {
    /// `podman` (or another required tool) is not on `PATH`.
    #[error("container runtime '{0}' not installed")]
    RuntimeMissing(&'static str),

    /// Runtime is installed but the detection probe failed (cgroup
    /// delegation, missing newuidmap, SELinux denial, etc.).
    #[error("container runtime '{tool}' is installed but not functional: {stderr}")]
    RuntimeBroken {
        /// Runtime name (e.g. `"podman"`).
        tool: &'static str,
        /// Captured stderr from the failing probe.
        stderr: String,
    },

    /// Probe succeeded but rootless mode is unavailable.
    #[error("rootless mode unavailable for container runtime '{runtime}': {reason}")]
    RootlessUnavailable {
        /// Runtime name (e.g. `"podman"`).
        runtime: &'static str,
        /// Human-readable reason.
        reason: String,
    },
}

/// Map an [`OciError`] coming from `detect()` into either a fallback
/// signal ([`Level2Unavailable`]) or a hard failure (`Err(OciError)`).
///
/// The three "treat as fallback" variants are exactly the ones F-595's
/// [`forge_oci::PodmanRuntime::detect`] documents as "podman not usable on
/// this host". Anything else (I/O failure mid-probe, malformed JSON,
/// CommandFailed mid-probe) is propagated up: those are bugs we want to
/// see, not silent fallbacks.
pub fn classify_detect_error(err: OciError) -> Result<Level2Unavailable, OciError> {
    match err {
        OciError::RuntimeMissing(tool) => Ok(Level2Unavailable::RuntimeMissing(tool)),
        OciError::RuntimeBroken { tool, stderr } => {
            Ok(Level2Unavailable::RuntimeBroken { tool, stderr })
        }
        OciError::RootlessUnavailable { runtime, reason } => {
            Ok(Level2Unavailable::RootlessUnavailable { runtime, reason })
        }
        other => Err(other),
    }
}

/// Pre-warmed container shared across every `SandboxedCommand`
/// in a session.
///
/// One per session, not per step. `pull` runs once, `create` + `start`
/// each run once, `stop` + `remove` run once at teardown. Per-step
/// execution flows through [`Self::exec_step`] which delegates to
/// `runtime.exec`.
pub struct Level2Session {
    runtime: Arc<dyn ContainerRuntime>,
    image: ImageRef,
    handle: ContainerHandle,
    limits: ContainerLimits,
    /// Set by [`Level2Session::teardown`]; checked by
    /// [`Level2Session::drop`] so an explicit clean shutdown skips the
    /// `podman rm -f` panic-safety fire-and-forget.
    teardown_done: AtomicBool,
}

impl Level2Session {
    /// Probe the runtime and bring up the container.
    ///
    /// Sequence (matches the F-595 lifecycle):
    /// 1. `runtime.pull(image)` — idempotent; layers cached if already
    ///    present.
    /// 2. `runtime.create(image, init_argv, opts)` — the container's
    ///    "init" process plus the F-642 hardening flags and F-654
    ///    resource caps. We default the argv to `sleep infinity` via
    ///    [`Self::default_init_argv`] so the container stays alive
    ///    long enough for `exec` to hit it.
    /// 3. `runtime.start(handle)` — flips the container to running.
    ///
    /// Resource limits travel through
    /// [`forge_oci::SecurityOpts::limits`]: callers passing
    /// [`ContainerLimits::default`] (every field `None`) get the
    /// F-654 strict preset
    /// ([`ContainerLimits::conservative_default`]) so a config
    /// without explicit overrides still gets bounded — 2 cpus, 4
    /// GiB memory with swap disabled, 1024 pids. Callers that need
    /// looser caps override the specific field; an explicit
    /// `Some(value)` is always honoured.
    pub async fn create(
        runtime: Arc<dyn ContainerRuntime>,
        image: ImageRef,
        limits: ContainerLimits,
    ) -> Result<Self, OciError> {
        runtime.pull(&image).await?;
        // F-642: every Level 2 container is created with the strict
        // hardening defaults — no-new-privileges, cap-drop ALL, read-only
        // rootfs, no network, keep-id user namespace. F-654 extends the
        // same opts struct with cgroup caps so the same
        // `runtime.create` call applies both layers.
        let effective_limits = effective_limits(limits);
        let opts = SecurityOpts {
            limits: effective_limits,
            ..SecurityOpts::hardened_default()
        };
        let handle = runtime
            .create(&image, Self::default_init_argv(), &opts)
            .await?;
        // F-655: if `start` fails after `create` succeeds, the
        // container exists in podman's store but `Level2Session` is
        // never constructed — so its `Drop` panic-safety net never
        // arms. Without an explicit reap here, every transient start
        // failure leaks a container. Best-effort cleanup: surface the
        // original start error regardless of whether `remove`
        // succeeds (the start error is the actionable signal; a
        // failing remove is logged for diagnosis but must not mask
        // it).
        if let Err(start_err) = runtime.start(&handle).await {
            if let Err(rm_err) = runtime.remove(&handle).await {
                tracing::warn!(
                    error = %rm_err,
                    container_id = %handle.id,
                    "Level 2 cleanup after failed start could not remove container; \
                     manual `podman rm -f <id>` may be required"
                );
            }
            return Err(start_err);
        }
        Ok(Self {
            runtime,
            image,
            handle,
            limits: effective_limits,
            teardown_done: AtomicBool::new(false),
        })
    }

    /// The init argv used by [`Self::create`]. `sleep infinity` is the
    /// idiom — minimal binary surface inside the image, no daemon
    /// behaviour, exits cleanly on `podman stop`.
    pub fn default_init_argv() -> &'static [&'static str] {
        &["sleep", "infinity"]
    }

    /// Run a single step inside the pre-warmed container and capture
    /// its result. Mirrors [`ContainerRuntime::exec`] — non-zero exits
    /// are surfaced via [`StepOutcome::exit_code`], not `Err`.
    pub async fn exec_step(&self, argv: &[&str]) -> Result<StepOutcome, OciError> {
        let res = self.runtime.exec(&self.handle, argv).await?;
        Ok(StepOutcome {
            exit_code: res.exit_code,
            stdout: res.stdout,
            stderr: res.stderr,
        })
    }

    /// Tear the container down. Idempotent — calling twice is harmless
    /// because podman's `rm -f` accepts an already-removed id, but
    /// callers should still only call this once.
    ///
    /// Successful teardown disarms the [`Drop`] panic-safety net so
    /// the synchronous `podman rm -f` fire-and-forget does not run on
    /// a container that no longer exists.
    pub async fn teardown(&self) -> Result<(), OciError> {
        // `stop` first so the container's processes get a chance to
        // exit gracefully; `remove(-f)` then cleans up the storage.
        // We swallow stop errors because `remove(-f)` will force-stop
        // anyway and surfacing both errors hides the more useful one.
        let _ = self.runtime.stop(&self.handle).await;
        let res = self.runtime.remove(&self.handle).await;
        if res.is_ok() {
            self.teardown_done.store(true, Ordering::Release);
        }
        res
    }

    /// Image this session was created against.
    pub fn image(&self) -> &ImageRef {
        &self.image
    }

    /// Container handle in case callers need to thread it elsewhere
    /// (e.g. `stats` for resource monitoring).
    pub fn handle(&self) -> &ContainerHandle {
        &self.handle
    }

    /// Resource limits configured on the session. Currently
    /// observability-only — see module docs for the F-595 follow-up.
    pub fn limits(&self) -> ContainerLimits {
        self.limits
    }

    /// Underlying runtime, exposed so callers needing direct access
    /// (e.g. resource monitor pulling `stats`) can reuse it.
    pub fn runtime(&self) -> &Arc<dyn ContainerRuntime> {
        &self.runtime
    }

    /// Disarm the [`Drop`] panic-safety net. Used by tests that drive
    /// the session against a `MockRuntime` and don't want the Drop
    /// impl to shell out to a real `podman rm -f` for a fake
    /// container id. Production callers should use [`Self::teardown`]
    /// instead — `teardown` arms this same flag on success.
    #[cfg(test)]
    #[doc(hidden)]
    pub(crate) fn disable_drop_cleanup(&self) {
        self.teardown_done.store(true, Ordering::Release);
    }
}

/// Argv passed to `podman` by [`Level2Session::drop`]'s panic-safety
/// net. Pinned by unit test so the eventual flag changes (or future
/// runtime abstraction) cannot silently regress the cleanup shape.
pub(crate) fn drop_cleanup_argv(handle: &ContainerHandle) -> [String; 3] {
    ["rm".to_string(), "-f".to_string(), handle.id.clone()]
}

impl Drop for Level2Session {
    /// Best-effort synchronous safety net for crash / panic / early-
    /// return paths.
    ///
    /// Drop runs in synchronous context, so we cannot await
    /// [`ContainerRuntime::remove`] (it is `async`) and we cannot
    /// re-enter the tokio runtime that owns the trait object. Instead
    /// we shell out directly to `podman rm -f <id>` and detach the
    /// child — fire-and-forget. Tradeoffs:
    ///
    /// - **Async path is preferred.** Callers should call
    ///   [`Self::teardown`] on the clean shutdown path; that sets
    ///   `teardown_done` and the Drop impl becomes a no-op. The Drop
    ///   path is for the cases where `teardown` could not run
    ///   (panic, early `?`, task cancellation).
    /// - **Hardcoded `podman`.** Today there is exactly one
    ///   `ContainerRuntime` impl in the workspace. If a second one
    ///   ships, this Drop should grow a tiny abstraction (each impl
    ///   exposing its own teardown argv).
    /// - **Errors are swallowed.** A failing `spawn` here would mean
    ///   `podman` is missing or the cleanup couldn't be launched —
    ///   both situations that already imply a leaked container we
    ///   cannot recover from in Drop. Logging via `tracing::warn!`
    ///   makes this diagnosable post-mortem.
    fn drop(&mut self) {
        if self.teardown_done.load(Ordering::Acquire) {
            return;
        }
        let argv = drop_cleanup_argv(&self.handle);
        match std::process::Command::new(DROP_CLEANUP_BINARY)
            .args(argv.iter().map(String::as_str))
            // Detach: we are a fire-and-forget guard, not a wait()er.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_child) => {
                // The child is detached on purpose — Rust's
                // `std::process::Child::drop` does NOT reap or kill
                // the child, so it runs to completion independently.
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    container_id = %self.handle.id,
                    binary = DROP_CLEANUP_BINARY,
                    "Level 2 panic-safety teardown could not spawn 'podman rm -f'; \
                     container may leak — invoke `podman rm -f <id>` manually"
                );
            }
        }
    }
}

/// Result of executing a single step. Shape-compatible with the
/// `{ stdout, stderr, exit_code }` JSON `SandboxedCommand`
/// emits via the `shell.exec` tool, so callers can treat Level 1 and
/// Level 2 outputs identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    /// Exit status; `None` if the step was signalled.
    pub exit_code: Option<i32>,
    /// Captured stdout (UTF-8 lossy).
    pub stdout: String,
    /// Captured stderr (UTF-8 lossy).
    pub stderr: String,
}

/// Probe the runtime and either return a [`Level2Session`] ready to use
/// or a logged-and-classified [`Level2Unavailable`] so the caller can
/// fall back to Level 1.
///
/// Emits a `tracing::warn` whenever fallback is chosen; the warning
/// includes the OciError variant name so operators can tell "podman
/// missing" from "rootless misconfigured" without re-running the probe.
pub async fn detect_or_fall_back(
    runtime: &Arc<dyn ContainerRuntime>,
    detect_fn: impl AsyncFnOnce() -> Result<(), OciError>,
) -> Result<(), Level2Unavailable> {
    match detect_fn().await {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = runtime; // accepted for symmetry; caller already holds Arc
            let unavailable = match classify_detect_error(err) {
                Ok(u) => u,
                Err(hard) => {
                    // Hard error from probe — still fall back, but log
                    // it as a `warn` with the variant so the operator
                    // can see something unusual happened. We do NOT
                    // surface this as `Err` because the F-596 contract
                    // is "auto-fallback if container runtime
                    // unreachable" and an unexpected probe failure is
                    // morally the same situation from the caller's
                    // perspective.
                    tracing::warn!(
                        error = %hard,
                        "Level 2 sandbox unavailable: unexpected OciError during detect, \
                         falling back to Level 1"
                    );
                    return Err(Level2Unavailable::RuntimeBroken {
                        tool: "podman",
                        stderr: hard.to_string(),
                    });
                }
            };
            tracing::warn!(
                variant = unavailable_variant_name(&unavailable),
                reason = %unavailable,
                "Level 2 sandbox unavailable, falling back to Level 1"
            );
            Err(unavailable)
        }
    }
}

/// Stable string for the [`Level2Unavailable`] variant — used as a
/// `tracing` field so log filters can pin on it.
fn unavailable_variant_name(u: &Level2Unavailable) -> &'static str {
    match u {
        Level2Unavailable::RuntimeMissing(_) => "RuntimeMissing",
        Level2Unavailable::RuntimeBroken { .. } => "RuntimeBroken",
        Level2Unavailable::RootlessUnavailable { .. } => "RootlessUnavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use forge_oci::{ExecResult, Stats};
    use std::sync::Mutex;

    /// In-process recorder of every [`ContainerRuntime`] call, in order.
    /// Equivalent to [`forge_oci::RecordingRunner`] but at the trait
    /// layer rather than the `CommandRunner` layer — we want to assert
    /// `pull → create → start → exec* → stop → remove`, not the argv
    /// shaping (that is F-595's job).
    #[derive(Default)]
    struct MockRuntime {
        calls: Mutex<Vec<String>>,
        // Optional canned exec outcome.
        exec_outcome: Mutex<Option<ExecResult>>,
        // Last SecurityOpts seen by `create`, captured for F-642 tests
        // that need to verify the hardened defaults reached the trait.
        last_create_opts: Mutex<Option<SecurityOpts>>,
        // F-655: force `start` to return the configured error so we can
        // exercise the create-then-start cleanup guard.
        start_error: Mutex<Option<OciError>>,
        // F-655: force `remove` to return the configured error so we can
        // verify the original start error is the one that propagates.
        remove_error: Mutex<Option<OciError>>,
    }

    impl MockRuntime {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, name: &str) {
            self.calls.lock().unwrap().push(name.to_string());
        }
        fn last_create_opts(&self) -> Option<SecurityOpts> {
            self.last_create_opts.lock().unwrap().clone()
        }
        fn fail_start(&self, err: OciError) {
            *self.start_error.lock().unwrap() = Some(err);
        }
        fn fail_remove(&self, err: OciError) {
            *self.remove_error.lock().unwrap() = Some(err);
        }
    }

    #[async_trait]
    impl ContainerRuntime for MockRuntime {
        async fn detect(&self) -> Result<(), OciError> {
            self.record("detect");
            Ok(())
        }
        async fn pull(&self, _image: &ImageRef) -> Result<(), OciError> {
            self.record("pull");
            Ok(())
        }
        async fn create(
            &self,
            _image: &ImageRef,
            _argv: &[&str],
            opts: &SecurityOpts,
        ) -> Result<ContainerHandle, OciError> {
            self.record("create");
            *self.last_create_opts.lock().unwrap() = Some(opts.clone());
            Ok(ContainerHandle::new("mock-container"))
        }
        async fn start(&self, _handle: &ContainerHandle) -> Result<(), OciError> {
            self.record("start");
            if let Some(err) = self.start_error.lock().unwrap().take() {
                return Err(err);
            }
            Ok(())
        }
        async fn exec(
            &self,
            _handle: &ContainerHandle,
            _argv: &[&str],
        ) -> Result<ExecResult, OciError> {
            self.record("exec");
            Ok(self
                .exec_outcome
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(ExecResult {
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                }))
        }
        async fn stop(&self, _handle: &ContainerHandle) -> Result<(), OciError> {
            self.record("stop");
            Ok(())
        }
        async fn remove(&self, _handle: &ContainerHandle) -> Result<(), OciError> {
            self.record("remove");
            if let Some(err) = self.remove_error.lock().unwrap().take() {
                return Err(err);
            }
            Ok(())
        }
        async fn stats(&self, _handle: &ContainerHandle) -> Result<Stats, OciError> {
            self.record("stats");
            Ok(Stats {
                cpu_percent: None,
                memory_bytes: None,
                pids: None,
            })
        }
        fn parse_stats(&self, _raw: &[u8]) -> Result<Stats, OciError> {
            Ok(Stats {
                cpu_percent: None,
                memory_bytes: None,
                pids: None,
            })
        }
    }

    fn alpine() -> ImageRef {
        // F-643: tag-only references are rejected at parse time for
        // non-allowlisted sources. Tests use a syntactically valid digest
        // — these tests don't actually contact a registry, so the digest
        // contents are irrelevant; only the supply-chain shape matters.
        ImageRef::parse(
            "docker.io/library/alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn create_runs_pull_then_create_then_start() {
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock.clone();

        let session = Level2Session::create(runtime, alpine(), ContainerLimits::default())
            .await
            .unwrap();
        // Disarm the Drop panic-safety net: this test exercises only
        // the lifecycle-ordering invariants, not the cleanup path,
        // and we don't want Drop shelling out to a real `podman` for
        // the mock container id.
        session.disable_drop_cleanup();
        // Lifecycle order is the load-bearing assertion: pull (so the
        // image is local before create), create (so the cgroup leaf
        // is shaped before exec), start (so exec has a running ns).
        assert_eq!(mock.calls(), vec!["pull", "create", "start"]);
        assert_eq!(
            session.image().to_image_string(),
            "docker.io/library/alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert_eq!(session.handle().id, "mock-container");
    }

    #[tokio::test]
    async fn exec_step_invokes_runtime_exec_and_maps_outcome() {
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        *mock.exec_outcome.lock().unwrap() = Some(ExecResult {
            exit_code: Some(2),
            stdout: "out\n".to_string(),
            stderr: "err\n".to_string(),
        });
        let runtime: Arc<dyn ContainerRuntime> = mock.clone();
        let session = Level2Session::create(runtime, alpine(), ContainerLimits::default())
            .await
            .unwrap();
        session.disable_drop_cleanup();
        let outcome = session.exec_step(&["echo", "hi"]).await.unwrap();
        assert_eq!(outcome.exit_code, Some(2));
        assert_eq!(outcome.stdout, "out\n");
        assert_eq!(outcome.stderr, "err\n");
        // Lifecycle + one exec.
        assert_eq!(mock.calls(), vec!["pull", "create", "start", "exec"]);
    }

    #[tokio::test]
    async fn multiple_steps_reuse_one_container() {
        // The "pre-warm + reuse" contract: N steps in one session must
        // see exactly one pull/create/start, N execs, and (after
        // teardown) one stop + one remove.
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock.clone();
        let session = Level2Session::create(runtime, alpine(), ContainerLimits::default())
            .await
            .unwrap();
        for _ in 0..3 {
            session.exec_step(&["true"]).await.unwrap();
        }
        session.teardown().await.unwrap();
        assert_eq!(
            mock.calls(),
            vec!["pull", "create", "start", "exec", "exec", "exec", "stop", "remove"]
        );
    }

    #[tokio::test]
    async fn teardown_runs_stop_then_remove() {
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock.clone();
        let session = Level2Session::create(runtime, alpine(), ContainerLimits::default())
            .await
            .unwrap();
        session.teardown().await.unwrap();
        // stop must precede remove so the workload's final IO is
        // flushed before the rootfs is reaped.
        let calls = mock.calls();
        let stop_idx = calls.iter().position(|c| c == "stop").unwrap();
        let remove_idx = calls.iter().position(|c| c == "remove").unwrap();
        assert!(stop_idx < remove_idx);
    }

    // ── Drop panic-safety net ────────────────────────────────────────

    #[test]
    fn drop_cleanup_argv_is_rm_force_with_handle_id() {
        // Pinned shape of the synchronous panic-safety command. If
        // this changes, the Drop impl's container-leak guarantee
        // changes too.
        let h = ContainerHandle::new("c-id-xyz");
        assert_eq!(
            drop_cleanup_argv(&h),
            ["rm".to_string(), "-f".to_string(), "c-id-xyz".to_string()]
        );
    }

    #[tokio::test]
    async fn teardown_disarms_drop_panic_safety_net() {
        // After a successful teardown(), Drop must NOT shell out to
        // `podman rm -f` again — the container is already gone and
        // running rm twice would log a spurious warn (or worse, race
        // a same-id container created after teardown).
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock.clone();
        let session = Level2Session::create(runtime, alpine(), ContainerLimits::default())
            .await
            .unwrap();
        session.teardown().await.unwrap();
        // The flag is set by teardown(); Drop reads it and skips.
        // We verify by interrogating the public flag accessor we
        // added for tests — there's no portable way to assert "this
        // process did not spawn `podman`" without process tracing.
        assert!(session.teardown_done.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn drop_panic_safety_armed_when_teardown_skipped() {
        // Inverse of the test above: a session that never had
        // teardown() called must arm the safety net so a panicking
        // caller does not leak the container. Here we check the
        // flag is *not* set, then explicitly disable it before
        // dropping (so this test itself doesn't shell out to
        // podman).
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock.clone();
        let session = Level2Session::create(runtime, alpine(), ContainerLimits::default())
            .await
            .unwrap();
        // Pre-condition: panic-safety net is armed (teardown_done is
        // false) right after `create`.
        assert!(!session.teardown_done.load(Ordering::Acquire));
        // Defuse for the actual drop in this test environment so we
        // don't fork off a real `podman rm -f`.
        session.disable_drop_cleanup();
    }

    // ── F-655: container leak on start failure ──────────────────────

    #[tokio::test]
    async fn create_removes_container_when_start_fails() {
        // Bug fix invariant: if `runtime.create` succeeds but
        // `runtime.start` fails, the partially-constructed container
        // must be reaped. Without this, every transient start failure
        // leaks a container into podman's store.
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        mock.fail_start(OciError::CommandFailed {
            tool: "podman",
            args: vec!["start".into(), "mock-container".into()],
            exit_code: Some(125),
            stderr: "transient infra failure".into(),
        });
        let runtime: Arc<dyn ContainerRuntime> = mock.clone();

        let res = Level2Session::create(runtime, alpine(), ContainerLimits::default()).await;
        assert!(res.is_err(), "create must propagate the start failure");

        // Lifecycle proof: pull → create → start (failed) → remove.
        // `remove` is the load-bearing assertion — it is what prevents
        // the orphan.
        assert_eq!(mock.calls(), vec!["pull", "create", "start", "remove"]);
    }

    #[tokio::test]
    async fn create_propagates_original_start_error_when_remove_also_fails() {
        // The cleanup is best-effort: if `remove` itself fails the
        // caller must still see the *start* error, not the remove
        // error. The start error is the actionable signal; the
        // remove failure is logged for diagnosis but must not mask it.
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        mock.fail_start(OciError::CommandFailed {
            tool: "podman",
            args: vec!["start".into(), "mock-container".into()],
            exit_code: Some(125),
            stderr: "ORIGINAL START FAILURE".into(),
        });
        mock.fail_remove(OciError::CommandFailed {
            tool: "podman",
            args: vec!["rm".into(), "-f".into(), "mock-container".into()],
            exit_code: Some(2),
            stderr: "remove also failed".into(),
        });
        let runtime: Arc<dyn ContainerRuntime> = mock.clone();

        let res = Level2Session::create(runtime, alpine(), ContainerLimits::default()).await;
        let err = match res {
            Ok(_) => panic!("create must return Err when start fails"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("ORIGINAL START FAILURE"),
            "expected original start error to propagate, got: {msg}"
        );
        // Best-effort cleanup still attempted.
        assert_eq!(mock.calls(), vec!["pull", "create", "start", "remove"]);
    }

    // ── F-642: Level2Session passes hardened defaults to runtime.create ──

    #[tokio::test]
    async fn create_passes_hardened_security_opts_to_runtime() {
        // Load-bearing F-642 invariant: every Level 2 container must be
        // created with the strict hardening preset. If a refactor drops
        // this propagation, the integration test would still pass with
        // a mock runtime — this test pins the explicit opt-in.
        //
        // F-654: the hardened preset now also carries the
        // conservative cgroup caps, which `Level2Session::create`
        // applies whenever the caller passes
        // `ContainerLimits::default()` (the historical "no caps"
        // signal). The full struct must round-trip identically.
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock.clone();

        let session = Level2Session::create(runtime, alpine(), ContainerLimits::default())
            .await
            .unwrap();
        session.disable_drop_cleanup();

        let opts = mock
            .last_create_opts()
            .expect("runtime.create must have been invoked");
        assert_eq!(
            opts,
            SecurityOpts::hardened_default(),
            "Level2Session must pass the hardened SecurityOpts preset to runtime.create"
        );
    }

    // ── F-654: Level2Session plumbs ContainerLimits through SecurityOpts ──

    #[tokio::test]
    async fn create_substitutes_conservative_limits_when_requested_is_default() {
        // The historical entry point passes `ContainerLimits::default()`
        // (every field None) to mean "no per-step caps configured".
        // F-654 says that must NOT mean "container runs unbounded" —
        // the strict preset takes over so the host stays protected.
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock.clone();

        let session = Level2Session::create(runtime, alpine(), ContainerLimits::default())
            .await
            .unwrap();
        session.disable_drop_cleanup();

        let opts = mock
            .last_create_opts()
            .expect("runtime.create must have been invoked");
        assert_eq!(
            opts.limits,
            ContainerLimits::conservative_default(),
            "default ContainerLimits must be substituted with the F-654 conservative preset"
        );
        // The session also reflects the substitution so callers
        // observing `session.limits()` see what actually landed on
        // the container, not the (empty) request.
        assert_eq!(session.limits(), ContainerLimits::conservative_default());
    }

    #[tokio::test]
    async fn create_honours_caller_supplied_limits_verbatim() {
        // An explicit non-default `ContainerLimits` reaches the
        // runtime untouched — including a single tightened field
        // with the rest left unset. Operators who pin a value get
        // exactly that value, no merge with the conservative preset.
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock.clone();

        let requested = ContainerLimits {
            cpus: Some(0.5),
            memory_bytes: Some(128 * 1024 * 1024),
            memory_swap_bytes: Some(128 * 1024 * 1024),
            pids_max: Some(64),
        };
        let session = Level2Session::create(runtime, alpine(), requested)
            .await
            .unwrap();
        session.disable_drop_cleanup();

        let opts = mock
            .last_create_opts()
            .expect("runtime.create must have been invoked");
        assert_eq!(opts.limits, requested);
        assert_eq!(session.limits(), requested);
    }

    #[tokio::test]
    async fn create_renders_limit_flags_in_security_opts_argv() {
        // Companion to the integration test against a real podman:
        // here we assert the SecurityOpts the trait observed would
        // render the four expected flags (`--cpus`, `--memory`,
        // `--memory-swap`, `--pids-limit`). The MockRuntime captures
        // the SecurityOpts struct directly so we can render off the
        // recorded value.
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock.clone();

        let _session = Level2Session::create(runtime, alpine(), ContainerLimits::default())
            .await
            .unwrap();
        _session.disable_drop_cleanup();

        let opts = mock.last_create_opts().expect("create invoked");
        let flags = opts.to_create_flags();
        for required in ["--cpus", "--memory", "--memory-swap", "--pids-limit"] {
            assert!(
                flags.iter().any(|f| f == required),
                "rendered argv missing F-654 limit flag {required:?}: {flags:?}"
            );
        }
    }

    // ── ContainerLimits flag shaping ─────────────────────────────────
    //
    // The struct itself, including `--cpus` / `--memory` /
    // `--memory-swap` / `--pids-limit` rendering and the F-654
    // conservative preset, is owned by `forge-oci` and tested in that
    // crate. The session-level wiring (substitute conservative when
    // the caller passes default; honour explicit limits verbatim) is
    // covered by the `create_substitutes_conservative_limits…` and
    // `create_honours_caller_supplied_limits_verbatim` tests above.

    #[test]
    fn level2_helper_delegates_to_forge_oci_rendering() {
        // `level2::limits_to_create_flags` is a thin wrapper kept on
        // this module path for older call sites; it must stay
        // consistent with the canonical `ContainerLimits` rendering
        // owned by forge-oci so call sites grep'd against either path
        // observe identical argv shape.
        let limits = ContainerLimits {
            cpus: Some(1.5),
            memory_bytes: Some(512 * 1024 * 1024),
            memory_swap_bytes: Some(512 * 1024 * 1024),
            pids_max: Some(256),
        };
        assert_eq!(limits_to_create_flags(limits), limits.to_create_flags());
    }

    // ── classify_detect_error: fallback variants ─────────────────────

    #[test]
    fn classify_detect_error_treats_runtime_missing_as_fallback() {
        let err = OciError::RuntimeMissing("podman");
        assert!(matches!(
            classify_detect_error(err),
            Ok(Level2Unavailable::RuntimeMissing("podman"))
        ));
    }

    #[test]
    fn classify_detect_error_treats_rootless_unavailable_as_fallback() {
        let err = OciError::RootlessUnavailable {
            runtime: "podman",
            reason: "rootless=false".into(),
        };
        assert!(matches!(
            classify_detect_error(err),
            Ok(Level2Unavailable::RootlessUnavailable { .. })
        ));
    }

    #[test]
    fn classify_detect_error_treats_runtime_broken_as_fallback() {
        let err = OciError::RuntimeBroken {
            tool: "podman",
            stderr: "newuidmap missing".into(),
        };
        assert!(matches!(
            classify_detect_error(err),
            Ok(Level2Unavailable::RuntimeBroken { .. })
        ));
    }

    #[test]
    fn classify_detect_error_propagates_unexpected_variants() {
        // CommandFailed is not a "runtime unavailable" signal — it
        // means the probe ran but reported a real error. Surface it
        // so the caller can decide what to do.
        let err = OciError::CommandFailed {
            tool: "podman",
            args: vec!["info".into()],
            exit_code: Some(1),
            stderr: "boom".into(),
        };
        assert!(matches!(
            classify_detect_error(err),
            Err(OciError::CommandFailed { .. })
        ));
    }

    // ── detect_or_fall_back: end-to-end fallback wiring ──────────────

    #[tokio::test]
    async fn detect_or_fall_back_returns_ok_when_detect_succeeds() {
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock;
        let res = detect_or_fall_back(&runtime, async || Ok(())).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn detect_or_fall_back_runtime_missing_returns_err() {
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock;
        let res =
            detect_or_fall_back(&runtime, async || Err(OciError::RuntimeMissing("podman"))).await;
        assert!(matches!(
            res,
            Err(Level2Unavailable::RuntimeMissing("podman"))
        ));
    }

    #[tokio::test]
    async fn detect_or_fall_back_runtime_broken_returns_err() {
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock;
        let res = detect_or_fall_back(&runtime, async || {
            Err(OciError::RuntimeBroken {
                tool: "podman",
                stderr: "newuidmap".into(),
            })
        })
        .await;
        assert!(matches!(res, Err(Level2Unavailable::RuntimeBroken { .. })));
    }

    #[tokio::test]
    async fn detect_or_fall_back_rootless_unavailable_returns_err() {
        let mock: Arc<MockRuntime> = Arc::new(MockRuntime::default());
        let runtime: Arc<dyn ContainerRuntime> = mock;
        let res = detect_or_fall_back(&runtime, async || {
            Err(OciError::RootlessUnavailable {
                runtime: "podman",
                reason: "rootless=false".into(),
            })
        })
        .await;
        assert!(matches!(
            res,
            Err(Level2Unavailable::RootlessUnavailable { .. })
        ));
    }

    // ── Integration with the real PodmanRuntime via RecordingRunner ──

    #[tokio::test]
    async fn integrates_with_podman_runtime_recording_runner() {
        // End-to-end at the trait layer: a `PodmanRuntime` backed by
        // `RecordingRunner` lets us prove the F-595 wiring would
        // produce the right podman argv, without a real podman binary.
        // Each `run_or_fail` consumes one stub from the queue.
        use forge_oci::{PodmanRuntime, RecordingRunner, StubResponse};

        let recorder = RecordingRunner::new();
        // pull → empty success
        recorder.push(StubResponse::ok_stdout(b"".to_vec()));
        // create → returns container id on stdout
        recorder.push(StubResponse::ok_stdout(b"abc123\n".to_vec()));
        // start → empty success
        recorder.push(StubResponse::ok_stdout(b"".to_vec()));
        // exec → stdout + exit 0
        recorder.push(StubResponse::ok_stdout(b"hello\n".to_vec()));
        // stop, remove
        recorder.push(StubResponse::ok_stdout(b"".to_vec()));
        recorder.push(StubResponse::ok_stdout(b"".to_vec()));

        let calls_handle = recorder.calls.clone();
        let runtime: Arc<dyn ContainerRuntime> =
            Arc::new(PodmanRuntime::with_runner(Box::new(recorder)));

        let session = Level2Session::create(runtime, alpine(), ContainerLimits::default())
            .await
            .unwrap();
        let outcome = session.exec_step(&["echo", "hello"]).await.unwrap();
        assert_eq!(outcome.stdout, "hello\n");
        assert_eq!(outcome.exit_code, Some(0));
        session.teardown().await.unwrap();

        let calls = calls_handle.lock().unwrap();
        // Every podman invocation in the right shape and order. We
        // pin both the count and the leading verb of each — argv
        // shaping itself is F-595's responsibility, owned by its
        // dedicated tests in `crates/forge-oci`.
        let leading: Vec<&str> = calls.iter().map(|(_, args)| args[0].as_str()).collect();
        assert_eq!(
            leading,
            vec!["pull", "create", "start", "exec", "stop", "rm"]
        );
        // create's argv ends with the init argv we shipped.
        let create_args = &calls[1].1;
        assert!(create_args.ends_with(&["sleep".into(), "infinity".into()]));
        // F-642: every hardening flag must be present in the create argv —
        // end-to-end proof that Level2Session ships the strict preset
        // through the trait into PodmanRuntime's flag rendering.
        // F-654: the conservative cgroup caps must travel through the
        // same rendering when the caller asked for the default
        // limits, so a host-default config still bounds the
        // container.
        for required in [
            "--security-opt",
            "no-new-privileges",
            "--cap-drop",
            "ALL",
            "--read-only",
            "--network",
            "none",
            "--userns",
            "keep-id",
            "--cpus",
            "--memory",
            "--memory-swap",
            "--pids-limit",
        ] {
            assert!(
                create_args.iter().any(|a| a == required),
                "create argv missing required flag {required:?}: {create_args:?}"
            );
        }
        // exec's argv carries the caller's command after the container id.
        let exec_args = &calls[3].1;
        assert!(exec_args.ends_with(&["echo".into(), "hello".into()]));
    }
}

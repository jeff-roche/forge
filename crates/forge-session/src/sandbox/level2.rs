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
//! Per-step caps are captured in [`ContainerLimits`] and intended for the
//! `--cpus` / `--memory` / `--pids-limit` flags `podman` applies at *create*
//! time (cgroup v2 leaf). The current F-595 [`ContainerRuntime::create`]
//! signature does not yet accept resource flags; this is tracked as
//! follow-up issue #631 (see `docs/architecture/isolation-model.md` §8.3).
//! Until that lands, [`Level2Session::create`] stores the limits on the
//! session for observability and the `argv`-shaping helper
//! [`limits_to_create_flags`] pins the canonical podman-flag rendering so
//! the eventual wiring is a one-line change.
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

/// Binary used by [`Level2Session::drop`]'s panic-safety net to reap
/// a leaked container. Hardcoded because the only `ContainerRuntime`
/// impl in the workspace today is `PodmanRuntime`; if a second
/// implementation is added the safety net would need a small
/// abstraction (e.g. each impl supplies its own teardown argv).
const DROP_CLEANUP_BINARY: &str = "podman";

/// Per-step resource limits enforced via the container's cgroup v2 leaf.
///
/// All fields are `Option` because Level 2 inherits whatever limit was set
/// on the daemon's slice (or "unlimited") when a field is absent. Callers
/// that want the same shape as [`super::SandboxConfig`] can map
/// `cpu_seconds` / `address_space_bytes` / `max_processes` onto the
/// matching container fields.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ContainerLimits {
    /// Equivalent of `podman create --cpus N.NN`. Float CPU shares
    /// (`1.5` = 1.5 cores). `None` leaves the cap unset.
    pub cpus: Option<f32>,
    /// Equivalent of `podman create --memory <bytes>`. `None` leaves the
    /// cap unset.
    pub memory_bytes: Option<u64>,
    /// Equivalent of `podman create --pids-limit N`. `None` leaves the
    /// cap unset.
    pub pids_max: Option<u64>,
}

impl ContainerLimits {
    /// Convert into the canonical podman create flag list. Returned in the
    /// order podman expects, with the units it expects (memory in bytes,
    /// cpus as a float). Pinned by unit tests so the eventual call-site
    /// wiring matches what the trait will receive.
    pub fn to_create_flags(self) -> Vec<String> {
        limits_to_create_flags(self)
    }
}

/// Render [`ContainerLimits`] into the argv fragment that would be
/// inserted between `podman create` and the IMAGE positional. Order is
/// deterministic so test assertions can pin it.
pub fn limits_to_create_flags(limits: ContainerLimits) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(cpus) = limits.cpus {
        out.push("--cpus".to_string());
        out.push(format_cpus(cpus));
    }
    if let Some(bytes) = limits.memory_bytes {
        out.push("--memory".to_string());
        out.push(bytes.to_string());
    }
    if let Some(pids) = limits.pids_max {
        out.push("--pids-limit".to_string());
        out.push(pids.to_string());
    }
    out
}

/// Format CPU shares without a trailing `.0` for integer values (`1.0` →
/// `"1"`) so the resulting podman command line matches the convention
/// `podman` itself emits in `inspect`.
fn format_cpus(cpus: f32) -> String {
    if cpus.fract() == 0.0 {
        format!("{}", cpus.trunc() as i64)
    } else {
        format!("{cpus}")
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
    /// 2. `runtime.create(image, init_argv)` — the container's "init"
    ///    process. We default this to a `sleep infinity`-style argv via
    ///    [`Self::default_init_argv`] so the container stays alive long
    ///    enough for `exec` to hit it.
    /// 3. `runtime.start(handle)` — flips the container to running.
    ///
    /// Resource limits live on the returned session for observability
    /// and will be passed at `create` time once F-595's trait is
    /// extended (see module docs).
    pub async fn create(
        runtime: Arc<dyn ContainerRuntime>,
        image: ImageRef,
        limits: ContainerLimits,
    ) -> Result<Self, OciError> {
        runtime.pull(&image).await?;
        // F-642: every Level 2 container is created with the strict
        // hardening defaults — no-new-privileges, cap-drop ALL, read-only
        // rootfs, no network, keep-id user namespace. Documented at
        // `docs/architecture/isolation-model.md` §8.3.
        let handle = runtime
            .create(
                &image,
                Self::default_init_argv(),
                &SecurityOpts::hardened_default(),
            )
            .await?;
        runtime.start(&handle).await?;
        // Operator-facing notice when limits are configured but not
        // yet enforced. Until follow-up #631 lands, the container
        // inherits whatever limits its parent slice applies — which
        // for rootless podman is "the user's slice", not the values
        // captured here. Logging at runtime catches accidental
        // reliance on the not-yet-wired path.
        if limits != ContainerLimits::default() {
            tracing::warn!(
                cpus = ?limits.cpus,
                memory_bytes = ?limits.memory_bytes,
                pids_max = ?limits.pids_max,
                container_id = %handle.id,
                "Level 2 ContainerLimits captured but not enforced; \
                 cgroup wiring deferred to follow-up #631 — container \
                 will run with host-slice default limits"
            );
        }
        Ok(Self {
            runtime,
            image,
            handle,
            limits,
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
        ImageRef::parse("alpine:3.19").unwrap()
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
        assert_eq!(session.image().to_image_string(), "alpine:3.19");
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

    // ── F-642: Level2Session passes hardened defaults to runtime.create ──

    #[tokio::test]
    async fn create_passes_hardened_security_opts_to_runtime() {
        // Load-bearing F-642 invariant: every Level 2 container must be
        // created with the strict hardening preset. If a refactor drops
        // this propagation, the integration test would still pass with
        // a mock runtime — this test pins the explicit opt-in.
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

    // ── ContainerLimits flag shaping ─────────────────────────────────

    #[test]
    fn limits_to_create_flags_emits_in_canonical_order() {
        // Order is fixed: --cpus, --memory, --pids-limit. The eventual
        // `create_with_limits` extension on the F-595 trait will feed
        // these directly into the IMAGE-prefixed argv.
        let limits = ContainerLimits {
            cpus: Some(1.5),
            memory_bytes: Some(512 * 1024 * 1024),
            pids_max: Some(256),
        };
        assert_eq!(
            limits.to_create_flags(),
            vec![
                "--cpus".to_string(),
                "1.5".to_string(),
                "--memory".to_string(),
                (512 * 1024 * 1024u64).to_string(),
                "--pids-limit".to_string(),
                "256".to_string(),
            ]
        );
    }

    #[test]
    fn limits_to_create_flags_skips_none_fields() {
        let limits = ContainerLimits {
            cpus: None,
            memory_bytes: Some(1_000),
            pids_max: None,
        };
        assert_eq!(
            limits.to_create_flags(),
            vec!["--memory".to_string(), "1000".to_string()]
        );
    }

    #[test]
    fn limits_to_create_flags_default_is_empty() {
        // No limits configured → no flags. Matches "inherit slice
        // limits" semantics.
        assert!(ContainerLimits::default().to_create_flags().is_empty());
    }

    #[test]
    fn cpus_integer_value_renders_without_decimal() {
        let limits = ContainerLimits {
            cpus: Some(1.0),
            ..Default::default()
        };
        assert_eq!(
            limits.to_create_flags(),
            vec!["--cpus".to_string(), "1".to_string()]
        );
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
        ] {
            assert!(
                create_args.iter().any(|a| a == required),
                "create argv missing F-642 hardening flag {required:?}: {create_args:?}"
            );
        }
        // exec's argv carries the caller's command after the container id.
        let exec_args = &calls[3].1;
        assert!(exec_args.ends_with(&["echo".into(), "hello".into()]));
    }
}

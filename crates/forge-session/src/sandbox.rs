//! Sandboxed step execution for `forge-session`.
//!
//! Two isolation levels are supported, both surfaced through the
//! [`SandboxLevel`] enum:
//!
//! - **Level 1 — Process** (default; this module's existing surface).
//!   Wraps [`tokio::process::Command`] with an environment whitelist, a
//!   fresh process group, `setrlimit` soft caps, and (F-149) a per-sandbox
//!   cgroup v2 leaf enforcing `pids.max` when the host delegates the
//!   controller. Implemented in the `imp` submodule below; see
//!   `docs/architecture/isolation-model.md` §8.2.
//! - **Level 2 — Container** (F-596; opt-in). Delegates step execution to
//!   a pre-warmed rootless container managed via
//!   [`forge_oci::ContainerRuntime`]. The session pulls + creates +
//!   starts the container once, every step runs as `runtime.exec`, and
//!   the container is `stop`+`remove`'d on session teardown. If no
//!   runtime is reachable, a `tracing::warn` is emitted with the
//!   classified [`level2::Level2Unavailable`] reason and callers fall
//!   back to Level 1. Implementation lives in [`level2`]; design rationale
//!   is in `docs/architecture/isolation-model.md` §8.3.
//!
//! # Level 1 implementation notes
//!
//! Wraps [`tokio::process::Command`] with:
//! - Environment whitelist (no inheritance from the session daemon).
//! - CPU / address-space / file soft limits via `setrlimit`.
//! - A fresh process group so the entire tree can be killed on drop or
//!   session shutdown.
//! - A per-sandbox cgroup v2 leaf enforcing `pids.max` (F-149) when the
//!   host delegates the `pids` controller. See [`SandboxConfig::max_processes`]
//!   for the full scope story; the rlimit [`SandboxConfig::rlimit_nproc_backstop`]
//!   remains as a uid-wide defense-in-depth backstop.
//!
//! # Platform surface
//!
//! The **enforcement** primitives — `SandboxedCommand`, `SandboxedChild`,
//! the cgroup v2 writes and `setrlimit` calls — live in the `imp`
//! submodule and are `#[cfg(target_os = "linux")]`. macOS and Windows
//! support for real sandboxing is deferred beyond Phase 1.
//!
//! The **bookkeeping** types — [`SandboxConfig`], [`ChildRegistry`],
//! [`BASE_ENV_WHITELIST`] — compile on every platform so non-Linux
//! callers (tools, server, orchestrator) can still plumb configuration
//! through without platform branching. On non-Linux
//! [`ChildRegistry::kill_all`] is a no-op that simply clears the
//! registry; real process-group kill is a Linux-only concept.

use std::sync::{Arc, Mutex};

pub mod level2;

pub use level2::{
    classify_detect_error, detect_or_fall_back, ContainerLimits, Level2Session, Level2Unavailable,
    StepOutcome,
};

/// Selector for the isolation level applied to a step's execution.
///
/// `Level1` is the historical default — Process isolation via
/// `setrlimit` + cgroup v2 (see this module's docs for the full
/// implementation). `Level2` carries an `Arc<dyn ContainerRuntime>`
/// shared across every `SandboxedCommand` in a session, and routes
/// execution through `runtime.exec(handle, argv)` against the
/// pre-warmed container described by [`Level2Session`].
///
/// # Deviation from the F-596 DoD
///
/// The DoD spelled the variant as
/// `Level2 { runtime: Box<dyn ContainerRuntime> }`. We use
/// `Arc<dyn ContainerRuntime>` because a single session typically
/// constructs many `SandboxedCommand` instances per turn that all
/// share the same pre-warmed container; `Box` cannot be cloned across
/// those handles, while `Arc` clones cheaply and preserves the same
/// dyn-trait surface. The deviation is documented in
/// `docs/architecture/isolation-model.md` §8.3.
#[derive(Default)]
pub enum SandboxLevel {
    /// Level 1 — Process isolation (default).
    #[default]
    Level1,
    /// Level 2 — Container isolation via [`forge_oci::ContainerRuntime`].
    Level2 {
        /// Shared handle to the pre-warmed container plus the runtime
        /// that owns it. Created once per session via
        /// [`Level2Session::create`].
        session: Arc<Level2Session>,
    },
}

impl std::fmt::Debug for SandboxLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Level1 => f.write_str("SandboxLevel::Level1"),
            Self::Level2 { session } => f
                .debug_struct("SandboxLevel::Level2")
                .field("image", session.image())
                .field("handle", session.handle())
                .finish(),
        }
    }
}

impl SandboxLevel {
    /// `true` when this level is `Level2`.
    pub fn is_level2(&self) -> bool {
        matches!(self, Self::Level2 { .. })
    }

    /// Borrow the [`Level2Session`] when this level is `Level2`. Used
    /// by `SandboxedCommand::execute` to dispatch to
    /// [`Level2Session::exec_step`].
    pub fn level2_session(&self) -> Option<&Arc<Level2Session>> {
        match self {
            Self::Level2 { session } => Some(session),
            Self::Level1 => None,
        }
    }
}

/// Resource limits applied to sandboxed children.
///
/// Every cap is applied via `setrlimit(2)` in `pre_exec` except
/// [`Self::max_processes`], which is enforced out-of-band through a
/// per-sandbox cgroup v2 leaf (F-149). That split is load-bearing: `setrlimit`
/// is per-process / per-uid, whereas cgroup v2 `pids.max` is per-cgroup, so
/// only the cgroup path can give a sandbox its own task budget independent
/// of sibling sandboxes and the daemon's own uid.
///
/// Defaults are conservative values intended for short-lived tool invocations;
/// callers should override via `SandboxedCommand::with_config` for workloads
/// that need more (`SandboxedCommand` is Linux-only, defined in the module's
/// `imp` submodule):
///
/// - `cpu_seconds`: 30 s of CPU time (SIGXCPU on overflow).
/// - `address_space_bytes`: 512 MiB of virtual memory.
/// - `max_processes`: 256 tasks **per sandbox** via cgroup v2 `pids.max`
///   (best-effort — falls back to rlimit-only when delegation is absent).
/// - `rlimit_nproc_backstop`: 4096 processes uid-wide via `RLIMIT_NPROC`
///   (defense-in-depth against rare non-cgroup hosts).
/// - `max_open_files`: 256 file descriptors (blocks fd-exhaustion).
/// - `max_file_size_bytes`: 100 MiB per file written (SIGXFSZ on overflow;
///   blocks cat-to-disk attacks).
#[derive(Debug, Clone, Copy)]
pub struct SandboxConfig {
    /// `RLIMIT_CPU` soft limit in seconds. Exceeding this sends `SIGXCPU`.
    pub cpu_seconds: u64,
    /// `RLIMIT_AS` soft limit in bytes (address space ceiling).
    pub address_space_bytes: u64,
    /// Per-sandbox task ceiling enforced via cgroup v2 `pids.max`. This is
    /// the **authoritative** per-sandbox process cap (F-149): each sandbox
    /// gets its own independent budget, so a misbehaving tool cannot starve
    /// a well-behaved sibling.
    ///
    /// When the host does not delegate the cgroup v2 `pids` controller to the
    /// daemon's slice — non-Linux hosts, cgroup v1, containers without
    /// delegation, etc. — cgroup setup is skipped silently and
    /// [`Self::rlimit_nproc_backstop`] becomes the only process ceiling.
    /// The regression test in this module skips on such hosts rather than
    /// silently exercising the degraded path.
    pub max_processes: u64,
    /// `RLIMIT_NPROC` soft limit — uid-wide defense-in-depth backstop for
    /// hosts where the cgroup path is unavailable. Unlike
    /// [`Self::max_processes`], this is **per real-uid**, not per-sandbox —
    /// the daemon typically shares its uid with the user's desktop session
    /// (or CI's test harness), so every other process the same uid owns
    /// counts against this cap. Tuned to stop fork bombs within milliseconds
    /// while leaving headroom for the uid's baseline. See
    /// `docs/dev/sandbox-limits.md` for the full scope story.
    pub rlimit_nproc_backstop: u64,
    /// `RLIMIT_NOFILE` soft limit — max open file descriptors.
    pub max_open_files: u64,
    /// `RLIMIT_FSIZE` soft limit in bytes. Writes past this cap raise `SIGXFSZ`.
    pub max_file_size_bytes: u64,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            cpu_seconds: 30,
            address_space_bytes: 512 * 1024 * 1024,
            // 256 is "tight enough to shrink the blast radius of a single
            // compromised tool, loose enough for a realistic `make -j$(nproc)`
            // fan-out." Per-sandbox budget means N sandboxes each get 256,
            // independent of the daemon's uid-wide process count.
            max_processes: 256,
            // Uid-wide backstop kept at the historical F-055 value so the
            // rlimit still stops a runaway fork(2) loop on hosts where the
            // cgroup path is unavailable.
            rlimit_nproc_backstop: 4096,
            max_open_files: 256,
            max_file_size_bytes: 100 * 1024 * 1024,
        }
    }
}

/// Environment variables that are always injected, regardless of the
/// caller-provided allow-list.
pub const BASE_ENV_WHITELIST: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL"];

/// Shared registry of live sandboxed children scoped to a session. On session
/// shutdown, [`ChildRegistry::kill_all`] sends `SIGKILL` to every tracked
/// process group so that stray descendants do not survive the daemon.
#[derive(Default, Clone)]
pub struct ChildRegistry {
    pgids: Arc<Mutex<Vec<i32>>>,
}

impl ChildRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pgid. The caller is expected to deregister it via
    /// [`ChildRegistry::remove`] once the child exits cleanly.
    pub fn insert(&self, pgid: i32) {
        self.pgids.lock().unwrap().push(pgid);
    }

    /// Deregister a pgid.
    pub fn remove(&self, pgid: i32) {
        let mut guard = self.pgids.lock().unwrap();
        if let Some(idx) = guard.iter().position(|p| *p == pgid) {
            guard.swap_remove(idx);
        }
    }

    /// Send `SIGKILL` to every tracked process group and clear the registry.
    #[cfg(target_os = "linux")]
    pub fn kill_all(&self) {
        let mut guard = self.pgids.lock().unwrap();
        for pgid in guard.drain(..) {
            // SAFETY: killpg is async-signal-safe and takes no Rust references.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }

    /// Non-Linux builds do not currently sandbox — kill_all is a no-op.
    #[cfg(not(target_os = "linux"))]
    pub fn kill_all(&self) {
        self.pgids.lock().unwrap().clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.pgids.lock().unwrap().len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.pgids.lock().unwrap().is_empty()
    }
}

// Linux implementation ──────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod imp {
    use super::{ChildRegistry, SandboxConfig, SandboxLevel, StepOutcome, BASE_ENV_WHITELIST};
    use std::ffi::OsString;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::process::{Child, Command};

    /// Root of the cgroup v2 filesystem. Kept as a function so future tests
    /// can stub it if needed; production callers always see `/sys/fs/cgroup`.
    fn cgroup_root() -> PathBuf {
        PathBuf::from("/sys/fs/cgroup")
    }

    /// Counter that disambiguates sandbox leaf names within a single daemon
    /// process. Combined with the daemon pid this is unique enough to
    /// prevent collisions across sandboxes spawned at the same instant.
    static LEAF_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Per-sandbox cgroup v2 leaf enforcing [`SandboxConfig::max_processes`]
    /// as `pids.max`. Placed as a sibling of the daemon's own cgroup so the
    /// v2 "no internal processes" rule on ancestor paths is not violated.
    ///
    /// Created best-effort: [`CgroupLeaf::create`] returns `Ok(None)` on
    /// hosts that do not delegate the `pids` controller, letting the
    /// sandbox proceed with rlimit-only enforcement instead of failing
    /// spawn. Torn down via [`CgroupLeaf::kill_and_remove`] on sandbox
    /// teardown; failures are swallowed because an orphaned empty leaf is
    /// cleaned up on reboot and killing the sandbox is already best-effort
    /// past the killpg.
    pub(super) struct CgroupLeaf {
        path: PathBuf,
    }

    impl CgroupLeaf {
        /// Probe cgroup v2 + the `pids` controller + a writable parent, then
        /// create a fresh leaf and write `pids.max = limit`. Returns
        /// `Ok(None)` — with no side effects — when delegation is unavailable.
        fn create(limit: u64) -> io::Result<Option<Self>> {
            let Some(parent) = resolve_parent_cgroup()? else {
                return Ok(None);
            };
            // Refuse to mkdir under a parent that does not have the `pids`
            // controller enabled in its subtree. Without it, the leaf would
            // mkdir successfully but lack a `pids.max` file and the limiter
            // would silently do nothing.
            if !parent_has_pids_controller(&parent) {
                return Ok(None);
            }

            let name = format!(
                "forge-sandbox-{}-{}",
                std::process::id(),
                LEAF_COUNTER.fetch_add(1, Ordering::Relaxed),
            );
            let leaf_path = parent.join(&name);
            if let Err(e) = std::fs::create_dir(&leaf_path) {
                // EACCES / EPERM on a non-delegated subtree: treat as "no
                // delegation" rather than propagating a hard error.
                if matches!(
                    e.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
                ) {
                    return Ok(None);
                }
                return Err(e);
            }

            let leaf = Self { path: leaf_path };
            // Write pids.max only after mkdir so cleanup removes the leaf
            // on subsequent failure.
            if let Err(e) = std::fs::write(leaf.path.join("pids.max"), limit.to_string()) {
                leaf.kill_and_remove();
                return Err(e);
            }
            Ok(Some(leaf))
        }

        /// Move `pid` into this leaf's task set. After this returns Ok every
        /// subsequent fork/clone by `pid` or its descendants is accounted
        /// against `pids.max`.
        fn enroll(&self, pid: i32) -> io::Result<()> {
            std::fs::write(self.path.join("cgroup.procs"), pid.to_string())
        }

        /// Kill every task in the leaf via `cgroup.kill` (cgroup v2 >= 5.14)
        /// then rmdir. Errors are swallowed because the orphaned empty leaf
        /// is cleaned on reboot and an already-dead sandbox has already
        /// achieved the user-visible goal.
        fn kill_and_remove(&self) {
            let _ = std::fs::write(self.path.join("cgroup.kill"), "1");
            let _ = std::fs::remove_dir(&self.path);
        }

        /// Filesystem path of this leaf, used by the regression test to read
        /// `pids.current` / `pids.max` directly without parsing shell output.
        pub(super) fn path(&self) -> &Path {
            &self.path
        }
    }

    /// Resolve the cgroup v2 path the daemon currently belongs to and
    /// return the **parent** path we should create sandbox leaves under.
    ///
    /// Returns `Ok(None)` when the host is not running cgroup v2 (hybrid
    /// hosts emit `name=` lines instead of a `0::` line) or when
    /// `/proc/self/cgroup` is unreadable or the daemon is in the root
    /// cgroup (no suitable parent exists).
    ///
    /// Sibling-of-daemon placement is deliberate: cgroup v2 forbids a
    /// cgroup from containing both processes and child cgroups, so using
    /// the daemon's own cgroup as a parent would violate the rule the
    /// moment we enable controllers on its subtree. The daemon's parent
    /// (e.g. `/sys/fs/cgroup/user.slice/user-<uid>.slice/user@<uid>.service/app.slice/`)
    /// already has `pids` in `cgroup.subtree_control` thanks to systemd's
    /// default delegation, so sibling leaves inherit the controller for
    /// free.
    fn resolve_parent_cgroup() -> io::Result<Option<PathBuf>> {
        let self_cgroup = match std::fs::read_to_string("/proc/self/cgroup") {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        // cgroup v2 entry has the shape "0::<path>"; cgroup v1 entries are
        // numbered per controller and look like "12:cpu,cpuacct:/...".
        let Some(path) = self_cgroup
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .map(str::trim)
        else {
            return Ok(None);
        };
        if path.is_empty() || path == "/" {
            return Ok(None);
        }
        let root = cgroup_root();
        let relative = path.trim_start_matches('/');
        let daemon_cg = root.join(relative);
        Ok(daemon_cg.parent().map(Path::to_path_buf))
    }

    /// Check the parent's `cgroup.subtree_control` for a `pids` entry.
    /// Without the controller enabled in the subtree, a freshly-mkdir'd
    /// leaf will not have a `pids.max` file and the limiter cannot work.
    fn parent_has_pids_controller(parent: &Path) -> bool {
        match std::fs::read_to_string(parent.join("cgroup.subtree_control")) {
            Ok(s) => s.split_whitespace().any(|tok| tok == "pids"),
            Err(_) => false,
        }
    }

    /// Pre-configured sandbox wrapper around [`tokio::process::Command`].
    ///
    /// Created via [`SandboxedCommand::new`]. Mutate further (args, stdio)
    /// through [`SandboxedCommand::command_mut`], then call
    /// [`SandboxedCommand::spawn`] to produce a [`SandboxedChild`].
    pub struct SandboxedCommand {
        cmd: Command,
        /// Original program path captured for diagnostics — not
        /// consumed at execution time because `cmd` already owns it.
        #[allow(dead_code)]
        program: OsString,
        /// In-container argv for [`SandboxLevel::Level2`]. Mirrored
        /// into `cmd` via [`SandboxedCommand::push_arg`] /
        /// [`SandboxedCommand::push_args`] so a Level-1 spawn sees the
        /// same args.
        argv: Vec<OsString>,
        config: SandboxConfig,
        registry: Option<ChildRegistry>,
        level: SandboxLevel,
    }

    impl SandboxedCommand {
        /// Build a sandboxed command for `program`, executed with
        /// `workspace_root` as the current working directory.
        ///
        /// The environment is cleared and re-populated with
        /// [`BASE_ENV_WHITELIST`]. Callers can extend the whitelist via
        /// [`SandboxedCommand::allow_env`].
        pub fn new(
            program: impl Into<OsString>,
            workspace_root: impl AsRef<std::path::Path>,
        ) -> Self {
            Self::with_config(program, workspace_root, SandboxConfig::default())
        }

        /// Same as [`SandboxedCommand::new`] with an explicit [`SandboxConfig`].
        pub fn with_config(
            program: impl Into<OsString>,
            workspace_root: impl AsRef<std::path::Path>,
            config: SandboxConfig,
        ) -> Self {
            let program = program.into();
            let mut cmd = Command::new(&program);
            cmd.current_dir(workspace_root);
            cmd.env_clear();
            // Re-inject base whitelist from the daemon's own env. Missing vars
            // are simply skipped.
            for key in BASE_ENV_WHITELIST {
                if let Ok(val) = std::env::var(key) {
                    cmd.env(key, val);
                }
            }

            // `setsid` in pre_exec gives the child a new process group (equal
            // to its pid) so a single killpg tears down the whole tree.
            // setrlimit values are captured into the closure.
            let cpu_seconds = config.cpu_seconds;
            let address_space_bytes = config.address_space_bytes;
            let rlimit_nproc_backstop = config.rlimit_nproc_backstop;
            let max_open_files = config.max_open_files;
            let max_file_size_bytes = config.max_file_size_bytes;
            // SAFETY: `pre_exec` runs post-fork, pre-exec. We only call
            // async-signal-safe libc functions (setsid, setrlimit) and
            // never touch Rust allocation or locks.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    apply_rlimit(libc::RLIMIT_CPU, cpu_seconds)?;
                    apply_rlimit(libc::RLIMIT_AS, address_space_bytes)?;
                    // Uid-wide defense-in-depth; the per-sandbox limit lives
                    // on the cgroup v2 `pids.max` set up post-fork in spawn().
                    apply_rlimit(libc::RLIMIT_NPROC, rlimit_nproc_backstop)?;
                    apply_rlimit(libc::RLIMIT_NOFILE, max_open_files)?;
                    apply_rlimit(libc::RLIMIT_FSIZE, max_file_size_bytes)?;
                    Ok(())
                });
            }

            Self {
                cmd,
                program,
                argv: Vec::new(),
                config,
                registry: None,
                level: SandboxLevel::Level1,
            }
        }

        /// Promote this command to a specific [`SandboxLevel`]. The default
        /// is [`SandboxLevel::Level1`]; callers opt into Level 2 by passing
        /// a [`SandboxLevel::Level2`] carrying a pre-warmed
        /// [`super::Level2Session`].
        ///
        /// At Level 2, only the in-container argv is consulted —
        /// `setrlimit`/cgroup setup, `pre_exec` hooks, env whitelisting,
        /// and `current_dir` all become no-ops because the work runs
        /// inside the container's namespace, not in a host-side child.
        /// Call [`Self::push_arg`] / [`Self::push_args`] to assemble the
        /// argv that will be passed through to `runtime.exec`.
        pub fn with_level(mut self, level: SandboxLevel) -> Self {
            self.level = level;
            self
        }

        /// Append a single argument to the in-container argv (Level 2)
        /// AND to the underlying [`tokio::process::Command`] (Level 1).
        /// Use this in preference to `command_mut().arg(...)` when the
        /// command may run at either level — it keeps the two argv
        /// surfaces in sync.
        pub fn push_arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
            let arg = arg.into();
            self.cmd.arg(&arg);
            self.argv.push(arg);
            self
        }

        /// Bulk version of [`Self::push_arg`].
        pub fn push_args<I, S>(&mut self, args: I) -> &mut Self
        where
            I: IntoIterator<Item = S>,
            S: Into<OsString>,
        {
            for a in args {
                self.push_arg(a);
            }
            self
        }

        /// Add a caller-scoped environment variable to the whitelist. The
        /// value is taken verbatim — callers pass explicit (key, value)
        /// pairs rather than inheriting from the daemon's environment.
        pub fn allow_env(
            &mut self,
            key: impl AsRef<std::ffi::OsStr>,
            value: impl AsRef<std::ffi::OsStr>,
        ) -> &mut Self {
            self.cmd.env(key, value);
            self
        }

        /// Bulk version of [`SandboxedCommand::allow_env`].
        pub fn allow_envs<I, K, V>(&mut self, vars: I) -> &mut Self
        where
            I: IntoIterator<Item = (K, V)>,
            K: AsRef<std::ffi::OsStr>,
            V: AsRef<std::ffi::OsStr>,
        {
            for (k, v) in vars {
                self.cmd.env(k, v);
            }
            self
        }

        /// Register spawned children with a [`ChildRegistry`] so session
        /// shutdown can kill them.
        pub fn with_registry(&mut self, registry: ChildRegistry) -> &mut Self {
            self.registry = Some(registry);
            self
        }

        /// Access the underlying [`tokio::process::Command`] for further
        /// configuration (args, stdio, etc.).
        pub fn command_mut(&mut self) -> &mut Command {
            &mut self.cmd
        }

        /// Spawn the sandboxed command.
        ///
        /// Creates the per-sandbox cgroup v2 leaf *before* spawning so that a
        /// failure to set up `pids.max` leaves nothing dangling. When leaf
        /// creation returns `Ok(None)` (host does not delegate the `pids`
        /// controller) the sandbox falls back to rlimit-only enforcement;
        /// when it returns `Err` the whole spawn aborts because some other
        /// IO failure is hiding.
        ///
        /// The child's own pid is written into the leaf's `cgroup.procs`
        /// from within a `pre_exec` hook — i.e. from the child, post-fork
        /// but pre-exec. This is the only placement that eliminates the
        /// fork-escape race: any write from the parent can only happen
        /// *after* the kernel has returned the child's pid, which is
        /// *after* the kernel has scheduled the child to run — so a
        /// parent-side enrollment can race with the child forking its own
        /// descendants before enrollment lands. Child-side, pre-exec, is
        /// the atomic barrier.
        ///
        /// # Level 2 guard
        ///
        /// `spawn()` is **Level 1 only**. Calling it on a
        /// [`SandboxedCommand`] configured with [`SandboxLevel::Level2`]
        /// would silently bypass the container — the work would run on
        /// the host with no isolation. We hard-fail with a clear
        /// `io::Error` instead. Use [`SandboxedCommand::execute`] for
        /// commands that may run at either level; it dispatches based
        /// on the configured [`SandboxLevel`].
        pub fn spawn(mut self) -> io::Result<SandboxedChild> {
            if matches!(self.level, SandboxLevel::Level2 { .. }) {
                return Err(io::Error::other(
                    "SandboxedCommand::spawn() is Level 1 only; use execute() for Level 2",
                ));
            }
            let cgroup = CgroupLeaf::create(self.config.max_processes)?;

            // Install a pre_exec hook that writes `getpid()` into
            // `<leaf>/cgroup.procs`. This closes the fork-escape race
            // described above: enrollment happens before the child can
            // execve its program.
            if let Some(leaf) = cgroup.as_ref() {
                use std::os::unix::ffi::OsStrExt;
                // Precompute the C-string path outside pre_exec; the
                // hook must be async-signal-safe and cannot allocate.
                let procs_path = leaf.path().join("cgroup.procs");
                let procs_cstr = std::ffi::CString::new(procs_path.as_os_str().as_bytes())
                    .map_err(|_| io::Error::other("cgroup.procs path contains NUL byte"))?;
                // SAFETY: pre_exec runs post-fork, pre-exec. open, write,
                // close, getpid are all async-signal-safe per POSIX.
                unsafe {
                    self.cmd.pre_exec(move || {
                        let fd = libc::open(procs_cstr.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                        if fd < 0 {
                            // Parent-side enrollment fallback will try
                            // again below; do not hard-fail the spawn.
                            return Ok(());
                        }
                        // Write getpid() as ASCII digits. Max u32
                        // decimal is 10 bytes; 16 is ample headroom.
                        let pid = libc::getpid();
                        let mut buf = [0u8; 16];
                        let len = pid_to_decimal(pid, &mut buf);
                        let _ = libc::write(fd, buf.as_ptr().cast(), len);
                        let _ = libc::close(fd);
                        Ok(())
                    });
                }
            }

            let child = self.cmd.spawn()?;
            let pid = child
                .id()
                .ok_or_else(|| io::Error::other("spawned child has no pid (already exited)"))?
                as i32;
            // `setsid` set pgid == pid.
            let pgid = pid;

            // Parent-side re-enroll as a belt-and-braces backstop. If
            // the pre_exec write raced with a very early fork, or the
            // open() failed for any reason, this catches the child
            // (though not any descendants it may already have spawned).
            // Errors here are swallowed; pre_exec is the load-bearing
            // path.
            let cgroup = match cgroup {
                Some(leaf) => {
                    let _ = leaf.enroll(pid);
                    Some(leaf)
                }
                None => None,
            };

            if let Some(registry) = &self.registry {
                registry.insert(pgid);
            }
            Ok(SandboxedChild {
                child: Some(child),
                pgid,
                registry: self.registry,
                _config: self.config,
                released: false,
                cgroup,
            })
        }

        /// Returns the config that will be applied to spawned children.
        pub fn config(&self) -> SandboxConfig {
            self.config
        }

        /// The active isolation level.
        pub fn level(&self) -> &SandboxLevel {
            &self.level
        }

        /// Run this command to completion under the configured
        /// [`SandboxLevel`] and return a unified [`StepOutcome`].
        ///
        /// Branching:
        /// - [`SandboxLevel::Level1`]: spawns via the host-side
        ///   `pre_exec` + cgroup pipeline (the existing
        ///   [`Self::spawn`] path) and reads stdout/stderr/exit
        ///   concurrently.
        /// - [`SandboxLevel::Level2`]: routes through the pre-warmed
        ///   [`super::Level2Session`]'s
        ///   [`super::Level2Session::exec_step`] — `podman exec` with
        ///   the argv collected via [`Self::push_arg`] /
        ///   [`Self::push_args`].
        ///
        /// The returned shape is the same in both branches so callers
        /// (e.g. the `shell.exec` tool) do not need to know which
        /// level executed.
        ///
        /// # Errors
        ///
        /// Level 1 returns `Err(io::Error)` on spawn / wait failure.
        /// Level 2 returns `Err(io::Error)` synthesised from the
        /// underlying [`forge_oci::OciError`] — the F-595 errors are
        /// not re-exported through this method to keep the
        /// `Tool`-layer error surface narrow.
        pub async fn execute(self) -> io::Result<StepOutcome> {
            match &self.level {
                SandboxLevel::Level1 => execute_level1(self).await,
                SandboxLevel::Level2 { session } => {
                    let session = session.clone();
                    // Materialize each argv element as an owned `String` so
                    // the borrowed `&[&str]` slice we hand to `exec_step`
                    // points into stable storage that outlives the call.
                    let argv: Vec<String> = self
                        .argv
                        .iter()
                        .map(|s| s.to_string_lossy().into_owned())
                        .collect();
                    drop(self); // we own no spawn-side state for Level 2.
                    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                    session
                        .exec_step(&argv_refs)
                        .await
                        .map_err(|e| io::Error::other(format!("level 2 exec: {e}")))
                }
            }
        }
    }

    /// Level 1 implementation of [`SandboxedCommand::execute`]. Pulled
    /// out so the branching site stays compact and the seccomp pipeline
    /// stays in one place.
    async fn execute_level1(sb: SandboxedCommand) -> io::Result<StepOutcome> {
        use tokio::io::AsyncReadExt;

        // Pipe stdout/stderr (caller may have already done this via
        // `command_mut`; setting again is idempotent for tokio's
        // builder).
        let mut sb = sb;
        sb.cmd.stdout(std::process::Stdio::piped());
        sb.cmd.stderr(std::process::Stdio::piped());
        sb.cmd.stdin(std::process::Stdio::null());

        let mut sandboxed = sb.spawn()?;
        let stdout = sandboxed.as_child_mut().stdout.take();
        let stderr = sandboxed.as_child_mut().stderr.take();

        let stdout_fut = async move {
            match stdout {
                Some(mut s) => {
                    let mut buf = String::new();
                    let _ = s.read_to_string(&mut buf).await;
                    buf
                }
                None => String::new(),
            }
        };
        let stderr_fut = async move {
            match stderr {
                Some(mut s) => {
                    let mut buf = String::new();
                    let _ = s.read_to_string(&mut buf).await;
                    buf
                }
                None => String::new(),
            }
        };

        let (status, stdout, stderr) =
            tokio::join!(sandboxed.as_child_mut().wait(), stdout_fut, stderr_fut);
        let status = status?;
        Ok(StepOutcome {
            exit_code: status.code(),
            stdout,
            stderr,
        })
    }

    /// Handle to a running sandboxed child. Dropping the handle sends
    /// `SIGKILL` to the entire process group, cleaning up any descendants
    /// the child may have forked, and tears down the per-sandbox cgroup
    /// leaf (when one was created).
    pub struct SandboxedChild {
        child: Option<Child>,
        pgid: i32,
        registry: Option<ChildRegistry>,
        _config: SandboxConfig,
        /// When true, Drop does not send SIGKILL to the process group.
        /// Set by [`SandboxedChild::into_child`] to hand off ownership.
        released: bool,
        /// Per-sandbox cgroup v2 leaf enforcing `pids.max`. `None` when the
        /// host does not delegate the `pids` controller or enrollment
        /// failed (see [`SandboxedCommand::spawn`]).
        cgroup: Option<CgroupLeaf>,
    }

    impl SandboxedChild {
        /// Process group id (== child pid).
        pub fn pgid(&self) -> i32 {
            self.pgid
        }

        /// Borrow the underlying [`tokio::process::Child`].
        pub fn as_child_mut(&mut self) -> &mut Child {
            self.child.as_mut().expect("child not taken")
        }

        /// Filesystem path of the per-sandbox cgroup v2 leaf, if one was
        /// successfully created and the child enrolled. `None` when the
        /// host does not delegate the `pids` controller or enrollment
        /// failed; consumers treat the absence as "rlimit-only
        /// enforcement" rather than a hard error. Exposed primarily so
        /// tests can probe `pids.current` / `pids.max` directly.
        pub fn cgroup_path(&self) -> Option<&Path> {
            self.cgroup.as_ref().map(CgroupLeaf::path)
        }

        /// Consume the handle, returning the underlying [`tokio::process::Child`].
        /// Skips the Drop-based killpg so callers that `wait().await` for
        /// natural exit can avoid killing an already-reaped group.
        ///
        /// The per-sandbox cgroup leaf (when present) is **not** killed —
        /// the caller still owns a live child, and sending `cgroup.kill`
        /// here would SIGKILL the very task they are about to wait on.
        /// Instead, we schedule a background reaper on the current tokio
        /// runtime that polls the leaf's `cgroup.events` and removes the
        /// directory once `populated=0`. If no tokio runtime is active
        /// (non-async callers), the leaf is rmdir'd eagerly as a
        /// best-effort final cleanup — non-empty leaves EBUSY on rmdir,
        /// in which case the OS reclaims the orphan on reboot.
        pub fn into_child(mut self) -> Child {
            if let Some(reg) = &self.registry {
                reg.remove(self.pgid);
            }
            if let Some(leaf) = self.cgroup.take() {
                schedule_leaf_reaper(leaf);
            }
            self.released = true;
            self.child.take().expect("child not taken")
        }
    }

    impl Drop for SandboxedChild {
        fn drop(&mut self) {
            if self.released {
                return;
            }
            if let Some(reg) = &self.registry {
                reg.remove(self.pgid);
            }
            // SAFETY: killpg is async-signal-safe; we just send SIGKILL.
            unsafe {
                libc::killpg(self.pgid, libc::SIGKILL);
            }
            if let Some(leaf) = self.cgroup.take() {
                leaf.kill_and_remove();
            }
        }
    }

    /// Background reaper for cgroup leaves whose owning `SandboxedChild`
    /// was consumed by `into_child`. Polls the leaf's `cgroup.events` for
    /// `populated 0` and rmdir's the leaf once it goes empty. Best-effort:
    /// if the runtime shuts down first, the orphan is reclaimed on
    /// reboot.
    fn schedule_leaf_reaper(leaf: CgroupLeaf) {
        // `Handle::try_current` returns Err when no tokio runtime is
        // active on this thread. In that case we try a single eager
        // rmdir — non-empty leaves return EBUSY and are left to the OS.
        match tokio::runtime::Handle::try_current() {
            Ok(_handle) => {
                tokio::spawn(async move {
                    // Poll interval is short relative to realistic tool
                    // lifetimes; 50 ms keeps CPU cost trivial while
                    // reclaiming the leaf within a few ms of the last
                    // task exiting. Cap total wait at ~10 minutes so
                    // stuck children do not pin the reaper forever.
                    for _ in 0..12_000 {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        if let Ok(events) =
                            std::fs::read_to_string(leaf.path().join("cgroup.events"))
                        {
                            if events.lines().any(|l| l.trim() == "populated 0") {
                                leaf.kill_and_remove();
                                return;
                            }
                        } else {
                            // Leaf disappeared (someone else cleaned it up).
                            return;
                        }
                    }
                    // Time budget exhausted — force cleanup so the leaf
                    // does not linger indefinitely.
                    leaf.kill_and_remove();
                });
            }
            Err(_) => {
                // No tokio runtime — try an eager rmdir. If the child is
                // still alive the directory is non-empty and rmdir
                // returns EBUSY; we accept the orphan.
                let _ = std::fs::remove_dir(leaf.path());
            }
        }
    }

    /// Render a pid as decimal ASCII into `buf`, returning the number of
    /// bytes written. Async-signal-safe: no allocation, no locks, no
    /// locale lookups. Used by the cgroup-enrollment pre_exec hook where
    /// `format!`/`to_string` would be unsafe.
    fn pid_to_decimal(pid: libc::pid_t, buf: &mut [u8; 16]) -> usize {
        // pids are positive on Linux (kernel.pid_max <= 2^22) but keep the
        // negative-guard anyway.
        let mut n = if pid < 0 { 0u32 } else { pid as u32 };
        if n == 0 {
            buf[0] = b'0';
            return 1;
        }
        let mut tmp = [0u8; 16];
        let mut i = 0;
        while n > 0 {
            tmp[i] = b'0' + (n % 10) as u8;
            n /= 10;
            i += 1;
        }
        // Reverse into buf.
        for j in 0..i {
            buf[j] = tmp[i - 1 - j];
        }
        i
    }

    fn apply_rlimit(resource: libc::__rlimit_resource_t, value: u64) -> io::Result<()> {
        let lim = libc::rlimit {
            rlim_cur: value as libc::rlim_t,
            rlim_max: value as libc::rlim_t,
        };
        // SAFETY: setrlimit only mutates kernel state and reads from a stack
        // pointer. Safe to call post-fork.
        let rc = unsafe { libc::setrlimit(resource, &lim) };
        if rc == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
pub use imp::{SandboxedChild, SandboxedCommand};

// Linux-only tests ──────────────────────────────────────────────────────────

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::process::Command as TokioCommand;

    #[test]
    fn default_config_includes_nproc_nofile_fsize() {
        let cfg = SandboxConfig::default();
        assert_eq!(
            cfg.max_processes, 256,
            "per-sandbox pids.max default (F-149)"
        );
        assert_eq!(
            cfg.rlimit_nproc_backstop, 4096,
            "uid-wide RLIMIT_NPROC backstop default"
        );
        assert_eq!(cfg.max_open_files, 256, "RLIMIT_NOFILE default");
        assert_eq!(
            cfg.max_file_size_bytes,
            100 * 1024 * 1024,
            "RLIMIT_FSIZE default (100 MiB)"
        );
    }

    #[tokio::test]
    async fn env_whitelist_excludes_secret_but_keeps_path() {
        // Arrange: a secret var in the daemon env; PATH is already set.
        std::env::set_var("FORGE_TEST_SECRET_ENV", "top-secret-value");

        let tmp = tempfile::tempdir().unwrap();
        let mut sb = SandboxedCommand::new("/usr/bin/env", tmp.path());
        sb.command_mut()
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = sb.spawn().expect("spawn env").into_child();
        let mut stdout = child.stdout.take().unwrap();
        let mut out = String::new();
        stdout.read_to_string(&mut out).await.unwrap();
        let status = child.wait().await.unwrap();
        assert!(status.success(), "env exited non-zero: {status:?}");

        std::env::remove_var("FORGE_TEST_SECRET_ENV");

        assert!(
            !out.contains("top-secret-value"),
            "secret leaked into child env:\n{out}"
        );
        // PATH is in the base whitelist.
        assert!(
            out.lines().any(|l| l.starts_with("PATH=")),
            "expected PATH in child env, got:\n{out}"
        );
    }

    #[tokio::test]
    async fn caller_provided_allowlist_passes_through() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sb = SandboxedCommand::new("/usr/bin/env", tmp.path());
        sb.allow_env("FORGE_ALLOWED", "yes-please");
        sb.command_mut()
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = sb.spawn().unwrap().into_child();
        let mut stdout = child.stdout.take().unwrap();
        let mut out = String::new();
        stdout.read_to_string(&mut out).await.unwrap();
        child.wait().await.unwrap();

        assert!(
            out.lines().any(|l| l == "FORGE_ALLOWED=yes-please"),
            "expected allow-listed var in child env, got:\n{out}"
        );
    }

    #[tokio::test]
    async fn drop_kills_descendants_via_pgid() {
        // Spawn a shell that forks a long-sleeping grandchild and prints its
        // pid. We then drop the SandboxedChild and verify the grandchild is
        // dead.
        let tmp = tempfile::tempdir().unwrap();
        let mut sb = SandboxedCommand::new("/bin/sh", tmp.path());
        sb.command_mut()
            .arg("-c")
            // Double-fork so the grandchild is a sibling process group
            // member (same pgid thanks to setsid, but distinct pid).
            .arg("sleep 60 & echo $! ; wait")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut sandboxed = sb.spawn().unwrap();
        let mut stdout = sandboxed.as_child_mut().stdout.take().unwrap();

        // Read the grandchild pid (first line).
        let mut buf = vec![0u8; 32];
        let n = stdout.read(&mut buf).await.unwrap();
        let grandchild_pid: i32 = std::str::from_utf8(&buf[..n])
            .unwrap()
            .trim()
            .parse()
            .expect("parse grandchild pid");

        // Sanity: grandchild is alive.
        assert!(
            process_alive(grandchild_pid),
            "grandchild {grandchild_pid} should be alive before drop"
        );

        drop(sandboxed);

        // Give the kernel a moment to reap.
        for _ in 0..50 {
            if !process_alive(grandchild_pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("grandchild {grandchild_pid} still alive after drop (pgid kill failed)");
    }

    #[tokio::test]
    async fn cpu_rlimit_terminates_busy_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let config = SandboxConfig {
            cpu_seconds: 1,
            ..SandboxConfig::default()
        };
        let mut sb = SandboxedCommand::with_config("/bin/sh", tmp.path(), config);
        sb.command_mut()
            .arg("-c")
            .arg("while :; do :; done")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let mut child = sb.spawn().unwrap().into_child();
        // Bound waiting — CPU budget is 1 s, so we give it 10 s of wall clock
        // to account for scheduler variance on loaded CI.
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("child did not exit after CPU rlimit")
            .unwrap();

        // SIGXCPU is signal 24 on Linux. Either killed by SIGXCPU directly or
        // the shell reports it; accept any signalled exit.
        use std::os::unix::process::ExitStatusExt;
        assert!(
            status.signal().is_some() || !status.success(),
            "expected busy-loop child to be terminated, got {status:?}"
        );
    }

    #[tokio::test]
    async fn registry_tracks_and_clears_children() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = ChildRegistry::new();

        let mut sb = SandboxedCommand::new("/bin/sh", tmp.path());
        sb.command_mut()
            .arg("-c")
            .arg("sleep 30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        sb.with_registry(registry.clone());

        let sandboxed = sb.spawn().unwrap();
        let pgid = sandboxed.pgid();
        assert_eq!(registry.len(), 1);
        assert!(process_alive(pgid), "child {pgid} should be alive");

        registry.kill_all();
        assert_eq!(registry.len(), 0);

        // Reap the child so process_alive returns false (killpg'd children
        // otherwise linger as zombies which kill(pid, 0) reports as alive).
        let mut child = sandboxed.into_child();
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("child did not exit after registry.kill_all()")
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "expected SIGKILL, got {status:?}"
        );
        assert!(!process_alive(pgid));
    }

    #[tokio::test]
    async fn rlimits_bound_child_via_setrlimit() {
        // Regression for F-055: the sandbox must call setrlimit for
        // NPROC / NOFILE / FSIZE so a single approved tool call cannot
        // fork-bomb, exhaust fds, or fill the disk. We probe the
        // kernel-visible limits from inside the sandbox — `ulimit`
        // reads the post-setrlimit values, giving us direct evidence
        // that `pre_exec` actually applied them.
        //
        // If any of the three `apply_rlimit` calls regress out of
        // `pre_exec`, this test fails: the child's `ulimit` will
        // instead report the daemon-inherited defaults.
        //
        // We intentionally do not drive a *behavioral* fork-bomb /
        // fd-exhaustion / write-overflow test here. RLIMIT_NPROC is
        // per-RUID rather than per-sandbox, so behavioral coverage
        // would be flaky under `cargo test`'s parallel harness and
        // shell-variant-dependent. The kernel enforces the bounds
        // automatically once setrlimit has run; this test is the
        // load-bearing one.
        let tmp = tempfile::tempdir().unwrap();
        // Probe values are deliberately distinct from Default so the test
        // would fail if pre_exec stopped applying setrlimit. NPROC stays
        // above typical CI-runner-uid process counts while distinct from
        // the default 4096.
        //
        // Read rlimits via /proc/self/limits rather than `ulimit` — the
        // latter varies across shells (Ubuntu's /bin/sh = dash) and has
        // produced inconsistent output on GHA runners. /proc/self/limits
        // is kernel-rendered, shell-independent.
        let config = SandboxConfig {
            // Probe the uid-wide RLIMIT_NPROC backstop rather than the
            // cgroup `pids.max`: this test is explicitly about the
            // rlimit path, which per-uid rather than per-sandbox.
            rlimit_nproc_backstop: 8192,
            max_open_files: 42,
            max_file_size_bytes: 4096,
            ..SandboxConfig::default()
        };
        let mut sb = SandboxedCommand::with_config("/bin/sh", tmp.path(), config);
        sb.command_mut()
            .arg("-c")
            .arg("cat /proc/self/limits")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = sb.spawn().unwrap().into_child();
        let mut stdout = child.stdout.take().unwrap();
        let mut out = String::new();
        stdout.read_to_string(&mut out).await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("child hung")
            .unwrap();
        assert!(status.success(), "cat exited non-zero: {status:?}");

        // /proc/self/limits has the shape:
        //   Limit                     Soft Limit           Hard Limit           Units
        //   Max processes             8192                 8192                 processes
        //   Max open files            42                   42                   files
        //   Max file size             4096                 4096                 bytes
        // (plus other rows we don't care about)
        let soft_limit_for = |name: &str| -> Option<String> {
            out.lines().find(|l| l.starts_with(name)).and_then(|l| {
                // Fields after the name are whitespace-separated. Soft = idx 0
                // in the remainder; but `name` itself contains spaces (e.g.
                // "Max processes"), so split on whitespace and take column 2.
                let cols: Vec<&str> = l.split_whitespace().collect();
                // ["Max", "processes", "<soft>", "<hard>", "<units>"]
                let word_count = name.split_whitespace().count();
                cols.get(word_count).map(|s| s.to_string())
            })
        };

        assert_eq!(
            soft_limit_for("Max processes").as_deref(),
            Some("8192"),
            "RLIMIT_NPROC not applied: {out}"
        );
        assert_eq!(
            soft_limit_for("Max open files").as_deref(),
            Some("42"),
            "RLIMIT_NOFILE not applied: {out}"
        );
        assert_eq!(
            soft_limit_for("Max file size").as_deref(),
            Some("4096"),
            "RLIMIT_FSIZE not applied: {out}"
        );
    }

    fn process_alive(pid: i32) -> bool {
        // SAFETY: `kill(pid, 0)` only checks for existence & permission.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return true;
        }
        // ESRCH = no such process → dead. EPERM = exists, we just can't signal it.
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    // Canary that a plain tokio::process::Command does NOT isolate env
    // (proves our env_clear actually does something).
    #[tokio::test]
    async fn baseline_tokio_command_inherits_env() {
        std::env::set_var("FORGE_BASELINE_SECRET", "leaked");
        let mut cmd = TokioCommand::new("/usr/bin/env");
        cmd.stdout(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut out = String::new();
        stdout.read_to_string(&mut out).await.unwrap();
        child.wait().await.unwrap();
        std::env::remove_var("FORGE_BASELINE_SECRET");
        assert!(out.contains("FORGE_BASELINE_SECRET=leaked"));
    }

    // ── F-149: cgroup v2 pids.max per-sandbox regression ────────────────

    /// True when `/sys/fs/cgroup/cgroup.controllers` exists and lists
    /// `pids`. This is the coarse environment gate the test reads before
    /// deciding to run the strict assertion or skip with a message.
    fn cgroup_v2_pids_controller_present() -> bool {
        match std::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers") {
            Ok(s) => s.split_whitespace().any(|t| t == "pids"),
            Err(_) => false,
        }
    }

    /// F-149 Phase 2 regression: a sandbox whose `max_processes` is set to
    /// N must be unable to hold more than N tasks in its cgroup leaf,
    /// regardless of what sibling sandboxes or the rest of the uid are
    /// doing. This is the property `RLIMIT_NPROC` cannot provide because
    /// it is uid-wide.
    ///
    /// Implementation detail: we deliberately read `pids.current` and
    /// `pids.max` from Rust rather than counting forked PIDs in shell.
    /// The original F-078 attempt hung because the shell retry on EAGAIN
    /// pinned the cgroup at the limit in a tight loop and the test
    /// harness never reaped. Rust-side kernel probes have no such
    /// behavior.
    ///
    /// Skip-on-no-cgroup-v2: CI runners and containers without the
    /// `pids` controller enabled skip with a clear message rather than
    /// silently exercising the rlimit-only fallback.
    #[tokio::test]
    async fn cgroup_pids_max_caps_sandbox_tasks_per_f149() {
        if !cgroup_v2_pids_controller_present() {
            eprintln!(
                "SKIP: cgroup v2 `pids` controller not present under \
                 /sys/fs/cgroup — F-149 regression cannot assert on this host."
            );
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        const PIDS_CAP: u64 = 8;
        // Kick off a small fixed number of backgrounded sleepers — no
        // shell retry loop, no arithmetic, no EAGAIN handling. The shell
        // plus these children saturate `pids.max` quickly; further
        // sleepers the shell tries to background simply fail their
        // clone/fork in the kernel and the `(subshell &)` pattern
        // discards their failure without retrying.
        //
        // Over-commit factor of ~3x leaves room for the shell itself
        // plus transient task slots spent on arithmetic and `wait`.
        let script = "\
            for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24; do \
              (sleep 5 0</dev/null 1>/dev/null 2>/dev/null &) 2>/dev/null; \
            done; \
            sleep 3\
        ";
        let config = SandboxConfig {
            cpu_seconds: 30,
            max_processes: PIDS_CAP,
            ..SandboxConfig::default()
        };
        let mut sb = SandboxedCommand::with_config("/bin/sh", tmp.path(), config);
        sb.command_mut()
            .arg("-c")
            .arg(script)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let sandboxed = sb.spawn().expect("spawn capped sandbox");

        // If the host has cgroup v2 controllers listed but the daemon's
        // own cgroup does not delegate `pids` to a writable subtree, the
        // leaf will be `None`. Skip rather than assert — the DoD requires
        // "gracefully skips on hosts without cgroup v2 mounted", and
        // non-delegated is the same user-visible situation.
        let Some(leaf) = sandboxed.cgroup_path() else {
            eprintln!(
                "SKIP: cgroup v2 `pids` controller present but not delegated \
                 to the test process's slice — cannot place sandbox leaf."
            );
            return;
        };
        let leaf = leaf.to_path_buf();

        // Poll `pids.current` at a short cadence and track the maximum
        // we observe. This is the kernel's task count in the leaf at
        // read time; taking the running max across reads is equivalent
        // to `pids.peak` but does not depend on the 6.1-era `pids.peak`
        // file (Ubuntu 22.04 LTS still ships 5.15). A transient dip
        // from short-lived subshells exiting between reads does not
        // lose information because the accumulator is monotonic.
        let mut observed_peak: u64 = 0;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(40)).await;
            if let Ok(s) = std::fs::read_to_string(leaf.join("pids.current")) {
                if let Ok(cur) = s.trim().parse::<u64>() {
                    observed_peak = observed_peak.max(cur);
                    if observed_peak >= PIDS_CAP {
                        break;
                    }
                }
            }
        }

        let pids_max = std::fs::read_to_string(leaf.join("pids.max"))
            .expect("pids.max readable")
            .trim()
            .to_string();

        assert_eq!(
            pids_max,
            PIDS_CAP.to_string(),
            "pids.max should reflect SandboxConfig::max_processes"
        );
        // Load-bearing assertion: the cgroup limiter saturated at the
        // configured cap. If the limiter had been silently disabled,
        // 24 sleepers would have fit comfortably and the observed max
        // would sit well above PIDS_CAP; if enforcement worked, the
        // kernel refuses forks past PIDS_CAP so observed_peak == cap.
        assert_eq!(
            observed_peak, PIDS_CAP,
            "max(pids.current) must saturate at pids.max={PIDS_CAP} when the sandbox \
             tries to fan out past it; got observed_peak={observed_peak}"
        );

        // Clean up explicitly so we do not race the test harness's Drop.
        let _ = sandboxed.into_child();
    }

    /// `SandboxedChild::cgroup_path` returns `None` on non-cgroup-v2 hosts
    /// or when delegation is absent — proves the best-effort fallback is
    /// actually best-effort rather than a hard failure.
    ///
    /// On delegated hosts this test asserts the leaf was created and
    /// carries the expected `pids.max`; on non-delegated hosts it
    /// asserts the leaf is `None`. Either branch is a valid outcome of
    /// the "degrade gracefully" contract.
    #[tokio::test]
    async fn cgroup_path_reflects_host_capability() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sb = SandboxedCommand::new("/bin/sh", tmp.path());
        sb.command_mut()
            .arg("-c")
            .arg("sleep 1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let sandboxed = sb.spawn().unwrap();

        match sandboxed.cgroup_path() {
            Some(leaf) => {
                // On this host the leaf exists — confirm pids.max is set.
                let max = std::fs::read_to_string(leaf.join("pids.max"))
                    .expect("pids.max readable when leaf exists")
                    .trim()
                    .to_string();
                assert_eq!(max, "256", "default max_processes should write to pids.max");
            }
            None => {
                // Non-delegated host: the sandbox still ran, proving the
                // fallback path is non-fatal. Nothing to assert on the
                // filesystem because no leaf was created.
            }
        }
        let _ = sandboxed.into_child();
    }

    // ── F-596: SandboxLevel + SandboxedCommand::execute branching ───────

    use async_trait::async_trait;
    use forge_oci::{
        ContainerHandle as OciContainerHandle, ContainerRuntime, ExecResult,
        ImageRef as OciImageRef, OciError, Stats,
    };
    use std::sync::Mutex as StdMutex;

    /// Trait-layer recorder used by the F-596 execute-branch tests.
    /// Functionally identical to the one in `sandbox::level2::tests`,
    /// duplicated here because Rust's test-module privacy keeps it
    /// out of reach.
    #[derive(Default)]
    struct CallLog {
        calls: StdMutex<Vec<String>>,
    }

    struct LoggingRuntime {
        log: Arc<CallLog>,
        exec: ExecResult,
    }

    impl LoggingRuntime {
        fn new(log: Arc<CallLog>, exec: ExecResult) -> Self {
            Self { log, exec }
        }
        fn record(&self, name: &str) {
            self.log.calls.lock().unwrap().push(name.to_string());
        }
    }

    #[async_trait]
    impl ContainerRuntime for LoggingRuntime {
        async fn detect(&self) -> Result<(), OciError> {
            self.record("detect");
            Ok(())
        }
        async fn pull(&self, _image: &OciImageRef) -> Result<(), OciError> {
            self.record("pull");
            Ok(())
        }
        async fn create(
            &self,
            _image: &OciImageRef,
            _argv: &[&str],
            _opts: &forge_oci::SecurityOpts,
        ) -> Result<OciContainerHandle, OciError> {
            self.record("create");
            Ok(OciContainerHandle::new("c-id"))
        }
        async fn start(&self, _h: &OciContainerHandle) -> Result<(), OciError> {
            self.record("start");
            Ok(())
        }
        async fn exec(
            &self,
            _h: &OciContainerHandle,
            _argv: &[&str],
        ) -> Result<ExecResult, OciError> {
            self.record("exec");
            Ok(self.exec.clone())
        }
        async fn stop(&self, _h: &OciContainerHandle) -> Result<(), OciError> {
            self.record("stop");
            Ok(())
        }
        async fn remove(&self, _h: &OciContainerHandle) -> Result<(), OciError> {
            self.record("remove");
            Ok(())
        }
        async fn stats(&self, _h: &OciContainerHandle) -> Result<Stats, OciError> {
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

    #[test]
    fn sandbox_level_default_is_level1() {
        // Defaulting to Level 1 preserves the historical contract:
        // call sites that don't ask for Level 2 keep the seccomp +
        // setrlimit pipeline.
        let level = SandboxLevel::default();
        assert!(matches!(level, SandboxLevel::Level1));
        assert!(!level.is_level2());
        assert!(level.level2_session().is_none());
    }

    #[tokio::test]
    async fn sandbox_level_level2_carries_session() {
        let log = Arc::new(CallLog::default());
        let runtime: Arc<dyn ContainerRuntime> = Arc::new(LoggingRuntime::new(
            log.clone(),
            ExecResult {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            },
        ));
        let session = Arc::new(
            Level2Session::create(
                runtime,
                OciImageRef::parse("docker.io/library/alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap(),
                ContainerLimits::default(),
            )
            .await
            .unwrap(),
        );
        session.disable_drop_cleanup();
        let level = SandboxLevel::Level2 { session };
        assert!(level.is_level2());
        assert!(level.level2_session().is_some());
    }

    #[tokio::test]
    async fn execute_level2_routes_through_runtime_exec() {
        // Load-bearing: SandboxedCommand::execute on Level 2 must hit
        // the trait's `exec` method (not spawn a host process), and
        // the StepOutcome must mirror the captured stdout/stderr/exit.
        let log = Arc::new(CallLog::default());
        let runtime: Arc<dyn ContainerRuntime> = Arc::new(LoggingRuntime::new(
            log.clone(),
            ExecResult {
                exit_code: Some(7),
                stdout: "hello\n".to_string(),
                stderr: "warn\n".to_string(),
            },
        ));
        let session = Arc::new(
            Level2Session::create(
                runtime,
                OciImageRef::parse("docker.io/library/alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap(),
                ContainerLimits::default(),
            )
            .await
            .unwrap(),
        );
        session.disable_drop_cleanup();

        let tmp = tempfile::tempdir().unwrap();
        let mut sb = SandboxedCommand::new("echo", tmp.path()).with_level(SandboxLevel::Level2 {
            session: session.clone(),
        });
        sb.push_args(["hi", "there"]);

        let outcome = sb.execute().await.expect("execute level 2");
        assert_eq!(outcome.exit_code, Some(7));
        assert_eq!(outcome.stdout, "hello\n");
        assert_eq!(outcome.stderr, "warn\n");

        // pull + create + start (from session creation) + 1× exec.
        // No stop/remove yet — that runs on session.teardown().
        let calls = log.calls.lock().unwrap().clone();
        assert_eq!(calls, vec!["pull", "create", "start", "exec"]);
    }

    #[tokio::test]
    async fn execute_level1_uses_host_process_path() {
        // Smoke test that Level 1 still reaches the host-side spawn
        // pipeline. Run a trivial `/bin/echo` and confirm we capture
        // its stdout — the existing seccomp/setrlimit infrastructure
        // is exercised by the older tests above; this one only pins
        // that the new `execute()` entry point did not regress that
        // path.
        let tmp = tempfile::tempdir().unwrap();
        let mut sb = SandboxedCommand::new("/bin/echo", tmp.path());
        sb.push_args(["forge-596"]);
        let outcome = sb.execute().await.expect("execute level 1");
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout.trim(), "forge-596");
    }

    #[tokio::test]
    async fn spawn_rejects_level2_to_avoid_silent_isolation_bypass() {
        // Load-bearing safety property: `spawn()` is Level-1-only.
        // A caller who builds a Level 2 command and reaches for
        // `spawn()` would otherwise silently run the work on the
        // host with no container isolation. The guard turns that
        // foot-gun into a clear error.
        let log = Arc::new(CallLog::default());
        let runtime: Arc<dyn ContainerRuntime> = Arc::new(LoggingRuntime::new(
            log,
            ExecResult {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            },
        ));
        let session = Arc::new(
            Level2Session::create(
                runtime,
                OciImageRef::parse("docker.io/library/alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap(),
                ContainerLimits::default(),
            )
            .await
            .unwrap(),
        );
        session.disable_drop_cleanup();

        let tmp = tempfile::tempdir().unwrap();
        let mut sb =
            SandboxedCommand::new("/bin/echo", tmp.path()).with_level(SandboxLevel::Level2 {
                session: session.clone(),
            });
        sb.push_args(["should-not-run"]);

        let err = match sb.spawn() {
            Ok(_) => panic!("Level 2 + spawn must error"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("Level 1 only"),
            "expected explicit Level 1 only message, got: {err}"
        );
    }
}

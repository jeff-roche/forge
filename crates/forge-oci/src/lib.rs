//! Container lifecycle for agent isolation.
//!
//! [`ContainerRuntime`] defines the abstract surface Forge consumes for
//! per-agent container sandboxes. The first implementation, [`PodmanRuntime`],
//! shells out to a rootless `podman` binary — no daemon, no privileged calls.
//!
//! See `docs/architecture/crate-architecture.md` §3.6 for the design rationale
//! and `docs/architecture/isolation-model.md` for how this slots into the
//! agent execution model.

#![deny(missing_docs)]

mod podman;
mod runner;

pub use podman::PodmanRuntime;
pub use runner::{
    CommandOutcome, CommandRunner, RecordedCall, RecordedCalls, RecordingRunner, StubResponse,
    TokioCommandRunner,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Reference to an OCI image, decomposed into the parts the runtime cares
/// about.
///
/// `parse` is intentionally lenient: it accepts the common forms
/// `name`, `name:tag`, `registry/name:tag`, `registry/namespace/name:tag`.
/// When `tag` is omitted it defaults to `latest`. When `registry` is omitted
/// it stays `None` so callers can decide whether to default to `docker.io` or
/// require an explicit registry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageRef {
    /// Optional registry hostname (e.g. `docker.io`, `quay.io`).
    pub registry: Option<String>,
    /// Image name including any namespace (e.g. `library/alpine`,
    /// `myorg/myapp`).
    pub name: String,
    /// Image tag (e.g. `3.19`, `latest`).
    pub tag: String,
}

impl ImageRef {
    /// Construct an [`ImageRef`] from explicit parts.
    pub fn new(
        registry: Option<impl Into<String>>,
        name: impl Into<String>,
        tag: impl Into<String>,
    ) -> Self {
        Self {
            registry: registry.map(Into::into),
            name: name.into(),
            tag: tag.into(),
        }
    }

    /// Parse an image reference string of the form
    /// `[registry/]name[:tag]`.
    ///
    /// A leading segment counts as a registry only when it contains `.` or `:`
    /// (port). This matches the convention `podman` and `docker` follow when
    /// disambiguating `library/alpine` (no registry, namespace `library`)
    /// from `quay.io/myorg/myapp`.
    pub fn parse(input: &str) -> Result<Self, OciError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(OciError::InvalidImageRef {
                input: input.to_string(),
                reason: "empty",
            });
        }

        let (path, tag) = match trimmed.rsplit_once(':') {
            // A `:` inside the first segment is part of the registry port,
            // not a tag separator. Detect by checking whether the substring
            // before the colon contains a `/`.
            Some((before, after)) if before.contains('/') => (before, after.to_string()),
            Some((before, after)) if !before.contains('/') && !after.contains('/') => {
                (before, after.to_string())
            }
            _ => (trimmed, "latest".to_string()),
        };

        let (registry, name) = match path.split_once('/') {
            Some((head, rest)) if head.contains('.') || head.contains(':') => {
                (Some(head.to_string()), rest.to_string())
            }
            _ => (None, path.to_string()),
        };

        if name.is_empty() {
            return Err(OciError::InvalidImageRef {
                input: input.to_string(),
                reason: "missing image name",
            });
        }

        Ok(Self {
            registry,
            name,
            tag,
        })
    }

    /// Render the reference back into the canonical `[registry/]name:tag` form
    /// that `podman` accepts.
    pub fn to_image_string(&self) -> String {
        match &self.registry {
            Some(reg) => format!("{}/{}:{}", reg, self.name, self.tag),
            None => format!("{}:{}", self.name, self.tag),
        }
    }
}

impl std::fmt::Display for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_image_string())
    }
}

/// Opaque handle to a created container.
///
/// The `id` is the runtime-assigned container ID returned by
/// `podman create` / equivalent. Callers should treat it as opaque and pass it
/// straight back into [`ContainerRuntime`] methods.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContainerHandle {
    /// Runtime-assigned container ID.
    pub id: String,
}

impl ContainerHandle {
    /// Wrap an existing container ID.
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

/// Captured result of an `exec` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResult {
    /// Exit status reported by the runtime. `None` means the runtime didn't
    /// produce one (e.g. signalled).
    pub exit_code: Option<i32>,
    /// Captured stdout bytes, decoded as UTF-8 (lossy).
    pub stdout: String,
    /// Captured stderr bytes, decoded as UTF-8 (lossy).
    pub stderr: String,
}

/// Runtime container resource snapshot.
///
/// Fields are best-effort: podman occasionally produces partial entries (e.g.
/// while a container is exiting) and we surface the missing pieces as `None`
/// rather than failing the whole call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    /// CPU usage as a `0.0..=100.0` percentage of total host CPU.
    pub cpu_percent: Option<f64>,
    /// Memory usage in bytes (resident set as the runtime defines it).
    pub memory_bytes: Option<u64>,
    /// Number of processes inside the container.
    pub pids: Option<u64>,
}

/// Errors surfaced by [`ContainerRuntime`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum OciError {
    /// `podman` (or another required tool) is not on `PATH`.
    #[error("container runtime '{0}' not found on PATH; install it to enable container isolation")]
    RuntimeMissing(&'static str),

    /// `podman` is on PATH but the detection probe (e.g. `podman info`) failed
    /// before we could read its output. Distinct from [`Self::RootlessUnavailable`]:
    /// here the runtime is *broken* (cgroup delegation, missing newuidmap,
    /// SELinux denial, etc.), not configured-rootful.
    #[error("container runtime '{tool}' is installed but not functional: {stderr}")]
    RuntimeBroken {
        /// Runtime name (e.g. `"podman"`).
        tool: &'static str,
        /// Captured stderr from the failing probe.
        stderr: String,
    },

    /// `podman` ran successfully but reports rootless mode is disabled. Only
    /// returned when the probe's JSON parsed cleanly and explicitly said so.
    #[error("rootless mode unavailable for container runtime '{runtime}': {reason}")]
    RootlessUnavailable {
        /// Runtime name (e.g. `"podman"`).
        runtime: &'static str,
        /// Human-readable reason produced by the detection probe.
        reason: String,
    },

    /// The runtime invocation exited non-zero.
    #[error("{tool} {args:?} failed (exit={exit_code:?}): {stderr}")]
    CommandFailed {
        /// Binary name (typically `"podman"`).
        tool: &'static str,
        /// Argv passed (excluding the binary name itself).
        args: Vec<String>,
        /// Exit code if the process produced one.
        exit_code: Option<i32>,
        /// Captured stderr (UTF-8 lossy).
        stderr: String,
    },

    /// The image reference could not be parsed.
    #[error("invalid image reference '{input}': {reason}")]
    InvalidImageRef {
        /// Original input that failed to parse.
        input: String,
        /// Why it failed.
        reason: &'static str,
    },

    /// Runtime spawn / I/O failure (process couldn't be launched, pipe died,
    /// etc.).
    #[error("io error invoking {tool}: {source}")]
    Io {
        /// Binary name.
        tool: &'static str,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Runtime produced unparseable JSON when one was expected (`info`,
    /// `stats`).
    #[error("could not parse {tool} {subcommand} output as JSON: {source}")]
    InvalidJson {
        /// Binary name.
        tool: &'static str,
        /// Subcommand that produced the bad output (e.g. `"info"`).
        subcommand: &'static str,
        /// Underlying parse error.
        #[source]
        source: serde_json::Error,
    },
}

/// Network policy applied at container create time.
///
/// Maps directly onto `podman create --network <value>`. The default for
/// Level 2 sandboxes is [`NetworkPolicy::None`] — no inbound or outbound
/// traffic — because containers ship without any tool-declared host
/// allow-list today (CNI policy is future work, see
/// `docs/architecture/isolation-model.md` §8.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NetworkPolicy {
    /// `--network none` — no network namespace beyond loopback.
    None,
    /// Inherit the host network namespace (`--network host`). Reserved for
    /// trusted callers; never the default for Level 2.
    Host,
    /// Use a named podman network. The string is passed verbatim to
    /// `--network <name>`.
    Named(String),
}

impl NetworkPolicy {
    /// Render as the value passed after `--network`. Returns `None` when
    /// the policy is "use the runtime default" — currently nothing maps to
    /// this, but it leaves room for future variants without churning the
    /// argv-shaping call sites.
    pub fn as_arg(&self) -> Option<&str> {
        match self {
            NetworkPolicy::None => Some("none"),
            NetworkPolicy::Host => Some("host"),
            NetworkPolicy::Named(name) => Some(name.as_str()),
        }
    }
}

/// User-namespace policy applied at container create time.
///
/// `--userns keep-id` anchors the container's user namespace to the host
/// uid/gid so file ownership round-trips cleanly between mounts. Without
/// it, rootless podman applies its default mapping (uid 0 inside maps to
/// the rootless uid outside) which makes workspace mounts hostile to read
/// in either direction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UserNsPolicy {
    /// `--userns keep-id` — caller's uid/gid is preserved inside the
    /// container.
    KeepId,
    /// Use the runtime default (rootless podman: nested mapping). No
    /// `--userns` flag emitted.
    Default,
}

/// Security hardening options applied at container create time.
///
/// Every field maps to a concrete `podman create` flag; the defaults are
/// the strict "Level 2" preset documented in
/// `docs/architecture/isolation-model.md` §8.3. Callers that need a
/// permissive policy (tests, legacy code paths) should construct this
/// explicitly with [`SecurityOpts::permissive`].
///
/// Threat model: rootless podman alone leaves `NoNewPrivs=0`, the default
/// rootless capability set, an open network namespace, and a writable
/// rootfs — every escape-class CVE in the kernel, podman, or runc that
/// these flags would otherwise defeat is exploitable from inside the
/// container.
///
/// Field ordering in [`SecurityOpts::to_create_flags`] is deterministic so
/// argv assertions can pin the rendering.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SecurityOpts {
    /// `--security-opt no-new-privileges` — once set, processes inside
    /// the container cannot gain privileges via setuid binaries or file
    /// capabilities. Default `true`.
    pub no_new_privileges: bool,

    /// Capabilities to drop. `["ALL"]` drops every capability and is the
    /// hardened default; downstream issues that need specific
    /// capabilities should add them to [`Self::cap_add`] rather than
    /// shrinking this list.
    pub cap_drop: Vec<String>,

    /// Capabilities to add back after the cap-drop. The hardened default
    /// is empty — the `sleep infinity` init process and the agent
    /// commands run via `podman exec` need none.
    pub cap_add: Vec<String>,

    /// `--read-only` — rootfs is mounted read-only. Writable workspace
    /// directories are surfaced as explicit tmpfs / volume mounts (future
    /// work tracked alongside the F-642 follow-ups). Default `true`.
    pub read_only_rootfs: bool,

    /// Network policy. Default [`NetworkPolicy::None`] — the strictest
    /// interpretation of the F-642 DoD "restricted network".
    pub network: NetworkPolicy,

    /// User-namespace policy. Default [`UserNsPolicy::KeepId`].
    pub userns: UserNsPolicy,
}

impl SecurityOpts {
    /// Strict Level-2 baseline: no-new-privileges, cap-drop ALL,
    /// read-only rootfs, no network, keep-id user namespace.
    ///
    /// This is what [`crate::ContainerRuntime::create`] callers should
    /// pass unless they have a specific reason to relax a flag — and
    /// every relaxation should be documented at the call site.
    pub fn hardened_default() -> Self {
        Self {
            no_new_privileges: true,
            cap_drop: vec!["ALL".to_string()],
            cap_add: Vec::new(),
            read_only_rootfs: true,
            network: NetworkPolicy::None,
            userns: UserNsPolicy::KeepId,
        }
    }

    /// Permissive preset — every hardening flag disabled. Reserved for
    /// tests that need a baseline argv without security flags interleaved
    /// in their assertions. Production callers must not use this.
    pub fn permissive() -> Self {
        Self {
            no_new_privileges: false,
            cap_drop: Vec::new(),
            cap_add: Vec::new(),
            read_only_rootfs: false,
            network: NetworkPolicy::Named("default".to_string()), // inherit runtime default
            userns: UserNsPolicy::Default,
        }
    }

    /// Render into the canonical podman create flag list. The order is
    /// pinned by tests so the exact argv is deterministic:
    ///
    /// 1. `--security-opt no-new-privileges` (if set)
    /// 2. one `--cap-drop <CAP>` per entry in `cap_drop`, in input order
    /// 3. one `--cap-add <CAP>` per entry in `cap_add`, in input order
    /// 4. `--read-only` (if set)
    /// 5. `--network <value>` (always emitted unless the variant maps to
    ///    `None`, which currently never happens)
    /// 6. `--userns keep-id` (if `KeepId`)
    ///
    /// The flags are inserted between `podman create` and the IMAGE
    /// positional so podman parses them as runtime options. See
    /// [`crate::ContainerRuntime::create`] docs for the positional grammar.
    pub fn to_create_flags(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.no_new_privileges {
            out.push("--security-opt".to_string());
            out.push("no-new-privileges".to_string());
        }
        for cap in &self.cap_drop {
            out.push("--cap-drop".to_string());
            out.push(cap.clone());
        }
        for cap in &self.cap_add {
            out.push("--cap-add".to_string());
            out.push(cap.clone());
        }
        if self.read_only_rootfs {
            out.push("--read-only".to_string());
        }
        if let Some(value) = self.network.as_arg() {
            // SecurityOpts::permissive uses Named("default") as a sentinel
            // for "do not pin --network" — emit nothing in that case so
            // the runtime's default network namespace is inherited.
            if !(matches!(self.network, NetworkPolicy::Named(ref n) if n == "default")) {
                out.push("--network".to_string());
                out.push(value.to_string());
            }
        }
        if matches!(self.userns, UserNsPolicy::KeepId) {
            out.push("--userns".to_string());
            out.push("keep-id".to_string());
        }
        out
    }
}

impl Default for SecurityOpts {
    /// Defaults to the strict [`Self::hardened_default`]. Production
    /// callers should never have to think about security defaults; opting
    /// out is the explicit step.
    fn default() -> Self {
        Self::hardened_default()
    }
}

/// One line of captured container output.
///
/// Surfaced by [`ContainerLogs::logs`]. The runtime hands back stdout and
/// stderr interleaved in emission order; the `stream` field tells the UI
/// which pipe each line came from so it can render them in different
/// colours without a second IPC round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    /// `"stdout"` or `"stderr"` — the pipe the line came from.
    pub stream: String,
    /// Captured line text (newline already stripped).
    pub line: String,
    /// RFC-3339 timestamp the runtime attached to the line. `None` when
    /// the underlying call did not request timestamps.
    pub timestamp: Option<String>,
}

/// Read-recent-logs surface. Added in F-597 as a non-breaking extension to
/// [`ContainerRuntime`] so the dashboard's logs viewer can stream output
/// from a running container without expanding the core trait.
///
/// Implementors return logs in emission order (oldest first). Pagination
/// is the caller's responsibility — `since` lets the dashboard poll
/// incrementally without re-fetching the full transcript.
#[async_trait]
pub trait ContainerLogs: Send + Sync {
    /// Fetch the recent log lines for `handle`. `since` is an optional
    /// RFC-3339 timestamp; lines emitted at or after `since` are returned.
    /// `tail` caps the number of lines returned (most-recent N) to keep
    /// the IPC payload bounded for huge workloads.
    async fn logs(
        &self,
        handle: &ContainerHandle,
        since: Option<&str>,
        tail: Option<usize>,
    ) -> Result<Vec<LogLine>, OciError>;
}

/// Container lifecycle surface. See module docs.
///
/// Implementations are runtime-specific (`PodmanRuntime`, future
/// `DockerRuntime`, etc.). The trait is shaped for argv-only invocation —
/// every method takes structured slices, never shell strings.
#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    /// Probe the host: confirm the runtime binary is present, functional, and
    /// configured for rootless operation. Each implementation classifies its
    /// own probe failures into the canonical `OciError` variants
    /// ([`OciError::RuntimeMissing`], [`OciError::RuntimeBroken`],
    /// [`OciError::RootlessUnavailable`]) so callers can switch on the variant
    /// without knowing which runtime is underneath.
    async fn detect(&self) -> Result<(), OciError>;

    /// Pull the image into the local runtime store. Idempotent.
    async fn pull(&self, image: &ImageRef) -> Result<(), OciError>;

    /// Create a container from `image` with `argv` as the command. The
    /// container is created but not started; call [`Self::start`] separately.
    ///
    /// `opts` carries the security-hardening flags rendered between
    /// `podman create` and the IMAGE positional. Production callers
    /// should pass [`SecurityOpts::hardened_default`] (the strict Level-2
    /// baseline); see `docs/architecture/isolation-model.md` §8.3 for the
    /// documented allow-list. The argv-parsing grammar is unchanged:
    /// caller-supplied `argv` still terminates flag parsing at the IMAGE
    /// positional (the `--privileged` flag-injection regression test in
    /// `crates/forge-oci/src/podman.rs` pins this).
    async fn create(
        &self,
        image: &ImageRef,
        argv: &[&str],
        opts: &SecurityOpts,
    ) -> Result<ContainerHandle, OciError>;

    /// Start a created container.
    async fn start(&self, handle: &ContainerHandle) -> Result<(), OciError>;

    /// Run `argv` inside an already-started container and capture its output.
    async fn exec(&self, handle: &ContainerHandle, argv: &[&str]) -> Result<ExecResult, OciError>;

    /// Stop a running container (graceful — runtime sends SIGTERM, then
    /// SIGKILL after its grace period).
    async fn stop(&self, handle: &ContainerHandle) -> Result<(), OciError>;

    /// Remove a container. Forces removal if it is still running.
    async fn remove(&self, handle: &ContainerHandle) -> Result<(), OciError>;

    /// Capture a single resource snapshot.
    async fn stats(&self, handle: &ContainerHandle) -> Result<Stats, OciError>;

    /// Parse a runtime-specific stats payload into the common [`Stats`]
    /// shape. Each runtime emits its own JSON schema and unit conventions
    /// (podman: `cpu_percent`/`mem_usage`/`pids`; docker would differ);
    /// pinning the parser at the trait surface keeps that schema-shape
    /// knowledge localized to the implementation while letting callers
    /// drive parsing through a single seam.
    ///
    /// Implementations should be tolerant of partial/missing fields — return
    /// `Stats { ..: None }` for fields the payload does not carry rather
    /// than failing the whole call. Hard parse failures (e.g. malformed
    /// JSON envelope) should surface as [`OciError::InvalidJson`].
    fn parse_stats(&self, raw: &[u8]) -> Result<Stats, OciError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_ref_parses_bare_name() {
        let r = ImageRef::parse("alpine").unwrap();
        assert_eq!(r.registry, None);
        assert_eq!(r.name, "alpine");
        assert_eq!(r.tag, "latest");
        assert_eq!(r.to_image_string(), "alpine:latest");
    }

    #[test]
    fn image_ref_parses_name_and_tag() {
        let r = ImageRef::parse("alpine:3.19").unwrap();
        assert_eq!(r.registry, None);
        assert_eq!(r.name, "alpine");
        assert_eq!(r.tag, "3.19");
    }

    #[test]
    fn image_ref_parses_full_form_round_trip() {
        let r = ImageRef::parse("docker.io/library/alpine:3.19").unwrap();
        assert_eq!(r.registry.as_deref(), Some("docker.io"));
        assert_eq!(r.name, "library/alpine");
        assert_eq!(r.tag, "3.19");
        assert_eq!(r.to_image_string(), "docker.io/library/alpine:3.19");
    }

    #[test]
    fn image_ref_parses_namespace_no_registry() {
        // `library/alpine` is a namespace + name, not a registry — there's no
        // `.` or `:` in the head segment.
        let r = ImageRef::parse("library/alpine:1").unwrap();
        assert_eq!(r.registry, None);
        assert_eq!(r.name, "library/alpine");
        assert_eq!(r.tag, "1");
    }

    #[test]
    fn image_ref_parses_registry_with_port() {
        let r = ImageRef::parse("localhost:5000/myapp:dev").unwrap();
        assert_eq!(r.registry.as_deref(), Some("localhost:5000"));
        assert_eq!(r.name, "myapp");
        assert_eq!(r.tag, "dev");
        assert_eq!(r.to_image_string(), "localhost:5000/myapp:dev");
    }

    #[test]
    fn image_ref_rejects_empty() {
        assert!(matches!(
            ImageRef::parse(""),
            Err(OciError::InvalidImageRef { .. })
        ));
        assert!(matches!(
            ImageRef::parse("   "),
            Err(OciError::InvalidImageRef { .. })
        ));
    }

    #[test]
    fn container_handle_round_trip_debug() {
        let h = ContainerHandle::new("abc123");
        let dbg = format!("{:?}", h);
        assert!(dbg.contains("abc123"));
    }

    #[test]
    fn exec_result_round_trip_debug() {
        let r = ExecResult {
            exit_code: Some(0),
            stdout: "hello\n".to_string(),
            stderr: String::new(),
        };
        let dbg = format!("{:?}", r);
        assert!(dbg.contains("hello"));
        assert!(dbg.contains("Some(0)"));
    }

    // Compile-only assertion: a trivial mock satisfies the trait surface.
    // Catches accidental breaking changes to the trait signature at compile
    // time.
    struct MockRuntime;

    #[async_trait]
    impl ContainerRuntime for MockRuntime {
        async fn detect(&self) -> Result<(), OciError> {
            Ok(())
        }
        async fn pull(&self, _image: &ImageRef) -> Result<(), OciError> {
            Ok(())
        }
        async fn create(
            &self,
            _image: &ImageRef,
            _argv: &[&str],
            _opts: &SecurityOpts,
        ) -> Result<ContainerHandle, OciError> {
            Ok(ContainerHandle::new("mock"))
        }
        async fn start(&self, _handle: &ContainerHandle) -> Result<(), OciError> {
            Ok(())
        }
        async fn exec(
            &self,
            _handle: &ContainerHandle,
            _argv: &[&str],
        ) -> Result<ExecResult, OciError> {
            Ok(ExecResult {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        async fn stop(&self, _handle: &ContainerHandle) -> Result<(), OciError> {
            Ok(())
        }
        async fn remove(&self, _handle: &ContainerHandle) -> Result<(), OciError> {
            Ok(())
        }
        async fn stats(&self, _handle: &ContainerHandle) -> Result<Stats, OciError> {
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

    #[tokio::test]
    async fn mock_runtime_satisfies_trait() {
        let rt: &dyn ContainerRuntime = &MockRuntime;
        rt.detect().await.unwrap();
        let img = ImageRef::parse("alpine:3.19").unwrap();
        rt.pull(&img).await.unwrap();
        // `create`/`exec` accept caller-borrowed `&str` slices directly —
        // no `.into()` allocation required.
        let h = rt
            .create(&img, &["echo", "hi"], &SecurityOpts::hardened_default())
            .await
            .unwrap();
        rt.start(&h).await.unwrap();
        let res = rt.exec(&h, &["true"]).await.unwrap();
        assert_eq!(res.exit_code, Some(0));
        rt.stop(&h).await.unwrap();
        rt.remove(&h).await.unwrap();
        let _ = rt.stats(&h).await.unwrap();
    }

    #[tokio::test]
    async fn detect_is_callable_through_dyn_trait() {
        // F-680: `detect` is part of the trait surface so callers can
        // probe any runtime without knowing the concrete type.
        let rt: &dyn ContainerRuntime = &MockRuntime;
        rt.detect().await.unwrap();
    }

    #[test]
    fn parse_stats_is_callable_through_dyn_trait() {
        // F-680: parsing a runtime-specific stats payload is a trait
        // method, so each runtime owns its own schema interpretation
        // while callers drive parsing through a single seam.
        let rt: &dyn ContainerRuntime = &MockRuntime;
        let _ = rt.parse_stats(b"ignored").unwrap();
    }

    #[tokio::test]
    async fn create_and_exec_accept_borrowed_str_slices() {
        // F-680: argv parameters are `&[&str]` so callers with borrowed
        // string slices can call directly without allocating a Vec<String>.
        let rt: &dyn ContainerRuntime = &MockRuntime;
        let img = ImageRef::parse("alpine:3.19").unwrap();
        let argv: [&str; 2] = ["echo", "hi"];
        let _ = rt
            .create(&img, &argv, &SecurityOpts::hardened_default())
            .await
            .unwrap();
        let _ = rt.exec(&ContainerHandle::new("x"), &argv).await.unwrap();
    }

    // ── F-642: SecurityOpts hardening defaults ───────────────────────

    #[test]
    fn hardened_default_includes_every_dod_flag() {
        // F-642: this is the load-bearing security invariant. If the
        // hardened default ever silently relaxes one of these flags,
        // the Level 2 sandbox loses an escape-class CVE mitigation.
        // Every assertion is keyed off the issue's DoD checklist.
        let opts = SecurityOpts::hardened_default();
        assert!(
            opts.no_new_privileges,
            "no-new-privileges must be enabled by default"
        );
        assert_eq!(
            opts.cap_drop,
            vec!["ALL".to_string()],
            "cap-drop must default to ALL"
        );
        assert!(
            opts.cap_add.is_empty(),
            "cap-add allow-list must default to empty (sleep-infinity init needs none)"
        );
        assert!(opts.read_only_rootfs, "rootfs must be read-only by default");
        assert_eq!(
            opts.network,
            NetworkPolicy::None,
            "network must be `none` by default"
        );
        assert!(
            matches!(opts.userns, UserNsPolicy::KeepId),
            "user namespace must default to keep-id"
        );
    }

    #[test]
    fn hardened_default_renders_canonical_flag_order() {
        // The order is pinned because the integration test asserts the
        // exact rendered argv. Any reorder is a breaking change to
        // operators reading the audit trail.
        let flags = SecurityOpts::hardened_default().to_create_flags();
        assert_eq!(
            flags,
            vec![
                "--security-opt".to_string(),
                "no-new-privileges".to_string(),
                "--cap-drop".to_string(),
                "ALL".to_string(),
                "--read-only".to_string(),
                "--network".to_string(),
                "none".to_string(),
                "--userns".to_string(),
                "keep-id".to_string(),
            ]
        );
    }

    #[test]
    fn default_trait_returns_hardened() {
        // Production callers that fall back to `Default` should get the
        // strict policy, never a permissive accident. F-654 will extend
        // this trait, not relax it.
        assert_eq!(SecurityOpts::default(), SecurityOpts::hardened_default());
    }

    #[test]
    fn permissive_emits_no_hardening_flags() {
        // The permissive preset exists for tests that want a clean argv
        // baseline. If any flag leaks through, those tests would silently
        // start asserting against the hardening flags.
        let flags = SecurityOpts::permissive().to_create_flags();
        assert!(
            flags.is_empty(),
            "permissive preset must render zero flags, got {flags:?}"
        );
    }

    #[test]
    fn cap_add_entries_render_after_cap_drop() {
        // Order matters: podman applies --cap-drop then --cap-add, so
        // the rendered argv mirroring that order is what an operator
        // grepping for the audit trail expects.
        let opts = SecurityOpts {
            cap_add: vec!["NET_BIND_SERVICE".to_string()],
            ..SecurityOpts::hardened_default()
        };
        let flags = opts.to_create_flags();
        let drop_idx = flags
            .iter()
            .position(|s| s == "--cap-drop")
            .expect("cap-drop must be present");
        let add_idx = flags
            .iter()
            .position(|s| s == "--cap-add")
            .expect("cap-add must be present");
        assert!(drop_idx < add_idx, "cap-drop must precede cap-add");
        assert_eq!(flags[add_idx + 1], "NET_BIND_SERVICE");
    }
}

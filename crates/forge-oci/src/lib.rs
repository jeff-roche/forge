//! Container lifecycle for agent isolation.
//!
//! [`ContainerRuntime`] defines the abstract surface Forge consumes for
//! per-agent container sandboxes. The first implementation, [`PodmanRuntime`],
//! shells out to a rootless `podman` binary — no daemon, no privileged calls.
//!
//! See `docs/architecture/crate-architecture.md` §3.6 for the design rationale
//! and `docs/architecture/isolation-model.md` for how this slots into the
//! agent execution model — including the supply-chain story (digest pinning
//! and signature verification) that [`ImageRef`] and
//! [`signature::SignatureVerifier`] enforce.

#![deny(missing_docs)]

mod podman;
mod runner;
pub mod signature;

pub use podman::PodmanRuntime;
pub use runner::{
    CommandOutcome, CommandRunner, RecordedCall, RecordedCalls, RecordingRunner, StubResponse,
    TokioCommandRunner,
};
pub use signature::{
    CosignVerifier, NoopVerifier, SignaturePolicy, SignatureVerifier, VerificationError,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Allowlist of image-reference prefixes for which a tag-only (no digest)
/// reference is accepted. Production runs should still pin even allowlisted
/// images by digest — this list exists so the first-party Forge tool images
/// can roll new tags during early Phase 3 without forcing every test fixture
/// and dev sandbox to know the latest digest.
///
/// A prefix matches the *canonical* `[registry/]name` portion of an
/// [`ImageRef`] (i.e. the part before the tag/digest). The empty string is
/// not allowed; every entry must include the registry so docker.io aliases
/// like `library/alpine` cannot accidentally satisfy the allowlist.
const TAG_ONLY_ALLOWLIST: &[&str] = &["oci.io/forge/"];

/// SHA-256 image digest (e.g. the bytes after `sha256:` in
/// `alpine@sha256:abcdef...`). Newtype so the parser can validate the format
/// once and downstream code never has to.
///
/// The wrapped string is the *full* canonical form including the `sha256:`
/// algorithm prefix — that is what `podman pull` and `cosign verify` both
/// expect to see verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Digest(String);

impl Digest {
    /// Parse a digest of the form `sha256:<64 lowercase hex chars>`.
    ///
    /// We only accept `sha256` today because that is what every relevant
    /// registry, signer, and policy emits. When/if a stronger algorithm
    /// becomes the default, extend this match without changing the
    /// representation.
    pub fn parse(input: &str) -> Result<Self, OciError> {
        let trimmed = input.trim();
        let (algo, hex) = trimmed.split_once(':').ok_or(OciError::InvalidDigest {
            input: input.to_string(),
            reason: "missing algorithm prefix (expected `sha256:<hex>`)",
        })?;
        if algo != "sha256" {
            return Err(OciError::InvalidDigest {
                input: input.to_string(),
                reason: "only sha256 digests are accepted",
            });
        }
        if hex.len() != 64
            || !hex
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(OciError::InvalidDigest {
                input: input.to_string(),
                reason: "sha256 digest must be 64 lowercase hex characters",
            });
        }
        Ok(Self(trimmed.to_string()))
    }

    /// Return the full canonical form including the `sha256:` prefix.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reference to an OCI image, decomposed into the parts the runtime cares
/// about.
///
/// `parse` accepts three reference shapes:
///
/// - `[registry/]name@sha256:<hex>` — pinned by digest. Always accepted.
/// - `[registry/]name@sha256:<hex>:tag` — digest with a human-readable tag
///   for diagnostics. The digest is what runs.
/// - `[registry/]name:tag` — tag-only. Rejected for any source not on the
///   first-party tag-only allowlist (today: `oci.io/forge/`). This is the
///   supply-chain mitigation in F-643: a tag is mutable, so an attacker who
///   pushes to the registry can replace the bytes that get pulled and
///   executed inside the Level 2 sandbox.
///
/// `name` here means the registry-qualified path (e.g.
/// `docker.io/library/alpine`) so the allowlist check cannot be bypassed by
/// dropping the registry — `parse` resolves an implicit `docker.io` and
/// `library/` namespace before testing the allowlist.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageRef {
    /// Optional registry hostname (e.g. `docker.io`, `quay.io`).
    pub registry: Option<String>,
    /// Image name including any namespace (e.g. `library/alpine`,
    /// `myorg/myapp`).
    pub name: String,
    /// Image tag (e.g. `3.19`, `latest`). For digest-pinned references this
    /// is informational only — `podman pull` runs on the digest.
    pub tag: String,
    /// Pinned content digest (`sha256:<hex>`). When present, the runtime
    /// fetches and verifies *this* exact image regardless of the tag.
    pub digest: Option<Digest>,
}

impl ImageRef {
    /// Construct an [`ImageRef`] from explicit parts. Use [`Self::parse`] for
    /// validated input — this constructor skips the supply-chain checks and
    /// is intended for tests and explicit programmatic references.
    pub fn new(
        registry: Option<impl Into<String>>,
        name: impl Into<String>,
        tag: impl Into<String>,
    ) -> Self {
        Self {
            registry: registry.map(Into::into),
            name: name.into(),
            tag: tag.into(),
            digest: None,
        }
    }

    /// Construct a digest-pinned [`ImageRef`] from explicit parts.
    pub fn with_digest(
        registry: Option<impl Into<String>>,
        name: impl Into<String>,
        tag: impl Into<String>,
        digest: Digest,
    ) -> Self {
        Self {
            registry: registry.map(Into::into),
            name: name.into(),
            tag: tag.into(),
            digest: Some(digest),
        }
    }

    /// Parse an image reference. See type-level docs for the accepted
    /// shapes and the allowlist semantics.
    pub fn parse(input: &str) -> Result<Self, OciError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(OciError::InvalidImageRef {
                input: input.to_string(),
                reason: "empty",
            });
        }

        // Split off the digest first so a `:` inside the digest portion (or
        // after it) cannot be misread as a tag separator.
        let (path_and_tag, digest) = match trimmed.split_once('@') {
            Some((before, after)) => (before, Some(Digest::parse(after)?)),
            None => (trimmed, None),
        };

        let (path, tag) = match path_and_tag.rsplit_once(':') {
            // A `:` inside the first segment is part of the registry port,
            // not a tag separator. Detect by checking whether the substring
            // before the colon contains a `/`.
            Some((before, after)) if before.contains('/') => (before, Some(after.to_string())),
            Some((before, after)) if !before.contains('/') && !after.contains('/') => {
                (before, Some(after.to_string()))
            }
            _ => (path_and_tag, None),
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

        // Tag fallback. We retain `latest` as the informational default so
        // the renderer keeps producing the canonical `name:tag` form podman
        // expects, but the F-643 supply-chain check below still rejects
        // the reference if it has no digest AND is not allowlisted.
        let tag = tag.unwrap_or_else(|| "latest".to_string());

        // Supply-chain guard: a reference with no digest is only safe if the
        // *canonical* registry-qualified path matches the first-party
        // allowlist. We canonicalize `docker.io` and the `library/`
        // namespace so a bare `alpine` cannot sneak past by stripping
        // identifiers the allowlist would otherwise see.
        if digest.is_none() {
            let canonical = canonical_path(registry.as_deref(), &name);
            if !TAG_ONLY_ALLOWLIST
                .iter()
                .any(|prefix| canonical.starts_with(prefix))
            {
                return Err(OciError::UntrustedTagOnlyRef {
                    input: input.to_string(),
                });
            }
        }

        Ok(Self {
            registry,
            name,
            tag,
            digest,
        })
    }

    /// Render the reference into the canonical form `podman` accepts.
    /// Digest-pinned refs render as `[registry/]name@sha256:<hex>` so the
    /// runtime fetches and verifies the exact bytes — the tag is dropped
    /// from the wire form because podman uses whichever of `:tag` or
    /// `@digest` comes last and we want the digest to win unambiguously.
    pub fn to_image_string(&self) -> String {
        let path = match &self.registry {
            Some(reg) => format!("{}/{}", reg, self.name),
            None => self.name.clone(),
        };
        match &self.digest {
            Some(d) => format!("{}@{}", path, d.as_str()),
            None => format!("{}:{}", path, self.tag),
        }
    }
}

/// Canonicalize a `(registry, name)` pair into a single comparison key for
/// the [`TAG_ONLY_ALLOWLIST`]. The convention is podman's: missing registry
/// is `docker.io`, missing namespace under docker.io is `library/`.
fn canonical_path(registry: Option<&str>, name: &str) -> String {
    let registry = registry.unwrap_or("docker.io");
    if registry == "docker.io" && !name.contains('/') {
        format!("docker.io/library/{}", name)
    } else {
        format!("{}/{}", registry, name)
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

    /// A digest string did not match the accepted form
    /// (`sha256:<64 lowercase hex chars>`).
    #[error("invalid image digest '{input}': {reason}")]
    InvalidDigest {
        /// Original digest input that failed to parse.
        input: String,
        /// Why it failed.
        reason: &'static str,
    },

    /// The image reference is tag-only and the source is not on the
    /// first-party allowlist. F-643 supply-chain mitigation: a mutable tag
    /// from a third-party registry could be repointed by a registry
    /// compromise and yield code execution inside the Level 2 sandbox.
    /// Pin the image by digest (`name@sha256:<hex>`) instead.
    #[error(
        "image reference '{input}' is tag-only and not on the digest-pin allowlist; \
         pin by digest (e.g. `{input}@sha256:<hex>`) to use this image"
    )]
    UntrustedTagOnlyRef {
        /// Original reference the caller supplied.
        input: String,
    },

    /// Signature / content-trust verification rejected the image before it
    /// was pulled. F-643 supply-chain mitigation.
    #[error("signature verification failed for image '{image}': {reason}")]
    SignatureVerificationFailed {
        /// Image reference that failed verification.
        image: String,
        /// Reason surfaced by the verifier (cosign output, policy mismatch,
        /// etc.).
        reason: String,
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
///
/// The "inherit the runtime default" intent has its own variant
/// ([`NetworkPolicy::RuntimeDefault`]) so callers can express it without
/// reaching for a string sentinel. [`NetworkPolicy::Named`] is reserved
/// for callers that want to pin a specific podman network by name —
/// including the literal string `"default"` if such a network actually
/// exists on the host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NetworkPolicy {
    /// `--network none` — no network namespace beyond loopback.
    None,
    /// Inherit the host network namespace (`--network host`). Reserved for
    /// trusted callers; never the default for Level 2.
    Host,
    /// Inherit whatever network namespace the runtime applies by
    /// default — emit no `--network` flag at all. Used by
    /// [`SecurityOpts::permissive`] so test assertions of the
    /// rendered argv are not perturbed by a network flag.
    RuntimeDefault,
    /// Use a named podman network. The string is passed verbatim to
    /// `--network <name>` — including the literal string `"default"`,
    /// which refers to a podman network of that name and is **not** a
    /// sentinel for "inherit the runtime default" (use
    /// [`NetworkPolicy::RuntimeDefault`] for that).
    Named(String),
}

impl NetworkPolicy {
    /// Render as the value passed after `--network`. Returns `None` for
    /// [`NetworkPolicy::RuntimeDefault`] — that variant means "emit no
    /// `--network` flag at all, inherit the runtime default" — and
    /// `Some(...)` for every other variant including `Named(...)`.
    pub fn as_arg(&self) -> Option<&str> {
        match self {
            NetworkPolicy::None => Some("none"),
            NetworkPolicy::Host => Some("host"),
            NetworkPolicy::RuntimeDefault => None,
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

/// Per-container cgroup-v2 caps applied at create time.
///
/// Every field maps to a `podman create` flag; the units mirror what
/// podman accepts:
///
/// | Field | Flag | Unit |
/// |---|---|---|
/// | `cpus` | `--cpus` | float CPU shares (e.g. `1.5` = 1.5 cores) |
/// | `memory_bytes` | `--memory` | bytes |
/// | `memory_swap_bytes` | `--memory-swap` | bytes; equal to `memory_bytes` disables swap |
/// | `pids_max` | `--pids-limit` | process count |
///
/// `None` on a field means "leave the cap unset" — the container
/// inherits whatever the parent cgroup slice applies. Production
/// callers should reach for [`ContainerLimits::conservative_default`]
/// (the F-654 strict preset: 4 GiB / 2 cpus / 1024 pids, no swap) so
/// a fork-bomb or memory-exhaust workload inside a Level 2 sandbox
/// cannot starve the host.
///
/// Field ordering in [`ContainerLimits::to_create_flags`] is
/// deterministic so argv assertions can pin the rendering:
/// `--cpus` → `--memory` → `--memory-swap` → `--pids-limit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ContainerLimits {
    /// `--cpus N.NN`. Float CPU shares; `1.5` = 1.5 cores. `None`
    /// inherits the slice cap.
    pub cpus: Option<f32>,
    /// `--memory <bytes>`. `None` inherits the slice cap.
    pub memory_bytes: Option<u64>,
    /// `--memory-swap <bytes>`. Setting this equal to `memory_bytes`
    /// disables swap entirely (the conservative default); leaving it
    /// `None` lets podman apply its own default, which is `2 *
    /// memory_bytes`.
    pub memory_swap_bytes: Option<u64>,
    /// `--pids-limit <N>`. Process count cap. `None` inherits.
    pub pids_max: Option<u64>,
}

impl ContainerLimits {
    /// F-654 strict preset: 2 cpus, 4 GiB memory with swap disabled,
    /// 1024 pids. Production callers should pass this to every Level
    /// 2 container so an unbounded workload inside the sandbox cannot
    /// take down the host.
    ///
    /// Tunables live on the [`ContainerLimits`] struct itself —
    /// callers that need more (or less) headroom override the
    /// specific field rather than reaching for an "unlimited" preset.
    pub const fn conservative_default() -> Self {
        const FOUR_GIB: u64 = 4 * 1024 * 1024 * 1024;
        Self {
            cpus: Some(2.0),
            memory_bytes: Some(FOUR_GIB),
            memory_swap_bytes: Some(FOUR_GIB),
            pids_max: Some(1024),
        }
    }

    /// Render into the canonical podman create flag list. Order is
    /// fixed: `--cpus`, `--memory`, `--memory-swap`, `--pids-limit`.
    /// Fields set to `None` emit nothing.
    pub fn to_create_flags(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(cpus) = self.cpus {
            out.push("--cpus".to_string());
            out.push(format_cpus(cpus));
        }
        if let Some(bytes) = self.memory_bytes {
            out.push("--memory".to_string());
            out.push(bytes.to_string());
        }
        if let Some(bytes) = self.memory_swap_bytes {
            out.push("--memory-swap".to_string());
            out.push(bytes.to_string());
        }
        if let Some(pids) = self.pids_max {
            out.push("--pids-limit".to_string());
            out.push(pids.to_string());
        }
        out
    }
}

impl Eq for ContainerLimits {}

impl std::hash::Hash for ContainerLimits {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // f32 has no `Hash`; round-trip via bits so callers that put
        // `SecurityOpts` in a set still work. Two limits compare equal
        // (PartialEq) iff their cpu shares are bit-identical, so this
        // hash is consistent with eq for all values that survive the
        // PartialEq check (NaN doesn't, but it's nonsensical to put a
        // NaN cpu share in here).
        self.cpus.map(f32::to_bits).hash(state);
        self.memory_bytes.hash(state);
        self.memory_swap_bytes.hash(state);
        self.pids_max.hash(state);
    }
}

/// Format CPU shares without a trailing `.0` for integer values
/// (`1.0` → `"1"`) so the resulting podman command line matches the
/// convention `podman` itself emits in `inspect`.
fn format_cpus(cpus: f32) -> String {
    if cpus.fract() == 0.0 {
        format!("{}", cpus.trunc() as i64)
    } else {
        format!("{cpus}")
    }
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
/// container. F-654 also bounds resource usage via [`Self::limits`] so a
/// fork-bomb or memory-exhaust workload inside the sandbox cannot starve
/// the host.
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

    /// Per-container cgroup caps. Default
    /// [`ContainerLimits::conservative_default`] — F-654's strict
    /// preset (2 cpus, 4 GiB memory with no swap, 1024 pids). Callers
    /// that need different headroom should override the specific
    /// field; reaching for [`ContainerLimits::default`] (every field
    /// `None`) silently disables resource enforcement and is what the
    /// permissive preset uses for tests.
    pub limits: ContainerLimits,
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
            limits: ContainerLimits::conservative_default(),
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
            network: NetworkPolicy::RuntimeDefault,
            userns: UserNsPolicy::Default,
            limits: ContainerLimits::default(),
        }
    }

    /// Render into the canonical podman create flag list. The order is
    /// pinned by tests so the exact argv is deterministic:
    ///
    /// 1. `--security-opt no-new-privileges` (if set)
    /// 2. one `--cap-drop <CAP>` per entry in `cap_drop`, in input order
    /// 3. one `--cap-add <CAP>` per entry in `cap_add`, in input order
    /// 4. `--read-only` (if set)
    /// 5. `--network <value>` (emitted for every [`NetworkPolicy`]
    ///    variant except [`NetworkPolicy::RuntimeDefault`], which renders
    ///    nothing so the runtime's own default network is inherited)
    /// 6. `--userns keep-id` (if `KeepId`)
    /// 7. resource limit flags from
    ///    [`ContainerLimits::to_create_flags`] — `--cpus`, `--memory`,
    ///    `--memory-swap`, `--pids-limit`, in that order, each emitted
    ///    only when its field is `Some`.
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
            // RuntimeDefault returns None from `as_arg` and is the
            // explicit "emit no --network flag" variant; every other
            // variant — including a `Named("default")` referring to a
            // podman network actually called `default` — renders here.
            out.push("--network".to_string());
            out.push(value.to_string());
        }
        if matches!(self.userns, UserNsPolicy::KeepId) {
            out.push("--userns".to_string());
            out.push("keep-id".to_string());
        }
        out.extend(self.limits.to_create_flags());
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

    /// Convenience: a 64-char lowercase hex literal for use in tests that
    /// need a syntactically valid sha256 digest.
    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn image_ref_rejects_bare_name_when_not_allowlisted() {
        // F-643: a bare image name resolves to docker.io/library/<name> with
        // tag `latest` — both the name source and the tag are mutable, so we
        // refuse to pull it. Callers must pin by digest.
        let err = ImageRef::parse("alpine").unwrap_err();
        assert!(
            matches!(err, OciError::UntrustedTagOnlyRef { .. }),
            "expected UntrustedTagOnlyRef, got {err:?}"
        );
    }

    #[test]
    fn image_ref_rejects_tag_only_when_not_allowlisted() {
        // F-643: tag-only references are mutable; reject for non-allowlisted
        // sources. End-state callers must supply `@sha256:<hex>`.
        for input in [
            "alpine:3.19",
            "library/alpine:1",
            "docker.io/library/alpine:3.19",
            "quay.io/myorg/myapp:v1",
            "localhost:5000/myapp:dev",
        ] {
            let err = ImageRef::parse(input).unwrap_err();
            assert!(
                matches!(err, OciError::UntrustedTagOnlyRef { .. }),
                "expected UntrustedTagOnlyRef for {input:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn image_ref_accepts_tag_only_for_allowlisted_first_party_source() {
        // F-643: the first-party `oci.io/forge/` namespace is allowlisted so
        // dev/test fixtures can roll tags without forcing every call site to
        // know the latest digest. Production should still pin even
        // allowlisted images by digest.
        let r = ImageRef::parse("oci.io/forge/rust-tools:0.1").unwrap();
        assert_eq!(r.registry.as_deref(), Some("oci.io"));
        assert_eq!(r.name, "forge/rust-tools");
        assert_eq!(r.tag, "0.1");
        assert!(r.digest.is_none());
        assert_eq!(r.to_image_string(), "oci.io/forge/rust-tools:0.1");
    }

    #[test]
    fn image_ref_parses_digest_pinned_form() {
        // F-643: `@sha256:<hex>` is the supply-chain-safe form. It's
        // accepted regardless of source because the digest is the
        // immutable identity of the image bytes.
        let input = format!("docker.io/library/alpine@sha256:{SHA}");
        let r = ImageRef::parse(&input).unwrap();
        assert_eq!(r.registry.as_deref(), Some("docker.io"));
        assert_eq!(r.name, "library/alpine");
        assert_eq!(
            r.digest.as_ref().map(|d| d.as_str()),
            Some(format!("sha256:{SHA}").as_str())
        );
        // Renderer drops the tag on the wire so podman cannot resolve the
        // tag in preference to the digest.
        assert_eq!(
            r.to_image_string(),
            format!("docker.io/library/alpine@sha256:{SHA}")
        );
    }

    #[test]
    fn image_ref_parses_digest_with_informational_tag() {
        // Some operators want a human-readable tag *and* a digest in their
        // config files. The digest is what runs.
        let input = format!("docker.io/library/alpine:3.19@sha256:{SHA}");
        let r = ImageRef::parse(&input).unwrap();
        assert_eq!(r.tag, "3.19");
        assert!(r.digest.is_some());
        assert_eq!(
            r.to_image_string(),
            format!("docker.io/library/alpine@sha256:{SHA}")
        );
    }

    #[test]
    fn image_ref_rejects_malformed_digest() {
        // Bad algorithm
        assert!(matches!(
            ImageRef::parse("alpine@md5:0123456789abcdef0123456789abcdef"),
            Err(OciError::InvalidDigest { .. })
        ));
        // Wrong hex length
        assert!(matches!(
            ImageRef::parse("alpine@sha256:abc"),
            Err(OciError::InvalidDigest { .. })
        ));
        // Uppercase hex (we require canonical lowercase)
        let upper = "ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert!(matches!(
            ImageRef::parse(&format!("alpine@sha256:{upper}")),
            Err(OciError::InvalidDigest { .. })
        ));
        // Missing algorithm prefix
        assert!(matches!(
            ImageRef::parse(&format!("alpine@{SHA}")),
            Err(OciError::InvalidDigest { .. })
        ));
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
    fn digest_round_trip_display() {
        let d = Digest::parse(&format!("sha256:{SHA}")).unwrap();
        assert_eq!(format!("{}", d), format!("sha256:{SHA}"));
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
        let img = ImageRef::parse(&format!("docker.io/library/alpine@sha256:{SHA}")).unwrap();
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
        let img = ImageRef::parse(&format!("docker.io/library/alpine@sha256:{SHA}")).unwrap();
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
        //
        // F-654: the hardened default also bounds resource use via
        // `ContainerLimits::conservative_default` — 2 cpus, 4 GiB
        // memory with swap disabled, 1024 pids. The limit flags
        // render after the security flags, in the order
        // `--cpus`, `--memory`, `--memory-swap`, `--pids-limit`.
        let flags = SecurityOpts::hardened_default().to_create_flags();
        const FOUR_GIB_STR: &str = "4294967296";
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
                "--cpus".to_string(),
                "2".to_string(),
                "--memory".to_string(),
                FOUR_GIB_STR.to_string(),
                "--memory-swap".to_string(),
                FOUR_GIB_STR.to_string(),
                "--pids-limit".to_string(),
                "1024".to_string(),
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

    // ── F-654: ContainerLimits plumbed through SecurityOpts ──────────

    #[test]
    fn container_limits_default_renders_no_flags() {
        // Default = every field None = container inherits the slice's
        // limits. No flags emitted; this is what the permissive
        // preset uses so existing tests aren't perturbed.
        assert!(ContainerLimits::default().to_create_flags().is_empty());
    }

    #[test]
    fn container_limits_renders_in_canonical_order() {
        // `--cpus`, `--memory`, `--memory-swap`, `--pids-limit`. The
        // integration test asserts this exact ordering against
        // `podman inspect`.
        let limits = ContainerLimits {
            cpus: Some(1.5),
            memory_bytes: Some(512 * 1024 * 1024),
            memory_swap_bytes: Some(512 * 1024 * 1024),
            pids_max: Some(256),
        };
        assert_eq!(
            limits.to_create_flags(),
            vec![
                "--cpus".to_string(),
                "1.5".to_string(),
                "--memory".to_string(),
                (512 * 1024 * 1024u64).to_string(),
                "--memory-swap".to_string(),
                (512 * 1024 * 1024u64).to_string(),
                "--pids-limit".to_string(),
                "256".to_string(),
            ]
        );
    }

    #[test]
    fn container_limits_skips_none_fields() {
        // Sparse limits emit only the flags that have a value — no
        // default-inserted flags that would trigger a podman warning
        // on hosts where the controller is missing.
        let limits = ContainerLimits {
            cpus: None,
            memory_bytes: Some(1_000),
            memory_swap_bytes: None,
            pids_max: None,
        };
        assert_eq!(
            limits.to_create_flags(),
            vec!["--memory".to_string(), "1000".to_string()]
        );
    }

    #[test]
    fn container_limits_integer_cpus_have_no_decimal() {
        // `1.0` renders as `"1"` so the rendered argv matches what
        // `podman inspect` reports back to operators.
        let limits = ContainerLimits {
            cpus: Some(1.0),
            ..Default::default()
        };
        assert_eq!(
            limits.to_create_flags(),
            vec!["--cpus".to_string(), "1".to_string()]
        );
    }

    #[test]
    fn conservative_default_caps_match_dod() {
        // F-654 strict preset is documented as 2 cpus / 4 GiB /
        // 1024 pids with swap disabled. Pinning the values here
        // catches accidental relaxation of the DoS-resistance
        // posture.
        let limits = ContainerLimits::conservative_default();
        assert_eq!(limits.cpus, Some(2.0), "default must cap at 2 cpus");
        assert_eq!(
            limits.memory_bytes,
            Some(4 * 1024 * 1024 * 1024),
            "default must cap memory at 4 GiB"
        );
        assert_eq!(
            limits.memory_swap_bytes, limits.memory_bytes,
            "default must disable swap by setting memory-swap == memory"
        );
        assert_eq!(limits.pids_max, Some(1024), "default must cap pids at 1024");
    }

    #[test]
    fn hardened_default_includes_conservative_limits() {
        // Production callers reach for SecurityOpts::hardened_default
        // and must transitively pick up the F-654 caps. If a refactor
        // ever drops `limits` from the hardened preset, the host is
        // back on "rootless slice's default limits" and the DoS
        // surface returns.
        let opts = SecurityOpts::hardened_default();
        assert_eq!(
            opts.limits,
            ContainerLimits::conservative_default(),
            "hardened_default must ship the F-654 conservative limits"
        );
    }

    #[test]
    fn permissive_includes_no_limits() {
        // The permissive preset is for tests that need a clean argv;
        // it must not interleave limit flags into those assertions.
        let opts = SecurityOpts::permissive();
        assert_eq!(opts.limits, ContainerLimits::default());
        let flags = opts.to_create_flags();
        assert!(
            flags.is_empty(),
            "permissive preset must render zero flags, got {flags:?}"
        );
    }

    #[test]
    fn limits_render_after_security_flags() {
        // Order is load-bearing for the audit trail: security flags
        // first, then resource caps. The integration test reads the
        // argv linearly and the operator-facing tracing log relies on
        // the same shape.
        let flags = SecurityOpts::hardened_default().to_create_flags();
        let userns_idx = flags.iter().position(|s| s == "--userns").unwrap();
        let cpus_idx = flags.iter().position(|s| s == "--cpus").unwrap();
        let memory_idx = flags.iter().position(|s| s == "--memory").unwrap();
        let memory_swap_idx = flags.iter().position(|s| s == "--memory-swap").unwrap();
        let pids_idx = flags.iter().position(|s| s == "--pids-limit").unwrap();
        assert!(userns_idx < cpus_idx, "security flags must precede limits");
        assert!(cpus_idx < memory_idx);
        assert!(memory_idx < memory_swap_idx);
        assert!(memory_swap_idx < pids_idx);
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

    // ── #779: NetworkPolicy::RuntimeDefault replaces Named("default") ────

    #[test]
    fn runtime_default_renders_no_network_arg() {
        // RuntimeDefault is the explicit "inherit the podman default
        // network namespace" variant. It must render no `--network`
        // flag so callers that want podman's own default get exactly
        // that without a string sentinel.
        assert_eq!(NetworkPolicy::RuntimeDefault.as_arg(), None);
    }

    #[test]
    fn permissive_uses_runtime_default_variant() {
        // The permissive preset is the canonical caller of
        // RuntimeDefault. Pinning the variant here catches any future
        // refactor that re-introduces a string sentinel.
        let opts = SecurityOpts::permissive();
        assert_eq!(opts.network, NetworkPolicy::RuntimeDefault);
    }

    #[test]
    fn named_default_now_emits_network_flag() {
        // The previous encoding silently swallowed `Named("default")`
        // because it doubled as the sentinel for "emit nothing". With
        // RuntimeDefault carrying that meaning, a real caller naming
        // a podman network "default" must get `--network default`
        // emitted verbatim.
        let opts = SecurityOpts {
            network: NetworkPolicy::Named("default".to_string()),
            ..SecurityOpts::permissive()
        };
        let flags = opts.to_create_flags();
        let net_idx = flags
            .iter()
            .position(|s| s == "--network")
            .expect("--network must be emitted for Named(...)");
        assert_eq!(flags[net_idx + 1], "default");
    }
}

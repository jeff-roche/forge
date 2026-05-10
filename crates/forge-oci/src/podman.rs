//! [`PodmanRuntime`] — first concrete [`crate::ContainerRuntime`] implementation.
//!
//! Shells out to a rootless `podman` binary, structured argv only. No daemon,
//! no `sh -c` invocations, no string concatenation. Every call goes through
//! the [`CommandRunner`] indirection so unit tests can drive the argv-shaping
//! logic without a real binary.

use crate::runner::{CommandOutcome, CommandRunner, TokioCommandRunner};
use crate::signature::{
    enforce_policy, CosignVerifier, NoopVerifier, SignaturePolicy, SignatureVerifier,
};
use crate::{
    ContainerHandle, ContainerLogs, ContainerRuntime, ExecResult, ImageRef, LogLine, OciError,
    SecurityOpts, Stats,
};
use async_trait::async_trait;

/// Default binary name. Resolved via `PATH`.
const PODMAN: &str = "podman";

/// `ContainerRuntime` backed by the rootless `podman` CLI.
///
/// Every [`Self::pull`] call runs the configured [`SignatureVerifier`]
/// before invoking `podman pull`. This is the F-643 supply-chain story:
/// even with digest pinning enforced by [`ImageRef`](crate::ImageRef),
/// signature verification gives operators an attestation that the bytes
/// behind the digest were produced by a trusted signer.
pub struct PodmanRuntime {
    runner: Box<dyn CommandRunner>,
    verifier: Box<dyn SignatureVerifier>,
    policy: SignaturePolicy,
}

impl PodmanRuntime {
    /// Build a runtime that shells out via `tokio::process::Command`. The
    /// runtime starts in [`SignaturePolicy::Permissive`] mode with a
    /// [`CosignVerifier`] — operators get identity-pinned signature
    /// verification by default once they set
    /// [`crate::signature::IDENTITY_ENV`] /
    /// [`crate::signature::OIDC_ENV`]; until then the permissive policy
    /// logs a warning and lets the pull through so dev environments still
    /// function. Production deployments call [`Self::with_verifier`] to
    /// switch to [`SignaturePolicy::Strict`] (or to swap in a custom
    /// verifier).
    ///
    /// **Why CosignVerifier and not NoopVerifier**: a previous default of
    /// [`NoopVerifier`] gave silent zero verification — operators who
    /// forgot to call [`Self::with_verifier`] got no signature gate AND
    /// no warning, ever. The new default ensures every pull either
    /// consults cosign or emits a warning explaining what was skipped.
    pub fn new() -> Self {
        Self {
            runner: Box::new(TokioCommandRunner),
            verifier: Box::new(CosignVerifier::new(SignaturePolicy::Permissive)),
            policy: SignaturePolicy::Permissive,
        }
    }

    /// Build a runtime backed by a custom [`CommandRunner`] — for tests.
    /// Defaults to a no-op verifier so tests that don't care about the
    /// supply-chain gate stay simple; tests that exercise the verification
    /// path call [`Self::with_verifier`] afterwards.
    pub fn with_runner(runner: Box<dyn CommandRunner>) -> Self {
        Self {
            runner,
            verifier: Box::new(NoopVerifier),
            policy: SignaturePolicy::Permissive,
        }
    }

    /// Wire a signature verifier and the policy that governs how its
    /// failures are handled. Returning `self` keeps the call site
    /// composable with [`Self::new`] / [`Self::with_runner`].
    pub fn with_verifier(
        mut self,
        verifier: Box<dyn SignatureVerifier>,
        policy: SignaturePolicy,
    ) -> Self {
        self.verifier = verifier;
        self.policy = policy;
        self
    }

    async fn run_or_fail(&self, args: &[&str]) -> Result<CommandOutcome, OciError> {
        let outcome = self
            .runner
            .run(PODMAN, args)
            .await
            .map_err(|source| OciError::Io {
                tool: PODMAN,
                source,
            })?;
        if !outcome.success() {
            return Err(OciError::CommandFailed {
                tool: PODMAN,
                args: args.iter().map(|s| s.to_string()).collect(),
                exit_code: outcome.exit_code,
                stderr: String::from_utf8_lossy(&outcome.stderr).to_string(),
            });
        }
        Ok(outcome)
    }
}

impl Default for PodmanRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ContainerRuntime for PodmanRuntime {
    /// Probe the host: confirm `podman --version` works AND `podman info`
    /// reports rootless mode is available.
    ///
    /// Returns:
    /// - `Ok(())` if both probes succeed.
    /// - [`OciError::RuntimeMissing`] if the version probe failed to spawn or
    ///   exited non-zero (treat as "podman not installed").
    /// - [`OciError::RuntimeBroken`] if `podman info` exited non-zero — podman
    ///   is installed but its environment is misconfigured (cgroup delegation,
    ///   missing newuidmap, SELinux, etc.).
    /// - [`OciError::RootlessUnavailable`] if `podman info` JSON parsed and
    ///   explicitly reports `host.security.rootless = false`.
    /// - [`OciError::InvalidJson`] if `podman info` JSON didn't parse.
    async fn detect(&self) -> Result<(), OciError> {
        let version = self
            .runner
            .run(PODMAN, &["--version"])
            .await
            .map_err(|_| OciError::RuntimeMissing(PODMAN))?;
        if !version.success() {
            return Err(OciError::RuntimeMissing(PODMAN));
        }

        let info = self
            .runner
            .run(PODMAN, &["info", "--format", "json"])
            .await
            .map_err(|source| OciError::Io {
                tool: PODMAN,
                source,
            })?;
        if !info.success() {
            // podman is installed but its runtime environment is broken.
            // Do NOT collapse this into RootlessUnavailable — that would tell
            // callers "configure rootless" when the real issue is podman
            // itself can't function (cgroup delegation, newuidmap, SELinux).
            return Err(OciError::RuntimeBroken {
                tool: PODMAN,
                stderr: String::from_utf8_lossy(&info.stderr).to_string(),
            });
        }

        let parsed: serde_json::Value =
            serde_json::from_slice(&info.stdout).map_err(|source| OciError::InvalidJson {
                tool: PODMAN,
                subcommand: "info",
                source,
            })?;

        // Drill down with explicit type checks so a malformed payload
        // (e.g. `host` returned as a string, `security` returned as a
        // list, `rootless` returned as a number) fails with a typed
        // `InvalidJson` instead of silently collapsing to "rootless
        // unavailable". Each step distinguishes "key absent" (still a
        // typed error — the schema is load-bearing) from "key present
        // but wrong shape".
        let rootless = extract_rootless(&parsed).map_err(|reason| OciError::InvalidJson {
            tool: PODMAN,
            subcommand: "info",
            source: <serde_json::Error as serde::de::Error>::custom(reason),
        })?;

        if !rootless {
            return Err(OciError::RootlessUnavailable {
                runtime: PODMAN,
                reason: "podman info reports rootless=false".to_string(),
            });
        }

        Ok(())
    }

    async fn pull(&self, image: &ImageRef) -> Result<(), OciError> {
        // F-643: verify the image's signature *before* podman is allowed to
        // write any bytes to the local store. A failed verification under a
        // strict policy aborts the pull entirely; under a permissive policy
        // a missing verifier degrades to a logged warning while a real
        // mismatch still aborts. See `signature::enforce_policy`.
        if let Err(verr) = self.verifier.verify(image).await {
            enforce_policy(image, self.policy, verr)?;
        }
        let img = image.to_image_string();
        match self.run_or_fail(&["pull", &img]).await {
            Ok(_) => {
                tracing::info!(
                    target: "forge_oci::podman",
                    image = %img,
                    "container.pull succeeded"
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    target: "forge_oci::podman",
                    image = %img,
                    error = %e,
                    "container.pull failed"
                );
                Err(e)
            }
        }
    }

    async fn create(
        &self,
        image: &ImageRef,
        argv: &[&str],
        opts: &SecurityOpts,
    ) -> Result<ContainerHandle, OciError> {
        let img = image.to_image_string();
        // `podman create [options] IMAGE [COMMAND [ARG...]]` — podman's
        // argument grammar terminates flag parsing at the IMAGE positional, so
        // every element after `&img` is taken as the in-container command,
        // even tokens beginning with `--`. We deliberately do NOT inject a
        // `--` separator here: doing so makes podman pass the literal `--` to
        // the OCI runtime as the command (crun then errors with
        // "executable file `--` not found"). The flag-injection regression
        // test in this module pins that behaviour by feeding `--privileged`
        // through and asserting podman does not apply it as a runtime flag.
        //
        // F-642: SecurityOpts flags are inserted between `create` and the
        // IMAGE positional so podman parses them as runtime options. Order
        // is pinned by `SecurityOpts::to_create_flags` and asserted by both
        // unit tests in this module and the integration test in
        // `tests/podman_integration.rs`.
        let security_flags = opts.to_create_flags();
        let mut args: Vec<&str> = Vec::with_capacity(2 + security_flags.len() + argv.len());
        args.push("create");
        for flag in &security_flags {
            args.push(flag.as_str());
        }
        args.push(&img);
        args.extend_from_slice(argv);
        let outcome = match self.run_or_fail(&args).await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    target: "forge_oci::podman",
                    image = %img,
                    error = %e,
                    "container.create failed"
                );
                return Err(e);
            }
        };
        let id = String::from_utf8_lossy(&outcome.stdout).trim().to_string();
        if id.is_empty() {
            return Err(OciError::CommandFailed {
                tool: PODMAN,
                args: args.iter().map(|s| s.to_string()).collect(),
                exit_code: outcome.exit_code,
                stderr: "podman create returned empty container id".to_string(),
            });
        }
        // Podman emits a 64-char lowercase hex container id. Reject
        // anything else so a corrupted / wrapper-prefixed stdout cannot
        // leak through to `start` / `exec` / `rm` and cause a
        // hard-to-diagnose podman error or, worse, hit a different
        // container with a coincidentally-matching short id.
        if !is_valid_container_id(&id) {
            return Err(OciError::CommandFailed {
                tool: PODMAN,
                args: args.iter().map(|s| s.to_string()).collect(),
                exit_code: outcome.exit_code,
                stderr: format!(
                    "podman create returned malformed container id (expected 64-char \
                     lowercase hex, got {} chars: {:?})",
                    id.len(),
                    id
                ),
            });
        }
        tracing::info!(
            target: "forge_oci::podman",
            image = %img,
            container_id = %id,
            "container.create succeeded"
        );
        Ok(ContainerHandle::new(id))
    }

    async fn start(&self, handle: &ContainerHandle) -> Result<(), OciError> {
        match self.run_or_fail(&["start", &handle.id]).await {
            Ok(_) => {
                tracing::info!(
                    target: "forge_oci::podman",
                    container_id = %handle.id,
                    "container.start succeeded"
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    target: "forge_oci::podman",
                    container_id = %handle.id,
                    error = %e,
                    "container.start failed"
                );
                Err(e)
            }
        }
    }

    async fn exec(&self, handle: &ContainerHandle, argv: &[&str]) -> Result<ExecResult, OciError> {
        // `podman exec [options] CONTAINER COMMAND [ARG...]` — same positional
        // grammar as `create`: podman stops parsing flags at the CONTAINER
        // positional, so caller-supplied argv elements that begin with `--`
        // are passed straight to the in-container command. We deliberately do
        // NOT inject a `--` separator here for the same reason as `create`:
        // it would be passed verbatim to crun as the command. The
        // flag-injection regression test pins this.
        let mut args: Vec<&str> = vec!["exec", &handle.id];
        args.extend_from_slice(argv);
        // exec captures the inner program's stdout/stderr/exit even on a
        // non-zero exit — that's a meaningful signal, not a runtime failure.
        // So we go around `run_or_fail` here.
        let outcome = match self.runner.run(PODMAN, &args).await {
            Ok(o) => o,
            Err(source) => {
                tracing::warn!(
                    target: "forge_oci::podman",
                    container_id = %handle.id,
                    error = %source,
                    "container.exec spawn failed"
                );
                return Err(OciError::Io {
                    tool: PODMAN,
                    source,
                });
            }
        };
        // exec captures the inner program's stdout/stderr/exit even on a
        // non-zero exit — that's a meaningful signal, not a runtime failure.
        // We log at info regardless so latency attribution stays consistent.
        tracing::info!(
            target: "forge_oci::podman",
            container_id = %handle.id,
            exit_code = ?outcome.exit_code,
            "container.exec completed"
        );
        Ok(ExecResult {
            exit_code: outcome.exit_code,
            stdout: String::from_utf8_lossy(&outcome.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&outcome.stderr).into_owned(),
        })
    }

    async fn stop(&self, handle: &ContainerHandle) -> Result<(), OciError> {
        match self.run_or_fail(&["stop", &handle.id]).await {
            Ok(_) => {
                tracing::info!(
                    target: "forge_oci::podman",
                    container_id = %handle.id,
                    "container.stop succeeded"
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    target: "forge_oci::podman",
                    container_id = %handle.id,
                    error = %e,
                    "container.stop failed"
                );
                Err(e)
            }
        }
    }

    async fn remove(&self, handle: &ContainerHandle) -> Result<(), OciError> {
        // -f forces removal of running containers (podman stop+rm in one step).
        match self.run_or_fail(&["rm", "-f", &handle.id]).await {
            Ok(_) => {
                tracing::info!(
                    target: "forge_oci::podman",
                    container_id = %handle.id,
                    "container.remove succeeded"
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    target: "forge_oci::podman",
                    container_id = %handle.id,
                    error = %e,
                    "container.remove failed"
                );
                Err(e)
            }
        }
    }

    async fn stats(&self, handle: &ContainerHandle) -> Result<Stats, OciError> {
        let outcome = self
            .run_or_fail(&["stats", "--no-stream", "--format", "json", &handle.id])
            .await?;
        // Delegate parsing to the trait method so the podman-specific JSON
        // schema (`cpu_percent`, `mem_usage`, `pids`) is not baked into the
        // lifecycle code. A future runtime emitting different field names or
        // unit conventions would supply its own `parse_stats`; the lifecycle
        // shape (run command → parse blob) stays the same.
        self.parse_stats(&outcome.stdout)
    }

    fn parse_stats(&self, raw: &[u8]) -> Result<Stats, OciError> {
        // Podman emits a JSON array; the first entry is the requested
        // container. Field names and unit conventions are podman's:
        //   - `cpu_percent`: string like `"1.35%"`
        //   - `mem_usage`: string like `"178.3MB / 67.31GB"` (we surface only
        //     the first number)
        //   - `pids`: string or integer
        //
        // We distinguish two cases on each field:
        //   1. **Absent / null** — `Ok(None)` on the matching `Stats`
        //      field. Podman omits fields for containers in transitional
        //      states (just-created, exited), and the `"-- / --"`
        //      placeholder for `mem_usage` is a documented podman convention
        //      we treat the same way.
        //   2. **Present but unparseable** — `Err(OciError::InvalidJson)`.
        //      Surfacing the error means version-skew or schema drift in
        //      podman bubbles up to the caller instead of being silently
        //      reported as "metric missing", which previously hid genuine
        //      breakage behind a `None` that looks identical to "container
        //      starting up".
        let parsed: serde_json::Value =
            serde_json::from_slice(raw).map_err(|source| OciError::InvalidJson {
                tool: PODMAN,
                subcommand: "stats",
                source,
            })?;
        let entry = parsed
            .as_array()
            .and_then(|a| a.first())
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        Ok(Stats {
            cpu_percent: parse_optional_field(&entry, "cpu_percent", parse_cpu_percent_field)?,
            memory_bytes: parse_optional_field(&entry, "mem_usage", parse_mem_usage_field)?,
            pids: parse_optional_field(&entry, "pids", parse_pids_field)?,
        })
    }
}

/// Read an optional Stats field. Absent / `null` → `Ok(None)`. Present
/// → delegate to `parser`, which returns `Ok(Some(_))` for a successful
/// parse, `Ok(None)` for a documented placeholder podman emits while a
/// container is mid-state, and `Err(reason)` for anything else (schema
/// drift, version skew). The `Err` path is wrapped in the same
/// [`OciError::InvalidJson`] variant the surrounding `parse_stats` uses
/// so callers get one typed error covering both "blob is not JSON" and
/// "blob is JSON but doesn't match the podman stats schema".
fn parse_optional_field<T>(
    entry: &serde_json::Value,
    field: &'static str,
    parser: impl FnOnce(&serde_json::Value) -> Result<Option<T>, String>,
) -> Result<Option<T>, OciError> {
    let value = match entry.get(field) {
        None => return Ok(None),
        Some(serde_json::Value::Null) => return Ok(None),
        Some(v) => v,
    };
    parser(value).map_err(|reason| OciError::InvalidJson {
        tool: PODMAN,
        subcommand: "stats",
        source: <serde_json::Error as serde::de::Error>::custom(format!(
            "stats field {field:?}: {reason}"
        )),
    })
}

fn parse_cpu_percent_field(v: &serde_json::Value) -> Result<Option<f64>, String> {
    let Some(s) = v.as_str() else {
        return Err(format!("expected string, got {v}"));
    };
    parse_percent(s).map(Some).ok_or_else(|| {
        format!("could not parse {s:?} as a podman percent string (expected e.g. \"1.35%\")")
    })
}

fn parse_mem_usage_field(v: &serde_json::Value) -> Result<Option<u64>, String> {
    let Some(s) = v.as_str() else {
        return Err(format!("expected string, got {v}"));
    };
    // Podman emits `"-- / --"` for containers mid-transition. That is
    // a documented placeholder, not a schema drift — keep it as
    // `Ok(None)` so callers see "metric missing" rather than an error
    // every time they sample a just-created container.
    if s.trim() == "-- / --" {
        return Ok(None);
    }
    parse_size_first(s).map(Some).ok_or_else(|| {
        format!("could not parse {s:?} as a podman mem_usage string (expected e.g. \"178.3MB / 67.31GB\")")
    })
}

fn parse_pids_field(v: &serde_json::Value) -> Result<Option<u64>, String> {
    match v {
        serde_json::Value::String(s) => s
            .parse::<u64>()
            .map(Some)
            .map_err(|e| format!("could not parse pids string {s:?} as u64: {e}")),
        serde_json::Value::Number(n) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("pids number {n} is not a non-negative u64")),
        other => Err(format!(
            "expected string or non-negative number, got {other}"
        )),
    }
}

/// Extract the `host.security.rootless` boolean from a `podman info --format
/// json` payload with explicit type checks at each step.
///
/// Returns `Err(reason)` if any intermediate node is missing or the wrong
/// shape so the caller can surface a typed `InvalidJson` error. A previous
/// `and_then` chain collapsed every malformed-shape case into "rootless =
/// false" via `unwrap_or(false)`, which masked schema drift behind the
/// existing `RootlessUnavailable` error.
fn extract_rootless(parsed: &serde_json::Value) -> Result<bool, String> {
    let Some(host) = parsed.get("host") else {
        return Err("podman info: missing top-level `host` object".to_string());
    };
    if !host.is_object() {
        return Err(format!(
            "podman info: expected `host` to be an object, got {host}"
        ));
    }
    let Some(security) = host.get("security") else {
        return Err("podman info: missing `host.security` object".to_string());
    };
    if !security.is_object() {
        return Err(format!(
            "podman info: expected `host.security` to be an object, got {security}"
        ));
    }
    let Some(rootless) = security.get("rootless") else {
        return Err("podman info: missing `host.security.rootless` field".to_string());
    };
    rootless.as_bool().ok_or_else(|| {
        format!("podman info: expected `host.security.rootless` to be a boolean, got {rootless}")
    })
}

/// Validate a podman container id from `podman create` stdout. Real
/// podman emits exactly 64 lowercase hex characters (the truncated form
/// `--format '{{.ID}}'` would surface 12 chars, but our argv passes no
/// such flag — we get the full form). Anything else is a schema-drift
/// or wrapper signal; reject so callers see a typed error rather than
/// a downstream `start` / `exec` / `rm` failure against a bogus id.
fn is_valid_container_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[async_trait]
impl ContainerLogs for PodmanRuntime {
    /// Fetch recent stdout+stderr lines from a running container. Shells
    /// out to `podman logs --timestamps [--since <since>] [--tail <n>]
    /// <id>`. Stderr is parsed from the captured stderr pipe; stdout from
    /// the captured stdout pipe. Each pipe is split on newlines into
    /// [`LogLine`] entries with `stream = "stdout"` or `stream = "stderr"`
    /// respectively.
    ///
    /// Order within each pipe is preserved; cross-pipe ordering follows
    /// the timestamp prefix podman emits with `--timestamps` so the UI
    /// can render a single chronological feed.
    async fn logs(
        &self,
        handle: &ContainerHandle,
        since: Option<&str>,
        tail: Option<usize>,
    ) -> Result<Vec<LogLine>, OciError> {
        let mut args: Vec<String> = vec!["logs".to_string(), "--timestamps".to_string()];
        if let Some(s) = since {
            args.push("--since".to_string());
            args.push(s.to_string());
        }
        if let Some(n) = tail {
            args.push("--tail".to_string());
            args.push(n.to_string());
        }
        args.push(handle.id.clone());

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let outcome = self
            .runner
            .run(PODMAN, &arg_refs)
            .await
            .map_err(|source| OciError::Io {
                tool: PODMAN,
                source,
            })?;
        if !outcome.success() {
            return Err(OciError::CommandFailed {
                tool: PODMAN,
                args,
                exit_code: outcome.exit_code,
                stderr: String::from_utf8_lossy(&outcome.stderr).to_string(),
            });
        }
        let mut lines: Vec<LogLine> = Vec::new();
        for raw in String::from_utf8_lossy(&outcome.stdout).lines() {
            lines.push(parse_podman_log_line("stdout", raw));
        }
        // KNOWN LIMITATION: `podman logs` writes container stderr AND its
        // own internal diagnostics (deprecation notices, conmon warnings,
        // etc.) to the same stderr pipe. We can't cheaply tell them apart
        // without a dedicated journald / k8s-file path, so podman-internal
        // lines surface here as `stream = "stderr"` log entries. The UI
        // renders them styled like container stderr — visible noise, but
        // not data loss. Revisit if/when a structured log API lands.
        for raw in String::from_utf8_lossy(&outcome.stderr).lines() {
            lines.push(parse_podman_log_line("stderr", raw));
        }
        // Best-effort chronological merge: sort by timestamp when present,
        // preserving original order within ties (Vec::sort is stable).
        lines.sort_by(|a, b| match (&a.timestamp, &b.timestamp) {
            (Some(at), Some(bt)) => at.cmp(bt),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
        Ok(lines)
    }
}

/// Parse one `podman logs --timestamps` line into a [`LogLine`]. Podman's
/// timestamp prefix is RFC-3339 followed by a single space; everything
/// after the first space is the original line text.
///
/// The first whitespace-delimited token is validated against strict
/// RFC-3339 via [`chrono::DateTime::parse_from_rfc3339`]. If the token
/// is not a valid RFC-3339 timestamp the entire `raw` line is returned
/// as the log message with `timestamp = None` — the previous heuristic
/// (`contains('T') && contains('-')`) would silently misparse any line
/// whose first word happened to contain both characters (e.g. a path
/// like `/etc/foo-bar.conf T=1`) and strip the first token.
fn parse_podman_log_line(stream: &str, raw: &str) -> LogLine {
    if let Some((maybe_ts, rest)) = raw.split_once(' ') {
        if chrono::DateTime::parse_from_rfc3339(maybe_ts).is_ok() {
            return LogLine {
                stream: stream.to_string(),
                line: rest.to_string(),
                timestamp: Some(maybe_ts.to_string()),
            };
        }
    }
    LogLine {
        stream: stream.to_string(),
        line: raw.to_string(),
        timestamp: None,
    }
}

/// Parse a podman percentage string like `"1.35%"` into a float.
fn parse_percent(s: &str) -> Option<f64> {
    s.trim().trim_end_matches('%').trim().parse().ok()
}

/// Parse the *first* size value from a podman `mem_usage` string of the form
/// `"178.3MB / 67.31GB"` into bytes. The second number is the host total,
/// which we don't surface.
///
/// Returns `None` when the input is the podman `"-- / --"` placeholder
/// (mid-state container, expected) **or** when the string shape diverges
/// from what we've seen in the wild. The latter is a podman-version-skew
/// signal — emit a `tracing::debug!` so the log captures the offending
/// payload while still treating the call as "metric missing" rather than
/// failing the whole `stats` invocation.
fn parse_size_first(s: &str) -> Option<u64> {
    fn warn(input: &str, reason: &'static str) -> Option<u64> {
        // Skip the noisy expected case — `"-- / --"` is podman's
        // "container is mid-transition" placeholder, not skew.
        if input.trim() != "-- / --" {
            tracing::debug!(
                target: "forge_oci::podman",
                input = %input,
                reason = %reason,
                "parse_size_first: returning None (possible podman version skew)"
            );
        }
        None
    }
    let Some(first) = s.split('/').next().map(str::trim) else {
        return warn(s, "no '/' separator");
    };
    let Some((num, unit)) = split_number_unit(first) else {
        return warn(s, "no number+unit split");
    };
    let Ok(value): std::result::Result<f64, _> = num.parse() else {
        return warn(s, "number parse failed");
    };
    let multiplier: f64 = match unit.to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "KB" | "K" => 1_000.0,
        "MB" | "M" => 1_000_000.0,
        "GB" | "G" => 1_000_000_000.0,
        "TB" | "T" => 1_000_000_000_000.0,
        "KIB" => 1_024.0,
        "MIB" => 1_024.0 * 1_024.0,
        "GIB" => 1_024.0 * 1_024.0 * 1_024.0,
        "TIB" => 1_024.0 * 1_024.0 * 1_024.0 * 1_024.0,
        _ => return warn(s, "unknown size unit"),
    };
    Some((value * multiplier) as u64)
}

fn split_number_unit(s: &str) -> Option<(&str, &str)> {
    let split = s.find(|c: char| c.is_ascii_alphabetic())?;
    Some((s[..split].trim(), s[split..].trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{RecordingRunner, StubResponse};

    /// 64-char lowercase hex literal — a syntactically valid sha256 digest
    /// for tests that need a digest-pinned [`ImageRef`].
    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn rt(runner: RecordingRunner) -> PodmanRuntime {
        PodmanRuntime::with_runner(Box::new(runner))
    }

    /// Helper: a digest-pinned alpine reference that satisfies the F-643
    /// supply-chain check.
    fn alpine_pinned() -> ImageRef {
        ImageRef::parse(&format!("docker.io/library/alpine@sha256:{SHA}")).unwrap()
    }

    // `ContainerRuntime` is already in scope via `use super::*;` — tests
    // call detect/create/exec/parse_stats through that trait surface so any
    // accidental migration back to inherent methods would fail to link.

    #[tokio::test]
    async fn detect_succeeds_when_version_and_rootless_ok() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"podman version 5.0\n".to_vec()));
        runner.push(StubResponse::ok_stdout(
            br#"{"host":{"security":{"rootless":true}}}"#.to_vec(),
        ));
        let calls = runner.calls.clone();

        rt(runner).detect().await.unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["info", "--format", "json"]);
    }

    #[tokio::test]
    async fn detect_reports_runtime_missing_when_version_fails() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::err(b"not found".to_vec()));

        let err = rt(runner).detect().await.unwrap_err();
        assert!(matches!(err, OciError::RuntimeMissing("podman")));
    }

    #[tokio::test]
    async fn detect_reports_rootless_unavailable_when_info_says_false() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"podman version 5.0\n".to_vec()));
        runner.push(StubResponse::ok_stdout(
            br#"{"host":{"security":{"rootless":false}}}"#.to_vec(),
        ));

        let err = rt(runner).detect().await.unwrap_err();
        assert!(matches!(err, OciError::RootlessUnavailable { .. }));
    }

    #[tokio::test]
    async fn detect_reports_invalid_json() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"podman version 5.0\n".to_vec()));
        runner.push(StubResponse::ok_stdout(b"not json".to_vec()));

        let err = rt(runner).detect().await.unwrap_err();
        assert!(matches!(err, OciError::InvalidJson { .. }));
    }

    #[tokio::test]
    async fn detect_reports_runtime_broken_when_info_exits_nonzero() {
        // podman --version succeeded, but `podman info` failed (cgroup
        // delegation broken, missing newuidmap, etc.). That's not "rootless
        // unavailable" — it's "podman itself is broken". The variant matters
        // because callers render different first-run banners for each.
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"podman version 5.0\n".to_vec()));
        runner.push(StubResponse::err(
            b"Error: cannot setup namespace using newuidmap\n".to_vec(),
        ));

        let err = rt(runner).detect().await.unwrap_err();
        assert!(
            matches!(err, OciError::RuntimeBroken { tool: "podman", .. }),
            "expected RuntimeBroken, got {err:?}"
        );
    }

    #[tokio::test]
    async fn pull_invokes_structured_args() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"".to_vec()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let img = alpine_pinned();
        runtime.pull(&img).await.unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "podman");
        assert_eq!(
            calls[0].1,
            vec!["pull", &format!("docker.io/library/alpine@sha256:{SHA}"),]
        );
    }

    #[tokio::test]
    async fn create_returns_handle_from_stdout() {
        let runner = RecordingRunner::new();
        // Podman emits a 64-char lowercase hex container id. The
        // post-`create` validator requires this exact shape.
        let id_bytes = format!("{SHA}\n");
        runner.push(StubResponse::ok_stdout(id_bytes.into_bytes()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let img = alpine_pinned();
        let h = runtime
            .create(&img, &["echo", "hi"], &SecurityOpts::permissive())
            .await
            .unwrap();
        assert_eq!(h.id, SHA);

        let calls = calls.lock().unwrap();
        // SecurityOpts::permissive emits zero flags, keeping the
        // historical argv shape for tests that pre-date F-642.
        assert_eq!(
            calls[0].1,
            vec![
                "create",
                &format!("docker.io/library/alpine@sha256:{SHA}"),
                "echo",
                "hi",
            ]
        );
    }

    #[tokio::test]
    async fn create_places_caller_argv_strictly_after_image_positional() {
        // Flag-injection guard: caller argv must come *after* the IMAGE
        // positional in podman's grammar (`podman create [options] IMAGE
        // [COMMAND [ARG...]]`). podman stops parsing its own flags at IMAGE,
        // so any `--privileged`-style token in caller argv is treated as the
        // in-container command, not a podman runtime flag. The end-to-end
        // proof of this lives in the `podman_integration` test
        // `create_does_not_apply_caller_flags_as_runtime_flags`; this unit
        // test pins the *positional* invariant the safety story rests on.
        let runner = RecordingRunner::new();
        let id_bytes = format!("{SHA}\n");
        runner.push(StubResponse::ok_stdout(id_bytes.into_bytes()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let img = alpine_pinned();
        runtime
            .create(&img, &["--privileged", "sh"], &SecurityOpts::permissive())
            .await
            .unwrap();

        let argv = calls.lock().unwrap()[0].1.clone();
        let image_str = format!("docker.io/library/alpine@sha256:{SHA}");
        let image_idx = argv
            .iter()
            .position(|a| a == &image_str)
            .expect("argv must contain image positional");
        let first_caller_idx = argv
            .iter()
            .position(|a| a == "--privileged")
            .expect("caller argv element should be present");
        assert!(
            image_idx < first_caller_idx,
            "caller argv must come after IMAGE positional (got {argv:?})"
        );
        // No podman flag (anything starting with `-`) may appear after the
        // IMAGE positional unless it came from caller argv — that's what
        // protects us from accidentally smuggling a runtime flag in.
        let suffix = &argv[image_idx + 1..];
        assert_eq!(
            suffix,
            &["--privileged".to_string(), "sh".to_string()],
            "nothing must be inserted between IMAGE and caller argv (got {argv:?})"
        );
    }

    #[tokio::test]
    async fn create_errors_on_empty_id() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"".to_vec()));
        let runtime = rt(runner);
        let img = alpine_pinned();
        let argv: [&str; 0] = [];
        let err = runtime
            .create(&img, &argv, &SecurityOpts::permissive())
            .await
            .unwrap_err();
        assert!(matches!(err, OciError::CommandFailed { .. }));
    }

    #[tokio::test]
    async fn start_uses_handle_id() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"".to_vec()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        runtime.start(&ContainerHandle::new("xyz")).await.unwrap();

        assert_eq!(calls.lock().unwrap()[0].1, vec!["start", "xyz"]);
    }

    #[tokio::test]
    async fn exec_captures_stdout_and_exit() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse {
            matches_args: None,
            outcome: CommandOutcome {
                exit_code: Some(0),
                stdout: b"hello\n".to_vec(),
                stderr: Vec::new(),
            },
        });
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let res = runtime
            .exec(&ContainerHandle::new("xyz"), &["echo", "hello"])
            .await
            .unwrap();
        assert_eq!(res.stdout, "hello\n");
        assert_eq!(res.exit_code, Some(0));

        assert_eq!(
            calls.lock().unwrap()[0].1,
            vec!["exec", "xyz", "echo", "hello"]
        );
    }

    #[tokio::test]
    async fn exec_places_caller_argv_strictly_after_container_positional() {
        // Same positional-invariant story as `create`: `podman exec [options]
        // CONTAINER COMMAND [ARG...]` — caller argv goes after CONTAINER, so
        // tokens like `--user` are treated as the in-container command, not
        // as `podman exec`'s `--user` flag. End-to-end proof lives in the
        // `podman_integration` test
        // `exec_does_not_apply_caller_flags_as_runtime_flags`.
        let runner = RecordingRunner::new();
        runner.push(StubResponse {
            matches_args: None,
            outcome: CommandOutcome {
                exit_code: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        });
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        runtime
            .exec(&ContainerHandle::new("xyz"), &["--user", "root", "id"])
            .await
            .unwrap();

        let argv = calls.lock().unwrap()[0].1.clone();
        let container_idx = argv
            .iter()
            .position(|a| a == "xyz")
            .expect("argv must contain container id positional");
        let first_caller_idx = argv
            .iter()
            .position(|a| a == "--user")
            .expect("caller argv element should be present");
        assert!(
            container_idx < first_caller_idx,
            "caller argv must come after CONTAINER positional (got {argv:?})"
        );
        let suffix = &argv[container_idx + 1..];
        assert_eq!(
            suffix,
            &["--user".to_string(), "root".to_string(), "id".to_string()],
            "nothing must be inserted between CONTAINER and caller argv (got {argv:?})"
        );
    }

    #[tokio::test]
    async fn exec_surfaces_nonzero_exit_without_failing() {
        // exec'd command exit codes are signal, not runtime failure.
        let runner = RecordingRunner::new();
        runner.push(StubResponse {
            matches_args: None,
            outcome: CommandOutcome {
                exit_code: Some(2),
                stdout: Vec::new(),
                stderr: b"oops\n".to_vec(),
            },
        });

        let res = rt(runner)
            .exec(&ContainerHandle::new("xyz"), &["false"])
            .await
            .unwrap();
        assert_eq!(res.exit_code, Some(2));
        assert_eq!(res.stderr, "oops\n");
    }

    #[tokio::test]
    async fn stop_and_remove_use_force_flag() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"".to_vec()));
        runner.push(StubResponse::ok_stdout(b"".to_vec()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let h = ContainerHandle::new("xyz");
        runtime.stop(&h).await.unwrap();
        runtime.remove(&h).await.unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls[0].1, vec!["stop", "xyz"]);
        assert_eq!(calls[1].1, vec!["rm", "-f", "xyz"]);
    }

    #[tokio::test]
    async fn stats_parses_podman_json() {
        let runner = RecordingRunner::new();
        let json = br#"[
            {"id":"xyz","name":"c","cpu_percent":"1.35%","mem_usage":"178.3MB / 67.31GB","pids":"4"}
        ]"#;
        runner.push(StubResponse::ok_stdout(json.to_vec()));
        let calls = runner.calls.clone();

        let s = rt(runner)
            .stats(&ContainerHandle::new("xyz"))
            .await
            .unwrap();
        assert_eq!(s.cpu_percent, Some(1.35));
        assert_eq!(s.memory_bytes, Some(178_300_000));
        assert_eq!(s.pids, Some(4));

        assert_eq!(
            calls.lock().unwrap()[0].1,
            vec!["stats", "--no-stream", "--format", "json", "xyz"]
        );
    }

    #[tokio::test]
    async fn stats_tolerates_missing_fields() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"[{\"id\":\"xyz\"}]".to_vec()));
        let s = rt(runner)
            .stats(&ContainerHandle::new("xyz"))
            .await
            .unwrap();
        assert_eq!(s.cpu_percent, None);
        assert_eq!(s.memory_bytes, None);
        assert_eq!(s.pids, None);
    }

    #[tokio::test]
    async fn stats_invalid_json_surfaces_typed_error() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"not json".to_vec()));
        let err = rt(runner)
            .stats(&ContainerHandle::new("xyz"))
            .await
            .unwrap_err();
        assert!(matches!(err, OciError::InvalidJson { .. }));
    }

    // ── F-680: trait-level stats parsing ─────────────────────────────

    #[test]
    fn parse_stats_handles_podman_json_payload() {
        // F-680: `parse_stats` is a trait method — the podman-specific
        // schema (`cpu_percent`, `mem_usage`, `pids` plus their string
        // formats) is owned by `PodmanRuntime`'s impl, not by free
        // helpers in the lifecycle path. Future runtimes will supply
        // their own `parse_stats` translating their own JSON shape into
        // the same `Stats` struct.
        let runtime = PodmanRuntime::new();
        let json = br#"[{"id":"xyz","cpu_percent":"2.5%","mem_usage":"64MB / 1GB","pids":"7"}]"#;
        let stats = runtime.parse_stats(json).unwrap();
        assert_eq!(stats.cpu_percent, Some(2.5));
        assert_eq!(stats.memory_bytes, Some(64_000_000));
        assert_eq!(stats.pids, Some(7));
    }

    #[test]
    fn parse_stats_callable_through_trait_object() {
        // Pinning the dyn-trait callability separately so a future
        // refactor cannot accidentally make `parse_stats` an inherent
        // method — that would defeat the whole abstraction.
        let runtime: Box<dyn ContainerRuntime> = Box::new(PodmanRuntime::new());
        let json = br#"[{"id":"x"}]"#;
        let stats = runtime.parse_stats(json).unwrap();
        assert!(stats.cpu_percent.is_none());
    }

    #[test]
    fn parse_stats_surfaces_invalid_json_as_typed_error() {
        let runtime = PodmanRuntime::new();
        let err = runtime.parse_stats(b"not json").unwrap_err();
        assert!(matches!(err, OciError::InvalidJson { tool: "podman", .. }));
    }

    // ── F-680: detect on the trait surface ───────────────────────────

    #[tokio::test]
    async fn detect_callable_through_trait_object() {
        // F-680: `detect` is part of the trait. Callers that want to
        // probe a runtime without knowing the concrete type call it
        // through `&dyn ContainerRuntime`.
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"podman version 5.0\n".to_vec()));
        runner.push(StubResponse::ok_stdout(
            br#"{"host":{"security":{"rootless":true}}}"#.to_vec(),
        ));
        let runtime: Box<dyn ContainerRuntime> = Box::new(rt(runner));
        runtime.detect().await.unwrap();
    }

    #[tokio::test]
    async fn command_failure_surfaces_typed_error() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::err(b"image not found\n".to_vec()));
        // Use a digest-pinned ref so the pull reaches `podman pull`; the
        // failure under test is the runtime's exit, not the supply-chain
        // gate.
        let img = alpine_pinned();
        let err = rt(runner).pull(&img).await.unwrap_err();
        assert!(matches!(
            err,
            OciError::CommandFailed { tool: "podman", .. }
        ));
    }

    // ── F-643: signature verification wired into pull ────────────────

    /// Test verifier that always rejects with a configurable variant.
    struct RejectingVerifier(crate::signature::VerificationError);

    #[async_trait]
    impl SignatureVerifier for RejectingVerifier {
        async fn verify(
            &self,
            _image: &ImageRef,
        ) -> Result<(), crate::signature::VerificationError> {
            Err(match &self.0 {
                crate::signature::VerificationError::Mismatch(s) => {
                    crate::signature::VerificationError::Mismatch(s.clone())
                }
                crate::signature::VerificationError::VerifierUnavailable(s) => {
                    crate::signature::VerificationError::VerifierUnavailable(s.clone())
                }
                crate::signature::VerificationError::Io(s) => {
                    crate::signature::VerificationError::Io(s.clone())
                }
            })
        }
    }

    #[tokio::test]
    async fn pull_runs_verifier_before_invoking_podman() {
        // Load-bearing: verification must happen *before* podman writes
        // anything to the local store. We assert ordering by checking the
        // verifier observed a call and the rejection short-circuits the
        // runner — no `podman pull` invocation reaches the recording stub.
        struct CountingVerifier {
            count: std::sync::atomic::AtomicUsize,
        }
        #[async_trait]
        impl SignatureVerifier for CountingVerifier {
            async fn verify(
                &self,
                _image: &ImageRef,
            ) -> Result<(), crate::signature::VerificationError> {
                self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(crate::signature::VerificationError::Mismatch(
                    "no signature".to_string(),
                ))
            }
        }
        let verifier = Box::new(CountingVerifier {
            count: std::sync::atomic::AtomicUsize::new(0),
        });
        let runner = RecordingRunner::new();
        // Push a happy-path response to prove it would have been used if
        // the verifier had not blocked — the test then asserts the runner
        // was *not* invoked.
        runner.push(StubResponse::ok_stdout(b"".to_vec()));
        let calls = runner.calls.clone();
        let runtime = rt(runner).with_verifier(verifier, SignaturePolicy::Strict);

        let err = runtime.pull(&alpine_pinned()).await.unwrap_err();
        assert!(
            matches!(err, OciError::SignatureVerificationFailed { .. }),
            "expected SignatureVerificationFailed, got {err:?}"
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            0,
            "podman pull must not run when signature verification fails"
        );
    }

    #[tokio::test]
    async fn pull_strict_policy_blocks_on_missing_verifier() {
        // F-643: in strict mode, even a missing cosign binary blocks the
        // pull. Operators must install the verifier to opt into Level 2.
        let verifier = Box::new(RejectingVerifier(
            crate::signature::VerificationError::VerifierUnavailable("missing".to_string()),
        ));
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"".to_vec()));
        let runtime = rt(runner).with_verifier(verifier, SignaturePolicy::Strict);
        let err = runtime.pull(&alpine_pinned()).await.unwrap_err();
        assert!(matches!(err, OciError::SignatureVerificationFailed { .. }));
    }

    #[tokio::test]
    async fn pull_permissive_policy_proceeds_when_verifier_unavailable() {
        // F-643: permissive mode permits the pull when cosign is not
        // installed, so dev environments still function. A real signature
        // mismatch is still fatal — covered by the next test.
        let verifier = Box::new(RejectingVerifier(
            crate::signature::VerificationError::VerifierUnavailable("missing".to_string()),
        ));
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"".to_vec()));
        let calls = runner.calls.clone();
        let runtime = rt(runner).with_verifier(verifier, SignaturePolicy::Permissive);
        runtime.pull(&alpine_pinned()).await.unwrap();
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "podman pull must run when permissive policy lets the missing verifier through"
        );
    }

    /// Test verifier that records every call so we can assert the
    /// default-constructed runtime actually wires a verifier in.
    struct CountingNoopVerifier {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl SignatureVerifier for CountingNoopVerifier {
        async fn verify(
            &self,
            _image: &ImageRef,
        ) -> Result<(), crate::signature::VerificationError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn new_defaults_to_a_real_verifier_not_noop() {
        // Regression for F-643 follow-up: `PodmanRuntime::new()` previously
        // wired `NoopVerifier`, so an operator who forgot to call
        // `with_verifier` got ZERO signature verification and no warning
        // ever logged. The new default is `CosignVerifier::new(Permissive)`
        // — pulls without identity env vars set are warned about, not
        // silently waved through.
        //
        // Asserting this through behaviour: the type-level default must
        // not return `Ok(())` for an arbitrary pull without producing
        // either a verification call or a permissive warning. We can't
        // assert tracing output cheaply, so we instead pin the
        // construction by checking the verifier is NOT a `NoopVerifier`
        // — concretely, by exercising the env-missing path which a real
        // CosignVerifier rejects with `VerifierUnavailable`.
        //
        // Use a SINGLE-threaded runtime for this test so the env mutation
        // doesn't race with sibling tests.
        use crate::signature::{IDENTITY_ENV, OIDC_ENV};
        // Snapshot/restore env to be polite to other tests.
        let prev_id = std::env::var(IDENTITY_ENV).ok();
        let prev_oidc = std::env::var(OIDC_ENV).ok();
        std::env::remove_var(IDENTITY_ENV);
        std::env::remove_var(OIDC_ENV);

        let runtime = PodmanRuntime::new();
        // Delegate verification through the public verifier accessor: we
        // call the verifier directly so the test doesn't need a real
        // podman on PATH.
        let img = alpine_pinned();
        let res = runtime.verifier.verify(&img).await;

        // Restore env.
        match prev_id {
            Some(v) => std::env::set_var(IDENTITY_ENV, v),
            None => std::env::remove_var(IDENTITY_ENV),
        }
        match prev_oidc {
            Some(v) => std::env::set_var(OIDC_ENV, v),
            None => std::env::remove_var(OIDC_ENV),
        }

        match res {
            Err(crate::signature::VerificationError::VerifierUnavailable(_)) => {}
            other => panic!(
                "expected default verifier to be CosignVerifier (returns VerifierUnavailable when \
                 identity env is unset); got {other:?} — has the default regressed back to NoopVerifier?"
            ),
        }
    }

    #[tokio::test]
    async fn with_verifier_noop_is_still_available_for_tests() {
        // NoopVerifier must remain a valid override so tests that don't
        // care about the signature gate stay terse. The new default for
        // production is CosignVerifier — but `.with_verifier(NoopVerifier)`
        // explicitly opts out, making the test intent visible.
        let counter = Box::new(CountingNoopVerifier {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"".to_vec()));
        let runtime = rt(runner).with_verifier(counter, SignaturePolicy::Permissive);
        runtime.pull(&alpine_pinned()).await.unwrap();
    }

    #[tokio::test]
    async fn pull_permissive_policy_still_blocks_on_signature_mismatch() {
        // The permissive escape hatch is for *missing tooling*, not for
        // bad signatures. A real mismatch must always block.
        let verifier = Box::new(RejectingVerifier(
            crate::signature::VerificationError::Mismatch("bad sig".to_string()),
        ));
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"".to_vec()));
        let runtime = rt(runner).with_verifier(verifier, SignaturePolicy::Permissive);
        let err = runtime.pull(&alpine_pinned()).await.unwrap_err();
        assert!(matches!(err, OciError::SignatureVerificationFailed { .. }));
    }

    #[test]
    fn parse_size_first_handles_bytes() {
        assert_eq!(parse_size_first("178.3MB / 67.31GB"), Some(178_300_000));
        assert_eq!(parse_size_first("2.253MB / 67.31GB"), Some(2_253_000));
        assert_eq!(parse_size_first("512B / 1GB"), Some(512));
        assert_eq!(parse_size_first("1MiB / 1GiB"), Some(1_048_576));
    }

    #[test]
    fn parse_size_first_returns_none_for_podman_placeholder() {
        // Podman emits `"-- / --"` for `mem_usage` on a container that's mid-
        // transition. We treat the metric as missing rather than failing the
        // whole `stats` call.
        assert_eq!(parse_size_first("-- / --"), None);
    }

    #[test]
    fn parse_size_first_handles_zero_byte_value() {
        assert_eq!(parse_size_first("0B / 67.31GB"), Some(0));
    }

    // ── F-597: ContainerLogs for PodmanRuntime ────────────────────────

    #[tokio::test]
    async fn logs_invokes_structured_args_with_timestamps() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(
            b"2025-04-26T10:00:00Z hello\n2025-04-26T10:00:01Z world\n".to_vec(),
        ));
        let calls = runner.calls.clone();
        let runtime = rt(runner);
        let h = ContainerHandle::new("abc123");
        let lines = runtime.logs(&h, None, None).await.unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "podman");
        assert_eq!(
            calls[0].1,
            vec![
                "logs".to_string(),
                "--timestamps".to_string(),
                "abc123".to_string()
            ]
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].stream, "stdout");
        assert_eq!(lines[0].line, "hello");
        assert_eq!(lines[0].timestamp.as_deref(), Some("2025-04-26T10:00:00Z"));
        assert_eq!(lines[1].line, "world");
    }

    #[tokio::test]
    async fn logs_passes_since_and_tail_flags() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"".to_vec()));
        let calls = runner.calls.clone();
        let runtime = rt(runner);
        let h = ContainerHandle::new("abc123");
        runtime
            .logs(&h, Some("2025-04-26T10:00:00Z"), Some(50))
            .await
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[0].1,
            vec![
                "logs".to_string(),
                "--timestamps".to_string(),
                "--since".to_string(),
                "2025-04-26T10:00:00Z".to_string(),
                "--tail".to_string(),
                "50".to_string(),
                "abc123".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn logs_separates_stdout_and_stderr_streams() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse {
            matches_args: None,
            outcome: CommandOutcome {
                exit_code: Some(0),
                stdout: b"2025-04-26T10:00:00Z out-line\n".to_vec(),
                stderr: b"2025-04-26T10:00:01Z err-line\n".to_vec(),
            },
        });
        let runtime = rt(runner);
        let h = ContainerHandle::new("abc123");
        let lines = runtime.logs(&h, None, None).await.unwrap();
        assert_eq!(lines.len(), 2);
        // sorted by timestamp; out < err
        assert_eq!(lines[0].stream, "stdout");
        assert_eq!(lines[0].line, "out-line");
        assert_eq!(lines[1].stream, "stderr");
        assert_eq!(lines[1].line, "err-line");
    }

    #[tokio::test]
    async fn logs_surfaces_command_failure() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::err(b"no such container\n".to_vec()));
        let runtime = rt(runner);
        let h = ContainerHandle::new("ghost");
        let err = runtime.logs(&h, None, None).await.unwrap_err();
        assert!(matches!(err, OciError::CommandFailed { .. }));
    }

    #[test]
    fn parse_podman_log_line_extracts_timestamp() {
        let l = parse_podman_log_line("stdout", "2025-04-26T10:00:00Z hello world");
        assert_eq!(l.stream, "stdout");
        assert_eq!(l.line, "hello world");
        assert_eq!(l.timestamp.as_deref(), Some("2025-04-26T10:00:00Z"));
    }

    // ── F-642: SecurityOpts plumbed through `create` ─────────────────

    #[tokio::test]
    async fn create_emits_every_hardened_default_flag_before_image() {
        // Load-bearing: the F-642 DoD says PodmanRuntime::create must
        // inject no-new-privileges + cap-drop ALL + restricted network
        // + read-only rootfs by default. This test pins the exact argv
        // shape so a regression that drops a flag (or moves it past
        // the IMAGE positional, where podman would treat it as the
        // in-container command) fails the test loudly.
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(format!("{SHA}\n").into_bytes()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let img = alpine_pinned();
        runtime
            .create(
                &img,
                &["sleep", "infinity"],
                &SecurityOpts::hardened_default(),
            )
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        let argv = &calls[0].1;
        // Every hardening flag must be present before the IMAGE positional.
        let image_str = format!("docker.io/library/alpine@sha256:{SHA}");
        let image_idx = argv
            .iter()
            .position(|a| a == &image_str)
            .expect("argv must contain image positional");
        let prefix = &argv[..image_idx];
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
                prefix.iter().any(|a| a == required),
                "missing hardening flag {required:?} in {argv:?}"
            );
        }
        // Caller argv lands strictly after IMAGE.
        let suffix = &argv[image_idx + 1..];
        assert_eq!(suffix, &["sleep".to_string(), "infinity".to_string()]);
    }

    #[tokio::test]
    async fn create_renders_security_flags_in_canonical_order() {
        // Pinning the exact rendered prefix so operators reading the
        // audit log see a stable shape and so reorderings can't sneak
        // through review. The order is documented on
        // `SecurityOpts::to_create_flags`.
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(format!("{SHA}\n").into_bytes()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let img = alpine_pinned();
        runtime
            .create(
                &img,
                &["sleep", "infinity"],
                &SecurityOpts::hardened_default(),
            )
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        let argv = &calls[0].1;
        // F-654: hardened_default carries the conservative cgroup
        // caps (2 cpus, 4 GiB, 1024 pids, no swap) which render after
        // the security flags and before the IMAGE positional.
        const FOUR_GIB_STR: &str = "4294967296";
        assert_eq!(
            argv,
            &vec![
                "create".to_string(),
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
                format!("docker.io/library/alpine@sha256:{SHA}"),
                "sleep".to_string(),
                "infinity".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn create_with_permissive_opts_emits_no_security_flags() {
        // Regression guard for the test-only `permissive` preset:
        // every test in this module that calls `create` with
        // SecurityOpts::permissive expects a clean argv with no
        // hardening flags interleaved.
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(format!("{SHA}\n").into_bytes()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let img = alpine_pinned();
        runtime
            .create(&img, &["sh"], &SecurityOpts::permissive())
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[0].1,
            vec![
                "create",
                &format!("docker.io/library/alpine@sha256:{SHA}"),
                "sh",
            ]
        );
    }

    #[test]
    fn parse_podman_log_line_handles_no_timestamp() {
        let l = parse_podman_log_line("stderr", "plain output");
        assert_eq!(l.stream, "stderr");
        assert_eq!(l.line, "plain output");
        assert_eq!(l.timestamp, None);
    }

    // ── #702: strict RFC-3339 in parse_podman_log_line ───────────────

    #[test]
    fn parse_podman_log_line_rejects_non_rfc3339_first_token() {
        // The old heuristic accepted any first token containing both
        // 'T' and '-', so a path like `/etc/foo-bar.conf` followed by
        // text was misread as a timestamp and the rest of the line was
        // silently truncated. With strict RFC-3339 the whole line is
        // preserved as the message.
        let l = parse_podman_log_line("stdout", "/etc/foo-bar.conf T=1 some message");
        assert_eq!(l.stream, "stdout");
        assert_eq!(l.line, "/etc/foo-bar.conf T=1 some message");
        assert_eq!(l.timestamp, None);
    }

    #[test]
    fn parse_podman_log_line_accepts_real_rfc3339_with_offset() {
        // RFC-3339 admits both 'Z' and explicit offsets — both must
        // parse cleanly so podman emissions across host TZs round-trip.
        let l = parse_podman_log_line("stdout", "2025-04-26T10:00:00+02:00 hello");
        assert_eq!(l.line, "hello");
        assert_eq!(l.timestamp.as_deref(), Some("2025-04-26T10:00:00+02:00"));
    }

    #[test]
    fn parse_podman_log_line_treats_almost_rfc3339_as_message() {
        // Looks similar to RFC-3339 (has 'T' and '-') but is missing
        // the timezone — chrono::parse_from_rfc3339 rejects this, so
        // the whole token is treated as part of the message.
        let l = parse_podman_log_line("stdout", "2025-04-26T10:00:00 hello");
        assert_eq!(l.line, "2025-04-26T10:00:00 hello");
        assert_eq!(l.timestamp, None);
    }

    // ── #702: type-checked detect JSON drill-down ────────────────────

    #[tokio::test]
    async fn detect_reports_invalid_json_when_host_is_wrong_type() {
        // `host` is a string instead of an object. The old `.and_then`
        // chain swallowed this and collapsed to `rootless=false`,
        // surfacing as a misleading `RootlessUnavailable`. The new code
        // surfaces it as `InvalidJson` — schema drift, not "rootless
        // off".
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"podman version 5.0\n".to_vec()));
        runner.push(StubResponse::ok_stdout(
            br#"{"host":"not an object"}"#.to_vec(),
        ));
        let err = rt(runner).detect().await.unwrap_err();
        assert!(
            matches!(
                err,
                OciError::InvalidJson {
                    tool: "podman",
                    subcommand: "info",
                    ..
                }
            ),
            "expected InvalidJson, got {err:?}"
        );
    }

    #[tokio::test]
    async fn detect_reports_invalid_json_when_security_is_wrong_type() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"podman version 5.0\n".to_vec()));
        runner.push(StubResponse::ok_stdout(
            br#"{"host":{"security":["unexpected","array"]}}"#.to_vec(),
        ));
        let err = rt(runner).detect().await.unwrap_err();
        assert!(matches!(
            err,
            OciError::InvalidJson {
                tool: "podman",
                subcommand: "info",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn detect_reports_invalid_json_when_rootless_is_not_bool() {
        // `rootless` is a string, not a boolean — the old code
        // collapsed this to `false` and reported `RootlessUnavailable`.
        // The new code reports schema drift.
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"podman version 5.0\n".to_vec()));
        runner.push(StubResponse::ok_stdout(
            br#"{"host":{"security":{"rootless":"true"}}}"#.to_vec(),
        ));
        let err = rt(runner).detect().await.unwrap_err();
        assert!(matches!(
            err,
            OciError::InvalidJson {
                tool: "podman",
                subcommand: "info",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn detect_reports_invalid_json_when_rootless_key_absent() {
        // The key is structurally missing entirely. This is also schema
        // drift relative to the documented podman `info` payload, so
        // surface it as InvalidJson rather than silently collapsing to
        // "rootless unavailable".
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"podman version 5.0\n".to_vec()));
        runner.push(StubResponse::ok_stdout(
            br#"{"host":{"security":{}}}"#.to_vec(),
        ));
        let err = rt(runner).detect().await.unwrap_err();
        assert!(matches!(
            err,
            OciError::InvalidJson {
                tool: "podman",
                subcommand: "info",
                ..
            }
        ));
    }

    // ── #702: container ID format validation in `create` ─────────────

    #[test]
    fn is_valid_container_id_accepts_64_char_lowercase_hex() {
        // 64 lowercase hex chars — what real podman emits.
        assert!(is_valid_container_id(SHA));
    }

    #[test]
    fn is_valid_container_id_rejects_short_id() {
        assert!(!is_valid_container_id("abc1234"));
        // Truncated 12-char form podman would emit with --format '{{.ID}}'
        // (we don't pass that flag, so we reject it here defensively).
        assert!(!is_valid_container_id("abc1234deadbe"));
    }

    #[test]
    fn is_valid_container_id_rejects_uppercase_hex() {
        // Podman's container ids are lowercase. An uppercase id is a
        // wrapper / parsing artefact, not real podman output.
        let upper: String = SHA.chars().map(|c| c.to_ascii_uppercase()).collect();
        assert!(!is_valid_container_id(&upper));
    }

    #[test]
    fn is_valid_container_id_rejects_non_hex_chars() {
        // 64 chars but contains a non-hex character.
        let mut bad = String::from(SHA);
        bad.replace_range(0..1, "g");
        assert_eq!(bad.len(), 64);
        assert!(!is_valid_container_id(&bad));
    }

    #[tokio::test]
    async fn create_rejects_malformed_container_id_from_podman_stdout() {
        // `podman create` returned a short / non-hex id. The new
        // validator must surface this as CommandFailed rather than
        // hand it back as a usable handle that would then misbehave
        // in `start` / `exec` / `rm`.
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"not-a-container-id\n".to_vec()));
        let runtime = rt(runner);
        let img = alpine_pinned();
        let err = runtime
            .create(&img, &["sh"], &SecurityOpts::permissive())
            .await
            .unwrap_err();
        match err {
            OciError::CommandFailed { stderr, .. } => {
                assert!(
                    stderr.contains("malformed container id"),
                    "expected malformed-id message, got {stderr:?}"
                );
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_rejects_container_id_with_uppercase_hex() {
        // 64 chars but uppercase — defensive rejection so wrapper /
        // schema-drift sources surface immediately.
        let upper_id: String = SHA.chars().map(|c| c.to_ascii_uppercase()).collect();
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(
            format!("{upper_id}\n").into_bytes(),
        ));
        let runtime = rt(runner);
        let img = alpine_pinned();
        let err = runtime
            .create(&img, &["sh"], &SecurityOpts::permissive())
            .await
            .unwrap_err();
        assert!(matches!(err, OciError::CommandFailed { .. }));
    }

    // ── #702: Stats parsing — Err vs absent ──────────────────────────

    #[test]
    fn parse_stats_returns_none_for_absent_fields() {
        // Container is mid-transition: podman emits the entry with no
        // metrics yet. `None` is the correct surface — caller renders
        // "metric pending" rather than "metric broken".
        let runtime = PodmanRuntime::new();
        let stats = runtime.parse_stats(b"[{\"id\":\"xyz\"}]").unwrap();
        assert_eq!(stats.cpu_percent, None);
        assert_eq!(stats.memory_bytes, None);
        assert_eq!(stats.pids, None);
    }

    #[test]
    fn parse_stats_returns_none_for_explicit_null_fields() {
        // Same shape as "absent", but the field is present with `null`.
        // Podman has emitted both forms across versions; both are the
        // documented "no data yet" signal, not an error.
        let runtime = PodmanRuntime::new();
        let stats = runtime
            .parse_stats(br#"[{"cpu_percent":null,"mem_usage":null,"pids":null}]"#)
            .unwrap();
        assert_eq!(stats.cpu_percent, None);
        assert_eq!(stats.memory_bytes, None);
        assert_eq!(stats.pids, None);
    }

    #[test]
    fn parse_stats_treats_mem_usage_placeholder_as_none() {
        // Documented podman placeholder for mid-state containers.
        // Predates this change; preserved so the dashboards continue to
        // render "metric pending" instead of an error every time a
        // container is just-starting.
        let runtime = PodmanRuntime::new();
        let stats = runtime
            .parse_stats(br#"[{"mem_usage":"-- / --"}]"#)
            .unwrap();
        assert_eq!(stats.memory_bytes, None);
    }

    #[test]
    fn parse_stats_errors_on_unparseable_cpu_percent_string() {
        // Field is present but unparseable — schema drift, not "no
        // data". The old code returned `None` here, masking the drift.
        let runtime = PodmanRuntime::new();
        let err = runtime
            .parse_stats(br#"[{"cpu_percent":"banana"}]"#)
            .unwrap_err();
        assert!(matches!(
            err,
            OciError::InvalidJson {
                tool: "podman",
                subcommand: "stats",
                ..
            }
        ));
    }

    #[test]
    fn parse_stats_errors_on_cpu_percent_wrong_type() {
        // Field is the wrong JSON type entirely (number instead of
        // string). Surface as a typed error so the caller can log /
        // alert on schema skew rather than silently dropping a metric.
        let runtime = PodmanRuntime::new();
        let err = runtime
            .parse_stats(br#"[{"cpu_percent":1.35}]"#)
            .unwrap_err();
        assert!(matches!(
            err,
            OciError::InvalidJson {
                tool: "podman",
                subcommand: "stats",
                ..
            }
        ));
    }

    #[test]
    fn parse_stats_errors_on_unparseable_mem_usage_string() {
        // String shape diverges from `"178.3MB / 67.31GB"` and is not
        // the `"-- / --"` placeholder — schema drift.
        let runtime = PodmanRuntime::new();
        let err = runtime
            .parse_stats(br#"[{"mem_usage":"definitely not a size"}]"#)
            .unwrap_err();
        assert!(matches!(
            err,
            OciError::InvalidJson {
                tool: "podman",
                subcommand: "stats",
                ..
            }
        ));
    }

    #[test]
    fn parse_stats_errors_on_unparseable_pids_string() {
        let runtime = PodmanRuntime::new();
        let err = runtime
            .parse_stats(br#"[{"pids":"not a number"}]"#)
            .unwrap_err();
        assert!(matches!(
            err,
            OciError::InvalidJson {
                tool: "podman",
                subcommand: "stats",
                ..
            }
        ));
    }

    #[test]
    fn parse_stats_errors_on_pids_wrong_type() {
        // pids as an array — wrong type entirely.
        let runtime = PodmanRuntime::new();
        let err = runtime.parse_stats(br#"[{"pids":[1,2]}]"#).unwrap_err();
        assert!(matches!(
            err,
            OciError::InvalidJson {
                tool: "podman",
                subcommand: "stats",
                ..
            }
        ));
    }
}

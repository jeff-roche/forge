//! [`PodmanRuntime`] — first concrete [`crate::ContainerRuntime`] implementation.
//!
//! Shells out to a rootless `podman` binary, structured argv only. No daemon,
//! no `sh -c` invocations, no string concatenation. Every call goes through
//! the [`CommandRunner`] indirection so unit tests can drive the argv-shaping
//! logic without a real binary.

use crate::runner::{CommandOutcome, CommandRunner, TokioCommandRunner};
use crate::{
    ContainerHandle, ContainerLogs, ContainerRuntime, ExecResult, ImageRef, LogLine, OciError,
    SecurityOpts, Stats,
};
use async_trait::async_trait;

/// Default binary name. Resolved via `PATH`.
const PODMAN: &str = "podman";

/// `ContainerRuntime` backed by the rootless `podman` CLI.
pub struct PodmanRuntime {
    runner: Box<dyn CommandRunner>,
}

impl PodmanRuntime {
    /// Build a runtime that shells out via `tokio::process::Command`.
    pub fn new() -> Self {
        Self {
            runner: Box::new(TokioCommandRunner),
        }
    }

    /// Build a runtime backed by a custom [`CommandRunner`] — for tests.
    pub fn with_runner(runner: Box<dyn CommandRunner>) -> Self {
        Self { runner }
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

        let rootless = parsed
            .get("host")
            .and_then(|h| h.get("security"))
            .and_then(|s| s.get("rootless"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !rootless {
            return Err(OciError::RootlessUnavailable {
                runtime: PODMAN,
                reason: "podman info reports rootless=false".to_string(),
            });
        }

        Ok(())
    }

    async fn pull(&self, image: &ImageRef) -> Result<(), OciError> {
        let img = image.to_image_string();
        self.run_or_fail(&["pull", &img]).await?;
        Ok(())
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
        let outcome = self.run_or_fail(&args).await?;
        let id = String::from_utf8_lossy(&outcome.stdout).trim().to_string();
        if id.is_empty() {
            return Err(OciError::CommandFailed {
                tool: PODMAN,
                args: args.iter().map(|s| s.to_string()).collect(),
                exit_code: outcome.exit_code,
                stderr: "podman create returned empty container id".to_string(),
            });
        }
        Ok(ContainerHandle::new(id))
    }

    async fn start(&self, handle: &ContainerHandle) -> Result<(), OciError> {
        self.run_or_fail(&["start", &handle.id]).await?;
        Ok(())
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
        let outcome = self
            .runner
            .run(PODMAN, &args)
            .await
            .map_err(|source| OciError::Io {
                tool: PODMAN,
                source,
            })?;
        Ok(ExecResult {
            exit_code: outcome.exit_code,
            stdout: String::from_utf8_lossy(&outcome.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&outcome.stderr).into_owned(),
        })
    }

    async fn stop(&self, handle: &ContainerHandle) -> Result<(), OciError> {
        self.run_or_fail(&["stop", &handle.id]).await?;
        Ok(())
    }

    async fn remove(&self, handle: &ContainerHandle) -> Result<(), OciError> {
        // -f forces removal of running containers (podman stop+rm in one step).
        self.run_or_fail(&["rm", "-f", &handle.id]).await?;
        Ok(())
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
        // Missing/transitional fields are surfaced as `None` rather than
        // failing the call — see `parse_size_first` for the `"-- / --"`
        // placeholder podman emits while a container is mid-state.
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
            cpu_percent: entry
                .get("cpu_percent")
                .and_then(|v| v.as_str())
                .and_then(parse_percent),
            memory_bytes: entry
                .get("mem_usage")
                .and_then(|v| v.as_str())
                .and_then(parse_size_first),
            pids: entry.get("pids").and_then(|v| match v {
                serde_json::Value::String(s) => s.parse().ok(),
                serde_json::Value::Number(n) => n.as_u64(),
                _ => None,
            }),
        })
    }
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
/// after the first space is the original line text. Lines without a
/// recognisable timestamp prefix are returned with `timestamp = None` so
/// the caller never silently drops them.
fn parse_podman_log_line(stream: &str, raw: &str) -> LogLine {
    if let Some((maybe_ts, rest)) = raw.split_once(' ') {
        if maybe_ts.contains('T') && maybe_ts.contains('-') {
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
fn parse_size_first(s: &str) -> Option<u64> {
    let first = s.split('/').next()?.trim();
    let (num, unit) = split_number_unit(first)?;
    let value: f64 = num.parse().ok()?;
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
        _ => return None,
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

    fn rt(runner: RecordingRunner) -> PodmanRuntime {
        PodmanRuntime::with_runner(Box::new(runner))
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
        let img = ImageRef::parse("docker.io/library/alpine:3.19").unwrap();
        runtime.pull(&img).await.unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "podman");
        assert_eq!(calls[0].1, vec!["pull", "docker.io/library/alpine:3.19"]);
    }

    #[tokio::test]
    async fn create_returns_handle_from_stdout() {
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"abc1234deadbeef\n".to_vec()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let img = ImageRef::parse("alpine:3.19").unwrap();
        let h = runtime
            .create(&img, &["echo", "hi"], &SecurityOpts::permissive())
            .await
            .unwrap();
        assert_eq!(h.id, "abc1234deadbeef");

        let calls = calls.lock().unwrap();
        // SecurityOpts::permissive emits zero flags, keeping the
        // historical argv shape for tests that pre-date F-642.
        assert_eq!(calls[0].1, vec!["create", "alpine:3.19", "echo", "hi"]);
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
        runner.push(StubResponse::ok_stdout(b"abc1234\n".to_vec()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let img = ImageRef::parse("alpine:3.19").unwrap();
        runtime
            .create(&img, &["--privileged", "sh"], &SecurityOpts::permissive())
            .await
            .unwrap();

        let argv = calls.lock().unwrap()[0].1.clone();
        let image_idx = argv
            .iter()
            .position(|a| a == "alpine:3.19")
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
        let img = ImageRef::parse("alpine:3.19").unwrap();
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
        let img = ImageRef::parse("does/not:exist").unwrap();
        let err = rt(runner).pull(&img).await.unwrap_err();
        assert!(matches!(
            err,
            OciError::CommandFailed { tool: "podman", .. }
        ));
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
        runner.push(StubResponse::ok_stdout(b"abc1234\n".to_vec()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let img = ImageRef::parse("alpine:3.19").unwrap();
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
        let image_idx = argv
            .iter()
            .position(|a| a == "alpine:3.19")
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
        runner.push(StubResponse::ok_stdout(b"abc1234\n".to_vec()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let img = ImageRef::parse("alpine:3.19").unwrap();
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
                "alpine:3.19".to_string(),
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
        runner.push(StubResponse::ok_stdout(b"abc1234\n".to_vec()));
        let calls = runner.calls.clone();

        let runtime = rt(runner);
        let img = ImageRef::parse("alpine:3.19").unwrap();
        runtime
            .create(&img, &["sh"], &SecurityOpts::permissive())
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls[0].1, vec!["create", "alpine:3.19", "sh"]);
    }

    #[test]
    fn parse_podman_log_line_handles_no_timestamp() {
        let l = parse_podman_log_line("stderr", "plain output");
        assert_eq!(l.stream, "stderr");
        assert_eq!(l.line, "plain output");
        assert_eq!(l.timestamp, None);
    }
}

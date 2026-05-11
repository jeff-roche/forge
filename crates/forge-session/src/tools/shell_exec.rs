//! `shell.exec` tool stub: runs a command through the Level-1 sandbox and
//! captures stdout / stderr / exit code. Streaming support is deferred.
//!
//! Args (JSON):
//! ```json
//! {
//!   "command": "/usr/bin/env",      // required
//!   "args": ["FOO"],                // optional
//!   "cwd": "/path/to/workspace",    // optional; defaults to ctx.workspace_root
//!   "env": { "FOO": "bar" },        // optional caller-scoped env allow-list
//!   "timeout_ms": 30000             // optional wall-clock guard; silently
//!                                   // clamped to `MAX_TIMEOUT_MS` (10 min)
//! }
//! ```
//!
//! Result shape: `{ stdout, stderr, exit_code, signal? }` or `{ error }`.

use super::{get_optional_str, Tool, ToolCtx};
#[cfg(target_os = "linux")]
use super::{get_optional_u64, get_required_str, ToolError};
#[cfg(target_os = "linux")]
use crate::sandbox::SandboxedCommand;
use forge_core::ApprovalPreview;
#[cfg(target_os = "linux")]
use std::time::Duration;

/// Upper bound for `shell.exec` `timeout_ms`. A provider-supplied value larger
/// than this is silently clamped. Prevents a runaway tool call from holding
/// the future open indefinitely (F-066 / CWE-400).
///
/// Gated to Linux because the entire `shell.exec` implementation is Linux-only
/// (drives the `SandboxedCommand` L1 sandbox). On non-Linux the constant
/// would be dead code — gate it so macOS/Windows compile cleanly.
#[cfg(target_os = "linux")]
pub(crate) const MAX_TIMEOUT_MS: u64 = 10 * 60 * 1000;

/// Default `timeout_ms` when the caller does not supply one.
///
/// Linux-gated for the same reason as [`MAX_TIMEOUT_MS`].
#[cfg(target_os = "linux")]
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 30_000;

pub struct ShellExecTool;

impl ShellExecTool {
    pub const NAME: &'static str = "shell.exec";
}

#[async_trait::async_trait]
impl Tool for ShellExecTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn approval_preview(&self, args: &serde_json::Value) -> ApprovalPreview {
        let command = get_optional_str(args, "command").unwrap_or("");
        let argv = args
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        // Surface `cwd` in the preview whenever it is supplied. When absent,
        // `run_linux` defaults to `ctx.workspace_root` — there is nothing for
        // the user to distinguish, so no `cwd` line is shown. When present,
        // the directory context is load-bearing and must appear in consent
        // (F-054 / audit finding M8).
        let cwd = get_optional_str(args, "cwd");
        let head = if argv.is_empty() {
            format!("Run command: {command}")
        } else {
            format!("Run command: {command} {argv}")
        };
        ApprovalPreview {
            description: match cwd {
                Some(p) => format!("{head} (cwd: {p})"),
                None => head,
            },
        }
    }

    async fn invoke(&self, args: &serde_json::Value, ctx: &ToolCtx) -> serde_json::Value {
        #[cfg(target_os = "linux")]
        {
            run_linux(args, ctx).await
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (args, ctx);
            serde_json::json!({
                "error": "shell.exec requires Linux (process isolation not yet implemented on this platform)"
            })
        }
    }
}

#[cfg(target_os = "linux")]
async fn run_linux(args: &serde_json::Value, ctx: &ToolCtx) -> serde_json::Value {
    // F-074: route through the shared `get_required_str` helper so the
    // missing-arg error shape matches the other tools (`tool.X: missing
    // required parameter 'Y'`). An explicit empty-string guard sits on top
    // because spawning `""` is meaningless — the helper itself accepts ""
    // so callers like `fs.write` can legitimately truncate a file. Both
    // the missing and empty cases surface the unified `MissingRequiredArg`
    // error so the IPC error string is identical across all tools.
    let command = match get_required_str(args, ShellExecTool::NAME, "command") {
        Ok(c) if !c.is_empty() => c,
        Ok(_) | Err(_) => {
            return serde_json::json!({
                "error": ToolError::MissingRequiredArg {
                    tool: ShellExecTool::NAME.to_string(),
                    arg: "command".to_string(),
                }
                .to_string()
            });
        }
    };

    let argv: Vec<String> = args
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    // Resolve cwd. If the caller supplied one, validate it lives under
    // `ctx.workspace_root` — canonicalize both sides so a symlink inside
    // the workspace that points out is rejected by the post-canonical prefix
    // check (F-054 / audit finding M8, symmetric with forge-fs root checks
    // for F-043). If no cwd is supplied, fall back to the workspace root.
    let cwd = match get_optional_str(args, "cwd") {
        Some(requested) => {
            let Some(root) = ctx.workspace_root.as_ref() else {
                return serde_json::json!({
                    "error": "shell.exec: cwd cannot be validated without workspace_root"
                });
            };
            let canonical_cwd = match std::fs::canonicalize(requested) {
                Ok(p) => p,
                Err(e) => {
                    return serde_json::json!({
                        "error": format!("shell.exec: cannot resolve cwd {requested:?}: {e}")
                    });
                }
            };
            let canonical_root = match std::fs::canonicalize(root) {
                Ok(p) => p,
                Err(e) => {
                    return serde_json::json!({
                        "error": format!(
                            "shell.exec: cannot resolve workspace_root {}: {e}",
                            root.display()
                        )
                    });
                }
            };
            if !canonical_cwd.starts_with(&canonical_root) {
                return serde_json::json!({
                    "error": format!(
                        "shell.exec: cwd {} is outside workspace {}",
                        canonical_cwd.display(),
                        canonical_root.display()
                    )
                });
            }
            canonical_cwd
        }
        None => ctx.workspace_root.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        }),
    };

    let timeout_ms = get_optional_u64(args, "timeout_ms")
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .min(MAX_TIMEOUT_MS);

    let mut sb = SandboxedCommand::new(command, &cwd);
    sb.command_mut().args(&argv);
    sb.command_mut().stdout(std::process::Stdio::piped());
    sb.command_mut().stderr(std::process::Stdio::piped());
    sb.command_mut().stdin(std::process::Stdio::null());

    if let Some(env_obj) = args.get("env").and_then(|v| v.as_object()) {
        let pairs = match validate_env(env_obj) {
            Ok(pairs) => pairs,
            Err(e) => return serde_json::json!({ "error": e }),
        };
        for (k, v) in pairs {
            sb.allow_env(k, v);
        }
    }

    if let Some(registry) = &ctx.child_registry {
        sb.with_registry(registry.clone());
    }

    // F-106: `Tool::invoke` is `async`, so `run_linux` is awaited directly
    // by the caller's runtime. The pre-F-106 `block_in_place + block_on +
    // throwaway-runtime` shim is gone — making the trait async made it
    // unnecessary.
    //
    // F-047 / H6: the `SandboxedChild` wrapper MUST stay live across the
    // timeout. Calling `.into_child()` here would set `released = true` and
    // remove the pgid from the session's `ChildRegistry`, so a timeout (or
    // task cancellation / panic) drop of the `tokio::process::Child` alone
    // would orphan the process group — the whole point of Level-1 isolation
    // defeated. Keeping `sandboxed` in scope guarantees `Drop::killpg` runs
    // on every exit path and the registry entry is cleaned up.
    let mut sandboxed = match sb.spawn() {
        Ok(c) => c,
        Err(e) => return serde_json::json!({ "error": format!("spawn: {e}") }),
    };

    // Take the stdout / stderr pipes by value so the concurrent reader
    // futures can own them; we only need `&mut Child` for `wait()`.
    let stdout = sandboxed.as_child_mut().stdout.take();
    let stderr = sandboxed.as_child_mut().stderr.take();

    let stdout_fut = async move {
        match stdout {
            Some(mut s) => {
                use tokio::io::AsyncReadExt;
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
                use tokio::io::AsyncReadExt;
                let mut buf = String::new();
                let _ = s.read_to_string(&mut buf).await;
                buf
            }
            None => String::new(),
        }
    };

    // Run wait + stdout + stderr concurrently so the child cannot block
    // on full pipe buffers while we wait on exit. `sandboxed` stays
    // borrowed by `as_child_mut()` only for the duration of the join.
    let combined = async { tokio::join!(sandboxed.as_child_mut().wait(), stdout_fut, stderr_fut,) };

    let result = match tokio::time::timeout(Duration::from_millis(timeout_ms), combined).await {
        Ok((Ok(status), stdout, stderr)) => {
            use std::os::unix::process::ExitStatusExt;
            let mut result = serde_json::json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": status.code(),
            });
            if let Some(sig) = status.signal() {
                result["signal"] = serde_json::json!(sig);
            }
            result
        }
        Ok((Err(e), _, _)) => serde_json::json!({ "error": format!("wait: {e}") }),
        Err(_) => serde_json::json!({ "error": format!("timeout after {timeout_ms}ms") }),
    };
    // `sandboxed` drops here — on both success (drop is idempotent;
    // killpg on an already-reaped pgid returns ESRCH) and on timeout
    // (drop sends SIGKILL to the pgid and removes it from the registry).
    result
}

/// Reject any env key or value containing an embedded NUL byte (`\0`).
///
/// POSIX `execve` terminates `argv` / `envp` entries at the first NUL, so a
/// caller-supplied env value carrying an embedded `\0` would silently truncate
/// the child's view of that variable — a confused-deputy primitive against
/// downstream tools that read process env. Fail loud with the offending
/// variable name; do not silently strip.
///
/// Returns the validated `(key, value)` pairs in the input's iteration order
/// so the caller can forward them to `allow_env` without re-parsing.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn validate_env(
    env_obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<(&str, &str)>, String> {
    let mut pairs = Vec::with_capacity(env_obj.len());
    for (k, v) in env_obj {
        if k.contains('\0') {
            return Err(format!("shell.exec: env key contains NUL byte: {k:?}"));
        }
        let Some(val) = v.as_str() else { continue };
        if val.contains('\0') {
            return Err(format!("shell.exec: env value for '{k}' contains NUL byte"));
        }
        pairs.push((k.as_str(), val));
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::validate_env;
    use serde_json::json;

    fn env_map(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn rejects_nul_in_env_value() {
        let env = env_map(json!({ "FOO": "ba\0r" }));
        let err = validate_env(&env).unwrap_err();
        assert!(
            err.contains("NUL") && err.contains("FOO"),
            "error must name the offending variable: {err}"
        );
    }

    #[test]
    fn rejects_nul_in_env_key() {
        let mut env = serde_json::Map::new();
        env.insert("BA\0D".to_string(), json!("ok"));
        let err = validate_env(&env).unwrap_err();
        assert!(err.contains("NUL"), "error must mention NUL: {err}");
        assert!(err.contains("key"), "error must say the key is bad: {err}");
    }

    #[test]
    fn accepts_clean_env() {
        let env = env_map(json!({ "FOO": "bar", "BAZ": "qux" }));
        let pairs = validate_env(&env).expect("clean env must pass");
        assert_eq!(pairs.len(), 2);
        for (k, v) in pairs {
            assert!(matches!(k, "FOO" | "BAZ"));
            assert!(matches!(v, "bar" | "qux"));
        }
    }

    #[test]
    fn skips_non_string_values_without_failing() {
        // Pre-existing contract: non-string values are silently ignored (the
        // old loop did `if let Some(val) = v.as_str()`). Preserve that —
        // NUL validation only applies to the values we would actually
        // forward.
        let env = env_map(json!({ "FOO": "bar", "N": 42 }));
        let pairs = validate_env(&env).expect("non-string values must be skipped");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("FOO", "bar"));
    }
}

//! F-608 step 7: daemon-side crash-dump reader.
//!
//! The sidecar binary (`forged-agent`) installs a panic hook that
//! persists a [`CrashDump`] to
//! `<XDG_DATA_HOME or $HOME/.local/share>/forge/crashes/<session-id>/<instance-id>-<unix-ts>.json`
//! before exiting. The supervisor side observes the EOF and recycles
//! the child; this module exposes the post-mortem half — given a
//! session id, enumerate every dump on disk so triage UIs and tests
//! can assert what the sidecar actually crashed on.
//!
//! The on-disk JSON shape lives in [`forge_ipc::sidecar::CrashDump`] so
//! the writer (sidecar process) and reader (daemon process) share a
//! single source of truth without forcing the sidecar to depend on
//! `forge-session`.
//!
//! ## Crash-dir resolution
//!
//! The base directory is, in priority order:
//!   1. `$FORGE_CRASH_DIR` — explicit override, used by tests so the
//!      writer and reader don't pollute the user's real data dir.
//!   2. `$XDG_DATA_HOME/forge/crashes` — the XDG base-dir spec data
//!      home.
//!   3. `$HOME/.local/share/forge/crashes` — the XDG default when
//!      `XDG_DATA_HOME` is unset.
//!
//! The reader does **not** create the directory on read — a missing
//! base dir is treated as "no crashes" and returns an empty list.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
pub use forge_ipc::sidecar::CrashDump;

/// Resolve the absolute path of the crash-dir for a given `session_id`.
///
/// Reads `$FORGE_CRASH_DIR`, `$XDG_DATA_HOME`, and `$HOME` from the
/// environment. The returned path may not exist — callers that need
/// the directory present must `create_dir_all` it themselves with
/// mode 0o700 (the sidecar writer does this; the reader does not).
pub fn crash_dir_for_session(session_id: &str) -> Result<PathBuf> {
    crash_dir_with(
        std::env::var("FORGE_CRASH_DIR").ok().as_deref(),
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        dirs::home_dir().as_deref(),
        session_id,
    )
}

/// DI variant of [`crash_dir_for_session`]. Takes the env-derived
/// inputs explicitly so tests can drive every branch with tempdirs
/// instead of mutating process-global env.
pub fn crash_dir_with(
    forge_crash_dir: Option<&str>,
    xdg_data_home: Option<&str>,
    home: Option<&Path>,
    session_id: &str,
) -> Result<PathBuf> {
    let base = resolve_crash_base(forge_crash_dir, xdg_data_home, home)?;
    Ok(base.join(session_id))
}

/// Resolve the *base* crashes directory — the parent of every per-
/// session subdir.
fn resolve_crash_base(
    forge_crash_dir: Option<&str>,
    xdg_data_home: Option<&str>,
    home: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(s) = forge_crash_dir.filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(s));
    }
    if let Some(s) = xdg_data_home.filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(s).join("forge").join("crashes"));
    }
    let home = home.ok_or_else(|| {
        anyhow::anyhow!(
            "cannot resolve crash directory: $FORGE_CRASH_DIR / $XDG_DATA_HOME / $HOME all unset"
        )
    })?;
    Ok(home.join(".local/share/forge/crashes"))
}

/// Enumerate every [`CrashDump`] persisted under the given session.
///
/// Returns the dumps sorted by `captured_at` (oldest first) so the
/// caller can render them in the order the panics happened. Files
/// that fail to parse are skipped with a warning rather than aborting
/// the whole enumeration — a single corrupt dump must not hide the
/// rest. A missing base directory returns an empty `Vec` (the
/// supervisor may have spawned no children yet, or the entire
/// session may have been crash-free).
pub fn collect_crashes_for_session(session_id: &str) -> Result<Vec<CrashDump>> {
    let dir = crash_dir_for_session(session_id)?;
    collect_crashes_in_dir(&dir)
}

/// Collect every `*.json` crash-dump file in `dir`. Used by both
/// [`collect_crashes_for_session`] and the integration test (which
/// passes its tempdir override directly to skip the env-var dance).
pub fn collect_crashes_in_dir(dir: &Path) -> Result<Vec<CrashDump>> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("read_dir {}", dir.display()));
        }
    };
    let mut out = Vec::new();
    for entry in read {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, dir = %dir.display(), "skipping unreadable dir entry");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        match read_dump_file(&path) {
            Ok(dump) => out.push(dump),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "skipping unparseable crash dump"
                );
            }
        }
    }
    out.sort_by_key(|d| d.captured_at);
    Ok(out)
}

fn read_dump_file(path: &Path) -> Result<CrashDump> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading crash dump {}", path.display()))?;
    let dump: CrashDump = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing crash dump {}", path.display()))?;
    Ok(dump)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use std::os::unix::fs::DirBuilderExt;

    #[test]
    fn forge_crash_dir_overrides_xdg_and_home() {
        let dir = crash_dir_with(
            Some("/tmp/forge-test-crashes"),
            Some("/should/be/ignored"),
            Some(Path::new("/home/x")),
            "sess-1",
        )
        .expect("ok");
        assert_eq!(dir, PathBuf::from("/tmp/forge-test-crashes/sess-1"));
    }

    #[test]
    fn xdg_data_home_overrides_home() {
        let dir = crash_dir_with(
            None,
            Some("/var/lib/forge"),
            Some(Path::new("/home/x")),
            "sess-2",
        )
        .expect("ok");
        assert_eq!(
            dir,
            PathBuf::from("/var/lib/forge/forge/crashes/sess-2"),
            "XDG path must be `<XDG_DATA_HOME>/forge/crashes/<session-id>`"
        );
    }

    #[test]
    fn home_fallback_uses_local_share() {
        let dir = crash_dir_with(None, None, Some(Path::new("/home/x")), "sess-3").expect("ok");
        assert_eq!(
            dir,
            PathBuf::from("/home/x/.local/share/forge/crashes/sess-3")
        );
    }

    #[test]
    fn empty_overrides_treated_as_unset() {
        // `std::env::var` returns Ok("") for `KEY=`, which must fall
        // through to the next priority — otherwise an operator who
        // accidentally exports `XDG_DATA_HOME=` would silently land
        // crashes in `/forge/crashes/<sess>` (the literal joined
        // path) instead of under `$HOME`.
        let dir = crash_dir_with(Some(""), Some(""), Some(Path::new("/home/x")), "s").expect("ok");
        assert_eq!(dir, PathBuf::from("/home/x/.local/share/forge/crashes/s"));
    }

    #[test]
    fn missing_dir_returns_empty_vec() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist");
        let got = collect_crashes_in_dir(&missing).expect("ok");
        assert!(got.is_empty(), "missing dir must yield empty vec");
    }

    #[test]
    fn collect_returns_sorted_dumps() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("mkdir");

        let earlier = CrashDump {
            instance_id: "inst-a".into(),
            session_id: "sess-x".into(),
            panic_message: "first".into(),
            backtrace: None,
            captured_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        };
        let later = CrashDump {
            instance_id: "inst-b".into(),
            session_id: "sess-x".into(),
            panic_message: "second".into(),
            backtrace: Some("trace".into()),
            captured_at: Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap(),
        };
        // Write files out of order to confirm the sort actually runs
        // (not just file-system ordering).
        std::fs::write(
            dir.join("inst-b-200.json"),
            serde_json::to_vec(&later).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("inst-a-100.json"),
            serde_json::to_vec(&earlier).unwrap(),
        )
        .unwrap();
        // Drop a non-json file to confirm the extension filter.
        std::fs::write(dir.join("README.txt"), b"not a dump").unwrap();
        // Drop an unparseable .json file to confirm the warn-and-skip
        // path doesn't break enumeration.
        std::fs::write(dir.join("garbage.json"), b"not json").unwrap();

        let got = collect_crashes_in_dir(&dir).expect("collect");
        assert_eq!(got.len(), 2, "got {got:?}");
        assert_eq!(got[0].panic_message, "first");
        assert_eq!(got[1].panic_message, "second");
    }
}

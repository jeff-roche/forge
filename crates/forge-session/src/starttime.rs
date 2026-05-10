//! Cross-platform process start-time probe (F-338).
//!
//! [`read_self_starttime`] returns an opaque `u64` that uniquely identifies
//! the current process across its lifetime on a single host. Consumers use
//! it as a second factor alongside the pid to detect kernel pid reuse
//! between the moment the pid is recorded and the moment something tries
//! to act on it (see `pid_file::OwnedPidFile` and
//! `forge_cli::socket::parse_pid_file_record`).
//!
//! The value's physical meaning is deliberately platform-dependent:
//!
//! - **Linux**: field 22 of `/proc/self/stat` — monotonic clock ticks
//!   since boot. Historically the only shape this value had, hence the
//!   "/proc/self/stat field 22" phrasing still in older comments.
//! - **macOS**: `libproc::BSDInfo::pbi_start_tvsec * 10^6 +
//!   pbi_start_tvusec` — start time in microseconds since the Unix
//!   epoch. Pulled via `proc_pidinfo`, which F-156's resource sampler
//!   already depends on.
//! - **Windows**: the creation-time `FILETIME` returned by
//!   `GetProcessTimes`, merged into a `u64` of 100ns units since 1601.
//!
//! **The value is not comparable across platforms.** Every consumer must
//! treat the `u64` as an opaque identity token and only compare values
//! produced on the same host. Nothing in the tree does arithmetic on it.
//!
//! Non-self lookups (arbitrary pid) intentionally stay Linux-only — see
//! `forge_cli::socket::read_process_starttime`. The pid-reuse kill path
//! layers on `pidfd_open`, which has no macOS/Windows analogue; falling
//! back to `libc::kill` there would re-open the race F-049 closed.

use anyhow::Result;

/// Read the starttime token for an arbitrary pid on Linux.
///
/// Mirrors [`read_self_starttime`] but targets `/proc/<pid>/stat` instead
/// of `/proc/self/stat` so callers can verify a recorded leader pid has
/// not been reaped and recycled (see `sandbox::ChildRegistry::kill_all`).
/// Linux-only because the pid-reuse mitigation it backs is itself
/// Linux-only (cgroup-paired killpg); macOS/Windows sandboxes do not
/// use this path.
#[cfg(target_os = "linux")]
pub fn read_process_starttime(pid: libc::pid_t) -> Result<u64> {
    let path = format!("/proc/{pid}/stat");
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("no such process {pid} (/proc/{pid}/stat missing)");
        }
        Err(e) => anyhow::bail!("failed to read /proc/{pid}/stat: {e}"),
    };
    linux::parse_proc_stat_starttime(&contents)
}

/// Return a `u64` that uniquely identifies this process across its
/// lifetime on the current host. The shape is platform-dependent; see
/// the module docs.
pub fn read_self_starttime() -> Result<u64> {
    #[cfg(target_os = "linux")]
    {
        linux::read_self_starttime()
    }
    #[cfg(target_os = "macos")]
    {
        macos::read_self_starttime()
    }
    #[cfg(target_os = "windows")]
    {
        windows::read_self_starttime()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        compile_error!(
            "forge-session starttime probe is not implemented on this target. \
             Supported: linux, macos, windows."
        )
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context, Result};

    pub(super) fn read_self_starttime() -> Result<u64> {
        let contents =
            std::fs::read_to_string("/proc/self/stat").context("read /proc/self/stat")?;
        parse_proc_stat_starttime(&contents)
    }

    /// Extract field 22 (`starttime`) from a `/proc/<pid>/stat` line.
    ///
    /// Field 2 (`comm`) is parenthesised and may contain arbitrary
    /// characters, including spaces and `(`/`)`. We locate the comm
    /// terminator by finding the **last** `)` in the line, then count
    /// space-separated fields from there: field 3 is at index 0 after
    /// the split, so `starttime` (field 22) lands at index 19.
    pub(super) fn parse_proc_stat_starttime(line: &str) -> Result<u64> {
        let close = line
            .rfind(')')
            .ok_or_else(|| anyhow::anyhow!("malformed /proc/self/stat: no closing paren"))?;
        let tail = &line[close + 1..];
        let st = tail
            .split_ascii_whitespace()
            .nth(19)
            .ok_or_else(|| anyhow::anyhow!("malformed /proc/self/stat: not enough fields"))?;
        st.parse::<u64>()
            .map_err(|_| anyhow::anyhow!("invalid starttime field: {st:?}"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_self_stat_field_22() {
            let line = "1 (init) S 0 1 1 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 7 0 0 0 0 0 0 0";
            let st = parse_proc_stat_starttime(line).expect("parses");
            assert_eq!(st, 7);
        }

        #[test]
        fn parses_comm_with_spaces() {
            let line =
                "1234 (weird (comm) name) S 1 0 0 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 98765 0 0 0 0 0";
            let st = parse_proc_stat_starttime(line).expect("parses comm with parens");
            assert_eq!(st, 98765);
        }

        #[test]
        fn rejects_missing_close_paren() {
            let line = "1234 (forged S 1";
            assert!(parse_proc_stat_starttime(line).is_err());
        }

        #[test]
        fn rejects_truncated_after_paren() {
            let line = "1234 (forged) S 1 1234 1234";
            assert!(parse_proc_stat_starttime(line).is_err());
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{Context, Result};
    use libproc::bsd_info::BSDInfo;
    use libproc::proc_pid::pidinfo;

    pub(super) fn read_self_starttime() -> Result<u64> {
        let pid = std::process::id() as i32;
        let bsd = pidinfo::<BSDInfo>(pid, 0)
            .map_err(|e| anyhow::anyhow!("proc_pidinfo(BSDInfo, pid={pid}): {e}"))
            .context("read self BSDInfo via libproc")?;
        // Compose microseconds-since-epoch into a single opaque u64.
        // `pbi_start_tvsec` and `pbi_start_tvusec` are both `u64` in
        // libproc 0.14's bindings — no i64 conversion needed. A process
        // whose start would overflow `u64::MAX / 1_000_000` seconds is
        // ~580k years in the future; saturating for that case keeps the
        // function total without changing observable behaviour.
        Ok(bsd
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(bsd.pbi_start_tvusec))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn read_self_starttime_is_non_zero_and_stable() {
            // A live process must have a non-zero start time (since the
            // Unix epoch), and reading twice within the same millisecond
            // must return the identical value — start-time is monotonic
            // per process.
            let a = read_self_starttime().expect("libproc must resolve self");
            let b = read_self_starttime().expect("second read must succeed");
            assert_eq!(a, b, "self start-time must be stable across reads");
            assert!(a > 0, "self start-time must be non-zero, got {a}");
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use anyhow::Result;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    pub(super) fn read_self_starttime() -> Result<u64> {
        let mut creation = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exit = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut kernel = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut user = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle that is
        // valid for the life of the process and must NOT be closed. All
        // four FILETIME out-pointers target stack locals that outlive
        // the call.
        let ok = unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            ) != 0
        };
        if !ok {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("GetProcessTimes(self) failed: {err}");
        }
        Ok(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // CI gap: no Windows runner in forge-ide/forge today (F-158
        // added macOS-latest only). This test compiles and runs on
        // Windows dev hosts; the macOS/Linux CI excludes this module at
        // the `cfg(target_os)` boundary so the suite stays green there.
        #[test]
        fn read_self_starttime_is_non_zero_and_stable() {
            let a = read_self_starttime().expect("GetProcessTimes(self) must succeed");
            let b = read_self_starttime().expect("second read must succeed");
            assert_eq!(a, b, "self start-time must be stable across reads");
            assert!(a > 0, "self start-time must be non-zero, got {a}");
        }
    }
}

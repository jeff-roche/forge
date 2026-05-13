use std::path::{Path, PathBuf};

/// Return `true` iff `s` matches the canonical `SessionId` wire format:
/// exactly 16 lowercase hex characters (8 random bytes rendered as `{:02x}`
/// in `forge_core::SessionId::new`).
///
/// This is the single chokepoint the CLI uses to refuse attacker-controlled
/// session ids before they reach path-building functions. A canonical id
/// cannot contain `/`, `.`, `..`, NUL, whitespace, or any character with
/// filesystem meaning, so validation here is sufficient to block the
/// `../../tmp/evil` style path-traversal identified in F-057 (T12a).
pub fn session_id_is_valid(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Resolve the per-user runtime directory, delegating to the shared helper
/// so the CLI, daemon, and Tauri shell agree on the same policy.
///
/// We deliberately do not consult `$UID` (an unreliable shell-only variable
/// that F-044 removed) nor fall back to `/tmp/forge-<uid>` — that fallback
/// was the defect H8 closed. On macOS (F-339) the shared helper resolves
/// `$HOME/Library/Application Support/Forge/run` at `0o700`, preserving the
/// per-user invariant via a platform-native path rather than relaxing it.
fn xdg_runtime_dir() -> anyhow::Result<PathBuf> {
    forge_core::runtime_dir::runtime_dir()
}

/// Pure composition: given a runtime dir `base`, return the Unix socket
/// path for `session_id`. Separated from [`socket_path`] so tests can
/// exercise the composition without mutating `XDG_RUNTIME_DIR` (F-344).
fn compose_socket_path(base: &Path, session_id: &str) -> PathBuf {
    base.join("forge/sessions")
        .join(format!("{session_id}.sock"))
}

/// Pure composition: given a runtime dir `base`, return the sessions
/// socket directory. Separated from [`sessions_socket_dir`] so tests
/// can exercise the composition without env mutation (F-344).
fn compose_sessions_socket_dir(base: &Path) -> PathBuf {
    base.join("forge/sessions")
}

/// Resolve the Unix socket path for a session, using the same logic as `forged`.
///
/// Returns an error when no per-user runtime directory can be established
/// (Linux without `XDG_RUNTIME_DIR`, or macOS with an unreadable `$HOME`).
/// `forged` refuses to start in the same situations (see
/// `forge_session::socket_path::resolve_socket_path`), so any CLI resolution
/// here is pointing at the same socket the daemon would bind.
pub fn socket_path(session_id: &str) -> anyhow::Result<PathBuf> {
    // Defense-in-depth for F-057 (T12a): the CLI's clap `value_parser`
    // already rejects malformed ids, but any future caller that constructs
    // a session id from a non-CLI source (env var, IPC message, disk) must
    // not be able to smuggle path-traversal through here silently. Debug
    // builds panic loudly; release builds still honour the upstream
    // `value_parser` contract.
    debug_assert!(
        session_id_is_valid(session_id),
        "socket_path: session_id_is_valid({session_id:?}) == false"
    );
    Ok(compose_socket_path(&xdg_runtime_dir()?, session_id))
}

/// Return the directory containing all session sockets.
pub fn sessions_socket_dir() -> anyhow::Result<PathBuf> {
    Ok(compose_sessions_socket_dir(&xdg_runtime_dir()?))
}

/// Resolve the PID file path for a session.
pub fn pid_path(session_id: &str) -> anyhow::Result<PathBuf> {
    // Defense-in-depth for F-057 (T12a): same rationale as `socket_path`.
    // `socket_path` below will also debug-assert, but stating the invariant
    // explicitly here keeps the failure message attached to the function
    // the caller actually invoked.
    debug_assert!(
        session_id_is_valid(session_id),
        "pid_path: session_id_is_valid({session_id:?}) == false"
    );
    Ok(socket_path(session_id)?.with_extension("pid"))
}

/// Parse and validate the contents of a session pid file.
///
/// Returns an error if the contents do not parse as an integer, or if the
/// resulting pid is less than or equal to zero. POSIX `kill(2)` treats
/// `pid == 0` as "signal every process in the caller's process group" and
/// `pid == -1` as "signal every process the user may signal"; both would
/// detonate far outside the intended target, so they must never reach
/// `libc::kill`.
pub fn parse_session_pid(raw: &str) -> anyhow::Result<libc::pid_t> {
    let trimmed = raw.trim();
    let pid: libc::pid_t = trimmed
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid pid file contents: {trimmed:?}"))?;
    anyhow::ensure!(pid > 0, "refusing to signal non-positive pid {pid}");
    Ok(pid)
}

/// Render the contents to write to a session pid file, given the result of
/// `std::process::Child::id()` / `tokio::process::Child::id()`.
///
/// Rejects `None` — a missing pid means the child was already reaped before
/// we could record it, and writing `"0"` to the pid file would later cause
/// `libc::kill(0, SIGTERM)` to signal the caller's entire process group.
/// Also rejects `Some(0)` defensively so that neither helper in this module
/// can ever emit a non-positive pid for `libc::kill`.
pub fn pid_file_contents(pid: Option<u32>) -> anyhow::Result<String> {
    let pid =
        pid.ok_or_else(|| anyhow::anyhow!("forged child exited before its pid could be recorded"))?;
    anyhow::ensure!(pid > 0, "refusing to write non-positive pid {pid}");
    Ok(pid.to_string())
}

/// Format a session pid-file record as `"<pid>\n<start_time>\n"`.
///
/// Two-line format pairs the pid with the daemon's platform-dependent
/// start-time (see `forge_session::starttime::read_self_starttime`) so
/// `session_kill` can detect kernel PID reuse before signaling: if the
/// recorded start-time differs from the value the live process reports
/// now, the pid has been recycled and we refuse to signal. The `u64`'s
/// physical meaning is platform-specific (Linux: clock ticks since
/// boot; macOS: microseconds since epoch; Windows: 100ns FILETIME
/// creation time) and is only ever compared against a freshly-probed
/// value on the same host.
pub fn format_pid_file_record(pid: libc::pid_t, start_time: u64) -> String {
    format!("{pid}\n{start_time}\n")
}

/// Parse a two-line pid-file record written by `format_pid_file_record`.
///
/// Returns `(pid, platform_start_time)`. Refuses single-line files
/// (legacy one-line format) so `session_kill` cannot silently skip the
/// identity check.
pub fn parse_pid_file_record(raw: &str) -> anyhow::Result<(libc::pid_t, u64)> {
    let mut lines = raw.lines();
    let pid_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("pid file is empty"))?;
    let start_line = lines.next().ok_or_else(|| {
        anyhow::anyhow!("pid file missing start-time line (expected two-line format)")
    })?;
    let pid: libc::pid_t = pid_line
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid pid in pid file: {:?}", pid_line))?;
    anyhow::ensure!(pid > 0, "refusing to signal non-positive pid {pid}");
    let start_time: u64 = start_line
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid start-time in pid file: {:?}", start_line))?;
    Ok((pid, start_time))
}

/// Extract field 22 (`starttime`) from a `/proc/<pid>/stat` line.
///
/// Field 2 (`comm`) is parenthesised and may contain arbitrary characters,
/// including spaces and `(`/`)`. We locate the comm terminator by finding
/// the **last** `)` in the line, then count space-separated fields from
/// there: field 3 is at index 0 after the split, so `starttime` (field 22)
/// lands at index 19.
#[cfg(target_os = "linux")]
pub fn parse_proc_stat_starttime(line: &str) -> anyhow::Result<u64> {
    let close = line
        .rfind(')')
        .ok_or_else(|| anyhow::anyhow!("malformed /proc/<pid>/stat: no closing paren"))?;
    let tail = &line[close + 1..];
    // Fields 3..=N, space-separated; starttime is field 22 -> index 19.
    let st = tail
        .split_ascii_whitespace()
        .nth(19)
        .ok_or_else(|| anyhow::anyhow!("malformed /proc/<pid>/stat: not enough fields"))?;
    st.parse::<u64>()
        .map_err(|_| anyhow::anyhow!("invalid starttime field in /proc/<pid>/stat: {st:?}"))
}

/// Read `/proc/self/stat`'s field 22 for an arbitrary `pid`.
///
/// Returns an error whose message mentions "stale" when `/proc/<pid>/stat`
/// is missing (kernel PID no longer allocated) so callers can distinguish
/// daemon-already-exited from pid-reuse mismatches.
#[cfg(target_os = "linux")]
pub fn read_process_starttime(pid: libc::pid_t) -> anyhow::Result<u64> {
    let path = format!("/proc/{pid}/stat");
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("stale pid file: no such process {pid} (/proc/{pid}/stat missing)");
        }
        Err(e) => anyhow::bail!("failed to read /proc/{pid}/stat: {e}"),
    };
    parse_proc_stat_starttime(&contents)
}

/// Confirm the process currently holding `pid` is the same one whose
/// start-time is `recorded_start_time`. If `/proc/<pid>/stat` is missing,
/// the daemon has already exited — report stale. If start-time differs,
/// the kernel has reused the PID for an unrelated process — refuse.
#[cfg(target_os = "linux")]
pub fn verify_kill_target(pid: libc::pid_t, recorded_start_time: u64) -> anyhow::Result<()> {
    let current =
        read_process_starttime(pid).map_err(|e| anyhow::anyhow!("{e}; session is not running"))?;
    anyhow::ensure!(
        current == recorded_start_time,
        "refusing to signal pid {pid}: PID reuse detected \
         (recorded start-time {recorded_start_time}, current {current})"
    );
    Ok(())
}

/// Race-free SIGTERM delivery via `pidfd_open` + `pidfd_send_signal`.
///
/// Between `pidfd_open` and `pidfd_send_signal`, the kernel guarantees the
/// fd resolves to the process we opened it for — even if that process has
/// since exited and its PID has been reused. This closes the narrow
/// open→signal window; caller is responsible for the broader pid-file
/// identity check (see `verify_kill_target`).
#[cfg(target_os = "linux")]
pub fn pidfd_send_sigterm(pid: libc::pid_t) -> anyhow::Result<()> {
    let pidfd = pidfd_open(pid)?;
    pidfd_send_signal_via_fd(pid, &pidfd, libc::SIGTERM)
}

/// F-747: race-free signal delivery for arbitrary `signal` via pidfd. Used
/// by the shell's `session_close` escalation path to deliver SIGTERM then
/// SIGKILL to a wedged `forged` whose pid is read from the on-disk pid file.
/// Pid identity / reuse is the caller's responsibility — the canonical safe
/// entry point is `kill_session_from_pid_file` (which re-reads
/// `/proc/<pid>/stat` to confirm start-time). This helper exists so the
/// escalation path can hold its own pidfd across `SIGTERM → wait → SIGKILL`
/// transitions without re-opening (and re-racing) the pidfd between
/// signals.
#[cfg(target_os = "linux")]
pub fn pidfd_send_signal_for_pid(pid: libc::pid_t, signal: libc::c_int) -> anyhow::Result<()> {
    let pidfd = pidfd_open(pid)?;
    pidfd_send_signal_via_fd(pid, &pidfd, signal)
}

/// Open a pidfd for `pid`. Caller owns the fd and must keep it alive to
/// anchor the kernel identity guarantee for any subsequent operation.
#[cfg(target_os = "linux")]
fn pidfd_open(pid: libc::pid_t) -> anyhow::Result<OwnedPidFd> {
    // SAFETY: `libc::syscall` is FFI; arguments are simple integers.
    // `pidfd_open(pid, 0)` returns a new fd on success and -1 on error
    // (errno set); the caller supplies `pid > 0` (enforced by our pid
    // parsers) so we cannot accidentally select a process group.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0 as libc::c_uint) };
    if raw < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            anyhow::bail!("pidfd_open({pid}): no such process (ESRCH); pid is stale");
        }
        anyhow::bail!("pidfd_open({pid}) failed: {err}");
    }
    // SAFETY: `raw` is a valid fd we just opened; ownership transfers.
    Ok(unsafe { OwnedPidFd::from_raw(raw as libc::c_int) })
}

#[cfg(target_os = "linux")]
fn pidfd_send_signal_via_fd(
    pid: libc::pid_t,
    pidfd: &OwnedPidFd,
    signal: libc::c_int,
) -> anyhow::Result<()> {
    // SAFETY: `pidfd` is a valid pidfd; `siginfo` is null per the syscall
    // contract to use the default action (sender context). `flags` is 0.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0 as libc::c_uint,
        )
    };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            anyhow::bail!(
                "pidfd_send_signal({pid}, {signal}): process exited between \
                 pidfd_open and signal; no signal delivered (ESRCH)"
            );
        }
        anyhow::bail!("pidfd_send_signal({pid}, {signal}) failed: {err}");
    }
    Ok(())
}

/// Owned `pidfd` that closes on drop. Small local wrapper to keep the
/// single-call-site simple without pulling in `OwnedFd` conversions.
#[cfg(target_os = "linux")]
struct OwnedPidFd {
    fd: libc::c_int,
}

#[cfg(target_os = "linux")]
impl OwnedPidFd {
    /// # Safety
    /// `fd` must be a valid open file descriptor owned by the caller;
    /// ownership transfers to the returned wrapper.
    unsafe fn from_raw(fd: libc::c_int) -> Self {
        Self { fd }
    }

    fn as_raw(&self) -> libc::c_int {
        self.fd
    }
}

#[cfg(target_os = "linux")]
impl Drop for OwnedPidFd {
    fn drop(&mut self) {
        // SAFETY: `self.fd` is a valid fd we own.
        unsafe {
            libc::close(self.fd);
        }
    }
}

/// Read a session pid file, confirm PID has not been reused, and deliver
/// SIGTERM via `pidfd_send_signal`. Returns the `(pid, start_time)` that
/// were signaled so callers can log. Does **not** remove the pid file;
/// the daemon owns its pid-file lifecycle (see F-049).
///
/// Race-freedom proof sketch (three windows):
///   1. **pid-file-write → pid-file-read** (seconds to days): closed by
///      the recorded start-time in the pid file — if forged has exited
///      and the kernel has reused the pid, `/proc/<pid>/stat` field 22
///      will not match, and we refuse.
///   2. **start-time-check → pidfd_open** (microseconds): closed by
///      opening the pidfd **first** and re-reading `/proc/<pid>/stat`
///      after. Once pidfd_open succeeds, the kernel pins the identity
///      of the process behind the fd; any subsequent `/proc/<pid>/stat`
///      read observes that same process (or `ESRCH` if it has since
///      exited, which `read_process_starttime` surfaces as "stale").
///   3. **pidfd_open → pidfd_send_signal** (microseconds): closed by
///      the kernel; the fd resolves to the original opened process
///      regardless of pid reuse.
#[cfg(target_os = "linux")]
pub fn kill_session_from_pid_file(
    pid_file: &std::path::Path,
) -> anyhow::Result<(libc::pid_t, u64)> {
    let raw = std::fs::read_to_string(pid_file)
        .map_err(|e| anyhow::anyhow!("cannot read pid file {}: {e}", pid_file.display()))?;
    let (pid, recorded_start_time) = parse_pid_file_record(&raw)?;
    // Open pidfd FIRST so the kernel anchors the identity of the process
    // we are about to inspect. Any subsequent /proc/<pid>/stat read —
    // used here to verify start-time — sees the pinned process (or
    // ESRCH, which we surface as stale).
    let pidfd = pidfd_open(pid).map_err(|e| anyhow::anyhow!("{e}; session is not running"))?;
    let current_start_time =
        read_process_starttime(pid).map_err(|e| anyhow::anyhow!("{e}; session is not running"))?;
    anyhow::ensure!(
        current_start_time == recorded_start_time,
        "refusing to signal pid {pid}: PID reuse detected \
         (recorded start-time {recorded_start_time}, current {current_start_time})"
    );
    pidfd_send_signal_via_fd(pid, &pidfd, libc::SIGTERM)?;
    Ok((pid, recorded_start_time))
}

/// Non-Linux stub for `kill_session_from_pid_file`. The race-free kill path
/// relies on `pidfd_open` and `/proc/<pid>/stat`, both Linux-only. On other
/// platforms we refuse to fall back to raw `libc::kill` (which F-049 removes
/// precisely because it SIGTERMs a reused PID), so the function exists but
/// returns a typed error — preserving the safety invariant without breaking
/// workspace builds on macOS/Windows.
///
/// Follow-up work: implement `kqueue` `EVFILT_PROC` (macOS) or Win32 handle
/// equivalent (Windows). See the H2 finding's "References" section.
#[cfg(not(target_os = "linux"))]
pub fn kill_session_from_pid_file(
    _pid_file: &std::path::Path,
) -> anyhow::Result<(libc::pid_t, u64)> {
    anyhow::bail!(
        "session_kill is only race-free on Linux (pidfd_open + /proc/<pid>/stat). \
         macOS/Windows would need kqueue EVFILT_PROC or a Win32 handle-based \
         equivalent; tracked as a follow-up to F-049. Terminate the daemon \
         manually on this platform (e.g. Activity Monitor / Task Manager)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_valid_accepts_canonical_16_char_lowercase_hex() {
        assert!(session_id_is_valid("deadbeefcafebabe"));
        assert!(session_id_is_valid("0123456789abcdef"));
    }

    #[test]
    fn session_id_is_valid_rejects_path_traversal() {
        assert!(!session_id_is_valid("../../tmp/x"));
        assert!(!session_id_is_valid("../etc/passwd"));
        assert!(!session_id_is_valid("."));
        assert!(!session_id_is_valid(".."));
        assert!(!session_id_is_valid("/absolute/path"));
    }

    #[test]
    fn session_id_is_valid_rejects_uppercase() {
        // SessionId::new() emits lowercase hex via {:02x}; any uppercase id
        // cannot have come from the canonical generator and must be refused
        // so the validator has a single normal form.
        assert!(!session_id_is_valid("DEADBEEFCAFEBABE"));
        assert!(!session_id_is_valid("DeadBeefCafeBabe"));
    }

    #[test]
    fn session_id_is_valid_rejects_wrong_length() {
        assert!(!session_id_is_valid(""));
        assert!(!session_id_is_valid("abc"));
        // 15 chars
        assert!(!session_id_is_valid("deadbeefcafebab"));
        // 17 chars
        assert!(!session_id_is_valid("deadbeefcafebabe0"));
        // 32 chars (uuid-like length)
        assert!(!session_id_is_valid("deadbeefcafebabedeadbeefcafebabe"));
    }

    #[test]
    fn session_id_is_valid_rejects_non_hex() {
        assert!(!session_id_is_valid("deadbeefcafebabg")); // 'g'
        assert!(!session_id_is_valid("deadbeef cafebabe")); // space
        assert!(!session_id_is_valid("deadbeef-cafebabe")); // dash
        assert!(!session_id_is_valid("deadbeef.cafebabe")); // dot
        assert!(!session_id_is_valid("deadbeef\u{00}bedbabe")); // NUL
    }

    #[test]
    fn session_id_is_valid_accepts_all_freshly_generated_ids() {
        // Round-trip property: whatever SessionId::new() emits must pass the
        // validator. If we ever widen the generator to, say, 32 hex chars,
        // this test will fail loudly and force the validator to be updated.
        for _ in 0..32 {
            let id = forge_core::SessionId::new().to_string();
            assert!(
                session_id_is_valid(&id),
                "freshly-generated SessionId must be valid: {id:?}"
            );
        }
    }

    /// F-344: exercise the path-composition helpers via DI instead of
    /// mutating `XDG_RUNTIME_DIR`. The env-reading policy itself
    /// (XDG-set / macOS fallback / Linux error) is covered by DI unit
    /// tests in `forge_core::runtime_dir`; this test only pins the
    /// `<base> + "forge/sessions" + "<id>.sock"` composition the CLI
    /// layers on top.
    #[test]
    fn path_composition_matches_daemon_layout() {
        let base = std::path::Path::new("/run/user/4242");

        let sock = compose_socket_path(base, "deadbeefcafebabe");
        assert_eq!(
            sock.to_string_lossy(),
            "/run/user/4242/forge/sessions/deadbeefcafebabe.sock"
        );

        let dir = compose_sessions_socket_dir(base);
        assert_eq!(dir.to_string_lossy(), "/run/user/4242/forge/sessions");

        // `pid_path` just swaps the extension on `socket_path`, so
        // assert the same shape on the composed sock path.
        let pid = compose_socket_path(base, "abc123def4560000").with_extension("pid");
        assert_eq!(
            pid.to_string_lossy(),
            "/run/user/4242/forge/sessions/abc123def4560000.pid"
        );
    }

    // F-044's Linux "XDG_RUNTIME_DIR unset -> error, no /tmp fallback"
    // invariant is covered by `forge_core::runtime_dir::tests` via DI
    // (no env mutation). `socket_path` / `pid_path` /
    // `sessions_socket_dir` each call `runtime_dir()?` and propagate the
    // error via `?`, so exercising the error shape again here would only
    // retest stdlib `?` behaviour at the cost of mutating process-global
    // env. Deliberately omitted (F-344).

    /// Defense-in-depth: if clap's `value_parser` is ever bypassed (e.g. a
    /// future caller constructs a session id from a different source and
    /// forgets to validate), the path-builders must still refuse.
    #[test]
    #[should_panic(expected = "session_id_is_valid")]
    #[cfg(debug_assertions)]
    fn pid_path_debug_asserts_valid_session_id() {
        let _ = pid_path("../../tmp/x");
    }

    #[test]
    #[should_panic(expected = "session_id_is_valid")]
    #[cfg(debug_assertions)]
    fn socket_path_debug_asserts_valid_session_id() {
        let _ = socket_path("../../tmp/x");
    }

    #[test]
    fn parse_session_pid_accepts_positive() {
        let pid = parse_session_pid("4242").expect("positive pid should parse");
        assert_eq!(pid, 4242);
    }

    #[test]
    fn parse_session_pid_trims_whitespace_and_newlines() {
        let pid = parse_session_pid("  1234\n").expect("trimmed pid should parse");
        assert_eq!(pid, 1234);
    }

    #[test]
    fn parse_session_pid_rejects_zero() {
        let err = parse_session_pid("0").expect_err("pid 0 must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("non-positive") && msg.contains('0'),
            "expected non-positive rejection mentioning 0, got: {msg}"
        );
    }

    #[test]
    fn parse_session_pid_rejects_negative_one() {
        let err = parse_session_pid("-1").expect_err("pid -1 must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("non-positive") && msg.contains("-1"),
            "expected non-positive rejection mentioning -1, got: {msg}"
        );
    }

    #[test]
    fn parse_session_pid_rejects_negative_group() {
        let err = parse_session_pid("-42").expect_err("negative pid must be rejected");
        assert!(err.to_string().contains("non-positive"));
    }

    #[test]
    fn parse_session_pid_rejects_garbage() {
        let err = parse_session_pid("not-a-number").expect_err("garbage must be rejected");
        assert!(err.to_string().contains("invalid"));
    }

    #[test]
    fn pid_file_contents_returns_string_for_known_pid() {
        let s = pid_file_contents(Some(4242)).expect("known pid should produce contents");
        assert_eq!(s, "4242");
    }

    #[test]
    fn pid_file_contents_rejects_none() {
        let err = pid_file_contents(None).expect_err("None pid must be rejected");
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("pid"),
            "expected message to mention pid, got: {msg}"
        );
    }

    #[test]
    fn pid_file_contents_rejects_zero_defensively() {
        let err = pid_file_contents(Some(0)).expect_err("pid 0 must be rejected");
        assert!(err.to_string().contains("non-positive"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_stat_starttime_handles_plain_comm() {
        let line = "1234 (forged) S 1 1234 1234 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 98765 0 0 0 0 0";
        let st = parse_proc_stat_starttime(line).expect("parses");
        assert_eq!(st, 98765);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_stat_starttime_handles_comm_with_spaces_and_parens() {
        // field 2 contains spaces AND nested parens, so naive whitespace-splits fail.
        let line = "4242 (weird (comm) name) S 1 4242 4242 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 555555 0 0 0 0 0";
        let st = parse_proc_stat_starttime(line).expect("parses comm with parens");
        assert_eq!(st, 555555);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_stat_starttime_rejects_missing_close_paren() {
        let line = "1234 (forged S 1";
        assert!(parse_proc_stat_starttime(line).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_stat_starttime_rejects_truncated_after_paren() {
        let line = "1234 (forged) S 1 1234 1234";
        assert!(parse_proc_stat_starttime(line).is_err());
    }

    #[test]
    fn pid_file_record_round_trip() {
        let raw = format_pid_file_record(4242, 98765);
        let (pid, st) = parse_pid_file_record(&raw).expect("parses");
        assert_eq!(pid, 4242);
        assert_eq!(st, 98765);
    }

    #[test]
    fn parse_pid_file_record_rejects_single_line() {
        // Old one-line format must be rejected so we don't silently skip the
        // start-time identity check.
        let err = parse_pid_file_record("4242\n").expect_err("single line must fail");
        assert!(err.to_string().to_lowercase().contains("start"));
    }

    #[test]
    fn parse_pid_file_record_rejects_non_positive_pid() {
        let err = parse_pid_file_record("0\n12345\n").expect_err("pid 0 must fail");
        assert!(err.to_string().contains("non-positive"));
    }

    #[test]
    fn parse_pid_file_record_rejects_garbage_starttime() {
        let err = parse_pid_file_record("42\nnot-a-number\n").expect_err("bad st must fail");
        assert!(err.to_string().to_lowercase().contains("start"));
    }

    #[test]
    fn format_pid_file_record_is_two_newline_terminated_lines() {
        let s = format_pid_file_record(7, 9);
        assert_eq!(s, "7\n9\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_process_starttime_for_self_matches_two_reads() {
        // Start-time is monotonic per process; reading twice must match.
        let pid = unsafe { libc::getpid() };
        let a = read_process_starttime(pid).expect("should read self starttime");
        let b = read_process_starttime(pid).expect("should read self starttime second time");
        assert_eq!(a, b);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_process_starttime_for_nonexistent_pid_reports_stale() {
        // PID 0x7fffffff is effectively unused; a fresh read must fail as stale.
        let err =
            read_process_starttime(i32::MAX).expect_err("unused pid should not yield starttime");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("stale") || msg.contains("no such"),
            "expected stale/no-such-process error, got: {msg}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_kill_target_refuses_when_starttime_mismatches() {
        // Simulate PID reuse: live pid = our own, but recorded start-time is bogus.
        let pid = unsafe { libc::getpid() };
        let err = verify_kill_target(pid, 1).expect_err("mismatched start-time must refuse");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("reuse") || msg.contains("mismatch") || msg.contains("recycled"),
            "expected pid-reuse error, got: {msg}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_kill_target_accepts_matching_starttime() {
        let pid = unsafe { libc::getpid() };
        let st = read_process_starttime(pid).expect("self starttime");
        verify_kill_target(pid, st).expect("matching start-time must succeed");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_send_sigterm_to_nonexistent_pid_fails() {
        // Delivery path must error, not silently succeed, for a pid with no
        // live process. (i32::MAX is effectively unused.)
        let err = pidfd_send_sigterm(i32::MAX).expect_err("pidfd_open on dead pid must fail");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("no such") || msg.contains("stale") || msg.contains("esrch"),
            "expected ESRCH-like error, got: {msg}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kill_session_from_pid_file_refuses_when_starttime_bogus() {
        // DoD item 3: "kill-scenario where pid is reused between write and
        // kill does not signal the reused process". Simulate reuse by
        // writing the pid of this test process paired with a bogus
        // start-time, then asserting the helper refuses without delivering
        // SIGTERM.
        //
        // If the helper were buggy and actually signaled our own pid with
        // SIGTERM, the test harness would be killed before reaching any
        // assertion, so a mere `is_err()` passing is itself evidence of
        // non-delivery.
        use tempfile::TempDir;
        let td = TempDir::new().expect("tempdir");
        let pid_file = td.path().join("session.pid");
        let my_pid = unsafe { libc::getpid() };
        // Bogus start-time (legitimate start-times are monotonic clock ticks;
        // 1 is well below any real /proc/self/stat value for a live process).
        std::fs::write(&pid_file, format_pid_file_record(my_pid, 1)).expect("write");

        let err = kill_session_from_pid_file(&pid_file)
            .expect_err("bogus recorded start-time must cause refusal");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("reuse") || msg.contains("mismatch") || msg.contains("recycled"),
            "expected pid-reuse error, got: {msg}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kill_session_from_pid_file_refuses_when_pid_gone() {
        use tempfile::TempDir;
        let td = TempDir::new().expect("tempdir");
        let pid_file = td.path().join("session.pid");
        std::fs::write(&pid_file, format_pid_file_record(i32::MAX, 12345)).expect("write");
        let err = kill_session_from_pid_file(&pid_file).expect_err("absent pid must fail");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("stale") || msg.contains("not running") || msg.contains("no such"),
            "expected stale error, got: {msg}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kill_session_from_pid_file_refuses_legacy_one_line_format() {
        // Old pid files (pre-F-049) must not be silently accepted; if we
        // accepted single-line format, `session_kill` would skip the
        // start-time identity check and revert to the original TOCTOU.
        use tempfile::TempDir;
        let td = TempDir::new().expect("tempdir");
        let pid_file = td.path().join("session.pid");
        let my_pid = unsafe { libc::getpid() };
        std::fs::write(&pid_file, format!("{my_pid}\n")).expect("write");
        let err = kill_session_from_pid_file(&pid_file)
            .expect_err("legacy single-line format must be rejected");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("start"),
            "expected start-time / format error, got: {msg}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_send_sigterm_to_live_child_delivers_signal() {
        // Race-free delivery: spawn `sleep 30`, send SIGTERM via pidfd, wait.
        use std::process::Command;
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as libc::pid_t;
        pidfd_send_sigterm(pid).expect("pidfd signal delivery should succeed");
        let status = child.wait().expect("wait sleep child");
        assert!(!status.success(), "sleep should have been killed");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_kill_target_reports_stale_when_pid_gone() {
        // Unused high PID -> error, but specifically stale (not mismatch).
        let err = verify_kill_target(i32::MAX, 12345).expect_err("nonexistent pid must fail");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("stale") || msg.contains("no such") || msg.contains("not running"),
            "expected stale error, got: {msg}"
        );
    }
}

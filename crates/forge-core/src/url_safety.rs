//! SSRF (Server-Side Request Forgery) guard for user-supplied HTTP(S) URLs.
//!
//! [`check_url`] validates that a URL supplied by a user-facing config (MCP
//! server registry, custom OpenAI-compatible provider base URL, etc.) cannot
//! be used to reach internal network targets: loopback, link-local (IMDS at
//! 169.254.169.254), or RFC-1918 private ranges. Only `https://` is accepted
//! in release builds; `http://` is additionally allowed for loopback hosts in
//! `cfg(debug_assertions)` builds so local dev servers work without TLS.
//!
//! ## History
//!
//! Originally landed under F-346 as `forge_mcp::transport::ssrf::check_url`
//! against `.mcp.json`-supplied HTTP MCP URLs. F-585 lifted the function here
//! into `forge-core` so the new `CustomOpenAiProvider` can reuse the same
//! guard against user-supplied OpenAI-compatible base URLs without
//! forge-providers gaining a wrong-direction dep on forge-mcp. The function
//! signature and semantics are unchanged; `forge_mcp::transport::ssrf` keeps
//! a thin re-export so existing MCP call sites do not move.

use anyhow::{bail, Result};
use url::Url;

/// Validate `raw` against SSRF policy. Returns an error describing the
/// violation if the URL targets a blocked host or uses a disallowed scheme.
///
/// Blocked IP ranges:
/// - 127.0.0.0/8 — IPv4 loopback (http allowed in debug builds, https always)
/// - 10.0.0.0/8 — RFC-1918 private
/// - 172.16.0.0/12 — RFC-1918 private
/// - 192.168.0.0/16 — RFC-1918 private
/// - 169.254.0.0/16 — link-local / cloud IMDS (e.g. AWS 169.254.169.254)
/// - ::1 — IPv6 loopback (same policy as IPv4 loopback)
/// - fc00::/7 — IPv6 unique-local
/// - fe80::/10 — IPv6 link-local
///
/// Scheme policy: `https` is always accepted; `http` is accepted only for
/// loopback hosts in debug builds. All other schemes are rejected.
pub fn check_url(raw: &str) -> Result<()> {
    let url = Url::parse(raw).map_err(|e| anyhow::anyhow!("invalid URL {raw:?}: {e}"))?;

    let scheme = url.scheme();
    if scheme != "https" && scheme != "http" {
        bail!(
            "SSRF guard: URL {raw:?} uses disallowed scheme {scheme:?}; \
             only https (or http for localhost in dev builds) is permitted"
        );
    }

    // url::Url::host() returns None only for non-host schemes (e.g. `data:`).
    // For https/http with an empty authority (e.g. `https:///path`), it
    // returns Some(Host::Domain("")) — reject that too.
    let host = url
        .host()
        .ok_or_else(|| anyhow::anyhow!("SSRF guard: URL {raw:?} has no host"))?;
    if let url::Host::Domain(d) = &host {
        if d.is_empty() {
            anyhow::bail!("SSRF guard: URL {raw:?} has an empty host");
        }
    }

    let is_loopback = is_host_loopback(&host);

    // http is only permitted for loopback hosts.
    if scheme == "http" && !is_loopback {
        bail!(
            "SSRF guard: URL {raw:?} uses http with a non-loopback host; \
             only https is allowed for remote servers"
        );
    }

    // In release builds, even loopback over http is rejected — TLS is
    // mandatory everywhere outside of developer machines.
    #[cfg(not(debug_assertions))]
    if scheme == "http" {
        bail!("SSRF guard: URL {raw:?} uses http; only https is permitted in release builds");
    }

    // Loopback hosts pass after the scheme checks above; skip range checks.
    if is_loopback {
        return Ok(());
    }

    match &host {
        url::Host::Ipv4(addr) => check_ipv4(*addr, raw)?,
        url::Host::Ipv6(addr) => check_ipv6(*addr, raw)?,
        url::Host::Domain(_) => {
            // Hostnames are resolved at connect time by reqwest; we cannot
            // pre-resolve them here without an async DNS round-trip. Blocking
            // by hostname pattern is also fragile against DNS rebinding. The
            // primary SSRF surface for user configs is direct IP literals,
            // which we block above. A future hardening step could wire a
            // custom `reqwest::dns::Resolve` that re-checks resolved IPs
            // post-lookup.
        }
    }

    Ok(())
}

fn is_host_loopback(host: &url::Host<&str>) -> bool {
    match host {
        // `localhost6` is the standard hostname for `::1` on most Linux
        // distributions (RHEL/Fedora/Debian ship it in `/etc/hosts` by
        // default). Without it, `http://localhost6:8000` would fail the
        // loopback exception and be rejected as a non-HTTPS remote in
        // debug builds — surprising for Linux developers running a local
        // OpenAI-compatible server bound to IPv6 loopback.
        url::Host::Domain(h) => matches!(*h, "localhost" | "localhost6"),
        url::Host::Ipv4(addr) => addr.is_loopback(),
        url::Host::Ipv6(addr) => addr.is_loopback(),
    }
}

/// Apply the IPv4 range policy used by [`check_url`] to a single address.
///
/// Exposed for [`crate::http`]: the SSRF-aware DNS resolver runs every
/// resolved IPv4 from `getaddrinfo` through the same checks `check_url`
/// would apply if the URL had been an IP literal. Loopback is *not*
/// rejected here — it's the caller's job to pre-filter loopback per the
/// [`check_url`] scheme/build-profile rules.
pub fn check_ipv4(addr: std::net::Ipv4Addr, raw: &str) -> Result<()> {
    let o = addr.octets();

    // 10.0.0.0/8
    if o[0] == 10 {
        bail!("SSRF guard: URL {raw:?} targets private range 10.0.0.0/8 ({addr})");
    }
    // 172.16.0.0/12
    if o[0] == 172 && (16..=31).contains(&o[1]) {
        bail!("SSRF guard: URL {raw:?} targets private range 172.16.0.0/12 ({addr})");
    }
    // 192.168.0.0/16
    if o[0] == 192 && o[1] == 168 {
        bail!("SSRF guard: URL {raw:?} targets private range 192.168.0.0/16 ({addr})");
    }
    // 169.254.0.0/16 — link-local / IMDS
    if o[0] == 169 && o[1] == 254 {
        bail!("SSRF guard: URL {raw:?} targets link-local/IMDS range 169.254.0.0/16 ({addr})");
    }

    Ok(())
}

/// Apply the IPv6 range policy used by [`check_url`] to a single address.
///
/// Exposed for [`crate::http`]: the SSRF-aware DNS resolver runs every
/// resolved IPv6 through the same checks `check_url` would apply. Loopback
/// (`::1`) is *not* rejected here — callers must pre-filter loopback if
/// their scheme/build-profile rules require it.
pub fn check_ipv6(addr: std::net::Ipv6Addr, raw: &str) -> Result<()> {
    let s = addr.segments();

    // fc00::/7 — unique-local (fc00:: through fdff::)
    if (s[0] & 0xfe00) == 0xfc00 {
        bail!("SSRF guard: URL {raw:?} targets IPv6 unique-local range fc00::/7 ({addr})");
    }
    // fe80::/10 — link-local
    if (s[0] & 0xffc0) == 0xfe80 {
        bail!("SSRF guard: URL {raw:?} targets IPv6 link-local range fe80::/10 ({addr})");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- scheme validation ---

    #[test]
    fn allows_public_https_url() {
        check_url("https://example.com/api").expect("public https must be allowed");
        check_url("https://8.8.8.8/api").expect("public IP via https must be allowed");
    }

    #[test]
    fn rejects_non_http_scheme() {
        let err = check_url("ftp://example.com/x").expect_err("ftp must be rejected");
        assert!(
            format!("{err}").contains("disallowed scheme"),
            "error should explain scheme: {err}"
        );
    }

    #[test]
    fn rejects_malformed_url() {
        check_url("not a url").expect_err("malformed URL must be rejected");
    }

    #[test]
    fn rejects_url_without_host() {
        check_url("file:///etc/passwd").expect_err("file:// must be rejected (disallowed scheme)");
        check_url("urn:isbn:0451450523").expect_err("urn: must be rejected (disallowed scheme)");
    }

    // --- loopback ---

    #[test]
    fn allows_https_loopback_always() {
        check_url("https://localhost/x").expect("https://localhost must always be allowed");
        check_url("https://127.0.0.1/x").expect("https://127.0.0.1 must always be allowed");
    }

    #[test]
    fn allows_localhost6_under_loopback_exception() {
        // `localhost6` resolves to `::1` on Linux distros that ship it in
        // /etc/hosts (RHEL/Fedora/Debian). Treating it as loopback aligns
        // the guard with Linux's IPv6 loopback hostname convention.
        check_url("https://localhost6/x").expect("https://localhost6 must be allowed");
        let result = check_url("http://localhost6:8000/x");
        #[cfg(debug_assertions)]
        result.expect("http://localhost6 must be allowed in debug builds");
        #[cfg(not(debug_assertions))]
        result.expect_err("http://localhost6 must be blocked in release builds");
    }

    #[test]
    fn http_loopback_policy_matches_build_profile() {
        let result = check_url("http://127.0.0.1/x");
        #[cfg(debug_assertions)]
        result.expect("http://127.0.0.1 must be allowed in debug builds");
        #[cfg(not(debug_assertions))]
        result.expect_err("http://127.0.0.1 must be blocked in release builds");
    }

    #[test]
    fn http_localhost_policy_matches_build_profile() {
        let result = check_url("http://localhost/x");
        #[cfg(debug_assertions)]
        result.expect("http://localhost must be allowed in debug builds");
        #[cfg(not(debug_assertions))]
        result.expect_err("http://localhost must be blocked in release builds");
    }

    #[test]
    fn http_remote_host_always_blocked() {
        check_url("http://example.com/x")
            .expect_err("http with remote host must always be blocked");
    }

    // --- IMDS / link-local ---

    #[test]
    fn blocks_imds_address() {
        let err = check_url("https://169.254.169.254/latest/meta-data")
            .expect_err("IMDS must be blocked");
        assert!(
            format!("{err}").contains("169.254"),
            "error should mention the address: {err}"
        );
    }

    #[test]
    fn blocks_link_local_range_boundary() {
        check_url("https://169.254.0.1/").expect_err("link-local boundary must be blocked");
    }

    // --- private ranges ---

    #[test]
    fn blocks_rfc1918_10_slash_8() {
        let err = check_url("https://10.0.0.1/x").expect_err("10/8 must be blocked");
        assert!(
            format!("{err}").contains("10.0.0.0/8"),
            "error should name the range: {err}"
        );
    }

    #[test]
    fn blocks_rfc1918_172_16_slash_12() {
        let err = check_url("https://172.16.0.1/x").expect_err("172.16/12 must be blocked");
        assert!(
            format!("{err}").contains("172.16.0.0/12"),
            "error should name the range: {err}"
        );
        check_url("https://172.31.255.255/x").expect_err("172.31.255.255 must be blocked");
        check_url("https://172.32.0.1/x").expect("172.32.0.1 is outside /12; must pass");
    }

    #[test]
    fn blocks_rfc1918_192_168_slash_16() {
        let err = check_url("https://192.168.1.100/x").expect_err("192.168/16 must be blocked");
        assert!(
            format!("{err}").contains("192.168.0.0/16"),
            "error should name the range: {err}"
        );
    }

    // --- IPv6 ---

    #[test]
    fn allows_https_ipv6_loopback() {
        check_url("https://[::1]/x").expect("https://[::1] must be allowed");
    }

    #[test]
    fn blocks_ipv6_unique_local() {
        let err = check_url("https://[fd00::1]/x").expect_err("fc00::/7 must be blocked");
        assert!(
            format!("{err}").contains("fc00::/7"),
            "error should name the range: {err}"
        );
    }

    #[test]
    fn blocks_ipv6_link_local() {
        let err = check_url("https://[fe80::1]/x").expect_err("fe80::/10 must be blocked");
        assert!(
            format!("{err}").contains("fe80::/10"),
            "error should name the range: {err}"
        );
    }

    #[test]
    fn allows_public_ipv6() {
        check_url("https://[2001:db8::1]/x").expect("public IPv6 must be allowed");
    }
}

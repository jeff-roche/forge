//! Hardened `reqwest::Client` construction shared across the workspace.
//!
//! [`check_url`](crate::url_safety::check_url) validates a URL at config
//! time, but the IP that `reqwest` actually dials is whatever DNS returns
//! at connect time — not what we parsed earlier. An attacker controlling a
//! DNS record with a short TTL can answer `8.8.8.8` for the safety check
//! and `169.254.169.254` for the connect that follows, smuggling a request
//! to AWS IMDS or another internal target through a guard that "passed".
//!
//! This module closes that gap with a `reqwest::dns::Resolve` implementation
//! that runs the same `url_safety` IPv4/IPv6 range checks against every
//! `SocketAddr` returned by the inner resolver. Addresses that match a
//! blocked range are filtered out; if the filtered set is empty the lookup
//! fails and reqwest never opens the socket.
//!
//! ## Helpers
//!
//! - [`secure_client_builder`] — `reqwest::ClientBuilder` pre-wired with the
//!   policy resolver. Zero-config drop-in for callers that don't already
//!   construct a builder of their own.
//! - [`policy_resolver`] — concrete [`PolicyEnforcingResolver`] for callers
//!   that compose their own builders (provider-specific timeouts, redirect
//!   policies, etc.) and just need to plug the resolver in via
//!   `ClientBuilder::dns_resolver(Arc::new(...))`. Returned as a sized
//!   concrete type because `dns_resolver` requires `R: Resolve + Sized`.
//! - [`policy_resolver_with_inner`] — same, but accepts a caller-supplied
//!   inner resolver. Used by the rebinding regression test to inject a
//!   scripted DNS sequence; downstream callers can use it to wrap a
//!   non-default resolver (e.g. hickory-dns) without losing the SSRF guard.
//!
//! ## Scope (F-644)
//!
//! This PR lands the helper. The downstream redirect-policy fixes
//! (F-645/646/647 — issues #681/#682/#683) adopt it across forge-mcp,
//! forge-providers, and forge-shell. The API exposes both a builder helper
//! and a bare resolver so those callers can adopt it without giving up
//! their own connect/read-timeout, redirect, or user-agent settings.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::url_safety;

/// Build a `reqwest::ClientBuilder` with the SSRF-safe DNS resolver
/// pre-installed. Callers chain their own settings (timeouts, headers,
/// redirect policy) and call `.build()` as usual.
///
/// ```no_run
/// let client = forge_core::http::secure_client_builder()
///     .connect_timeout(std::time::Duration::from_secs(5))
///     .build()
///     .expect("client build");
/// # let _ = client;
/// ```
pub fn secure_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().dns_resolver(Arc::new(policy_resolver()))
}

/// SSRF-aware DNS resolver wrapping the system resolver. Use this when you
/// already have a `reqwest::ClientBuilder` and just want to install the
/// guard without inheriting any other defaults — pass the returned value to
/// [`reqwest::ClientBuilder::dns_resolver`] (wrapped in an `Arc`).
///
/// The return type is the concrete [`PolicyEnforcingResolver`] rather than
/// `Arc<dyn Resolve>` because reqwest's `dns_resolver` accepts `Arc<R>`
/// over a sized `R: Resolve + 'static`, not a trait object.
pub fn policy_resolver() -> PolicyEnforcingResolver {
    PolicyEnforcingResolver::new(Arc::new(SystemResolver))
}

/// Same as [`policy_resolver`] but with a caller-supplied inner resolver.
/// Allows tests to script DNS answers and lets downstream callers stack the
/// SSRF guard on top of an alternate resolver (e.g. hickory) without losing
/// the policy check.
pub fn policy_resolver_with_inner(inner: Arc<dyn Resolve>) -> PolicyEnforcingResolver {
    PolicyEnforcingResolver::new(inner)
}

/// reqwest DNS resolver that wraps an inner resolver and filters resolved
/// `SocketAddr`s through the `url_safety` IPv4/IPv6 policy. If every
/// resolved address is blocked the lookup returns an `io::Error` and
/// reqwest aborts the connect — so a hostile DNS answer can never reach
/// the TCP layer.
pub struct PolicyEnforcingResolver {
    inner: Arc<dyn Resolve>,
}

impl PolicyEnforcingResolver {
    fn new(inner: Arc<dyn Resolve>) -> Self {
        Self { inner }
    }
}

impl Resolve for PolicyEnforcingResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let inner = Arc::clone(&self.inner);
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = inner.resolve(name).await?;
            let mut accepted: Vec<SocketAddr> = Vec::new();
            let mut rejections: Vec<String> = Vec::new();
            for sa in addrs {
                match check_addr(sa.ip(), &host) {
                    Ok(()) => accepted.push(sa),
                    Err(reason) => rejections.push(reason),
                }
            }
            if accepted.is_empty() {
                return Err(Box::new(std::io::Error::other(format!(
                    "SSRF guard: DNS for {host:?} resolved exclusively to \
                     url_safety-blocked IP ranges: {}",
                    rejections.join("; ")
                )))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(accepted.into_iter()) as Addrs)
        })
    }
}

/// Run a single resolved IP through the `url_safety` IPv4/IPv6 policy. Threads
/// the offending hostname through to `check_ipv4`/`check_ipv6` so block errors
/// name the host that triggered the rejection (operator-grade triage context).
///
/// IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is unwrapped to its embedded IPv4 so
/// `https://[::ffff:169.254.169.254]/...` is rejected as link-local rather
/// than slipping past as "unknown IPv6".
fn check_addr(ip: IpAddr, host: &str) -> Result<(), String> {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                return Ok(()); // url_safety treats loopback as allowed
            }
            url_safety::check_ipv4(v4, host).map_err(|e| e.to_string())
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                if mapped.is_loopback() {
                    return Ok(());
                }
                return url_safety::check_ipv4(mapped, host).map_err(|e| e.to_string());
            }
            if v6.is_loopback() {
                return Ok(());
            }
            url_safety::check_ipv6(v6, host).map_err(|e| e.to_string())
        }
    }
}

/// System-default DNS resolver used as the inner resolver in production.
/// Wraps `tokio::net::lookup_host`, which delegates to the platform
/// `getaddrinfo`. This is the same shape that reqwest's built-in resolver
/// uses; we stand it up explicitly so the [`PolicyEnforcingResolver`] has
/// a stable inner without depending on a private reqwest type.
struct SystemResolver;

/// Placeholder port handed to `tokio::net::lookup_host`. `lookup_host` requires
/// a `host:port` shape but the port we hand it is irrelevant — reqwest
/// overwrites it with the URL's explicit port (or the scheme default) before
/// dialing, so the placeholder never reaches TCP.
///
/// **Invariant pin:** see `SocketAddrs::extend` in reqwest 0.12.28
/// (<https://github.com/seanmonstar/reqwest/blob/v0.12.28/src/dns/resolve.rs#L98-L106>):
/// every resolved `SocketAddr` whose port is `0` (or whose URL specifies a
/// port explicitly) is rewritten via `addr.set_port(port)` prior to connect.
/// If a future reqwest release drops that rewrite, this resolver will start
/// returning addresses with port `0` and connects will fail loudly — that is
/// the intended failure mode (loud over silent).
const LOOKUP_HOST_PORT_PLACEHOLDER: u16 = 0;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), LOOKUP_HOST_PORT_PLACEHOLDER))
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                    .collect();
            // Every returned addr carries the placeholder port; reqwest's
            // contract (linked above) is to overwrite it pre-connect. We
            // verify the precondition we control: that `lookup_host` is
            // honoring the placeholder we passed it. If this trips, the
            // platform resolver mutated our port and the invariant
            // documented above no longer applies.
            debug_assert!(
                addrs
                    .iter()
                    .all(|a| a.port() == LOOKUP_HOST_PORT_PLACEHOLDER),
                "tokio::net::lookup_host returned a non-placeholder port; \
                 reqwest's port-rewrite contract assumes port 0 in"
            );
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test resolver that returns a scripted sequence of `SocketAddr` lists.
    /// The first call returns `answers[0]`, the second `answers[1]`, and so
    /// on (saturating at the last entry). Used to simulate a DNS-rebinding
    /// attacker who answers safely on one lookup and hostilely on the next.
    struct ScriptedResolver {
        answers: Vec<Vec<SocketAddr>>,
        call: AtomicUsize,
    }

    impl ScriptedResolver {
        fn new(answers: Vec<Vec<SocketAddr>>) -> Self {
            Self {
                answers,
                call: AtomicUsize::new(0),
            }
        }
    }

    impl Resolve for ScriptedResolver {
        fn resolve(&self, _name: Name) -> Resolving {
            let idx = self.call.fetch_add(1, Ordering::SeqCst);
            let i = idx.min(self.answers.len() - 1);
            let answer = self.answers[i].clone();
            Box::pin(async move { Ok(Box::new(answer.into_iter()) as Addrs) })
        }
    }

    fn name(s: &str) -> Name {
        Name::from_str(s).expect("valid DNS name")
    }

    fn sa_v4(a: u8, b: u8, c: u8, d: u8) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::new(a, b, c, d), 0))
    }

    fn sa_v6(addr: &str) -> SocketAddr {
        SocketAddr::from((Ipv6Addr::from_str(addr).expect("ipv6"), 0))
    }

    async fn collect(
        resolver: &PolicyEnforcingResolver,
        n: &str,
    ) -> Result<Vec<SocketAddr>, String> {
        match resolver.resolve(name(n)).await {
            Ok(addrs) => Ok(addrs.collect()),
            Err(e) => Err(e.to_string()),
        }
    }

    // --- DoD #1: resolver validates resolved IPs against url_safety policy ---

    #[tokio::test]
    async fn resolver_passes_public_ipv4() {
        let inner = Arc::new(ScriptedResolver::new(vec![vec![sa_v4(8, 8, 8, 8)]]));
        let resolver = PolicyEnforcingResolver::new(inner);
        let got = collect(&resolver, "example.com")
            .await
            .expect("public IP must pass");
        assert_eq!(got, vec![sa_v4(8, 8, 8, 8)]);
    }

    #[tokio::test]
    async fn resolver_blocks_imds() {
        let inner = Arc::new(ScriptedResolver::new(vec![vec![sa_v4(169, 254, 169, 254)]]));
        let resolver = PolicyEnforcingResolver::new(inner);
        let err = collect(&resolver, "metadata.attacker.test")
            .await
            .expect_err("IMDS must be filtered out");
        assert!(
            err.contains("SSRF guard"),
            "error must name the guard: {err}"
        );
        assert!(
            err.contains("metadata.attacker.test"),
            "error must name the offending hostname: {err}"
        );
        assert!(
            err.contains("169.254.169.254"),
            "error must name the offending IP: {err}"
        );
        assert!(
            err.contains("link-local") || err.contains("169.254.0.0/16"),
            "error must name the offending range: {err}"
        );
    }

    #[tokio::test]
    async fn resolver_block_error_names_hostname_for_ipv6() {
        let inner = Arc::new(ScriptedResolver::new(vec![vec![sa_v6("fd00::1")]]));
        let resolver = PolicyEnforcingResolver::new(inner);
        let err = collect(&resolver, "v6ula.attacker.test")
            .await
            .expect_err("fc00::/7 must be filtered");
        assert!(
            err.contains("v6ula.attacker.test"),
            "error must name the offending hostname: {err}"
        );
        assert!(
            err.contains("fd00::1"),
            "error must name the offending IP: {err}"
        );
    }

    #[tokio::test]
    async fn resolver_blocks_rfc1918_ranges() {
        for ip in [
            sa_v4(10, 0, 0, 1),
            sa_v4(172, 16, 0, 1),
            sa_v4(192, 168, 0, 1),
        ] {
            let inner = Arc::new(ScriptedResolver::new(vec![vec![ip]]));
            let resolver = PolicyEnforcingResolver::new(inner);
            collect(&resolver, "private.attacker.test")
                .await
                .expect_err(&format!("{ip} must be filtered out"));
        }
    }

    #[tokio::test]
    async fn resolver_filters_partial_answer() {
        // Mixed answer: one blocked, one public. Filter keeps only the
        // public one so the connect can still proceed against a safe IP.
        let inner = Arc::new(ScriptedResolver::new(vec![vec![
            sa_v4(169, 254, 169, 254),
            sa_v4(8, 8, 8, 8),
        ]]));
        let resolver = PolicyEnforcingResolver::new(inner);
        let got = collect(&resolver, "mixed.test")
            .await
            .expect("public IP survives filter");
        assert_eq!(got, vec![sa_v4(8, 8, 8, 8)]);
    }

    #[tokio::test]
    async fn resolver_blocks_ipv4_mapped_ipv6_imds() {
        // `::ffff:169.254.169.254` must be rejected as link-local even
        // though the literal is an IPv6 address — otherwise an attacker
        // smuggles IMDS through an IPv6 DNS answer.
        let mapped = sa_v6("::ffff:169.254.169.254");
        let inner = Arc::new(ScriptedResolver::new(vec![vec![mapped]]));
        let resolver = PolicyEnforcingResolver::new(inner);
        collect(&resolver, "v6mapped.test")
            .await
            .expect_err("IPv4-mapped IPv6 IMDS must be filtered");
    }

    #[tokio::test]
    async fn resolver_blocks_ipv6_unique_local() {
        let inner = Arc::new(ScriptedResolver::new(vec![vec![sa_v6("fd00::1")]]));
        let resolver = PolicyEnforcingResolver::new(inner);
        collect(&resolver, "v6ula.test")
            .await
            .expect_err("fc00::/7 must be filtered");
    }

    #[tokio::test]
    async fn resolver_passes_loopback_v4() {
        // url_safety treats loopback as allowed (debug-build dev servers,
        // https://localhost). The resolver must not over-filter.
        let inner = Arc::new(ScriptedResolver::new(vec![vec![sa_v4(127, 0, 0, 1)]]));
        let resolver = PolicyEnforcingResolver::new(inner);
        collect(&resolver, "localhost")
            .await
            .expect("loopback must pass");
    }

    // --- DoD #3: rebinding regression — first lookup safe, second hostile ---

    #[tokio::test]
    async fn rebinding_second_lookup_is_blocked() {
        // Simulates the F-644 attack: hostile DNS serves `8.8.8.8` for the
        // first resolution (would bypass `check_url`) and `169.254.169.254`
        // for the second (would hit IMDS without this resolver). The guard
        // must reject the second lookup so the connect fails.
        let inner = Arc::new(ScriptedResolver::new(vec![
            vec![sa_v4(8, 8, 8, 8)],
            vec![sa_v4(169, 254, 169, 254)],
        ]));
        let resolver = PolicyEnforcingResolver::new(inner);

        let first = collect(&resolver, "rebinding.attacker.test")
            .await
            .expect("first lookup mirrors check_url's view — passes");
        assert_eq!(first, vec![sa_v4(8, 8, 8, 8)]);

        let err = collect(&resolver, "rebinding.attacker.test")
            .await
            .expect_err("second lookup hits IMDS — must be blocked at the resolver");
        assert!(
            err.contains("SSRF guard"),
            "error must name the guard: {err}"
        );
    }

    // --- DoD #2: helper APIs build clients that use the resolver ---

    #[test]
    fn secure_client_builder_builds_a_client() {
        let _client = secure_client_builder()
            .build()
            .expect("default secure client must build");
    }

    #[test]
    fn policy_resolver_is_send_sync() {
        // The concrete resolver must be Send + Sync for reqwest to accept
        // it via `ClientBuilder::dns_resolver`. This compiles iff the bound
        // is met.
        fn assert_send_sync<T: Send + Sync>(_: &T) {}
        let r = policy_resolver();
        assert_send_sync(&r);
    }

    #[test]
    fn policy_resolver_is_installable_on_client_builder() {
        // Compile-time check: the type the public API returns is the same
        // shape `reqwest::ClientBuilder::dns_resolver` accepts. Catches a
        // future regression where someone changes the return type to a
        // trait object that no longer satisfies the `R: Sized` bound.
        let _builder = reqwest::Client::builder().dns_resolver(Arc::new(policy_resolver()));
    }
}

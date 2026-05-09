//! Image signature verification — the second half of F-643's supply-chain
//! mitigation. [`ImageRef`] makes the *identity* of the image bytes
//! immutable (digest pinning); this module makes the *provenance* of those
//! bytes attestable (signature verification).
//!
//! ## Design
//!
//! Verification runs **before** [`crate::ContainerRuntime::pull`] writes
//! anything to the local store. The trait is stateless and synchronous-shape:
//! it takes an `&ImageRef` and returns either `Ok(())` (good — proceed with
//! pull) or [`VerificationError`] (stop — never pull this image).
//!
//! Two implementations ship today:
//!
//! - [`CosignVerifier`] shells out to the `cosign` binary. The default
//!   keyless flow validates against the configured Fulcio root + Rekor
//!   transparency log AND pins the trusted signer identity through
//!   [`IDENTITY_ENV`] + [`OIDC_ENV`] (forwarded to cosign as the matching
//!   `--certificate-identity` / `--certificate-oidc-issuer` flags). Without
//!   identity pinning cosign accepts any Fulcio-signed artifact, so the
//!   verifier refuses to invoke cosign at all when those env vars are
//!   unset under [`SignaturePolicy::Strict`]. This is what production
//!   runs should use.
//! - [`NoopVerifier`] accepts every image. It exists for tests where the
//!   verification path is irrelevant. Production wiring should never use
//!   it; [`crate::PodmanRuntime::new`] defaults to [`CosignVerifier`] in
//!   [`SignaturePolicy::Permissive`] so an operator who forgets to wire a
//!   verifier still gets identity-pinned verification once they set the
//!   env vars.
//!
//! ## Policy
//!
//! [`SignaturePolicy`] selects the operational stance:
//!
//! - [`SignaturePolicy::Strict`] — every pull must verify successfully.
//!   Missing `cosign` binary, signature absent, signature mismatch all
//!   abort the pull. This is the production default once cosign rolls out.
//! - [`SignaturePolicy::Permissive`] — verification still runs when
//!   feasible, but a missing `cosign` binary degrades to a logged warning
//!   instead of hard-failing. Useful for early Phase 3 dev environments.
//!
//! The policy lives next to the verifier rather than inside `PodmanRuntime`
//! so a future host (`DockerRuntime`, an in-process containerd, etc.) can
//! reuse the same verification surface.

use crate::{ImageRef, OciError};
use async_trait::async_trait;

use crate::runner::{CommandRunner, TokioCommandRunner};

/// Stance the runtime takes when verifying images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignaturePolicy {
    /// Every pull must verify successfully. Missing tooling, missing
    /// signatures, and mismatched signatures all hard-fail. This is the
    /// production target.
    Strict,
    /// Verification runs when the verifier is operational; a missing
    /// verifier (e.g. `cosign` not on `PATH`) degrades to a logged warning
    /// rather than blocking the pull. Mismatched signatures still
    /// hard-fail — the policy only relaxes the "verifier available" check.
    Permissive,
}

impl Default for SignaturePolicy {
    /// `Permissive` is the default so existing dev environments without
    /// `cosign` do not break on upgrade. Production wiring should opt into
    /// [`Self::Strict`] explicitly.
    fn default() -> Self {
        Self::Permissive
    }
}

/// Reasons a verifier may reject an image.
#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    /// The verifier itself was not available (binary missing, daemon not
    /// running, etc.). [`SignaturePolicy::Strict`] turns this into a hard
    /// failure; [`SignaturePolicy::Permissive`] downgrades to a warning.
    #[error("signature verifier unavailable: {0}")]
    VerifierUnavailable(String),
    /// The verifier ran but rejected the image (signature missing, expired,
    /// or signed by an untrusted identity).
    #[error("signature mismatch: {0}")]
    Mismatch(String),
    /// I/O error invoking the verifier.
    #[error("io error invoking signature verifier: {0}")]
    Io(String),
}

/// Verify an image's signature before it is pulled. Implementations are
/// stateless — they take only the image reference and return a verdict.
#[async_trait]
pub trait SignatureVerifier: Send + Sync {
    /// Return `Ok(())` if the image is acceptable, [`VerificationError`]
    /// otherwise.
    async fn verify(&self, image: &ImageRef) -> Result<(), VerificationError>;
}

/// Permissive verifier that accepts every image. Use only in tests or when
/// the operator has explicitly opted into [`SignaturePolicy::Permissive`]
/// and no real verifier is installed.
pub struct NoopVerifier;

#[async_trait]
impl SignatureVerifier for NoopVerifier {
    async fn verify(&self, _image: &ImageRef) -> Result<(), VerificationError> {
        Ok(())
    }
}

/// Default binary name used by [`CosignVerifier`].
const COSIGN: &str = "cosign";

/// Env var operators set to pin the trusted signer identity. The verifier
/// reads it at [`CosignVerifier::verify`] time and forwards the value as
/// `--certificate-identity=<value>` to the cosign argv.
///
/// The variable name matches the cosign convention, but cosign itself does
/// **not** read it — it only reads the equivalent CLI flag. The verifier
/// bridges the two so operators can configure trust through the daemon
/// environment (the same place every other Forge runtime knob lives)
/// instead of patching argv at every call site.
pub const IDENTITY_ENV: &str = "COSIGN_CERTIFICATE_IDENTITY";

/// Env var operators set to pin the trusted OIDC issuer. Forwarded to
/// cosign as `--certificate-oidc-issuer=<value>`. Same naming convention
/// and bridging rationale as [`IDENTITY_ENV`].
pub const OIDC_ENV: &str = "COSIGN_CERTIFICATE_OIDC_ISSUER";

/// Verify images by shelling out to `cosign verify`. Runs in keyless mode
/// against the configured Fulcio root + Rekor transparency log.
///
/// **Identity pinning is mandatory.** Without `--certificate-identity` and
/// `--certificate-oidc-issuer`, cosign accepts *any* signature from *any*
/// Fulcio-issued cert — a legitimate signing run by a random user passes.
/// The verifier reads [`IDENTITY_ENV`] and [`OIDC_ENV`] from the process
/// environment at [`Self::verify`] time and forwards them as the matching
/// cosign flags. Note: cosign reads the flags, not the env vars — Forge
/// imposes the env-var convention so operator config lives in the daemon
/// environment.
///
/// Behaviour when the env vars are unset:
/// - [`SignaturePolicy::Strict`] — verify fails with
///   [`VerificationError::VerifierUnavailable`] before cosign is invoked.
///   Operators must set both env vars before opting in to Strict.
/// - [`SignaturePolicy::Permissive`] — same error shape (so the existing
///   policy enforcement layer downgrades to a warning), with an extra
///   `tracing::warn!` describing the gap. Useful in dev environments where
///   the operator hasn't decided on a signer identity yet.
///
/// The verifier is parameterised on [`CommandRunner`] so the unit tests in
/// this module can drive it without a real cosign binary on `PATH`.
pub struct CosignVerifier {
    runner: Box<dyn CommandRunner>,
    policy: SignaturePolicy,
}

impl CosignVerifier {
    /// Build a verifier that shells out via `tokio::process::Command`.
    pub fn new(policy: SignaturePolicy) -> Self {
        Self {
            runner: Box::new(TokioCommandRunner),
            policy,
        }
    }

    /// Build a verifier with a custom [`CommandRunner`] — for tests.
    pub fn with_runner(runner: Box<dyn CommandRunner>, policy: SignaturePolicy) -> Self {
        Self { runner, policy }
    }

    /// The policy this verifier was built with.
    pub fn policy(&self) -> SignaturePolicy {
        self.policy
    }
}

#[async_trait]
impl SignatureVerifier for CosignVerifier {
    async fn verify(&self, image: &ImageRef) -> Result<(), VerificationError> {
        // Identity pinning gate — see the type-level docs. Without these
        // flags cosign accepts ANY Fulcio-signed artifact, which defeats
        // the entire purpose of keyless verification.
        let identity = std::env::var(IDENTITY_ENV).ok().filter(|v| !v.is_empty());
        let oidc = std::env::var(OIDC_ENV).ok().filter(|v| !v.is_empty());
        let (identity, oidc) = match (identity, oidc) {
            (Some(i), Some(o)) => (i, o),
            _ => {
                let reason = format!(
                    "{IDENTITY_ENV} and {OIDC_ENV} must both be set so cosign can pin the trusted signer; \
                     unset means every Fulcio-issued cert would be accepted"
                );
                if matches!(self.policy, SignaturePolicy::Permissive) {
                    tracing::warn!(
                        image = %image,
                        reason = %reason,
                        "cosign identity env unset; permissive policy will skip signature verification"
                    );
                }
                return Err(VerificationError::VerifierUnavailable(reason));
            }
        };

        // We invoke cosign on the canonical image string — for digest-pinned
        // refs that resolves to `name@sha256:<hex>`, which is exactly what
        // cosign expects to verify. Refs that reach this path without a
        // digest are first-party allowlisted (`oci.io/forge/`); cosign will
        // resolve their tag to a digest itself before validating.
        let img = image.to_image_string();
        let identity_flag = format!("--certificate-identity={identity}");
        let oidc_flag = format!("--certificate-oidc-issuer={oidc}");
        let args: [&str; 4] = ["verify", &identity_flag, &oidc_flag, &img];
        let result = self.runner.run(COSIGN, &args).await;
        match result {
            Ok(outcome) if outcome.success() => Ok(()),
            Ok(outcome) => {
                let stderr = String::from_utf8_lossy(&outcome.stderr).to_string();
                Err(VerificationError::Mismatch(stderr))
            }
            // `Io` covers "binary not found" — kind == NotFound on Linux —
            // as well as other spawn failures. Distinguish them so the
            // policy layer can downgrade missing-binary to a warning while
            // still treating a real I/O blow-up as fatal.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(
                VerificationError::VerifierUnavailable(format!("cosign not on PATH: {e}")),
            ),
            Err(e) => Err(VerificationError::Io(e.to_string())),
        }
    }
}

/// Apply [`SignaturePolicy`] to a [`VerificationError`]: in
/// [`SignaturePolicy::Permissive`] mode a missing verifier is downgraded to
/// a logged warning so dev environments without cosign still function;
/// every other variant hard-fails regardless of policy.
///
/// Returns `Ok(())` if the policy permits proceeding, `Err(OciError)` if it
/// does not.
pub fn enforce_policy(
    image: &ImageRef,
    policy: SignaturePolicy,
    err: VerificationError,
) -> Result<(), OciError> {
    match (&policy, &err) {
        (SignaturePolicy::Permissive, VerificationError::VerifierUnavailable(reason)) => {
            tracing::warn!(
                image = %image,
                reason = %reason,
                "signature verifier unavailable; permissive policy permits pull"
            );
            Ok(())
        }
        _ => Err(OciError::SignatureVerificationFailed {
            image: image.to_image_string(),
            reason: err.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutcome, RecordingRunner, StubResponse};
    use std::sync::Mutex;

    fn alpine_digest() -> ImageRef {
        const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        ImageRef::parse(&format!("docker.io/library/alpine@sha256:{SHA}")).unwrap()
    }

    /// Tests that mutate `IDENTITY_ENV`/`OIDC_ENV` must serialise through
    /// this lock — otherwise the tokio multi-threaded runner interleaves
    /// `set_var` / `remove_var` calls across tests and they all race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Set the identity env vars to representative values and clear them
    /// at scope exit. Tests that need to drive cosign argv assertions use
    /// this so they're decoupled from whatever the host env happens to
    /// have set.
    struct IdentityEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl IdentityEnvGuard {
        fn set() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            std::env::set_var(IDENTITY_ENV, "ci@forge.example");
            std::env::set_var(OIDC_ENV, "https://token.actions.githubusercontent.com");
            Self { _lock: lock }
        }

        fn unset() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            std::env::remove_var(IDENTITY_ENV);
            std::env::remove_var(OIDC_ENV);
            Self { _lock: lock }
        }
    }

    impl Drop for IdentityEnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(IDENTITY_ENV);
            std::env::remove_var(OIDC_ENV);
        }
    }

    #[tokio::test]
    async fn noop_verifier_accepts_anything() {
        NoopVerifier.verify(&alpine_digest()).await.unwrap();
    }

    #[tokio::test]
    async fn cosign_verifier_invokes_cosign_with_canonical_image_string() {
        // Load-bearing: cosign must see the digest-pinned canonical form,
        // not the human-readable `name:tag`. The renderer drops the tag in
        // that case so the wire form is unambiguous.
        let _env = IdentityEnvGuard::set();
        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"verified\n".to_vec()));
        let calls = runner.calls.clone();
        let verifier = CosignVerifier::with_runner(Box::new(runner), SignaturePolicy::Strict);

        verifier.verify(&alpine_digest()).await.unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "cosign");
        assert_eq!(calls[0].1[0], "verify");
        // The image string is the LAST positional after the identity flags.
        let last = calls[0].1.last().expect("cosign argv must be non-empty");
        assert!(
            last.contains("@sha256:"),
            "cosign verify must run on the digest-pinned form, got {:?}",
            calls[0].1
        );
    }

    #[tokio::test]
    async fn cosign_verifier_surfaces_mismatch_when_cosign_exits_nonzero() {
        let _env = IdentityEnvGuard::set();
        let runner = RecordingRunner::new();
        runner.push(StubResponse {
            matches_args: None,
            outcome: CommandOutcome {
                exit_code: Some(1),
                stdout: Vec::new(),
                stderr: b"no matching signatures\n".to_vec(),
            },
        });
        let verifier = CosignVerifier::with_runner(Box::new(runner), SignaturePolicy::Strict);
        let err = verifier.verify(&alpine_digest()).await.unwrap_err();
        assert!(
            matches!(err, VerificationError::Mismatch(_)),
            "expected Mismatch, got {err:?}"
        );
    }

    #[tokio::test]
    async fn cosign_verifier_classifies_missing_binary_as_unavailable() {
        // Simulate `cosign` not on PATH by pushing a spawn-error stub. The
        // RecordingRunner uses `StubResponse::err` for non-zero exit but we
        // need a real I/O NotFound to exercise the unavailable branch —
        // emulate it via a custom runner.
        let _env = IdentityEnvGuard::set();
        struct NotFoundRunner;
        #[async_trait::async_trait]
        impl crate::runner::CommandRunner for NotFoundRunner {
            async fn run(
                &self,
                _bin: &str,
                _args: &[&str],
            ) -> std::io::Result<crate::runner::CommandOutcome> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such file",
                ))
            }
        }
        let verifier =
            CosignVerifier::with_runner(Box::new(NotFoundRunner), SignaturePolicy::Strict);
        let err = verifier.verify(&alpine_digest()).await.unwrap_err();
        assert!(
            matches!(err, VerificationError::VerifierUnavailable(_)),
            "expected VerifierUnavailable, got {err:?}"
        );
    }

    #[test]
    fn permissive_policy_downgrades_unavailable_to_ok() {
        // F-643 dev-friendliness: missing cosign in permissive mode is a
        // warning, not a block. Any other error stays fatal.
        let img = alpine_digest();
        let permitted = enforce_policy(
            &img,
            SignaturePolicy::Permissive,
            VerificationError::VerifierUnavailable("not on PATH".to_string()),
        );
        assert!(permitted.is_ok());

        let blocked = enforce_policy(
            &img,
            SignaturePolicy::Permissive,
            VerificationError::Mismatch("no signatures".to_string()),
        );
        assert!(matches!(
            blocked,
            Err(OciError::SignatureVerificationFailed { .. })
        ));
    }

    #[test]
    fn strict_policy_blocks_every_failure_including_unavailable() {
        // F-643 production stance: even a missing verifier is fatal in
        // strict mode. Production must install cosign before opting in.
        let img = alpine_digest();
        for err in [
            VerificationError::VerifierUnavailable("missing".to_string()),
            VerificationError::Mismatch("bad".to_string()),
            VerificationError::Io("kapow".to_string()),
        ] {
            let r = enforce_policy(&img, SignaturePolicy::Strict, err);
            assert!(
                matches!(r, Err(OciError::SignatureVerificationFailed { .. })),
                "strict policy must block {r:?}"
            );
        }
    }

    // ── F-643 follow-up: identity pinning gate ───────────────────────
    //
    // The default keyless cosign flow only checks that *some* Fulcio cert
    // signed the artifact — every public Sigstore signing run satisfies
    // that. The verifier MUST pass `--certificate-identity` /
    // `--certificate-oidc-issuer` so cosign rejects signatures from
    // identities the operator hasn't whitelisted.

    #[tokio::test]
    async fn cosign_verifier_passes_identity_flags_from_env() {
        let _env = IdentityEnvGuard::set();

        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"verified\n".to_vec()));
        let calls = runner.calls.clone();
        let verifier = CosignVerifier::with_runner(Box::new(runner), SignaturePolicy::Strict);

        verifier.verify(&alpine_digest()).await.unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let argv = &calls[0].1;
        assert!(
            argv.iter()
                .any(|a| a == "--certificate-identity=ci@forge.example"),
            "expected --certificate-identity flag in argv, got {argv:?}"
        );
        assert!(
            argv.iter()
                .any(|a| a
                    == "--certificate-oidc-issuer=https://token.actions.githubusercontent.com"),
            "expected --certificate-oidc-issuer flag in argv, got {argv:?}"
        );
    }

    #[tokio::test]
    async fn cosign_verifier_strict_fails_when_identity_env_missing() {
        let _env = IdentityEnvGuard::unset();

        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"verified\n".to_vec()));
        let calls = runner.calls.clone();
        let verifier = CosignVerifier::with_runner(Box::new(runner), SignaturePolicy::Strict);

        let err = verifier.verify(&alpine_digest()).await.unwrap_err();
        match &err {
            VerificationError::VerifierUnavailable(reason) => {
                assert!(
                    reason.contains(IDENTITY_ENV) && reason.contains(OIDC_ENV),
                    "error must mention both env vars so operators know what to set; got {reason}"
                );
            }
            other => panic!("expected VerifierUnavailable, got {other:?}"),
        }
        assert_eq!(
            calls.lock().unwrap().len(),
            0,
            "cosign must NOT be invoked when identity env is missing under Strict"
        );
    }

    #[tokio::test]
    async fn cosign_verifier_permissive_returns_unavailable_when_identity_env_missing() {
        // Under Permissive the verifier still returns
        // `VerifierUnavailable` so the existing `enforce_policy` plumbing
        // can downgrade to a warning. The verifier itself logs an extra
        // `tracing::warn!` so operators see the gap, but the error shape
        // matches the missing-binary path for symmetry.
        let _env = IdentityEnvGuard::unset();

        let runner = RecordingRunner::new();
        runner.push(StubResponse::ok_stdout(b"verified\n".to_vec()));
        let calls = runner.calls.clone();
        let verifier = CosignVerifier::with_runner(Box::new(runner), SignaturePolicy::Permissive);

        let err = verifier.verify(&alpine_digest()).await.unwrap_err();
        assert!(
            matches!(err, VerificationError::VerifierUnavailable(_)),
            "permissive + missing identity env must yield VerifierUnavailable, got {err:?}"
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            0,
            "cosign must NOT be invoked when identity env is missing"
        );
    }
}

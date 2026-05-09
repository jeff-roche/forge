//! F-587: Tauri command surface for per-provider credential management.
//!
//! Three commands, all gated on the `webview` feature so non-webview test
//! builds compile without dragging Tauri:
//!
//! - [`login_provider`] — write a credential into the active store.
//! - [`logout_provider`] — remove a credential.
//! - [`has_credential`] — presence probe; never returns the value.
//!
//! The Tauri-managed state is a single `Arc<dyn Credentials>` boxed under
//! [`CredentialsState`]. Production wires a `LayeredStore<KeyringStore,
//! EnvFallbackStore>`; tests can wire a `MemoryStore` directly. State
//! attachment is idempotent — see [`manage_credentials`].
//!
//! # Authorization
//!
//! Credential commands are dashboard-scoped — only the `dashboard` window
//! label is permitted to invoke them. A session window has no business
//! managing keys; routing them to the dashboard's settings panel is the
//! intended UX flow. Each command delegates to the canonical
//! `crate::ipc::require_window_label` helper (label-bound, never an open
//! invocation surface) so authz semantics and rejection logging stay in
//! lockstep with the rest of the IPC surface.
//!
//! # Logging
//!
//! Emissions in this module use `tracing::trace!` / `tracing::warn!` only.
//! The credential value is **never** in a tracing field — not even at
//! `trace`. Provider id and outcome (`hit`, `miss`, `error_kind`) are the
//! observable surface.

use std::sync::Arc;

use forge_core::{Credentials, EnvFallbackStore, LayeredStore};
#[cfg(feature = "webview")]
use secrecy::SecretString;
#[cfg(feature = "webview")]
use tauri::{AppHandle, Manager, Runtime, State, Webview};
#[allow(unused_imports)]
use tracing;

/// Window label that owns credential management. Only this label is
/// permitted to invoke the three credential commands.
pub const CREDENTIALS_OWNER_LABEL: &str = "dashboard";

/// Per-field byte cap on the inbound API key. Real provider keys are
/// short (Anthropic ~108 bytes, OpenAI ~51 bytes); 8 KiB is a comfortable
/// upper bound that still rejects a hostile renderer that loops megabyte
/// `login_provider` calls.
pub const MAX_API_KEY_BYTES: usize = 8 * 1024;

// F-675: `MAX_PROVIDER_ID_BYTES` is defined canonically in `crate::ipc` so
// the credentials and providers IPC surfaces share one cap. Re-import here
// rather than redeclaring.
use crate::ipc::MAX_PROVIDER_ID_BYTES;

/// Tauri-managed handle to the credential store. Held as an
/// `Arc<dyn Credentials>` so the same state can wrap any `Credentials`
/// implementation (production [`LayeredStore`], tests' `MemoryStore`).
pub struct CredentialsState {
    inner: Arc<dyn Credentials>,
}

impl CredentialsState {
    pub fn new(store: Arc<dyn Credentials>) -> Self {
        Self { inner: store }
    }

    /// Production wiring: keyring primary + env-var fallback. Falls back
    /// to env-only on platforms where `KeyringStore` is not compiled in
    /// (cfg-gated; see `forge_core::credentials::keyring`).
    pub fn production() -> Self {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            use forge_core::credentials::KeyringStore;
            let layered = LayeredStore::new(KeyringStore::new(), EnvFallbackStore::default());
            Self::new(Arc::new(layered))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            // Headless / unsupported targets: env-only. `set` will error
            // (read-only fallback); `get` works for env-supplied keys.
            Self::new(Arc::new(EnvFallbackStore::default()))
        }
    }

    /// Borrow the inner store. Used by `forge-session` over the IPC bridge
    /// so the orchestrator's `run_turn` can pull a credential at turn start.
    pub fn store(&self) -> Arc<dyn Credentials> {
        Arc::clone(&self.inner)
    }
}

/// Idempotent state attachment. Matches the `manage_terminals` /
/// `manage_bg_agents` pattern.
#[cfg(feature = "webview")]
pub fn manage_credentials<R: Runtime>(app: &AppHandle<R>) {
    if app.try_state::<CredentialsState>().is_none() {
        app.manage(CredentialsState::production());
    }
}

/// Test-only attachment — wires a caller-supplied store (typically
/// `MemoryStore`) so integration tests can observe `set`/`get`/`remove`
/// without touching the OS keyring.
#[cfg(feature = "webview-test")]
pub fn manage_credentials_with<R: Runtime>(app: &AppHandle<R>, store: Arc<dyn Credentials>) {
    if app.try_state::<CredentialsState>().is_none() {
        app.manage(CredentialsState::new(store));
    }
}

fn require_size(field: &'static str, value: &str, cap: usize) -> Result<(), String> {
    if value.len() > cap {
        return Err(format!(
            "{field} too large: {} bytes exceeds cap of {} bytes",
            value.len(),
            cap
        ));
    }
    Ok(())
}

/// Pure validation helper exposed for unit tests. Mirrors the body of
/// [`login_provider`] minus the Tauri-runtime-bound argument extraction.
pub fn validate_login_inputs(provider_id: &str, key: &str) -> Result<(), String> {
    require_size("provider_id", provider_id, MAX_PROVIDER_ID_BYTES)?;
    require_size("key", key, MAX_API_KEY_BYTES)?;
    if provider_id.is_empty() {
        return Err("provider_id is empty".to_string());
    }
    if key.is_empty() {
        return Err("key is empty".to_string());
    }
    Ok(())
}

/// Pure validation helper exposed for unit tests.
pub fn validate_provider_id(provider_id: &str) -> Result<(), String> {
    require_size("provider_id", provider_id, MAX_PROVIDER_ID_BYTES)?;
    if provider_id.is_empty() {
        return Err("provider_id is empty".to_string());
    }
    Ok(())
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn login_provider<R: Runtime>(
    provider_id: String,
    key: String,
    webview: Webview<R>,
    state: State<'_, CredentialsState>,
) -> Result<(), String> {
    crate::ipc::require_window_label(&webview, CREDENTIALS_OWNER_LABEL, "login_provider")?;
    validate_login_inputs(&provider_id, &key)?;

    // Wrap the inbound key in `SecretString` immediately. Past this point
    // the value is never copied into a longer-lived `String`; redaction
    // applies to every downstream `Debug` / `format!` call.
    let secret = SecretString::from(key);

    state.inner.set(&provider_id, secret).await.map_err(|e| {
        tracing::warn!(
            target: "forge_shell::credentials",
            provider_id = %provider_id,
            error = %e,
            "login_provider failed",
        );
        e.to_string()
    })?;

    tracing::trace!(
        target: "forge_shell::credentials",
        provider_id = %provider_id,
        "login_provider stored",
    );
    Ok(())
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn logout_provider<R: Runtime>(
    provider_id: String,
    webview: Webview<R>,
    state: State<'_, CredentialsState>,
) -> Result<(), String> {
    crate::ipc::require_window_label(&webview, CREDENTIALS_OWNER_LABEL, "logout_provider")?;
    validate_provider_id(&provider_id)?;

    state.inner.remove(&provider_id).await.map_err(|e| {
        tracing::warn!(
            target: "forge_shell::credentials",
            provider_id = %provider_id,
            error = %e,
            "logout_provider failed",
        );
        e.to_string()
    })?;

    tracing::trace!(
        target: "forge_shell::credentials",
        provider_id = %provider_id,
        "logout_provider removed",
    );
    Ok(())
}

#[cfg(feature = "webview")]
#[tauri::command]
pub async fn has_credential<R: Runtime>(
    provider_id: String,
    webview: Webview<R>,
    state: State<'_, CredentialsState>,
) -> Result<bool, String> {
    crate::ipc::require_window_label(&webview, CREDENTIALS_OWNER_LABEL, "has_credential")?;
    validate_provider_id(&provider_id)?;

    let present = state.inner.has(&provider_id).await.map_err(|e| {
        tracing::warn!(
            target: "forge_shell::credentials",
            provider_id = %provider_id,
            error = %e,
            "has_credential failed",
        );
        e.to_string()
    })?;

    Ok(present)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::MemoryStore;
    use secrecy::{ExposeSecret, SecretString};

    #[test]
    fn validate_login_inputs_rejects_empty_provider_id() {
        let err = validate_login_inputs("", "k").unwrap_err();
        assert!(err.contains("provider_id"));
    }

    #[test]
    fn validate_login_inputs_rejects_empty_key() {
        let err = validate_login_inputs("anthropic", "").unwrap_err();
        assert!(err.contains("key"));
    }

    #[test]
    fn validate_login_inputs_rejects_oversize_key() {
        let huge = "x".repeat(MAX_API_KEY_BYTES + 1);
        let err = validate_login_inputs("anthropic", &huge).unwrap_err();
        assert!(err.contains("key"));
        assert!(err.contains("exceeds cap"));
    }

    #[test]
    fn validate_login_inputs_rejects_oversize_provider_id() {
        let huge = "x".repeat(MAX_PROVIDER_ID_BYTES + 1);
        let err = validate_login_inputs(&huge, "k").unwrap_err();
        assert!(err.contains("provider_id"));
    }

    #[test]
    fn validate_login_inputs_accepts_realistic_keys() {
        // Anthropic key shape: ~108 bytes. OpenAI: ~51 bytes. Both fit
        // comfortably under the 8 KiB cap.
        let anthropic_shaped = format!("sk-ant-api03-{}", "a".repeat(95));
        validate_login_inputs("anthropic", &anthropic_shaped).expect("realistic anthropic key");
        let openai_shaped = format!("sk-{}", "a".repeat(48));
        validate_login_inputs("openai", &openai_shaped).expect("realistic openai key");
    }

    /// Pin the canonical authz contract for the credential commands: only
    /// the dashboard window label is admitted. The helper itself lives in
    /// `crate::ipc`; this test guards the policy choice (dashboard-only)
    /// rather than the helper's mechanics (covered by `ipc_authz_tracing`).
    #[test]
    fn credentials_dashboard_authz_policy() {
        use crate::ipc::require_window_label_for_test;

        assert!(require_window_label_for_test(
            "session-abc",
            CREDENTIALS_OWNER_LABEL,
            "login_provider"
        )
        .is_err());
        assert!(require_window_label_for_test(
            "forge://dashboard",
            CREDENTIALS_OWNER_LABEL,
            "login_provider"
        )
        .is_err());
        assert!(require_window_label_for_test(
            CREDENTIALS_OWNER_LABEL,
            CREDENTIALS_OWNER_LABEL,
            "login_provider"
        )
        .is_ok());
    }

    /// Pin the trait wiring of `CredentialsState`: the inner `Arc` round-trips
    /// `set` / `get` / `has` / `remove` cleanly. This is the contract the
    /// Tauri commands rely on.
    #[tokio::test]
    async fn credentials_state_round_trips_through_inner_store() {
        let store: Arc<dyn Credentials> = Arc::new(MemoryStore::new());
        let state = CredentialsState::new(store);

        assert!(!state.inner.has("anthropic").await.unwrap());

        state
            .inner
            .set("anthropic", SecretString::from("sk-ant-1"))
            .await
            .unwrap();
        assert!(state.inner.has("anthropic").await.unwrap());

        let got = state.inner.get("anthropic").await.unwrap().unwrap();
        assert_eq!(got.expose_secret(), "sk-ant-1");

        state.inner.remove("anthropic").await.unwrap();
        assert!(!state.inner.has("anthropic").await.unwrap());
    }
}

//! F-664 — Tauri webview CSP regression.
//!
//! The production webview reads its CSP from `tauri.conf.json`
//! (`app.security.csp`). This test pins the high-impact directives so a
//! regression that re-introduces `'unsafe-inline'` for `<style>` blocks
//! (the CSS-injection / attribute-selector exfiltration vector cited in
//! the F-664 finding) fails CI instead of shipping silently.
//!
//! What this test does *not* assert:
//! - The Vite dev server's `index.html` meta tag is not covered here;
//!   it intentionally diverges from production to keep Vite HMR working
//!   (see `web/packages/app/index.html`).
//! - The Monaco-host iframe (`web/packages/monaco-host/index.html`) has
//!   its own CSP; Monaco injects `<style>` tags at runtime and is out of
//!   scope for this directive.
//!
//! Source of truth: `crates/forge-shell/tauri.conf.json`.
//! Mirror (dev-only):  `web/packages/app/index.html` meta tag.
//! Iframe (Monaco):    `web/packages/monaco-host/index.html` meta tag.

use std::fs;
use std::path::PathBuf;

fn load_csp() -> String {
    let conf_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
    let raw = fs::read_to_string(&conf_path).expect("read tauri.conf.json");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("parse tauri.conf.json");
    v.pointer("/app/security/csp")
        .and_then(|s| s.as_str())
        .unwrap_or_else(|| panic!("tauri.conf.json missing app.security.csp"))
        .to_string()
}

fn directive<'a>(csp: &'a str, name: &str) -> Option<&'a str> {
    csp.split(';')
        .map(str::trim)
        .find(|d| d.starts_with(name) && d.as_bytes().get(name.len()) == Some(&b' '))
        .map(|d| d[name.len()..].trim())
}

#[test]
fn style_src_elem_drops_unsafe_inline() {
    let csp = load_csp();
    let elem = directive(&csp, "style-src-elem")
        .expect("CSP must declare style-src-elem; F-664 splits style-src into -elem/-attr");
    assert!(
        !elem.contains("'unsafe-inline'"),
        "style-src-elem must not allow 'unsafe-inline'; got: {elem}"
    );
    assert!(
        elem.contains("'self'"),
        "style-src-elem must allow 'self' so the bundled hashed CSS still loads; got: {elem}"
    );
}

#[test]
fn style_src_attr_drops_unsafe_inline() {
    // F-805: once the 11 catalogued JSX `style={...}` bindings are migrated
    // to ref-based `element.style.setProperty(...)` calls, `style-src-attr`
    // can drop `'unsafe-inline'` and lock to `'self'`. This pins that
    // tightening so a regression that re-introduces an inline `style="..."`
    // binding (and re-adds `'unsafe-inline'` to keep it working) fails CI.
    let csp = load_csp();
    let attr = directive(&csp, "style-src-attr")
        .expect("CSP must declare style-src-attr; F-664 splits style-src into -elem/-attr");
    assert!(
        !attr.contains("'unsafe-inline'"),
        "style-src-attr must not allow 'unsafe-inline' (F-805); got: {attr}"
    );
    assert!(
        attr.contains("'self'"),
        "style-src-attr must allow 'self'; got: {attr}"
    );
}

#[test]
fn no_top_level_style_src_unsafe_inline() {
    // `style-src` (without -elem/-attr) acts as a fallback for both. If it
    // carries `'unsafe-inline'`, the F-664 finding is reintroduced. The
    // policy must use the granular form or omit `style-src` entirely.
    let csp = load_csp();
    if let Some(style_src) = directive(&csp, "style-src") {
        assert!(
            !style_src.contains("'unsafe-inline'"),
            "top-level style-src must not allow 'unsafe-inline' (F-664); \
             use style-src-elem / style-src-attr instead. Got: {style_src}"
        );
    }
}

#[test]
fn anchor_directives_preserved() {
    // F-050 / H9 anchor directives — keep as a regression guard so the
    // F-664 rewrite of style-src does not accidentally weaken the rest
    // of the policy.
    let csp = load_csp();
    for needle in [
        "default-src 'self'",
        "script-src 'self'",
        "object-src 'none'",
        "frame-ancestors 'none'",
        "base-uri 'self'",
    ] {
        assert!(
            csp.contains(needle),
            "CSP must contain `{needle}`; got: {csp}"
        );
    }
}

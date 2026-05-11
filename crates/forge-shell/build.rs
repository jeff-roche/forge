//! Build script.
//!
//! Real Forge brand icons (PNG / ICO / ICNS) are committed under `icons/`
//! and consumed directly by the Tauri bundler — see `tauri.conf.json`.
//! Source of truth for the mark is `icons/forge-mark.svg`; regenerate the
//! raster set with ImageMagick if the SVG changes. The build script's only
//! remaining job is to invoke `tauri_build::build()`, which Cargo runs with
//! CWD set to `CARGO_MANIFEST_DIR` (the same directory `tauri.conf.json`
//! lives in), so no chdir is needed.

#[cfg(feature = "webview")]
fn main() {
    tauri_build::build();
}

#[cfg(not(feature = "webview"))]
fn main() {}

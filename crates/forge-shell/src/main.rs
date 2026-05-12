#[cfg(feature = "webview")]
fn main() {
    // Linux: GTK's icon pipeline shells out to glycin for SVG decoding,
    // and glycin spawns `bwrap` to sandbox each decode. On Fedora and
    // other distros with restrictive user-namespace policies (or a
    // saturated pid space), bwrap returns ENOMEM and GTK aborts the
    // whole process via `g_error()` — taking down the file picker and
    // the rest of the app with it. Setting the documented escape-hatch
    // env vars makes glycin decode in-process, which trades the
    // per-decode sandbox for a working file dialog. Both names are
    // honored across glycin versions; respect a pre-existing value so
    // a user/operator override still wins.
    #[cfg(target_os = "linux")]
    {
        for var in ["GLYCIN_TEST_SKIP_SANDBOX", "GLYCIN_SKIP_SANDBOX"] {
            if std::env::var_os(var).is_none() {
                std::env::set_var(var, "1");
            }
        }
    }
    forge_shell::window_manager::run().expect("forge-shell failed");
}

#[cfg(not(feature = "webview"))]
fn main() {
    eprintln!("forge-shell requires the `webview` feature to run");
    std::process::exit(1);
}

//! F-371: shipped forge-session code must not `eprintln!`.
//!
//! Source-level audit so a post-F-371 regression (someone reintroduces
//! ad-hoc stderr in `server.rs`, `bg_agents.rs`, etc.) fails CI instead of
//! silently routing around the structured-logging contract. Test-only
//! `eprintln!` inside `#[cfg(test)] mod …` blocks is tolerated.
//!
//! `main.rs` is exempted: `forged` intentionally installs no tracing
//! subscriber (scope contract — emission-only crate), so startup banners
//! and operator-facing URL disclosure stay on stderr. Runtime surface
//! (`server.rs`, `mcp.rs`, `bg_agents.rs`) is fully instrumented.

use std::fs;
use std::path::PathBuf;

/// Walk every `.rs` file under `src/` and assert no top-level `eprintln!`.
/// Lines inside `#[cfg(test)] mod tests { … }` blocks are allowed — they
/// only compile under `cargo test`, not in the shipped binary.
#[test]
fn no_eprintln_in_shipped_session_sources() {
    let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let offenders = collect_eprintln_offenders(&src_dir);
    assert!(
        offenders.is_empty(),
        "eprintln! found in shipped forge-session surface:\n{}",
        offenders.join("\n")
    );
}

/// `main.rs` hosts the daemon startup banners. `forged` installs no
/// tracing subscriber (emission-only scope contract), so these
/// operator-facing lines must reach stderr directly. Runtime surface
/// remains audited.
fn is_bin_main(path: &std::path::Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some("main.rs")
}

fn collect_eprintln_offenders(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let entries = fs::read_dir(dir).expect("read src dir");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(collect_eprintln_offenders(&path));
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        if is_bin_main(&path) {
            continue;
        }
        let src = fs::read_to_string(&path).expect("read rs file");
        let mut in_cfg_test = false;
        let mut depth: i32 = 0;
        for (idx, line) in src.lines().enumerate() {
            // Detect `#[cfg(test)]` and `#[cfg(all(test, …))]` attributes that
            // gate the immediately-following item (typically `mod tests { … }`).
            let trimmed = line.trim_start();
            if !in_cfg_test
                && (trimmed.starts_with("#[cfg(test)]")
                    || (trimmed.starts_with("#[cfg(all(") && trimmed.contains("test")))
            {
                in_cfg_test = true;
                depth = 0;
                continue;
            }
            if in_cfg_test {
                let opens = line.matches('{').count() as i32;
                let closes = line.matches('}').count() as i32;
                depth += opens;
                depth -= closes;
                // The first `{` enters the gated item; once depth returns to 0
                // after we've entered (at least one `{` seen) we exit the block.
                if depth <= 0 && opens > 0 {
                    in_cfg_test = false;
                }
                continue;
            }
            if line.contains("eprintln!") {
                out.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim()));
            }
        }
    }
    out
}

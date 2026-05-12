//! Linux-only, debug-only XDG desktop integration for `cargo tauri dev`.
//!
//! Background: on Wayland desktops (COSMIC, GNOME, KDE) the dock and
//! taskbar associate a running app with an icon by matching the app's
//! runtime `app_id` against a `.desktop` entry under
//! `$XDG_DATA_DIRS/applications`, then resolving that entry's `Icon=`
//! against the XDG icon theme. The toolkit-set window icon (via
//! `WebviewWindowBuilder::icon`) only paints the window-chrome icon —
//! the dock/taskbar never read it. In an installed bundle the deb/rpm
//! installs both the `.desktop` file and the themed icon system-wide;
//! in dev mode (`cargo tauri dev`) nothing installs anything, so the
//! dock falls back to the generic "unknown app" gray box.
//!
//! Two `app_id` values are possible at runtime:
//!   - `dev.forge-ide.forge` — when Tauri's `enableGTKAppId` successfully
//!     applies the bundle identifier as the GTK Application ID, or
//!   - `forge-shell` — the binary name, GTK's fallback via
//!     `g_get_prgname()`. This is what the dev binary actually reports
//!     today (confirmed by `ext_foreign_toplevel_list_v1` probing).
//!
//! Fix: for each id, install the icon into the user XDG icon theme at
//! `$XDG_DATA_HOME/icons/hicolor/512x512/apps/<app_id>.png` and write a
//! `.desktop` entry that references it by theme name (`Icon=<app_id>`).
//! Compositors that ignore absolute `Icon=` paths (some COSMIC builds
//! among them) still resolve themed icons correctly.

use std::path::{Path, PathBuf};

const ICON_BYTES: &[u8] = include_bytes!("../icons/512x512.png");

/// Possible Wayland `app_id` values the dev binary may report. Each id
/// becomes the `.desktop` filename, the `StartupWMClass`, and the icon
/// filename inside the user XDG icon theme.
const APP_IDS: &[&str] = &[
    // What Tauri *says* it sets via `enableGTKAppId: true`.
    "dev.forge-ide.forge",
    // What GTK falls back to (`g_get_prgname()`) in dev — the binary name.
    "forge-shell",
];

/// Materialize the dev `.desktop` entries and themed icons. Logs and
/// swallows any I/O error — the icon is a polish item and must never
/// block the app from starting.
pub fn ensure() {
    if !cfg!(debug_assertions) {
        return;
    }
    if let Err(e) = try_ensure() {
        tracing::debug!(error = %e, "dev_desktop_entry: skipped XDG icon install");
    }
}

fn try_ensure() -> std::io::Result<()> {
    let data_home = xdg_data_home()?;
    let apps_dir = data_home.join("applications");
    let icons_dir = data_home.join("icons/hicolor/512x512/apps");
    std::fs::create_dir_all(&apps_dir)?;
    std::fs::create_dir_all(&icons_dir)?;

    let exec = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "forge-shell".to_string());

    for app_id in APP_IDS {
        write_if_changed_bytes(&icons_dir.join(format!("{app_id}.png")), ICON_BYTES)?;
        let desktop = apps_dir.join(format!("{app_id}.desktop"));
        let contents = render_entry(app_id, &exec);
        write_if_changed(&desktop, &contents)?;
    }
    Ok(())
}

fn write_if_changed(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == contents {
            return Ok(());
        }
    }
    std::fs::write(path, contents)
}

fn write_if_changed_bytes(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::read(path) {
        if existing == contents {
            return Ok(());
        }
    }
    std::fs::write(path, contents)
}

fn xdg_data_home() -> std::io::Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Ok(xdg);
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME env var not set"))?;
    Ok(PathBuf::from(home).join(".local/share"))
}

fn render_entry(app_id: &str, exec: &str) -> String {
    // `Icon={app_id}` is a *theme name*, not a file path — the icon was
    // already written to `~/.local/share/icons/hicolor/512x512/apps/
    // {app_id}.png` and the compositor resolves it through the XDG icon
    // theme search. Compositors that ignore absolute Icon= paths still
    // find themed icons.
    //
    // `NoDisplay=true` is intentionally omitted: some compositors skip
    // NoDisplay entries when resolving app_id → icon. The trade-off is
    // the entry shows up in the apps menu in dev; release builds don't
    // run this module at all.
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Forge (dev)\n\
         Comment=Forge IDE running from cargo tauri dev\n\
         Exec={exec}\n\
         Icon={app_id}\n\
         Terminal=false\n\
         Categories=Development;IDE;\n\
         StartupWMClass={app_id}\n\
         StartupNotify=true\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_entry_pins_app_id_and_themed_icon() {
        let body = render_entry("dev.forge-ide.forge", "/tmp/forge-shell");
        assert!(body.contains("StartupWMClass=dev.forge-ide.forge"));
        assert!(body.contains("Exec=/tmp/forge-shell"));
        // Icon is the theme name, not an absolute path.
        assert!(body.contains("Icon=dev.forge-ide.forge\n"));
        assert!(!body.contains("Icon=/"));
    }

    #[test]
    fn render_entry_does_not_mark_no_display() {
        assert!(!render_entry("dev.forge-ide.forge", "/x").contains("NoDisplay"));
    }

    #[test]
    fn covers_both_known_dev_app_ids() {
        assert!(APP_IDS.contains(&"dev.forge-ide.forge"));
        assert!(APP_IDS.contains(&"forge-shell"));
    }

    #[test]
    fn icon_bytes_are_non_empty_png() {
        // First eight bytes of any PNG are the magic header.
        assert!(ICON_BYTES.len() > 8);
        assert_eq!(&ICON_BYTES[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn xdg_data_home_prefers_env() {
        let prev = std::env::var_os("XDG_DATA_HOME");
        unsafe { std::env::set_var("XDG_DATA_HOME", "/tmp/forge-test-xdg") };
        let got = xdg_data_home().unwrap();
        assert_eq!(got, PathBuf::from("/tmp/forge-test-xdg"));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }
}

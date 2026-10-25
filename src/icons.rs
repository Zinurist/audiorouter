//! The app's own icons, embedded in the binary and written to `$XDG_CACHE_HOME/audiorouter/icons`
//! so GTK and the tray host can look them up by name.
//!
//! The files lie directly in that directory ("unthemed" icons). A `hicolor/…` layout would need an
//! `index.theme`; without one, the AppIndicator extension's icon lookup does not find them.

use std::path::PathBuf;

pub const APP_ICON: &str = "io.github.audiorouter.AudioRouter";
pub const TRAY_ICON: &str = "io.github.audiorouter.AudioRouter-symbolic";

const FILES: [(&str, &str); 2] = [
    (APP_ICON, include_str!("../data/io.github.audiorouter.AudioRouter.svg")),
    (TRAY_ICON, include_str!("../data/io.github.audiorouter.AudioRouter-symbolic.svg")),
];

/// Write the icons (if changed) and return the theme root to add to icon search paths.
pub fn install() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"));
    let root = base.join("audiorouter/icons");
    let _ = std::fs::remove_dir_all(root.join("hicolor")); // layout of earlier versions
    if let Err(e) = std::fs::create_dir_all(&root) {
        eprintln!("audiorouter: icons: {e}");
        return None;
    }
    for (name, svg) in FILES {
        let path = root.join(format!("{name}.svg"));
        if std::fs::read_to_string(&path).ok().as_deref() != Some(svg)
            && let Err(e) = std::fs::write(&path, svg)
        {
            eprintln!("audiorouter: icons: {e}");
        }
    }
    Some(root)
}

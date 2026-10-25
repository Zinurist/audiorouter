//! XDG autostart entry that runs `audiorouter --background` at login.

use std::path::PathBuf;

fn entry_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("autostart/audiorouter.desktop")
}

pub fn is_enabled() -> bool {
    entry_path().is_file()
}

pub fn set_enabled(enabled: bool) -> std::io::Result<()> {
    let path = entry_path();
    if !enabled {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe()?;
    let exe = exe.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        path,
        format!(
            "[Desktop Entry]\nType=Application\nName=Audio Router\nComment=Apply the saved audio routing at login\nExec=\"{exe}\" --background\nIcon=audio-card\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
        ),
    )
}

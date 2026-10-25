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
    // The entry records this executable's path; a cargo build directory is rebuilt and cleaned
    // during development, so only installed binaries may be started at login.
    let comps: Vec<_> = exe.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if comps.windows(2).any(|w| w[0] == "target" && (w[1] == "debug" || w[1] == "release")) {
        return Err(std::io::Error::other(
            "this is a development build. Install it with ./install.sh, then enable Start at Login in the installed app",
        ));
    }
    // install.sh puts the app icon next to the binary.
    let icon = exe.with_file_name("audiorouter.svg");
    let icon = if icon.is_file() { icon.to_string_lossy().into_owned() } else { "audio-card".into() };
    let exe = exe.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        path,
        format!(
            "[Desktop Entry]\nType=Application\nName=Audio Router\nComment=Apply the saved audio routing at login\nExec=\"{exe}\" --background\nIcon={icon}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
        ),
    )
}

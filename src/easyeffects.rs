//! EasyEffects integration: read its "process all outputs" setting and launch it.

use std::path::PathBuf;
use std::process::{Command, Stdio};

const FLATPAK_ID: &str = "com.github.wwmm.easyeffects";

fn config_file() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("easyeffects/db/easyeffectsrc")
}

/// EasyEffects' "process all output streams" setting, if it is set explicitly.
/// `None` means "not set" (the default applies), which the caller resolves from live routing.
pub fn process_all_outputs_setting() -> Option<bool> {
    let text = std::fs::read_to_string(config_file()).ok()?;
    parse_process_all(&text)
}

fn parse_process_all(text: &str) -> Option<bool> {
    let mut section = "";
    for line in text.lines().map(str::trim) {
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s;
        } else if section == "StreamOutputs"
            && let Some(v) = line.strip_prefix("processAllOutputs=")
        {
            return Some(v.trim() == "true");
        }
    }
    None
}

fn in_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

fn spawn_detached(cmd: &mut Command) -> std::io::Result<()> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Start EasyEffects in service mode (no window). Returns a user-facing error if it is not installed.
pub fn launch() -> Result<(), String> {
    if in_path("easyeffects") {
        // EasyEffects 8 uses --service-mode; older versions use --gapplication-service.
        let help =
            Command::new("easyeffects").arg("--help").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        let flag = if help.contains("--service-mode") { "--service-mode" } else { "--gapplication-service" };
        return spawn_detached(Command::new("easyeffects").arg(flag)).map_err(|e| format!("Could not start EasyEffects: {e}"));
    }
    if in_path("flatpak")
        && Command::new("flatpak")
            .args(["info", FLATPAK_ID])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    {
        return spawn_detached(Command::new("flatpak").args(["run", FLATPAK_ID, "--service-mode"]))
            .map_err(|e| format!("Could not start EasyEffects: {e}"));
    }
    Err("EasyEffects is not installed".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_setting() {
        assert_eq!(parse_process_all("[StreamOutputs]\noutputDevice=x\n"), None);
        assert_eq!(parse_process_all("[StreamOutputs]\nprocessAllOutputs=false\n"), Some(false));
        assert_eq!(parse_process_all("[Other]\nprocessAllOutputs=false\n[StreamOutputs]\nprocessAllOutputs=true"), Some(true));
    }
}

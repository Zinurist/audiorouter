//! App preferences (not part of the audio setup). Stored as TOML next to the setup.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What the window's close button does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloseAction {
    /// Ask every time (with an option to remember the answer).
    #[default]
    Ask,
    /// Hide the window and keep running in the background (shown in the system tray).
    Background,
    Quit,
}

impl CloseAction {
    pub fn as_str(self) -> &'static str {
        match self {
            CloseAction::Ask => "ask",
            CloseAction::Background => "background",
            CloseAction::Quit => "quit",
        }
    }

    pub fn parse(s: &str) -> Option<CloseAction> {
        [CloseAction::Ask, CloseAction::Background, CloseAction::Quit].into_iter().find(|a| a.as_str() == s)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub close_action: CloseAction,
    /// Show an icon in the system tray while the app runs.
    #[serde(default = "yes")]
    pub show_tray: bool,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { close_action: CloseAction::default(), show_tray: true }
    }
}

fn path() -> PathBuf {
    crate::setup::Setup::default_path().with_file_name("settings.toml")
}

impl Settings {
    /// Missing or unreadable settings fall back to the defaults.
    pub fn load() -> Settings {
        std::fs::read_to_string(path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

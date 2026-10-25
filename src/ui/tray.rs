//! System tray icon (StatusNotifierItem, shown by GNOME's AppIndicator extension or KDE).
//!
//! ksni runs the D-Bus service on its own thread; events are forwarded to the GTK thread.

use std::path::PathBuf;

use gtk::glib;
use ksni::blocking::TrayMethods;

pub enum TrayEvent {
    Open,
    Quit,
}

struct AppTray {
    tx: async_channel::Sender<TrayEvent>,
    /// Directory with our own icon theme; without it the tray uses a stock icon.
    icon_root: Option<PathBuf>,
    status: String,
    problem: bool,
}

impl ksni::Tray for AppTray {
    fn id(&self) -> String {
        "audiorouter".into()
    }

    fn title(&self) -> String {
        "Audio Router".into()
    }

    fn icon_name(&self) -> String {
        match (self.problem, &self.icon_root) {
            (true, _) => "dialog-warning-symbolic",
            (false, Some(_)) => crate::icons::TRAY_ICON,
            (false, None) => "audio-card-symbolic",
        }
        .into()
    }

    fn icon_theme_path(&self) -> String {
        self.icon_root.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "Audio Router".into(), description: self.status.clone(), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send_blocking(TrayEvent::Open);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Open Audio Router".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send_blocking(TrayEvent::Open);
                }),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send_blocking(TrayEvent::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub struct Tray {
    handle: ksni::blocking::Handle<AppTray>,
    last: (String, bool),
}

impl Tray {
    /// Show the tray icon. Returns `None` when the desktop has no tray (no StatusNotifierWatcher).
    pub fn spawn(icon_root: Option<PathBuf>, on_event: impl Fn(TrayEvent) + 'static) -> Option<Tray> {
        let (tx, rx) = async_channel::unbounded();
        let handle = match (AppTray { tx, icon_root, status: String::new(), problem: false }).spawn() {
            Ok(h) => h,
            Err(e) => {
                eprintln!("audiorouter: no system tray available: {e}");
                return None;
            }
        };
        glib::spawn_future_local(async move {
            while let Ok(ev) = rx.recv().await {
                on_event(ev);
            }
        });
        Some(Tray { handle, last: (String::new(), false) })
    }

    pub fn shutdown(self) {
        // Do not wait for it: the D-Bus thread finishes on its own.
        drop(self.handle.shutdown());
    }

    pub fn set_status(&mut self, status: &str, problem: bool) {
        if self.last.0 == status && self.last.1 == problem {
            return;
        }
        self.last = (status.to_string(), problem);
        let status = status.to_string();
        self.handle.update(move |t| {
            t.status = status;
            t.problem = problem;
        });
    }
}

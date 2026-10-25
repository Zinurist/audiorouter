mod autostart;
mod backend;
mod easyeffects;
mod engine;
mod icons;
mod model;
mod reconcile;
mod settings;
mod setup;
mod ui;

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use engine::{Engine, EngineRef, Severity};
use settings::{CloseAction, Settings};
use ui::tray::{Tray, TrayEvent};

/// Debug builds use their own ID, so `cargo run` starts a separate instance instead of
/// attaching to an installed one that is already running.
const APP_ID: &str = if cfg!(debug_assertions) { "io.github.audiorouter.AudioRouter.Devel" } else { "io.github.audiorouter.AudioRouter" };
/// In background mode, problems still present this long after start-up are reported.
const STARTUP_GRACE: Duration = Duration::from_secs(25);

thread_local! {
    static ENGINE: OnceCell<EngineRef> = const { OnceCell::new() };
    static HOLD: RefCell<Option<gio::ApplicationHoldGuard>> = const { RefCell::new(None) };
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
    static ICON_ROOT: RefCell<Option<std::path::PathBuf>> = const { RefCell::new(None) };
    static SETTINGS: RefCell<Settings> = RefCell::new(Settings::load());
}

/// The app version `X.Y`. Cargo needs three numbers, so Cargo.toml says `X.Y.0`.
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION").strip_suffix(".0").unwrap_or(env!("CARGO_PKG_VERSION"))
}

fn engine() -> EngineRef {
    ENGINE.with(|e| e.get_or_init(Engine::start).clone())
}

fn is_background() -> bool {
    HOLD.with(|h| h.borrow().is_some())
}

fn present_window(app: &adw::Application) {
    if let Some(w) = app.windows().first() {
        w.present();
        return;
    }
    let window = ui::window::build(app, engine());
    window.connect_close_request(|w| {
        let Some(app) = w.application().and_downcast::<adw::Application>() else { return glib::Propagation::Proceed };
        match SETTINGS.with(|s| s.borrow().close_action) {
            CloseAction::Background => run_in_background(&app),
            CloseAction::Quit => app.quit(),
            CloseAction::Ask => ask_on_close(w.upcast_ref(), &app),
        }
        glib::Propagation::Stop
    });
    window.present();
    if let Some(path) = std::env::var_os("AUDIOROUTER_SCREENSHOT") {
        screenshot_and_quit(window.upcast(), path.into());
    }
}

/// Debug helper: save a PNG of the window after it has settled, then quit.
fn screenshot_and_quit(window: gtk::Widget, path: std::path::PathBuf) {
    glib::timeout_add_local_once(Duration::from_millis(2500), move || {
        let paintable = gtk::WidgetPaintable::new(Some(&window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, window.width() as f64, window.height() as f64);
        if let (Some(node), Some(renderer)) = (snapshot.to_node(), window.native().and_then(|n| n.renderer())) {
            let texture = renderer.render_texture(&node, None);
            if let Err(e) = texture.save_to_png(&path) {
                eprintln!("screenshot: {e}");
            }
        }
        if let Some(app) = window.root().and_downcast::<gtk::Window>().and_then(|w| w.application()) {
            app.quit();
        }
    });
}

/// Hide the window and keep running (shown in the tray, if there is one).
fn run_in_background(app: &adw::Application) {
    hold(app);
    for w in app.windows() {
        w.set_visible(false);
    }
    if TRAY.with(|t| t.borrow().is_none()) {
        let n = gio::Notification::new("Audio Router is running in the background");
        n.set_body(Some("Start Audio Router again to open its window."));
        app.send_notification(Some("background"), &n);
    }
}

fn save_settings(change: impl FnOnce(&mut Settings)) {
    let res = SETTINGS.with(|s| {
        let mut s = s.borrow_mut();
        change(&mut s);
        s.save()
    });
    if let Err(e) = res {
        engine().borrow().toast(&format!("Could not save settings: {e}"));
    }
}

fn set_close_action(app: &adw::Application, action: CloseAction) {
    save_settings(|s| s.close_action = action);
    if let Some(a) = app.lookup_action("close-action").and_downcast::<gio::SimpleAction>() {
        a.set_state(&action.as_str().to_variant());
    }
}

/// The close button asks whether to quit or to keep running in the tray.
fn ask_on_close(window: &gtk::Window, app: &adw::Application) {
    let dlg = adw::AlertDialog::new(
        Some("Keep Audio Router Running?"),
        Some(
            "In the background, Audio Router keeps your setup applied: it reconnects devices when they are plugged in and restarts \
             EasyEffects if needed. Connections that already exist stay in place either way.",
        ),
    );
    dlg.add_responses(&[("cancel", "Cancel"), ("quit", "Quit"), ("background", "Run in Background")]);
    dlg.set_response_appearance("background", adw::ResponseAppearance::Suggested);
    dlg.set_default_response(Some("background"));
    dlg.set_close_response("cancel");
    let remember = gtk::CheckButton::builder().label("Remember my choice").halign(gtk::Align::Center).build();
    dlg.set_extra_child(Some(&remember));
    let app = app.clone();
    dlg.connect_response(None, move |_, resp| {
        let Some(action) = CloseAction::parse(resp).filter(|a| *a != CloseAction::Ask) else { return };
        if remember.is_active() {
            set_close_action(&app, action);
        }
        match action {
            CloseAction::Quit => app.quit(),
            _ => run_in_background(&app),
        }
    });
    dlg.present(Some(window));
}

/// Keep running without a window.
fn hold(app: &adw::Application) {
    if !is_background() {
        let guard = app.hold();
        HOLD.with(|h| *h.borrow_mut() = Some(guard));
    }
}

/// Show or remove the tray icon.
fn set_tray_visible(app: &adw::Application, visible: bool) {
    if !visible {
        if let Some(t) = TRAY.with(|t| t.borrow_mut().take()) {
            t.shutdown();
        }
        return;
    }
    if TRAY.with(|t| t.borrow().is_some()) {
        return;
    }
    let icon_root = ICON_ROOT.with(|r| r.borrow().clone());
    let tray = Tray::spawn(icon_root, {
        let app = app.clone();
        move |ev| match ev {
            TrayEvent::Open => present_window(&app),
            TrayEvent::Quit => app.quit(),
        }
    });
    TRAY.with(|t| *t.borrow_mut() = tray);
    update_tray_status(&engine());
}

fn update_tray_status(engine: &EngineRef) {
    let e = engine.borrow();
    let problem = e.problems.iter().any(|p| p.severity == Severity::Error);
    let status = e.status_text();
    TRAY.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.set_status(&status, problem);
        }
    });
}

fn start_background(app: &adw::Application) {
    if is_background() {
        return;
    }
    hold(app);
    let engine = engine();
    let app = app.clone();
    glib::timeout_add_local_once(STARTUP_GRACE, move || {
        let problems = engine.borrow().problems.clone();
        if problems.is_empty() {
            return;
        }
        let body = problems.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n");
        let n = gio::Notification::new("Audio setup could not be applied");
        n.set_body(Some(&body));
        n.set_priority(gio::NotificationPriority::High);
        app.send_notification(Some("apply-failed"), &n);
        present_window(&app);
    });
}

fn main() -> glib::ExitCode {
    if let Some(out) = std::env::var_os("AUDIOROUTER_RENDER") {
        return render_png(std::path::Path::new(&out));
    }

    let app = adw::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE).build();
    app.add_main_option(
        "background",
        glib::Char::from(b'b'),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Run without a window and keep the saved setup applied (used for autostart)",
        None,
    );
    app.add_main_option("version", glib::Char::from(b'v'), glib::OptionFlags::NONE, glib::OptionArg::None, "Print the version", None);
    // Handled locally, so it works without (and does not talk to) a running instance.
    app.connect_handle_local_options(|_, opts| {
        if opts.contains("version") {
            println!("audiorouter {}", version());
            return std::ops::ControlFlow::Break(glib::ExitCode::SUCCESS);
        }
        std::ops::ControlFlow::Continue(())
    });

    app.connect_startup(|app| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        let icon_root = icons::install();
        if let (Some(root), Some(display)) = (&icon_root, gtk::gdk::Display::default()) {
            gtk::IconTheme::for_display(&display).add_search_path(root);
        }
        gtk::Window::set_default_icon_name(icons::APP_ICON);
        ICON_ROOT.with(|r| *r.borrow_mut() = icon_root);
        // Refresh an existing autostart entry (path and icon of this installed binary).
        // Development builds are refused by set_enabled and leave it alone.
        if autostart::is_enabled() {
            let _ = autostart::set_enabled(true);
        }
        let quit = gio::SimpleAction::new("quit", None);
        quit.connect_activate({
            let app = app.clone();
            move |_, _| app.quit()
        });
        app.add_action(&quit);
        app.set_accels_for_action("app.quit", &["<Control>q"]);

        let autostart = gio::SimpleAction::new_stateful("autostart", None, &autostart::is_enabled().to_variant());
        autostart.connect_change_state(|action, value| {
            let Some(enabled) = value.and_then(|v| v.get::<bool>()) else { return };
            match autostart::set_enabled(enabled) {
                Ok(()) => action.set_state(&enabled.to_variant()),
                Err(e) => engine().borrow().toast(&format!("Could not change autostart: {e}")),
            }
        });
        app.add_action(&autostart);

        let background = gio::SimpleAction::new("background", None);
        background.connect_activate({
            let app = app.clone();
            move |_, _| run_in_background(&app)
        });
        app.add_action(&background);

        let current = SETTINGS.with(|s| s.borrow().close_action);
        let close_action = gio::SimpleAction::new_stateful("close-action", Some(glib::VariantTy::STRING), &current.as_str().to_variant());
        close_action.connect_activate({
            let app = app.clone();
            move |_, value| {
                if let Some(a) = value.and_then(|v| v.get::<String>()).and_then(|s| CloseAction::parse(&s)) {
                    set_close_action(&app, a);
                }
            }
        });
        app.add_action(&close_action);

        let show_tray = SETTINGS.with(|s| s.borrow().show_tray);
        let tray_action = gio::SimpleAction::new_stateful("show-tray", None, &show_tray.to_variant());
        tray_action.connect_change_state({
            let app = app.clone();
            move |action, value| {
                let Some(visible) = value.and_then(|v| v.get::<bool>()) else { return };
                action.set_state(&visible.to_variant());
                set_tray_visible(&app, visible);
                save_settings(|s| s.show_tray = visible);
            }
        });
        app.add_action(&tray_action);

        let engine = engine();
        let weak = Rc::downgrade(&engine);
        engine.borrow_mut().subscribe(Rc::new(move || {
            if let Some(engine) = weak.upgrade() {
                update_tray_status(&engine);
            }
        }));
        if show_tray && std::env::var_os("AUDIOROUTER_SCREENSHOT").is_none() {
            set_tray_visible(app, true);
        }
    });

    app.connect_command_line(|app, cmd| {
        if cmd.options_dict().contains("background") {
            start_background(app);
        } else {
            app.activate();
        }
        glib::ExitCode::SUCCESS
    });
    app.connect_activate(present_window);
    app.run()
}

/// Debug helper: render the canvas for `AUDIOROUTER_FIXTURE` (+ `AUDIOROUTER_SETUP`) to a PNG.
fn render_png(out: &std::path::Path) -> glib::ExitCode {
    use gtk::cairo;
    let engine = Engine::start();
    engine::reconcile_now(&engine);
    let scene = ui::scene::Scene::build(&engine.borrow(), &mut HashMap::new());
    let (w, h) = (scene.width.min(1600.0), scene.height.min(1200.0));
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32).expect("surface");
    let cr = cairo::Context::new(&surface).expect("context");
    let pal = ui::scene::Palette::new(std::env::var_os("AUDIOROUTER_DARK").is_some());
    ui::scene::draw(&cr, &scene, &pal, &ui::scene::Overlay::default(), ui::scene::View::default(), w, h);
    drop(cr);
    let mut file = std::fs::File::create(out).expect("create png");
    surface.write_to_png(&mut file).expect("write png");
    for p in &engine.borrow().problems {
        eprintln!("problem: {:?} {}", p.severity, p.text);
    }
    glib::ExitCode::SUCCESS
}

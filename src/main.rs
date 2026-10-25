mod autostart;
mod backend;
mod easyeffects;
mod engine;
mod model;
mod reconcile;
mod setup;
mod ui;

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use engine::{Engine, EngineRef};

const APP_ID: &str = "io.github.audiorouter.AudioRouter";
/// In background mode, problems still present this long after start-up are reported.
const STARTUP_GRACE: Duration = Duration::from_secs(25);

thread_local! {
    static ENGINE: OnceCell<EngineRef> = const { OnceCell::new() };
    static HOLD: RefCell<Option<gio::ApplicationHoldGuard>> = const { RefCell::new(None) };
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
    window.set_hide_on_close(true);
    window.connect_close_request(|w| {
        // Without --background, closing the window quits; otherwise keep reconciling.
        if !is_background()
            && let Some(app) = w.application()
        {
            app.quit();
        }
        glib::Propagation::Proceed
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

fn start_background(app: &adw::Application) {
    if is_background() {
        return;
    }
    let guard = app.hold();
    HOLD.with(|h| *h.borrow_mut() = Some(guard));
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

    app.connect_startup(|app| {
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
    ui::scene::draw(&cr, &scene, &pal, &ui::scene::Overlay::default(), w, h);
    drop(cr);
    let mut file = std::fs::File::create(out).expect("create png");
    surface.write_to_png(&mut file).expect("write png");
    for p in &engine.borrow().problems {
        eprintln!("problem: {:?} {}", p.severity, p.text);
    }
    glib::ExitCode::SUCCESS
}

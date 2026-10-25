//! Main window: header bar, problem banner, toasts and the canvas.

use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;

use super::canvas::{self, Canvas};
use super::dialogs;
use crate::engine::{EngineRef, Severity};

const CSS: &str = "
banner.problem-error > revealer > widget { background-color: @error_bg_color; color: @error_fg_color; }
banner.problem-warning > revealer > widget { background-color: @warning_bg_color; color: @warning_fg_color; }
";

pub fn build(app: &adw::Application, engine: EngineRef) -> adw::ApplicationWindow {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

    let toasts = adw::ToastOverlay::new();
    let toast: Rc<dyn Fn(&str)> = Rc::new({
        let toasts = toasts.clone();
        move |msg: &str| {
            let t = adw::Toast::builder().title(gtk::glib::markup_escape_text(msg)).timeout(6).build();
            toasts.add_toast(t);
        }
    });
    engine.borrow_mut().set_toast_handler(toast.clone());

    let canvas = Canvas::new(engine.clone(), toast);
    toasts.set_child(Some(&canvas.area));

    let banner = adw::Banner::builder().button_label("Details").build();
    let title = adw::WindowTitle::new("Audio Router", "");
    let header = adw::HeaderBar::builder().title_widget(&title).build();

    let add_group = gtk::Button::builder().icon_name("view-grid-symbolic").tooltip_text("Add group").build();
    header.pack_start(&add_group);

    let menu = gio::Menu::new();
    let section = gio::Menu::new();
    section.append(Some("Run in Background"), Some("app.background"));
    menu.append_section(None, &section);
    let section = gio::Menu::new();
    let on_close = gio::Menu::new();
    on_close.append(Some("Ask Every Time"), Some("app.close-action::ask"));
    on_close.append(Some("Run in Background"), Some("app.close-action::background"));
    on_close.append(Some("Quit"), Some("app.close-action::quit"));
    section.append_submenu(Some("When Closing the Window"), &on_close);
    section.append(Some("Start at Login"), Some("app.autostart"));
    section.append(Some("Show in System Tray"), Some("app.show-tray"));
    menu.append_section(None, &section);
    let section = gio::Menu::new();
    section.append(Some("Help"), Some("win.help"));
    section.append(Some("Quit"), Some("app.quit"));
    menu.append_section(None, &section);
    let menu_button = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).primary(true).build();
    header.pack_end(&menu_button);
    header.pack_end(&canvas.zoom_button);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.add_top_bar(&banner);
    view.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Audio Router")
        .default_width(1200)
        .default_height(800)
        .content(&view)
        .build();

    {
        let (c, e) = (canvas.clone(), engine.clone());
        add_group.connect_clicked(move |b| canvas::add_group(b, e.clone(), c.visible_origin()));
    }

    let help = gio::SimpleAction::new("help", None);
    help.connect_activate({
        let w = window.clone();
        move |_, _| {
            dialogs::message(
                &w,
                "How to use Audio Router",
                "• Drag from the dot on the right of a box onto another box to connect them.\n\
                 • Dashed boxes are live devices that are not in your setup yet; connecting them adds them.\n\
                 • Solid lines are your setup, dashed lines were made by something else (e.g. WirePlumber).\n\
                 • Click a line and press Delete, or right-click it, to disconnect.\n\
                 • Groups: drop boxes into a group; audio sent to the group goes to all of them.\n\
                 • Right-click a box → Duplicate to put the same device into several groups.\n\
                 • Double-click a box to choose how it is matched (exact device, same model, vendor, name pattern).\n\
                 • Input devices play through their targets with a name you choose: double-click an input to rename it, \
                 and that name is shown in the system sound settings.\n\
                 • Changes apply immediately and are saved automatically.\n\
                 • Run in Background (menu) keeps the setup applied without a window (reconnecting devices, restarting \
                 EasyEffects). Start Audio Router again or click its tray icon to get the window back; Quit (Ctrl+Q) stops it.",
            )
        }
    });
    window.add_action(&help);

    banner.connect_button_clicked({
        let (w, e) = (window.clone(), engine.clone());
        move |_| {
            let text = e.borrow().problems.iter().map(|p| format!("• {}", p.text)).collect::<Vec<_>>().join("\n");
            dialogs::message(&w, "Problems", &text);
        }
    });

    let update_status = {
        let (e, banner, title) = (engine.clone(), banner.clone(), title.clone());
        move || {
            let e = e.borrow();
            let worst = e.problems.iter().map(|p| p.severity).max();
            banner.remove_css_class("problem-error");
            banner.remove_css_class("problem-warning");
            match worst {
                Some(sev) => {
                    let first = &e.problems.iter().find(|p| p.severity == sev).unwrap().text;
                    let more = e.problems.len() - 1;
                    let t = if more > 0 { format!("{first} (+{more} more)") } else { first.clone() };
                    banner.set_title(&gtk::glib::markup_escape_text(&t));
                    banner.add_css_class(if sev == Severity::Error { "problem-error" } else { "problem-warning" });
                    banner.set_revealed(true);
                }
                None => banner.set_revealed(false),
            }
            title.set_subtitle(&e.status_text());
        }
    };
    update_status();
    engine.borrow_mut().subscribe(Rc::new(update_status));

    window
}

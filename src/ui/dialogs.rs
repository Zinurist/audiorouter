//! Small dialogs: text prompt, confirmation, and node settings (selector editing).

use adw::prelude::*;
use gtk::glib;

use crate::engine::{self, EngineRef};
use crate::setup::SelectorKind;

pub fn prompt(parent: &impl IsA<gtk::Widget>, heading: &str, initial: &str, action: &str, done: impl Fn(String) + 'static) {
    let entry = gtk::Entry::builder().text(initial).activates_default(true).build();
    let dlg = adw::AlertDialog::new(Some(heading), None);
    dlg.add_responses(&[("cancel", "Cancel"), ("ok", action)]);
    dlg.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dlg.set_default_response(Some("ok"));
    dlg.set_close_response("cancel");
    dlg.set_extra_child(Some(&entry));
    dlg.connect_response(None, move |_, resp| {
        let text = entry.text().trim().to_string();
        if resp == "ok" && !text.is_empty() {
            done(text);
        }
    });
    dlg.present(Some(parent));
}

pub fn confirm(parent: &impl IsA<gtk::Widget>, heading: &str, body: &str, action: &str, done: impl Fn() + 'static) {
    let dlg = adw::AlertDialog::new(Some(heading), Some(body));
    dlg.add_responses(&[("cancel", "Cancel"), ("ok", action)]);
    dlg.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
    dlg.set_close_response("cancel");
    dlg.connect_response(None, move |_, resp| {
        if resp == "ok" {
            done();
        }
    });
    dlg.present(Some(parent));
}

pub fn message(parent: &impl IsA<gtk::Widget>, heading: &str, body: &str) {
    let dlg = adw::AlertDialog::new(Some(heading), Some(body));
    dlg.add_response("close", "Close");
    dlg.present(Some(parent));
}

/// Edit a setup node's name and how it is matched to devices.
pub fn node_settings(parent: &impl IsA<gtk::Widget>, engine: EngineRef, node_id: &str) {
    let e = engine.borrow();
    let Some(node) = e.setup.node(node_id).cloned() else { return };
    let matches: Vec<String> =
        e.analysis.node_matches.get(node_id).into_iter().flatten().map(|&u| e.analysis.units.list[u].title.clone()).collect();
    let first_match_name =
        e.analysis.node_matches.get(node_id).and_then(|m| m.first()).map(|&u| e.analysis.units.list[u].node_name.clone());
    drop(e);

    let group = adw::PreferencesGroup::new();
    let name_row = adw::EntryRow::builder().title("Name").text(&node.label).build();
    group.add(&name_row);

    let sel = node.selector.clone();
    let kinds: Vec<SelectorKind> = match (sel.kind, sel.device.is_some()) {
        (SelectorKind::Role, _) => vec![],
        (_, true) => SelectorKind::HARDWARE.to_vec(),
        (_, false) => vec![SelectorKind::Name],
    };
    let kind_row = adw::ComboRow::builder().title("Match").build();
    let pattern_row =
        adw::EntryRow::builder().title("Name pattern (regular expression)").text(sel.pattern.clone().unwrap_or_default()).build();
    let profile_row =
        adw::EntryRow::builder().title("Profile (optional, e.g. analog-stereo)").text(sel.profile.clone().unwrap_or_default()).build();
    if !kinds.is_empty() {
        let labels: Vec<&str> = kinds.iter().map(|k| k.label()).collect();
        kind_row.set_model(Some(&gtk::StringList::new(&labels)));
        kind_row.set_selected(kinds.iter().position(|k| *k == sel.kind).unwrap_or(0) as u32);
        group.add(&kind_row);
        group.add(&pattern_row);
        if sel.device.is_some() {
            group.add(&profile_row);
        }
        let update = {
            let (kinds, kind_row, pattern_row, profile_row) = (kinds.clone(), kind_row.clone(), pattern_row.clone(), profile_row.clone());
            move || {
                let k = kinds[kind_row.selected() as usize];
                pattern_row.set_visible(k == SelectorKind::Name);
                profile_row.set_visible(k != SelectorKind::Name);
            }
        };
        update();
        kind_row.connect_selected_notify(move |_| update());
    }
    if let Some(d) = &sel.device {
        let mut info = format!("{} · vendor {} · product {}", d.bus, d.vendor_id, d.product_id);
        if let Some(s) = &d.serial {
            info.push_str(&format!(" · serial {s}"));
        }
        let row =
            adw::ActionRow::builder().title(glib::markup_escape_text(&d.description)).subtitle(glib::markup_escape_text(&info)).build();
        row.add_css_class("property");
        group.add(&row);
    }
    let matches_text = if matches.is_empty() { "Nothing right now".to_string() } else { matches.join("\n") };
    let row = adw::ActionRow::builder().title("Currently matches").subtitle(glib::markup_escape_text(&matches_text)).build();
    row.add_css_class("property");
    group.add(&row);

    let dlg = adw::AlertDialog::new(Some("Node settings"), None);
    dlg.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
    dlg.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dlg.set_default_response(Some("save"));
    dlg.set_close_response("cancel");
    dlg.set_extra_child(Some(&group));
    let node_id = node_id.to_string();
    let parent_w = parent.clone().upcast::<gtk::Widget>();
    dlg.connect_response(None, move |_, resp| {
        if resp != "save" {
            return;
        }
        let mut e = engine.borrow_mut();
        let Some(n) = e.setup.node_mut(&node_id) else { return };
        let label = name_row.text().trim().to_string();
        if !label.is_empty() {
            n.label = label;
        }
        if !kinds.is_empty() {
            let k = kinds[kind_row.selected() as usize];
            n.selector.kind = k;
            if k == SelectorKind::Name {
                let mut p = pattern_row.text().trim().to_string();
                if p.is_empty() {
                    p = first_match_name.as_deref().map(|n| format!("^{}$", regex::escape(n))).unwrap_or_else(|| ".*".into());
                }
                if let Err(err) = regex::Regex::new(&p) {
                    drop(e);
                    message(&parent_w, "Invalid pattern", &err.to_string());
                    return;
                }
                n.selector.pattern = Some(p);
            } else {
                let prof = profile_row.text().trim().to_string();
                n.selector.profile = (!prof.is_empty()).then_some(prof);
            }
        }
        drop(e);
        engine::setup_changed(&engine);
    });
    dlg.present(Some(parent));
}

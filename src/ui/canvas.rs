//! Interactive graph canvas: dragging nodes and groups, drawing connections, context menus.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

use super::dialogs;
use super::scene::{EdgeTarget, ItemKey, Overlay, Palette, Rect, SOCKET_R, Scene, draw};
use crate::engine::{self, Endpoint, EngineRef};
use crate::model::UnitKind;
use crate::setup::{Group, Placement, Setup};

type MenuEntries = Vec<(String, Box<dyn Fn()>)>;

enum Drag {
    None,
    Click,
    Item { key: ItemKey, start: [f64; 2] },
    Group { id: String, start: [f64; 4], members: Vec<(String, [f64; 2])> },
    Resize { id: String, start: [f64; 4] },
    Connect { from: Endpoint, from_pos: [f64; 2], socket: (f64, f64) },
}

struct State {
    engine: EngineRef,
    scene: Scene,
    auto_pos: HashMap<String, [f64; 2]>,
    overlay: Overlay,
    drag: Drag,
    drag_origin: (f64, f64),
    animating: bool,
}

#[derive(Clone)]
pub struct Canvas {
    pub area: gtk::DrawingArea,
    state: Rc<RefCell<State>>,
}

enum Hit {
    OutSocket(Endpoint, [f64; 2], (f64, f64)),
    Item(usize),
    GroupHeader(usize),
    GroupResize(usize),
    GroupBody(usize),
    Edge(EdgeTarget),
    Nothing,
}

fn snap(v: f64) -> f64 {
    (v / 8.0).round() * 8.0
}

/// Placements belong to the group whose box contains their centre (smallest box wins).
fn refresh_membership(setup: &mut Setup) {
    let groups: Vec<Group> = setup.groups.clone();
    for p in &mut setup.placements {
        let (cx, cy) = (p.pos[0] + super::scene::ITEM_W / 2.0, p.pos[1] + super::scene::HEADER_H / 2.0);
        p.group = groups
            .iter()
            .filter(|g| cx >= g.rect[0] && cx <= g.rect[0] + g.rect[2] && cy >= g.rect[1] && cy <= g.rect[1] + g.rect[3])
            .min_by(|a, b| (a.rect[2] * a.rect[3]).total_cmp(&(b.rect[2] * b.rect[3])))
            .map(|g| g.id.clone());
    }
}

impl Canvas {
    pub fn new(engine: EngineRef, toast: Rc<dyn Fn(&str)>) -> Canvas {
        let area = gtk::DrawingArea::builder().focusable(true).can_focus(true).build();
        let state = Rc::new(RefCell::new(State {
            engine: engine.clone(),
            scene: Scene::default(),
            auto_pos: HashMap::new(),
            overlay: Overlay::default(),
            drag: Drag::None,
            drag_origin: (0.0, 0.0),
            animating: false,
        }));
        let canvas = Canvas { area, state };

        canvas.area.set_draw_func({
            let c = canvas.clone();
            move |_, cr, w, h| {
                let mut st = c.state.borrow_mut();
                st.overlay.phase = glib::monotonic_time() as f64 / 1e6;
                let pal = Palette::new(adw::StyleManager::default().is_dark());
                draw(cr, &st.scene, &pal, &st.overlay, w as f64, h as f64);
                if st.scene.has_pending() && !st.animating {
                    st.animating = true;
                    let c2 = c.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_millis(40), move || {
                        c2.state.borrow_mut().animating = false;
                        c2.area.queue_draw();
                    });
                }
            }
        });

        adw::StyleManager::default().connect_dark_notify({
            let c = canvas.clone();
            move |_| c.area.queue_draw()
        });

        engine.borrow_mut().subscribe(Rc::new({
            let c = canvas.clone();
            move || c.refresh()
        }));

        canvas.setup_drag(toast.clone());
        canvas.setup_clicks(toast.clone());
        canvas.setup_keys(toast);
        canvas.refresh();
        canvas
    }

    /// Rebuild the scene from the engine and redraw.
    pub fn refresh(&self) {
        let mut st = self.state.borrow_mut();
        let engine = st.engine.clone();
        let e = engine.borrow();
        let scene = Scene::build(&e, &mut st.auto_pos);
        self.area.set_content_width(scene.width as i32);
        self.area.set_content_height(scene.height as i32);
        if let Some(sel) = &st.overlay.selected_edge
            && !scene.edges.iter().any(|ed| &ed.target == sel)
        {
            st.overlay.selected_edge = None;
        }
        st.scene = scene;
        drop(st);
        self.area.queue_draw();
    }

    fn engine(&self) -> EngineRef {
        self.state.borrow().engine.clone()
    }

    fn hit(&self, x: f64, y: f64) -> Hit {
        let st = self.state.borrow();
        let sc = &st.scene;
        let near = |p: (f64, f64)| ((p.0 - x).powi(2) + (p.1 - y).powi(2)).sqrt() <= SOCKET_R + 5.0;
        for it in sc.items.iter().rev() {
            if it.has_out && near(it.out_socket()) {
                return Hit::OutSocket(it.endpoint.clone(), [it.rect.x, it.rect.y], it.out_socket());
            }
        }
        for g in sc.groups.iter().rev() {
            if near(g.out_socket()) {
                return Hit::OutSocket(Endpoint::Setup(g.id.clone()), [g.rect.x, g.rect.y], g.out_socket());
            }
        }
        if let Some(i) = sc.items.iter().rposition(|it| it.rect.contains(x, y)) {
            return Hit::Item(i);
        }
        if let Some(i) = sc.groups.iter().rposition(|g| g.resize_handle().contains(x, y)) {
            return Hit::GroupResize(i);
        }
        if let Some(i) = sc.groups.iter().rposition(|g| g.header().contains(x, y)) {
            return Hit::GroupHeader(i);
        }
        let best = sc.edges.iter().map(|e| (e.distance(x, y), e)).filter(|(d, _)| *d < 7.0).min_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((_, e)) = best {
            return Hit::Edge(e.target.clone());
        }
        if let Some(i) = sc.groups.iter().rposition(|g| g.rect.contains(x, y)) {
            return Hit::GroupBody(i);
        }
        Hit::Nothing
    }

    /// Item or group under the pointer that can receive a connection.
    fn drop_target(&self, x: f64, y: f64) -> Option<(Endpoint, [f64; 2], Rect)> {
        let st = self.state.borrow();
        if let Some(it) =
            st.scene.items.iter().rev().find(|it| {
                it.rect.contains(x, y) || (it.has_in && Rect { x: it.rect.x - 12.0, y: it.rect.y, w: 24.0, h: 48.0 }.contains(x, y))
            })
        {
            return it.has_in.then(|| (it.endpoint.clone(), [it.rect.x, it.rect.y], it.rect));
        }
        st.scene.groups.iter().rev().find(|g| g.rect.contains(x, y)).map(|g| (Endpoint::Setup(g.id.clone()), [g.rect.x, g.rect.y], g.rect))
    }

    fn setup_drag(&self, toast: Rc<dyn Fn(&str)>) {
        let drag = gtk::GestureDrag::builder().button(gdk::BUTTON_PRIMARY).build();
        drag.connect_drag_begin({
            let c = self.clone();
            move |_, x, y| {
                c.area.grab_focus();
                let hit = c.hit(x, y);
                let engine = c.engine();
                let e = engine.borrow();
                let mut st = c.state.borrow_mut();
                st.drag_origin = (x, y);
                st.drag = match hit {
                    Hit::OutSocket(from, from_pos, socket) => Drag::Connect { from, from_pos, socket },
                    Hit::Item(i) => {
                        let it = &st.scene.items[i];
                        Drag::Item { key: it.key.clone(), start: [it.rect.x, it.rect.y] }
                    }
                    Hit::GroupHeader(i) => {
                        let g = &st.scene.groups[i];
                        let members = e
                            .setup
                            .placements
                            .iter()
                            .filter(|p| p.group.as_deref() == Some(g.id.as_str()))
                            .map(|p| (p.id.clone(), p.pos))
                            .collect();
                        Drag::Group { id: g.id.clone(), start: [g.rect.x, g.rect.y, g.rect.w, g.rect.h], members }
                    }
                    Hit::GroupResize(i) => {
                        let g = &st.scene.groups[i];
                        Drag::Resize { id: g.id.clone(), start: [g.rect.x, g.rect.y, g.rect.w, g.rect.h] }
                    }
                    _ => Drag::Click,
                };
            }
        });
        drag.connect_drag_update({
            let c = self.clone();
            move |_, ox, oy| {
                let engine = c.engine();
                let (x0, y0) = c.state.borrow().drag_origin;
                let (x, y) = (x0 + ox, y0 + oy);
                let mut target_rect = None;
                {
                    let st = c.state.borrow();
                    let mut e = engine.borrow_mut();
                    match &st.drag {
                        Drag::Item { key, start } => {
                            let pos = [snap((start[0] + ox).max(0.0)), snap((start[1] + oy).max(0.0))];
                            match key {
                                ItemKey::Placement(pid) => {
                                    if let Some(p) = e.setup.placements.iter_mut().find(|p| &p.id == pid) {
                                        p.pos = pos;
                                    }
                                }
                                ItemKey::Unit(k) => {
                                    e.setup.layout.insert(k.clone(), pos);
                                }
                            }
                        }
                        Drag::Group { id, start, members } => {
                            let (dx, dy) = (snap(ox), snap(oy));
                            if let Some(g) = e.setup.groups.iter_mut().find(|g| &g.id == id) {
                                g.rect[0] = (start[0] + dx).max(0.0);
                                g.rect[1] = (start[1] + dy).max(0.0);
                            }
                            for (pid, pos) in members {
                                if let Some(p) = e.setup.placements.iter_mut().find(|p| &p.id == pid) {
                                    p.pos = [pos[0] + dx, pos[1] + dy];
                                }
                            }
                        }
                        Drag::Resize { id, start } => {
                            if let Some(g) = e.setup.groups.iter_mut().find(|g| &g.id == id) {
                                g.rect[2] = snap((start[2] + ox).max(280.0));
                                g.rect[3] = snap((start[3] + oy).max(140.0));
                            }
                        }
                        Drag::Connect { .. } => {}
                        Drag::Click | Drag::None => return,
                    }
                }
                let connecting = matches!(c.state.borrow().drag, Drag::Connect { .. });
                if connecting {
                    target_rect = c.drop_target(x, y).map(|t| t.2);
                } else {
                    c.refresh();
                }
                let mut st = c.state.borrow_mut();
                if let Drag::Connect { socket, .. } = &st.drag {
                    st.overlay.rubber_band = Some((*socket, (x, y)));
                }
                st.overlay.drop_target = target_rect;
                drop(st);
                c.area.queue_draw();
            }
        });
        drag.connect_drag_end({
            let c = self.clone();
            move |_, ox, oy| {
                let engine = c.engine();
                let (x0, y0) = c.state.borrow().drag_origin;
                let (x, y) = (x0 + ox, y0 + oy);
                let drag = std::mem::replace(&mut c.state.borrow_mut().drag, Drag::None);
                {
                    let mut st = c.state.borrow_mut();
                    st.overlay.rubber_band = None;
                    st.overlay.drop_target = None;
                }
                let moved = ox.abs() > 3.0 || oy.abs() > 3.0;
                match drag {
                    Drag::Connect { from, from_pos, .. } => {
                        if let Some((to, to_pos, _)) = c.drop_target(x, y) {
                            let res = engine.borrow_mut().connect(&from, &to, from_pos, to_pos);
                            match res {
                                Ok(()) => {
                                    refresh_membership(&mut engine.borrow_mut().setup);
                                    engine::setup_changed(&engine);
                                }
                                Err(msg) => {
                                    toast(&msg);
                                    c.refresh();
                                }
                            }
                        } else {
                            c.area.queue_draw();
                        }
                    }
                    Drag::Item { .. } | Drag::Group { .. } | Drag::Resize { .. } if moved => {
                        refresh_membership(&mut engine.borrow_mut().setup);
                        engine::setup_changed(&engine);
                    }
                    Drag::Click | Drag::Item { .. } | Drag::Group { .. } | Drag::Resize { .. } => {
                        let sel = match c.hit(x, y) {
                            Hit::Edge(t) => Some(t),
                            _ => None,
                        };
                        c.state.borrow_mut().overlay.selected_edge = sel;
                        c.area.queue_draw();
                    }
                    Drag::None => {}
                }
            }
        });
        self.area.add_controller(drag);
    }

    fn setup_keys(&self, toast: Rc<dyn Fn(&str)>) {
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let c = self.clone();
            move |_, key, _, _| {
                if matches!(key, gdk::Key::Delete | gdk::Key::BackSpace | gdk::Key::KP_Delete) {
                    let sel = c.state.borrow().overlay.selected_edge.clone();
                    if let Some(t) = sel {
                        c.remove_edge(&t, &toast);
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            }
        });
        self.area.add_controller(keys);
    }

    fn remove_edge(&self, t: &EdgeTarget, toast: &Rc<dyn Fn(&str)>) {
        let engine = self.engine();
        match t {
            EdgeTarget::Setup(id) => {
                engine.borrow_mut().setup.links.retain(|l| &l.id != id);
                engine::setup_changed(&engine);
            }
            EdgeTarget::Live { from, to, links } => {
                let res = engine.borrow_mut().remove_live_links(*from, *to, links);
                if let Err(m) = res {
                    toast(&m);
                }
            }
        }
        self.state.borrow_mut().overlay.selected_edge = None;
        self.refresh();
    }

    fn setup_clicks(&self, toast: Rc<dyn Fn(&str)>) {
        let right = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        right.connect_pressed({
            let c = self.clone();
            let toast = toast.clone();
            move |_, _, x, y| c.context_menu(x, y, &toast)
        });
        self.area.add_controller(right);

        let double = gtk::GestureClick::builder().button(gdk::BUTTON_PRIMARY).build();
        double.connect_pressed({
            let c = self.clone();
            move |_, n, x, y| {
                if n != 2 {
                    return;
                }
                if let Hit::Item(i) = c.hit(x, y) {
                    let ep = c.state.borrow().scene.items[i].endpoint.clone();
                    if let Endpoint::Setup(nid) = ep {
                        dialogs::node_settings(&c.area, c.engine(), &nid);
                    }
                }
            }
        });
        self.area.add_controller(double);
    }

    fn context_menu(&self, x: f64, y: f64, toast: &Rc<dyn Fn(&str)>) {
        let engine = self.engine();
        let mut entries: MenuEntries = vec![];
        let c = self.clone();
        let hit = self.hit(x, y);
        // A right-click on an output socket counts as a click on its item.
        let item_idx = match &hit {
            Hit::Item(i) => Some(*i),
            Hit::OutSocket(..) => self.state.borrow().scene.items.iter().rposition(|it| it.rect.contains(x - 10.0, y)),
            _ => None,
        };
        match hit {
            _ if item_idx.is_some() => {
                let item = self.state.borrow().scene.items[item_idx.unwrap()].clone();
                let e = engine.borrow();
                // Default device actions for the units this item shows.
                for &u in &item.units {
                    let unit = &e.analysis.units.list[u];
                    let sink = match unit.kind {
                        UnitKind::HwSink | UnitKind::VirtSink => true,
                        UnitKind::HwSource | UnitKind::VirtSource => false,
                        _ => continue,
                    };
                    let label = if item.units.len() > 1 {
                        format!("Use “{}” as default {}", unit.title, if sink { "output" } else { "input" })
                    } else {
                        format!("Use as default {}", if sink { "output" } else { "input" })
                    };
                    let name = unit.node_name.clone();
                    let en = engine.clone();
                    entries.push((label, Box::new(move || en.borrow().set_default(&name, sink))));
                }
                match (&item.key, &item.endpoint) {
                    (ItemKey::Placement(pid), Endpoint::Setup(nid)) => {
                        let (en, nid2, area) = (engine.clone(), nid.clone(), self.area.clone());
                        entries.insert(0, ("Settings…".into(), Box::new(move || dialogs::node_settings(&area, en.clone(), &nid2))));
                        let (en, nid2, pos) = (engine.clone(), nid.clone(), [item.rect.x + 32.0, item.rect.y + 32.0]);
                        entries.push((
                            "Duplicate".into(),
                            Box::new(move || {
                                let mut e = en.borrow_mut();
                                let id = e.setup.new_id("p");
                                e.setup.placements.push(Placement { id, node: nid2.clone(), pos, group: None });
                                refresh_membership(&mut e.setup);
                                drop(e);
                                engine::setup_changed(&en);
                            }),
                        ));
                        let copies = e.setup.placements.iter().filter(|p| &p.node == nid).count();
                        let label = if copies > 1 { "Remove this copy" } else { "Remove from setup" };
                        let (en, pid2) = (engine.clone(), pid.clone());
                        entries.push((
                            label.into(),
                            Box::new(move || {
                                en.borrow_mut().setup.remove_placement(&pid2);
                                engine::setup_changed(&en);
                            }),
                        ));
                    }
                    (ItemKey::Unit(key), _) => {
                        let unit = e.analysis.units.list.iter().find(|u| &u.key == key).cloned();
                        if let Some(UnitKind::Loopback { ours: None, module_id: Some(m) }) = unit.as_ref().map(|u| u.kind.clone()) {
                            let (en, area) = (engine.clone(), self.area.clone());
                            entries.push((
                                "Remove this loopback…".into(),
                                Box::new(move || {
                                    let en = en.clone();
                                    dialogs::confirm(
                                        &area,
                                        "Remove loopback?",
                                        "This unloads the loopback module from the sound server.",
                                        "Remove",
                                        move || en.borrow().unload_module(m),
                                    );
                                }),
                            ));
                        } else if !unit.as_ref().is_some_and(|u| u.kind.is_app_stream()) {
                            let (en, key2, pos, t) = (engine.clone(), key.clone(), [item.rect.x, item.rect.y], toast.clone());
                            entries.insert(
                                0,
                                (
                                    "Add to setup".into(),
                                    Box::new(move || {
                                        let res = en.borrow_mut().adopt(&key2, pos);
                                        match res {
                                            Ok(_) => {
                                                refresh_membership(&mut en.borrow_mut().setup);
                                                engine::setup_changed(&en);
                                            }
                                            Err(m) => t(&m),
                                        }
                                    }),
                                ),
                            );
                        }
                    }
                    _ => {}
                }
            }
            Hit::GroupHeader(i) | Hit::GroupBody(i) | Hit::GroupResize(i) => {
                let g = self.state.borrow().scene.groups[i].clone();
                let (en, id, name, area) = (engine.clone(), g.id.clone(), g.name.clone(), self.area.clone());
                entries.push((
                    "Rename…".into(),
                    Box::new(move || {
                        let (en, id) = (en.clone(), id.clone());
                        dialogs::prompt(&area, "Rename group", &name, "Rename", move |text| {
                            if let Some(g) = en.borrow_mut().setup.groups.iter_mut().find(|g| g.id == id) {
                                g.name = text;
                            }
                            engine::setup_changed(&en);
                        });
                    }),
                ));
                let (en, id) = (engine.clone(), g.id.clone());
                entries.push((
                    "Delete group".into(),
                    Box::new(move || {
                        en.borrow_mut().setup.remove_group(&id);
                        engine::setup_changed(&en);
                    }),
                ));
                self.push_canvas_entries(&mut entries, x, y);
            }
            Hit::Edge(t) => {
                c.state.borrow_mut().overlay.selected_edge = Some(t.clone());
                c.area.queue_draw();
                let (c2, toast) = (self.clone(), toast.clone());
                entries.push(("Remove connection".into(), Box::new(move || c2.remove_edge(&t, &toast))));
            }
            Hit::Nothing | Hit::OutSocket(..) | Hit::Item(_) => self.push_canvas_entries(&mut entries, x, y),
        }
        if !entries.is_empty() {
            popup_menu(&self.area, x, y, entries);
        }
    }

    fn push_canvas_entries(&self, entries: &mut MenuEntries, x: f64, y: f64) {
        let (c, area) = (self.clone(), self.area.clone());
        entries.push(("Add group here…".into(), Box::new(move || add_group(&area, c.engine(), [snap(x), snap(y)]))));
        let (c, area) = (self.clone(), self.area.clone());
        entries.push(("Add loopback here…".into(), Box::new(move || add_loopback(&area, c.engine(), [snap(x), snap(y)]))));
    }

    /// Top-left of the visible part of the canvas (for placing new things).
    pub fn visible_origin(&self) -> [f64; 2] {
        let mut p = [40.0, 40.0];
        if let Some(sw) = self.area.ancestor(gtk::ScrolledWindow::static_type()).and_downcast::<gtk::ScrolledWindow>() {
            p = [snap(sw.hadjustment().value() + 40.0), snap(sw.vadjustment().value() + 40.0)];
        }
        p
    }
}

pub fn add_group(parent: &impl IsA<gtk::Widget>, engine: EngineRef, pos: [f64; 2]) {
    dialogs::prompt(parent, "New group", "Speakers", "Add", move |name| {
        let mut e = engine.borrow_mut();
        let id = e.setup.new_id("g");
        e.setup.groups.push(Group { id, name, rect: [pos[0], pos[1], 320.0, 240.0] });
        refresh_membership(&mut e.setup);
        drop(e);
        engine::setup_changed(&engine);
    });
}

pub fn add_loopback(parent: &impl IsA<gtk::Widget>, engine: EngineRef, pos: [f64; 2]) {
    dialogs::prompt(parent, "New loopback", "Line in", "Add", move |name| {
        engine.borrow_mut().add_loopback(&name, pos);
        engine::setup_changed(&engine);
    });
}

fn popup_menu(area: &gtk::DrawingArea, x: f64, y: f64, entries: MenuEntries) {
    let popover = gtk::Popover::builder().has_arrow(false).position(gtk::PositionType::Bottom).halign(gtk::Align::Start).build();
    popover.add_css_class("menu");
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for (label, action) in entries {
        let b = gtk::Button::builder().child(&gtk::Label::builder().label(&label).xalign(0.0).build()).css_classes(["flat"]).build();
        let p = popover.clone();
        b.connect_clicked(move |_| {
            p.popdown();
            action();
        });
        list.append(&b);
    }
    popover.set_child(Some(&list));
    popover.set_parent(area);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    popover.popup();
}

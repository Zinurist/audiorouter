//! Canvas scene: computes item, group and edge geometry from the engine state, and draws it
//! with cairo. Used by the interactive canvas and by the offline PNG renderer.

use std::collections::HashMap;

use gtk::{cairo, pango};

use crate::engine::{Endpoint, Engine, LinkUi};
use crate::model::UnitKind;

pub const ITEM_W: f64 = 260.0;
pub const HEADER_H: f64 = 48.0;
pub const LINE_H: f64 = 18.0;
pub const GROUP_HEADER_H: f64 = 32.0;
pub const SOCKET_R: f64 = 7.0;
pub const RESIZE_HANDLE: f64 = 16.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ItemKey {
    Placement(String),
    Unit(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineStyle {
    Normal,
    Dim,
    Bad,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub key: ItemKey,
    pub endpoint: Endpoint,
    pub rect: Rect,
    pub title: String,
    pub badge: String,
    pub lines: Vec<(String, LineStyle)>,
    pub has_in: bool,
    pub has_out: bool,
    /// Part of the saved setup (solid) or just a live node (dashed).
    pub managed: bool,
    pub units: Vec<usize>,
}

impl Item {
    pub fn in_socket(&self) -> (f64, f64) {
        (self.rect.x, self.rect.y + HEADER_H / 2.0)
    }
    pub fn out_socket(&self) -> (f64, f64) {
        (self.rect.x + self.rect.w, self.rect.y + HEADER_H / 2.0)
    }
}

#[derive(Clone, Debug)]
pub struct GroupBox {
    pub id: String,
    pub name: String,
    pub rect: Rect,
    pub live_members: usize,
}

impl GroupBox {
    pub fn in_socket(&self) -> (f64, f64) {
        (self.rect.x, self.rect.y + GROUP_HEADER_H / 2.0)
    }
    pub fn out_socket(&self) -> (f64, f64) {
        (self.rect.x + self.rect.w, self.rect.y + GROUP_HEADER_H / 2.0)
    }
    pub fn header(&self) -> Rect {
        Rect { h: GROUP_HEADER_H, ..self.rect }
    }
    pub fn resize_handle(&self) -> Rect {
        Rect {
            x: self.rect.x + self.rect.w - RESIZE_HANDLE,
            y: self.rect.y + self.rect.h - RESIZE_HANDLE,
            w: RESIZE_HANDLE,
            h: RESIZE_HANDLE,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeStyle {
    Setup(LinkUi),
    /// Live link made by another tool (WirePlumber, EasyEffects, …).
    Foreign,
    /// Live link the app made that is no longer wanted (about to be removed).
    Stale,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeTarget {
    Setup(String),
    Live { from: usize, to: usize, links: Vec<u32> },
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub style: EdgeStyle,
    pub target: EdgeTarget,
}

impl Edge {
    fn controls(&self) -> ((f64, f64), (f64, f64)) {
        let d = ((self.to.0 - self.from.0).abs() / 2.0).max(60.0);
        ((self.from.0 + d, self.from.1), (self.to.0 - d, self.to.1))
    }

    pub fn distance(&self, x: f64, y: f64) -> f64 {
        let (c1, c2) = self.controls();
        let mut best = f64::MAX;
        for i in 0..=32 {
            let t = i as f64 / 32.0;
            let mt = 1.0 - t;
            let px = mt * mt * mt * self.from.0 + 3.0 * mt * mt * t * c1.0 + 3.0 * mt * t * t * c2.0 + t * t * t * self.to.0;
            let py = mt * mt * mt * self.from.1 + 3.0 * mt * mt * t * c1.1 + 3.0 * mt * t * t * c2.1 + t * t * t * self.to.1;
            best = best.min(((px - x).powi(2) + (py - y).powi(2)).sqrt());
        }
        best
    }
}

#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub items: Vec<Item>,
    pub groups: Vec<GroupBox>,
    pub edges: Vec<Edge>,
    pub width: f64,
    pub height: f64,
}

fn column_of(kind: &UnitKind) -> usize {
    match kind {
        UnitKind::HwSource | UnitKind::VirtSource | UnitKind::AppOut | UnitKind::EeIn => 0,
        UnitKind::Loopback { .. } => 1,
        UnitKind::EeOut => 2,
        UnitKind::HwSink | UnitKind::VirtSink | UnitKind::AppIn => 3,
    }
}

const COLUMN_X: [f64; 4] = [40.0, 380.0, 720.0, 1060.0];

impl Scene {
    /// Build the scene. `auto_pos` remembers positions picked for live nodes that the user has not
    /// placed yet, so they do not jump around.
    pub fn build(e: &Engine, auto_pos: &mut HashMap<String, [f64; 2]>) -> Scene {
        let a = &e.analysis;
        let s = &e.setup;
        let mut scene = Scene::default();
        let default_sink = e.live.default_node_name(true);
        let default_source = e.live.default_node_name(false);
        let unit_lines = |idx: usize| -> (String, LineStyle) {
            let u = &a.units.list[idx];
            let mut t = u.title.clone();
            if Some(&u.node_name) == default_sink.as_ref() || Some(&u.node_name) == default_source.as_ref() {
                t.push_str("  ★ default");
            }
            (t, LineStyle::Normal)
        };

        for g in &s.groups {
            let live_members = a.endpoint_units(s, &g.id).len();
            scene.groups.push(GroupBox {
                id: g.id.clone(),
                name: g.name.clone(),
                rect: Rect { x: g.rect[0], y: g.rect[1], w: g.rect[2], h: g.rect[3] },
                live_members,
            });
        }

        for p in &s.placements {
            let Some(n) = s.node(&p.node) else { continue };
            let matches = a.node_matches.get(&n.id).cloned().unwrap_or_default();
            let mut lines: Vec<(String, LineStyle)> = matches.iter().map(|&i| unit_lines(i)).collect();
            if lines.is_empty() {
                lines.push(("Not connected".into(), LineStyle::Bad));
            }
            if n.selector.role.as_deref() == Some("easyeffects:output") && a.ee_grabs_outputs {
                lines.push(("Processes all app audio".into(), LineStyle::Dim));
            }
            let mut badge = n.selector.kind.short().to_string();
            if n.selector.role.as_deref().is_some_and(|r| r.starts_with("easyeffects:")) {
                badge = "effects".into();
            }
            if matches.len() > 1 {
                badge.push_str(&format!(" · {} connected", matches.len()));
            }
            let h = HEADER_H + LINE_H * lines.len() as f64 + 10.0;
            scene.items.push(Item {
                key: ItemKey::Placement(p.id.clone()),
                endpoint: Endpoint::Setup(n.id.clone()),
                rect: Rect { x: p.pos[0], y: p.pos[1], w: ITEM_W, h },
                title: n.label.clone(),
                badge,
                lines,
                has_in: e.can_input(&n.id),
                has_out: e.can_output(&n.id),
                managed: true,
                units: matches,
            });
        }

        // Live units that are not in the setup.
        auto_pos.retain(|k, _| a.units.list.iter().any(|u| &u.key == k));
        for (i, u) in a.units.list.iter().enumerate() {
            if a.managed_units.contains(&i) || a.hidden_units.contains(&i) {
                continue;
            }
            let mut lines = vec![];
            if u.kind == UnitKind::EeOut && a.ee_grabs_outputs {
                lines.push(("Processes all app audio".into(), LineStyle::Dim));
            }
            if Some(&u.node_name) == default_sink.as_ref() || Some(&u.node_name) == default_source.as_ref() {
                lines.push(("★ default".into(), LineStyle::Dim));
            }
            let h = HEADER_H + LINE_H * lines.len() as f64 + if lines.is_empty() { 0.0 } else { 10.0 };
            let pos = match s.layout.get(&u.key).or(auto_pos.get(&u.key)) {
                Some(p) => *p,
                None => {
                    let col = column_of(&u.kind);
                    let x = COLUMN_X[col];
                    let y = scene
                        .items
                        .iter()
                        .map(|it| it.rect)
                        .chain(scene.groups.iter().map(|g| g.rect))
                        .filter(|r| r.x < x + ITEM_W && r.x + r.w > x)
                        .map(|r| r.y + r.h + 18.0)
                        .fold(40.0, f64::max);
                    auto_pos.insert(u.key.clone(), [x, y]);
                    [x, y]
                }
            };
            scene.items.push(Item {
                key: ItemKey::Unit(u.key.clone()),
                endpoint: Endpoint::Unit(u.key.clone()),
                rect: Rect { x: pos[0], y: pos[1], w: ITEM_W, h },
                title: u.title.clone(),
                badge: u.subtitle.clone(),
                lines,
                has_in: u.in_node.is_some(),
                has_out: u.out_node.is_some(),
                managed: false,
                units: vec![i],
            });
        }

        // Which items show a unit.
        let mut unit_items: HashMap<usize, Vec<usize>> = HashMap::new();
        for (ii, it) in scene.items.iter().enumerate() {
            for &u in &it.units {
                unit_items.entry(u).or_default().push(ii);
            }
        }
        let item_sockets = |id: &str, out: bool| -> Vec<(f64, f64)> {
            if let Some(g) = scene.groups.iter().find(|g| g.id == id) {
                return vec![if out { g.out_socket() } else { g.in_socket() }];
            }
            scene
                .items
                .iter()
                .filter(|it| it.endpoint == Endpoint::Setup(id.to_string()))
                .map(|it| if out { it.out_socket() } else { it.in_socket() })
                .collect()
        };
        let mut edges = vec![];
        for l in &s.links {
            let style = EdgeStyle::Setup(e.link_ui(&l.id));
            for from in item_sockets(&l.from, true) {
                for to in item_sockets(&l.to, false) {
                    edges.push(Edge { from, to, style, target: EdgeTarget::Setup(l.id.clone()) });
                }
            }
        }
        for ue in &a.units.edges {
            if a.explained.contains(&(ue.from, ue.to)) {
                continue;
            }
            let style = if ue.managed { EdgeStyle::Stale } else { EdgeStyle::Foreign };
            for &fi in unit_items.get(&ue.from).into_iter().flatten() {
                for &ti in unit_items.get(&ue.to).into_iter().flatten() {
                    edges.push(Edge {
                        from: scene.items[fi].out_socket(),
                        to: scene.items[ti].in_socket(),
                        style,
                        target: EdgeTarget::Live { from: ue.from, to: ue.to, links: ue.link_ids.clone() },
                    });
                }
            }
        }
        scene.edges = edges;

        let rects = scene.items.iter().map(|i| i.rect).chain(scene.groups.iter().map(|g| g.rect));
        let (mut w, mut h) = (900.0_f64, 600.0_f64);
        for r in rects {
            w = w.max(r.x + r.w + 300.0);
            h = h.max(r.y + r.h + 300.0);
        }
        scene.width = w;
        scene.height = h;
        scene
    }

    pub fn has_pending(&self) -> bool {
        self.edges.iter().any(|e| e.style == EdgeStyle::Setup(LinkUi::Pending))
    }
}

// ---------------------------------------------------------------------------
// Drawing

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: (f64, f64, f64),
    pub grid: (f64, f64, f64),
    pub card: (f64, f64, f64),
    pub card_border: (f64, f64, f64),
    pub text: (f64, f64, f64),
    pub dim: (f64, f64, f64),
    pub accent: (f64, f64, f64),
    pub ok: (f64, f64, f64),
    pub warn: (f64, f64, f64),
    pub bad: (f64, f64, f64),
}

fn hex(c: u32) -> (f64, f64, f64) {
    (((c >> 16) & 0xff) as f64 / 255.0, ((c >> 8) & 0xff) as f64 / 255.0, (c & 0xff) as f64 / 255.0)
}

impl Palette {
    pub fn new(dark: bool) -> Palette {
        if dark {
            Palette {
                bg: hex(0x1d1d20),
                grid: hex(0x27272b),
                card: hex(0x2e2e32),
                card_border: hex(0x48484f),
                text: hex(0xf2f2f4),
                dim: hex(0x9a9aa1),
                accent: hex(0x78aeed),
                ok: hex(0x57e389),
                warn: hex(0xf8e45c),
                bad: hex(0xff7b63),
            }
        } else {
            Palette {
                bg: hex(0xf6f5f4),
                grid: hex(0xe8e7e5),
                card: hex(0xffffff),
                card_border: hex(0xc0bfbc),
                text: hex(0x241f31),
                dim: hex(0x77767b),
                accent: hex(0x3584e4),
                ok: hex(0x26a269),
                warn: hex(0xc88800),
                bad: hex(0xe01b24),
            }
        }
    }
}

fn set(cr: &cairo::Context, c: (f64, f64, f64)) {
    cr.set_source_rgb(c.0, c.1, c.2);
}

fn set_a(cr: &cairo::Context, c: (f64, f64, f64), a: f64) {
    cr.set_source_rgba(c.0, c.1, c.2, a);
}

fn rounded(cr: &cairo::Context, r: Rect, rad: f64) {
    use std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(r.x + r.w - rad, r.y + rad, rad, -PI / 2.0, 0.0);
    cr.arc(r.x + r.w - rad, r.y + r.h - rad, rad, 0.0, PI / 2.0);
    cr.arc(r.x + rad, r.y + r.h - rad, rad, PI / 2.0, PI);
    cr.arc(r.x + rad, r.y + rad, rad, PI, 1.5 * PI);
    cr.close_path();
}

#[allow(clippy::too_many_arguments)]
fn text(cr: &cairo::Context, s: &str, x: f64, y: f64, max_w: f64, size: f64, bold: bool, color: (f64, f64, f64)) {
    let layout = pangocairo::functions::create_layout(cr);
    let mut font = pango::FontDescription::from_string("Sans");
    font.set_absolute_size(size * pango::SCALE as f64);
    if bold {
        font.set_weight(pango::Weight::Bold);
    }
    layout.set_font_description(Some(&font));
    layout.set_text(s);
    layout.set_width((max_w * pango::SCALE as f64) as i32);
    layout.set_ellipsize(pango::EllipsizeMode::End);
    set(cr, color);
    cr.move_to(x, y);
    pangocairo::functions::show_layout(cr, &layout);
}

fn edge_path(cr: &cairo::Context, e: &Edge) {
    let (c1, c2) = e.controls();
    cr.move_to(e.from.0, e.from.1);
    cr.curve_to(c1.0, c1.1, c2.0, c2.1, e.to.0, e.to.1);
}

/// Extra things to draw on top of the scene.
#[derive(Default)]
pub struct Overlay {
    pub selected_edge: Option<EdgeTarget>,
    pub rubber_band: Option<((f64, f64), (f64, f64))>,
    pub drop_target: Option<Rect>,
    /// Time in seconds, drives the pending animation.
    pub phase: f64,
}

/// Pan and zoom of the canvas: screen = world * zoom + pan.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub pan: (f64, f64),
    pub zoom: f64,
}

impl Default for View {
    fn default() -> View {
        View { pan: (0.0, 0.0), zoom: 1.0 }
    }
}

impl View {
    pub const MIN_ZOOM: f64 = 0.25;
    pub const MAX_ZOOM: f64 = 3.0;

    pub fn to_world(self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.pan.0) / self.zoom, (y - self.pan.1) / self.zoom)
    }

    /// Change the zoom while keeping the world point under screen position (x, y) in place.
    pub fn zoom_at(&mut self, x: f64, y: f64, zoom: f64) {
        let (wx, wy) = self.to_world(x, y);
        self.zoom = zoom.clamp(View::MIN_ZOOM, View::MAX_ZOOM);
        self.pan = (x - wx * self.zoom, y - wy * self.zoom);
    }
}

/// Draw the scene into a `width` x `height` screen area.
pub fn draw(cr: &cairo::Context, scene: &Scene, pal: &Palette, ov: &Overlay, view: View, width: f64, height: f64) {
    set(cr, pal.bg);
    cr.paint().ok();
    cr.save().ok();
    cr.translate(view.pan.0, view.pan.1);
    cr.scale(view.zoom, view.zoom);
    // Dot grid over the visible part of the (unbounded) canvas.
    const GRID: f64 = 24.0;
    let (x0, y0) = view.to_world(0.0, 0.0);
    let (x1, y1) = view.to_world(width, height);
    set(cr, pal.grid);
    let mut y = (y0 / GRID).floor() * GRID;
    while y < y1 {
        let mut x = (x0 / GRID).floor() * GRID;
        while x < x1 {
            cr.rectangle(x, y, 2.0, 2.0);
            x += GRID;
        }
        y += GRID;
    }
    cr.fill().ok();

    for g in &scene.groups {
        rounded(cr, g.rect, 14.0);
        set_a(cr, pal.accent, 0.06);
        cr.fill_preserve().ok();
        set_a(cr, pal.accent, 0.8);
        cr.set_line_width(2.0);
        cr.set_dash(&[8.0, 5.0], 0.0);
        cr.stroke().ok();
        cr.set_dash(&[], 0.0);
        text(cr, &g.name, g.rect.x + 22.0, g.rect.y + 7.0, g.rect.w - 140.0, 14.0, true, pal.accent);
        let sub = match g.live_members {
            0 => "empty".to_string(),
            1 => "1 device".to_string(),
            n => format!("{n} devices"),
        };
        text(cr, &sub, g.rect.x + g.rect.w - 120.0, g.rect.y + 9.0, 100.0, 11.0, false, pal.dim);
        // Resize grip.
        let h = g.resize_handle();
        set_a(cr, pal.accent, 0.6);
        cr.set_line_width(1.5);
        for i in 1..=3 {
            let o = i as f64 * 4.0;
            cr.move_to(h.x + h.w - o, h.y + h.h - 2.0);
            cr.line_to(h.x + h.w - 2.0, h.y + h.h - o);
        }
        cr.stroke().ok();
    }

    for e in &scene.edges {
        edge_path(cr, e);
        let selected = ov.selected_edge.as_ref() == Some(&e.target);
        let (color, width, dash): ((f64, f64, f64), f64, Vec<f64>) = match e.style {
            EdgeStyle::Setup(LinkUi::Ok) => (pal.accent, 3.0, vec![]),
            EdgeStyle::Setup(LinkUi::Partial) => (pal.warn, 3.0, vec![]),
            EdgeStyle::Setup(LinkUi::Waiting) => (pal.dim, 2.0, vec![2.0, 6.0]),
            EdgeStyle::Setup(LinkUi::Pending) => (pal.accent, 3.0, vec![10.0, 8.0]),
            EdgeStyle::Setup(LinkUi::Failed) => (pal.bad, 3.0, vec![]),
            EdgeStyle::Foreign => (pal.dim, 2.0, vec![6.0, 5.0]),
            EdgeStyle::Stale => (pal.warn, 2.0, vec![6.0, 5.0]),
        };
        if selected {
            set_a(cr, pal.accent, 0.3);
            cr.set_line_width(width + 8.0);
            cr.stroke_preserve().ok();
        }
        set(cr, color);
        cr.set_line_width(width);
        let offset = if e.style == EdgeStyle::Setup(LinkUi::Pending) { -ov.phase * 40.0 } else { 0.0 };
        cr.set_dash(&dash, offset);
        cr.stroke().ok();
        cr.set_dash(&[], 0.0);
    }

    for it in &scene.items {
        rounded(cr, it.rect, 10.0);
        if it.managed {
            set(cr, pal.card);
        } else {
            set_a(cr, pal.card, 0.6);
        }
        cr.fill_preserve().ok();
        set(cr, pal.card_border);
        cr.set_line_width(1.0);
        if !it.managed {
            cr.set_dash(&[5.0, 4.0], 0.0);
        }
        cr.stroke().ok();
        cr.set_dash(&[], 0.0);
        text(cr, &it.title, it.rect.x + 16.0, it.rect.y + 8.0, it.rect.w - 32.0, 13.5, true, pal.text);
        text(cr, &it.badge, it.rect.x + 16.0, it.rect.y + 27.0, it.rect.w - 32.0, 11.0, false, pal.dim);
        let mut ly = it.rect.y + HEADER_H;
        for (line, style) in &it.lines {
            let c = match style {
                LineStyle::Normal => pal.text,
                LineStyle::Dim => pal.dim,
                LineStyle::Bad => pal.bad,
            };
            if *style == LineStyle::Normal {
                set(cr, pal.ok);
                cr.new_path();
                cr.arc(it.rect.x + 20.0, ly + 8.0, 3.0, 0.0, std::f64::consts::TAU);
                cr.fill().ok();
            }
            text(cr, line, it.rect.x + 30.0, ly, it.rect.w - 46.0, 11.5, false, c);
            ly += LINE_H;
        }
        for (has, (sx, sy)) in [(it.has_in, it.in_socket()), (it.has_out, it.out_socket())] {
            if has {
                cr.new_path();
                cr.arc(sx, sy, SOCKET_R, 0.0, std::f64::consts::TAU);
                set(cr, pal.card);
                cr.fill_preserve().ok();
                set(cr, pal.accent);
                cr.set_line_width(2.0);
                cr.stroke().ok();
            }
        }
    }
    for g in &scene.groups {
        for (sx, sy) in [g.in_socket(), g.out_socket()] {
            cr.new_path();
            cr.arc(sx, sy, SOCKET_R, 0.0, std::f64::consts::TAU);
            set(cr, pal.accent);
            cr.fill().ok();
        }
    }

    if let Some(r) = ov.drop_target {
        rounded(cr, Rect { x: r.x - 4.0, y: r.y - 4.0, w: r.w + 8.0, h: r.h + 8.0 }, 12.0);
        set(cr, pal.accent);
        cr.set_line_width(2.5);
        cr.stroke().ok();
    }
    if let Some((a, b)) = ov.rubber_band {
        let e = Edge { from: a, to: b, style: EdgeStyle::Foreign, target: EdgeTarget::Setup(String::new()) };
        edge_path(cr, &e);
        set(cr, pal.accent);
        cr.set_line_width(3.0);
        cr.stroke().ok();
    }
    cr.restore().ok();
}

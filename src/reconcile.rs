//! Selector matching, link expansion and the desired-vs-live diff.
//!
//! Pure functions over `Setup` and `LiveGraph`; no GTK or PipeWire types.

use std::collections::{HashMap, HashSet};

use regex::Regex;

use crate::model::{LiveGraph, Props, Unit, UnitKind, Units, map_ports, profile_of};
use crate::setup::{DeviceIdent, Direction, Selector, SelectorKind, Setup};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    CreateLink { out_node: u32, out_port: u32, in_node: u32, in_port: u32 },
    DestroyLink(u32),
    CreateLoopback { id: String, name: String },
    UnloadModule(u32),
}

#[derive(Clone, Debug)]
pub struct DesiredLink {
    pub setup_link: String,
    pub out_node: u32,
    pub out_port: u32,
    pub in_node: u32,
    pub in_port: u32,
}

/// Expansion of one setup link into unit pairs, and how many of them exist live.
#[derive(Clone, Debug, Default)]
pub struct LinkState {
    pub pairs: Vec<(usize, usize)>,
    pub present: usize,
    /// Input nodes whose hidden loopback this link needs but which does not exist yet.
    pub awaiting_loopback: Vec<String>,
}

/// A hidden loopback the setup needs: one per input node that feeds something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WantedLoopback {
    pub node: String,
    /// Display name (`node.description` / `media.name`), derived from the node label.
    pub name: String,
}

#[derive(Clone, Debug, Default)]
pub struct Analysis {
    pub units: Units,
    /// Setup node id -> matched unit indices.
    pub node_matches: HashMap<String, Vec<usize>>,
    /// Units matched by at least one setup node.
    pub managed_units: HashSet<usize>,
    pub link_states: HashMap<String, LinkState>,
    /// Unit pairs that some setup link asks for.
    pub explained: HashSet<(usize, usize)>,
    pub desired: Vec<DesiredLink>,
    pub ee_present: bool,
    /// EasyEffects moves every application output stream to its own sink.
    pub ee_grabs_outputs: bool,
    pub loopbacks: Vec<WantedLoopback>,
    /// Loopbacks created by the app. They are not drawn; links through them are drawn from the input.
    pub hidden_units: HashSet<usize>,
}

impl Analysis {
    /// Units an endpoint (node or group id) stands for.
    pub fn endpoint_units(&self, setup: &Setup, id: &str) -> Vec<usize> {
        if setup.is_group(id) {
            let mut v: Vec<usize> =
                setup.group_members(id).iter().flat_map(|n| self.node_matches.get(n).cloned().unwrap_or_default()).collect();
            v.sort();
            v.dedup();
            v
        } else {
            self.node_matches.get(id).cloned().unwrap_or_default()
        }
    }
}

pub fn device_ident(p: &Props) -> DeviceIdent {
    let get = |k: &str| p.get(k).cloned().filter(|s| !s.is_empty());
    DeviceIdent {
        bus: get("device.bus").unwrap_or_default(),
        vendor_id: get("device.vendor.id").unwrap_or_default(),
        product_id: get("device.product.id").unwrap_or_default(),
        serial: get("device.serial"),
        bus_path: get("device.bus-path"),
        description: get("device.description").or(get("device.name")).unwrap_or_default(),
    }
}

fn hw_match(kind: SelectorKind, want: &DeviceIdent, have: &DeviceIdent) -> bool {
    let model = want.bus == have.bus && want.vendor_id == have.vendor_id && want.product_id == have.product_id;
    match kind {
        SelectorKind::Vendor => want.bus == have.bus && want.vendor_id == have.vendor_id,
        SelectorKind::Model => model,
        SelectorKind::Exact => model && if want.serial.is_some() { want.serial == have.serial } else { want.bus_path == have.bus_path },
        _ => false,
    }
}

fn direction_ok(dir: Direction, kind: &UnitKind) -> bool {
    match dir {
        Direction::Sink => matches!(kind, UnitKind::HwSink | UnitKind::VirtSink),
        Direction::Source => matches!(kind, UnitKind::HwSource | UnitKind::VirtSource),
        Direction::Any => true,
    }
}

pub fn selector_matches(sel: &Selector, unit: &Unit, live: &LiveGraph, regex: Option<&Regex>) -> bool {
    if unit.kind.is_app_stream() || unit.kind.is_hidden_loopback() {
        return false;
    }
    match sel.kind {
        SelectorKind::Role => matches!(
            (sel.role.as_deref(), &unit.kind),
            (Some("easyeffects:output"), UnitKind::EeOut) | (Some("easyeffects:input"), UnitKind::EeIn)
        ),
        SelectorKind::Name => {
            let Some(re) = regex else { return false };
            direction_ok(sel.direction, &unit.kind) && (re.is_match(&unit.node_name) || re.is_match(&unit.title))
        }
        SelectorKind::Exact | SelectorKind::Model | SelectorKind::Vendor => {
            let (Some(want), Some(dev)) = (&sel.device, unit.device) else { return false };
            let Some(props) = live.devices.get(&dev) else { return false };
            if !matches!(unit.kind, UnitKind::HwSink | UnitKind::HwSource) || !direction_ok(sel.direction, &unit.kind) {
                return false;
            }
            if let Some(p) = sel.profile.as_deref().filter(|p| !p.is_empty())
                && profile_of(&unit.node_name).as_deref() != Some(p)
            {
                return false;
            }
            hw_match(sel.kind, want, &device_ident(props))
        }
    }
}

/// Does EasyEffects redirect app output streams to its sink? Uses the EasyEffects setting when
/// known, otherwise looks for stream targets in the "default" metadata that point at its sink.
fn detect_ee_grab(live: &LiveGraph, units: &Units, setting: Option<bool>) -> bool {
    let Some(ee) = units.list.iter().find(|u| u.kind == UnitKind::EeOut) else { return false };
    let Some(sink) = ee.in_node else { return false };
    if let Some(s) = setting {
        return s;
    }
    let serial = live.nodes.get(&sink).and_then(|p| p.get("object.serial")).cloned().unwrap_or_default();
    live.metadata.iter().any(|((subject, key), value)| {
        (key == "target.object" || key == "target.node")
            && (value.trim_matches('"') == serial || value.trim_matches('"') == sink.to_string())
            && units.by_node.get(subject).is_some_and(|&u| units.list[u].kind == UnitKind::AppOut)
    })
}

pub fn analyze(setup: &Setup, live: &LiveGraph, ee_process_all_setting: Option<bool>) -> Analysis {
    let units = Units::build(live);
    let mut a = Analysis { ee_present: units.list.iter().any(|u| u.kind == UnitKind::EeOut), ..Default::default() };
    a.ee_grabs_outputs = detect_ee_grab(live, &units, ee_process_all_setting);

    for n in &setup.nodes {
        let re = n.selector.pattern.as_deref().and_then(|p| Regex::new(p).ok());
        let m: Vec<usize> =
            units.list.iter().enumerate().filter(|(_, u)| selector_matches(&n.selector, u, live, re.as_ref())).map(|(i, _)| i).collect();
        a.managed_units.extend(m.iter().copied());
        a.node_matches.insert(n.id.clone(), m);
    }
    a.units = units;

    // Inputs never link to their targets directly: each input node that feeds something gets one
    // hidden loopback (device -> loopback -> targets), named after the node.
    let ours: HashMap<String, usize> = a
        .units
        .list
        .iter()
        .enumerate()
        .filter_map(|(i, u)| match &u.kind {
            UnitKind::Loopback { ours: Some(id), .. } => Some((id.clone(), i)),
            _ => None,
        })
        .collect();
    a.hidden_units = ours.values().copied().collect();
    let mut feeding: HashSet<&str> = HashSet::new();
    for l in &setup.links {
        if setup.is_group(&l.from) {
            feeding.extend(setup.group_members(&l.from).into_iter().filter_map(|m| setup.node(&m).map(|n| n.id.as_str())));
        } else {
            feeding.insert(l.from.as_str());
        }
    }
    let inputs_of = |a: &Analysis, node: &str| -> Vec<usize> {
        a.node_matches.get(node).into_iter().flatten().copied().filter(|&u| a.units.list[u].kind.is_input()).collect()
    };
    for n in &setup.nodes {
        if feeding.contains(n.id.as_str()) && !inputs_of(&a, &n.id).is_empty() {
            a.loopbacks.push(WantedLoopback { node: n.id.clone(), name: loopback_name(&n.label) });
        }
    }

    let live_pairs: HashSet<(u32, u32)> = live.links.values().map(|l| (l.out_port, l.in_port)).collect();
    let ports_present = |ports: &[(u32, u32)]| !ports.is_empty() && ports.iter().all(|p| live_pairs.contains(p));
    for l in &setup.links {
        let from_nodes = if setup.is_group(&l.from) { setup.group_members(&l.from) } else { vec![l.from.clone()] };
        let tos = a.endpoint_units(setup, &l.to);
        let mut st = LinkState::default();
        let mut froms: Vec<(usize, bool)> = vec![];
        for node in &from_nodes {
            let matched = a.node_matches.get(node).cloned().unwrap_or_default();
            if !a.loopbacks.iter().any(|w| &w.node == node) {
                froms.extend(matched.into_iter().map(|u| (u, true)));
                continue;
            }
            froms.extend(matched.iter().copied().filter(|&u| !a.units.list[u].kind.is_input()).map(|u| (u, true)));
            let Some(&lb) = ours.get(node) else {
                st.awaiting_loopback.push(node.clone());
                continue;
            };
            // Feed: every matched input device -> the loopback (mixed).
            let mut fed = true;
            for src in inputs_of(&a, node) {
                let (Some(out_node), Some(in_node)) = (a.units.list[src].out_node, a.units.list[lb].in_node) else { continue };
                a.explained.insert((src, lb));
                let ports = map_ports(live, out_node, in_node);
                fed &= ports_present(&ports);
                for (out_port, in_port) in ports {
                    a.desired.push(DesiredLink { setup_link: l.id.clone(), out_node, out_port, in_node, in_port });
                }
            }
            froms.push((lb, fed));
        }
        let mut seen = HashSet::new();
        for &(f, fed) in &froms {
            for &t in &tos {
                let (fu, tu) = (&a.units.list[f], &a.units.list[t]);
                let (Some(out_node), Some(in_node)) = (fu.out_node, tu.in_node) else { continue };
                if f == t || !seen.insert((f, t)) {
                    continue;
                }
                st.pairs.push((f, t));
                a.explained.insert((f, t));
                let ports = map_ports(live, out_node, in_node);
                if fed && ports_present(&ports) {
                    st.present += 1;
                }
                for (out_port, in_port) in ports {
                    a.desired.push(DesiredLink { setup_link: l.id.clone(), out_node, out_port, in_node, in_port });
                }
            }
        }
        a.link_states.insert(l.id.clone(), st);
    }
    a
}

/// Name of the hidden loopback for a node label, as it appears in system sound settings.
/// Quotes and backslashes would break pactl's module argument parsing.
pub fn loopback_name(label: &str) -> String {
    label.replace(['"', '\\'], "").trim().to_string()
}

/// Commands that bring the live graph closer to the setup. Never touches links the app did not create.
pub fn plan(live: &LiveGraph, a: &Analysis) -> Vec<Command> {
    let mut cmds = vec![];
    let live_pairs: HashSet<(u32, u32)> = live.links.values().map(|l| (l.out_port, l.in_port)).collect();
    let desired_pairs: HashSet<(u32, u32)> = a.desired.iter().map(|d| (d.out_port, d.in_port)).collect();
    let mut seen = HashSet::new();
    for d in &a.desired {
        if !live_pairs.contains(&(d.out_port, d.in_port)) && seen.insert((d.out_port, d.in_port)) {
            cmds.push(Command::CreateLink { out_node: d.out_node, out_port: d.out_port, in_node: d.in_node, in_port: d.in_port });
        }
    }
    for l in live.links.values() {
        // Only remove links between nodes we fully understand, so a half-known graph never
        // causes removals.
        let known = a.units.by_node.contains_key(&l.out_node) && a.units.by_node.contains_key(&l.in_node);
        if l.managed && known && !desired_pairs.contains(&(l.out_port, l.in_port)) {
            cmds.push(Command::DestroyLink(l.id));
        }
    }
    // The EasyEffects output is routed by EasyEffects itself (its "output device" setting, stored
    // by node name). When the setup routes it, the setup wins: EasyEffects' own links to other
    // targets would otherwise duplicate the audio.
    if let Some(ee) = a.units.list.iter().position(|u| u.kind == UnitKind::EeOut)
        && a.link_states.values().any(|st| st.pairs.iter().any(|(f, _)| *f == ee))
    {
        let ee_nodes = &a.units.list[ee].nodes;
        for l in live.links.values() {
            let target = a.units.by_node.get(&l.in_node);
            if !l.managed
                && ee_nodes.contains(&l.out_node)
                && target.is_some_and(|&t| t != ee)
                && !desired_pairs.contains(&(l.out_port, l.in_port))
            {
                cmds.push(Command::DestroyLink(l.id));
            }
        }
    }
    // Hidden loopbacks: create missing ones; unload unneeded ones, and renamed ones (the name is
    // fixed at creation, so a rename recreates the loopback).
    for w in &a.loopbacks {
        let exists = a.units.list.iter().any(|u| matches!(&u.kind, UnitKind::Loopback { ours: Some(id), .. } if *id == w.node));
        if !exists {
            cmds.push(Command::CreateLoopback { id: w.node.clone(), name: w.name.clone() });
        }
    }
    for u in &a.units.list {
        if let UnitKind::Loopback { ours: Some(id), module_id: Some(m) } = &u.kind
            && !a.loopbacks.iter().any(|w| &w.node == id && w.name == u.title)
        {
            cmds.push(Command::UnloadModule(*m));
        }
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::{Placement, SetupNode};

    const FIXTURE: &str = include_str!("../tests/fixtures/ee-loopback.json");

    fn live() -> LiveGraph {
        LiveGraph::from_pw_dump(FIXTURE).unwrap()
    }

    fn unit<'a>(u: &'a Units, key_part: &str) -> (usize, &'a Unit) {
        u.list.iter().enumerate().find(|(_, u)| u.key.contains(key_part)).unwrap_or_else(|| panic!("no unit {key_part}"))
    }

    fn hw_node(id: &str, kind: SelectorKind, dir: Direction, dev: DeviceIdent, profile: &str) -> SetupNode {
        SetupNode {
            id: id.into(),
            label: id.into(),
            selector: Selector { kind, direction: dir, profile: Some(profile.into()), device: Some(dev), pattern: None, role: None },
        }
    }

    fn role_node(id: &str, role: &str) -> SetupNode {
        SetupNode {
            id: id.into(),
            label: id.into(),
            selector: Selector {
                kind: SelectorKind::Role,
                direction: Direction::Any,
                profile: None,
                device: None,
                pattern: None,
                role: Some(role.into()),
            },
        }
    }

    fn headphone_dev(g: &LiveGraph) -> DeviceIdent {
        let (_, p) = g.devices.iter().find(|(_, p)| p.get("device.description").is_some_and(|d| d.contains("Headphone"))).unwrap();
        device_ident(p)
    }

    #[test]
    fn units_collapse_loopback_and_easyeffects() {
        let u = Units::build(&live());
        let (_, lb) = unit(&u, "linkgroup:");
        assert!(matches!(lb.kind, UnitKind::Loopback { ours: None, module_id: Some(_) }));
        let (ee, eeu) = unit(&u, "ee:output");
        assert!(eeu.nodes.len() > 3);
        // TV loopback goes into EasyEffects, EasyEffects goes to the headphones.
        let (lbi, _) = unit(&u, "linkgroup:");
        assert!(u.edge(lbi, ee).is_some());
        let (hp, _) = unit(&u, "Headphone_Jack_Adapter");
        assert!(u.edge(ee, hp).is_some());
        // EasyEffects internal links are hidden.
        assert!(u.edges.iter().all(|e| e.from != e.to));
    }

    #[test]
    fn ee_grab_detected_from_metadata() {
        let g = live();
        let a = analyze(&Setup::default(), &g, None);
        assert!(a.ee_present);
        assert!(a.ee_grabs_outputs);
        assert!(!analyze(&Setup::default(), &g, Some(false)).ee_grabs_outputs);
    }

    #[test]
    fn model_selector_survives_renamed_node_and_new_serial() {
        let mut g = live();
        let dev = headphone_dev(&g);
        // Simulate a replacement unit of the same model: new serial, new node name suffix.
        for p in g.devices.values_mut() {
            if p.get("device.serial") == dev.serial.as_ref() {
                p.insert("device.serial".into(), "OTHER-SERIAL".into());
            }
        }
        for p in g.nodes.values_mut() {
            if let Some(n) = p.get_mut("node.name") {
                *n = n.replace("-00.", "-01.");
            }
        }
        let mut s = Setup::default();
        s.nodes.push(hw_node("model", SelectorKind::Model, Direction::Sink, dev.clone(), "analog-stereo"));
        s.nodes.push(hw_node("exact", SelectorKind::Exact, Direction::Sink, dev.clone(), "analog-stereo"));
        s.nodes.push(hw_node("vendor_src", SelectorKind::Vendor, Direction::Source, dev, ""));
        let a = analyze(&s, &g, None);
        assert_eq!(a.node_matches["model"].len(), 1);
        assert!(a.node_matches["exact"].is_empty());
        // The adapter's mono input.
        assert_eq!(a.node_matches["vendor_src"].len(), 1);
    }

    #[test]
    fn name_selector_and_app_streams_excluded() {
        let g = live();
        let mut s = Setup::default();
        s.nodes.push(SetupNode {
            id: "all".into(),
            label: "all".into(),
            selector: Selector {
                kind: SelectorKind::Name,
                direction: Direction::Any,
                profile: None,
                device: None,
                pattern: Some(".*".into()),
                role: None,
            },
        });
        let a = analyze(&s, &g, None);
        assert!(a.node_matches["all"].iter().all(|&i| !a.units.list[i].kind.is_app_stream()));
    }

    #[test]
    fn group_fans_out_and_plan_creates_missing_links() {
        let g = live();
        let dev = headphone_dev(&g);
        let mut s = Setup::default();
        s.nodes.push(role_node("ee", "easyeffects:output"));
        s.nodes.push(hw_node("hp", SelectorKind::Model, Direction::Sink, dev, "analog-stereo"));
        s.nodes.push(SetupNode {
            id: "hdmi".into(),
            label: "hdmi".into(),
            selector: Selector {
                kind: SelectorKind::Name,
                direction: Direction::Sink,
                profile: None,
                device: None,
                pattern: Some("hdmi".into()),
                role: None,
            },
        });
        s.groups.push(crate::setup::Group { id: "g1".into(), name: "Outputs".into(), rect: [0.0; 4] });
        for (p, n) in [("p1", "hp"), ("p2", "hdmi")] {
            s.placements.push(Placement { id: p.into(), node: n.into(), pos: [0.0; 2], group: Some("g1".into()) });
        }
        s.add_link("ee", "g1");
        let a = analyze(&s, &g, None);
        let st = &a.link_states[&s.links[0].id];
        assert_eq!(st.pairs.len(), 2);
        assert_eq!(st.present, 1, "EasyEffects -> headphones already exists");
        let cmds = plan(&g, &a);
        let creates = cmds.iter().filter(|c| matches!(c, Command::CreateLink { .. })).count();
        assert_eq!(creates, 2, "stereo link to HDMI: FL and FR");
        assert!(!cmds.iter().any(|c| matches!(c, Command::DestroyLink(_))), "foreign links are left alone");
    }

    #[test]
    fn setup_overrides_easyeffects_output_device() {
        let g = live();
        let mut s = Setup::default();
        s.nodes.push(role_node("ee", "easyeffects:output"));
        s.nodes.push(SetupNode {
            id: "hdmi".into(),
            label: "hdmi".into(),
            selector: Selector {
                kind: SelectorKind::Name,
                direction: Direction::Sink,
                profile: None,
                device: None,
                pattern: Some("hdmi".into()),
                role: None,
            },
        });
        // Without an EasyEffects link in the setup, its own routing is left alone.
        let a = analyze(&s, &g, None);
        assert!(!plan(&g, &a).iter().any(|c| matches!(c, Command::DestroyLink(_))));
        // Routing EasyEffects to HDMI removes its own link to the headphones.
        s.add_link("ee", "hdmi");
        let a = analyze(&s, &g, None);
        let destroys = plan(&g, &a).iter().filter(|c| matches!(c, Command::DestroyLink(_))).count();
        assert_eq!(destroys, 2, "FL and FR to the headphones");
    }

    fn tv_setup() -> Setup {
        let mut s = Setup::default();
        s.nodes.push(SetupNode {
            id: "tv".into(),
            label: "TV".into(),
            selector: Selector {
                kind: SelectorKind::Name,
                direction: Direction::Source,
                profile: None,
                device: None,
                pattern: Some("USB_PnP".into()),
                role: None,
            },
        });
        s.nodes.push(role_node("ee", "easyeffects:output"));
        s.add_link("tv", "ee");
        s
    }

    /// Turn the fixture's external loopback into one the app created for node "tv".
    fn adopt_fixture_loopback(g: &mut LiveGraph, name: &str) {
        for p in g.nodes.values_mut() {
            if p.get("node.link-group").is_some_and(|l| l.starts_with("loopback-")) {
                p.insert(crate::model::LOOPBACK_PROP.into(), "tv".into());
                p.insert("node.description".into(), name.into());
            }
        }
    }

    #[test]
    fn inputs_get_a_hidden_loopback() {
        let g = live();
        let s = tv_setup();
        let a = analyze(&s, &g, None);
        assert_eq!(a.loopbacks, vec![WantedLoopback { node: "tv".into(), name: "TV".into() }]);
        assert_eq!(a.link_states[&s.links[0].id].awaiting_loopback, vec!["tv".to_string()]);
        let cmds = plan(&g, &a);
        assert!(cmds.contains(&Command::CreateLoopback { id: "tv".into(), name: "TV".into() }));
        // No direct device -> EasyEffects links, and the external loopback is left alone.
        assert!(!cmds.iter().any(|c| matches!(c, Command::CreateLink { .. } | Command::UnloadModule(_))));
    }

    #[test]
    fn hidden_loopback_is_used_renamed_and_removed() {
        let mut g = live();
        adopt_fixture_loopback(&mut g, "TV");
        let mut s = tv_setup();
        let a = analyze(&s, &g, None);
        let (lb, _) = unit(&a.units, "loopback:tv");
        assert!(a.hidden_units.contains(&lb));
        assert!(!a.managed_units.contains(&lb));
        let st = &a.link_states[&s.links[0].id];
        assert_eq!((st.pairs.len(), st.present), (1, 1), "device -> loopback -> EasyEffects already exists");
        assert!(plan(&g, &a).is_empty());

        s.node_mut("tv").unwrap().label = "Television".into();
        let a = analyze(&s, &g, None);
        assert!(matches!(plan(&g, &a).as_slice(), [Command::UnloadModule(_)]), "rename recreates the loopback");

        s.links.clear();
        let a = analyze(&s, &g, None);
        assert!(a.loopbacks.is_empty());
        assert!(plan(&g, &a).iter().any(|c| matches!(c, Command::UnloadModule(_))));
    }
}

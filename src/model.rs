//! Live PipeWire graph (raw objects) and the collapsed "unit" view built from it.
//!
//! Pure data: no GTK or PipeWire types, so everything here can be tested with fixtures.

use std::collections::{BTreeMap, HashMap};

pub type Props = BTreeMap<String, String>;

/// Property set on links created by Audio Router.
pub const MANAGED_LINK_PROP: &str = "audiorouter.managed";
/// Property set on both halves of a loopback created by Audio Router (value: loopback id).
pub const LOOPBACK_PROP: &str = "audiorouter.loopback";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortDir {
    In,
    Out,
}

#[derive(Clone, Debug)]
pub struct Port {
    pub id: u32,
    pub node_id: u32,
    pub dir: PortDir,
    pub channel: String,
    pub monitor: bool,
}

#[derive(Clone, Debug)]
pub struct Link {
    pub id: u32,
    pub out_node: u32,
    pub out_port: u32,
    pub in_node: u32,
    pub in_port: u32,
    pub managed: bool,
}

/// Snapshot of the PipeWire objects the app cares about.
#[derive(Clone, Debug, Default)]
pub struct LiveGraph {
    pub devices: BTreeMap<u32, Props>,
    pub nodes: BTreeMap<u32, Props>,
    pub ports: BTreeMap<u32, Port>,
    pub links: BTreeMap<u32, Link>,
    /// Entries of the "default" metadata object: (subject, key) -> value.
    pub metadata: BTreeMap<(u32, String), String>,
    /// True once the initial registry enumeration has completed.
    pub synced: bool,
}

impl LiveGraph {
    pub fn remove(&mut self, id: u32) {
        self.devices.remove(&id);
        self.nodes.remove(&id);
        self.ports.remove(&id);
        self.links.remove(&id);
        self.metadata.retain(|(subject, _), _| *subject != id);
    }

    /// Default sink node name, from the "default" metadata.
    pub fn default_node_name(&self, sink: bool) -> Option<String> {
        let key = if sink { "default.audio.sink" } else { "default.audio.source" };
        let raw = self.metadata.get(&(0, key.to_string()))?;
        let v: serde_json::Value = serde_json::from_str(raw).ok()?;
        v.get("name")?.as_str().map(str::to_string)
    }

    /// Parse the output of `pw-dump` (used for test fixtures and the offline fixture mode).
    pub fn from_pw_dump(json: &str) -> anyhow::Result<Self> {
        let items: Vec<serde_json::Value> = serde_json::from_str(json)?;
        let mut g = LiveGraph { synced: true, ..Default::default() };
        for item in items {
            let id = item["id"].as_u64().unwrap_or(u32::MAX as u64) as u32;
            let ty = item["type"].as_str().unwrap_or_default();
            let props = json_props(&item["info"]["props"]);
            match ty {
                "PipeWire:Interface:Device" => {
                    g.devices.insert(id, props);
                }
                "PipeWire:Interface:Node" => {
                    g.nodes.insert(id, props);
                }
                "PipeWire:Interface:Port" => {
                    let dir = if item["info"]["direction"] == "input" { PortDir::In } else { PortDir::Out };
                    g.ports.insert(id, port_from_props(id, dir, &props));
                }
                "PipeWire:Interface:Link" => {
                    let info = &item["info"];
                    let n = |k: &str| info[k].as_u64().unwrap_or(0) as u32;
                    g.links.insert(
                        id,
                        Link {
                            id,
                            out_node: n("output-node-id"),
                            out_port: n("output-port-id"),
                            in_node: n("input-node-id"),
                            in_port: n("input-port-id"),
                            managed: props.get(MANAGED_LINK_PROP).is_some_and(|v| v == "true" || v == "1"),
                        },
                    );
                }
                "PipeWire:Interface:Metadata" if item["props"]["metadata.name"] == "default" => {
                    for e in item["metadata"].as_array().into_iter().flatten() {
                        let subject = e["subject"].as_u64().unwrap_or(0) as u32;
                        let key = e["key"].as_str().unwrap_or_default().to_string();
                        let value = match &e["value"] {
                            serde_json::Value::String(s) => s.clone(),
                            v => v.to_string(),
                        };
                        g.metadata.insert((subject, key), value);
                    }
                }
                _ => {}
            }
        }
        Ok(g)
    }
}

fn json_props(v: &serde_json::Value) -> Props {
    let mut p = Props::new();
    if let Some(obj) = v.as_object() {
        for (k, v) in obj {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => continue,
                v => v.to_string(),
            };
            p.insert(k.clone(), s);
        }
    }
    p
}

pub fn port_from_props(id: u32, dir: PortDir, props: &Props) -> Port {
    Port {
        id,
        node_id: props.get("node.id").and_then(|s| s.parse().ok()).unwrap_or(u32::MAX),
        dir,
        channel: props.get("audio.channel").cloned().unwrap_or_default(),
        monitor: props.get("port.monitor").is_some_and(|v| v == "true"),
    }
}

// ---------------------------------------------------------------------------
// Units: what the user sees as one box.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitKind {
    /// Hardware output (has a PipeWire Device).
    HwSink,
    /// Hardware input.
    HwSource,
    /// Virtual sink (no device), e.g. a null sink.
    VirtSink,
    VirtSource,
    /// Application playback stream.
    AppOut,
    /// Application recording stream.
    AppIn,
    /// A pair of linked stream nodes (module-loopback and similar).
    /// `ours` is the setup node id of the input it belongs to, if the app created it.
    Loopback {
        ours: Option<String>,
        module_id: Option<u32>,
    },
    /// EasyEffects output chain (easyeffects_sink + ee_soe_* filters).
    EeOut,
    /// EasyEffects input chain (ee_sie_* filters + easyeffects_source).
    EeIn,
}

impl UnitKind {
    pub fn is_app_stream(&self) -> bool {
        matches!(self, UnitKind::AppOut | UnitKind::AppIn)
    }

    /// Something that produces audio on its own (an input device or the EasyEffects mic chain).
    /// Inputs reach their targets through a hidden loopback.
    pub fn is_input(&self) -> bool {
        matches!(self, UnitKind::HwSource | UnitKind::VirtSource | UnitKind::EeIn)
    }

    /// A loopback the app created for an input. Never shown and never matched by selectors.
    pub fn is_hidden_loopback(&self) -> bool {
        matches!(self, UnitKind::Loopback { ours: Some(_), .. })
    }
}

#[derive(Clone, Debug)]
pub struct Unit {
    /// Stable within a session; reasonably stable across sessions (used for layout).
    pub key: String,
    pub kind: UnitKind,
    pub title: String,
    pub subtitle: String,
    /// Node that receives audio (input ports), if any.
    pub in_node: Option<u32>,
    /// Node that emits audio (output ports), if any.
    pub out_node: Option<u32>,
    pub nodes: Vec<u32>,
    pub device: Option<u32>,
    pub node_name: String,
}

#[derive(Clone, Debug)]
pub struct UnitEdge {
    pub from: usize,
    pub to: usize,
    pub link_ids: Vec<u32>,
    pub managed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Units {
    pub list: Vec<Unit>,
    pub by_node: HashMap<u32, usize>,
    pub edges: Vec<UnitEdge>,
}

fn prop<'a>(p: &'a Props, k: &str) -> Option<&'a str> {
    p.get(k).map(String::as_str).filter(|s| !s.is_empty())
}

fn node_title(p: &Props) -> String {
    prop(p, "node.description").or(prop(p, "node.nick")).or(prop(p, "node.name")).unwrap_or("Unknown").to_string()
}

/// Profile part of an ALSA node name, e.g. "analog-stereo" for "alsa_output.usb-…-00.analog-stereo".
pub fn profile_of(node_name: &str) -> Option<String> {
    let (prefix, rest) = node_name.split_once('.')?;
    if !prefix.starts_with("alsa_") && !prefix.starts_with("bluez_") {
        return None;
    }
    rest.rsplit_once('.').map(|(_, p)| p.to_string())
}

impl Units {
    pub fn build(g: &LiveGraph) -> Units {
        let mut u = Units::default();
        let mut ee_out: Vec<u32> = vec![];
        let mut ee_in: Vec<u32> = vec![];
        let mut link_groups: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        let mut singles: Vec<u32> = vec![];

        for (&id, p) in &g.nodes {
            let name = prop(p, "node.name").unwrap_or_default();
            let class = prop(p, "media.class").unwrap_or_default();
            match prop(p, "node.group") {
                Some("ee_sink_group") => {
                    ee_out.push(id);
                    continue;
                }
                Some("ee_source_group") => {
                    ee_in.push(id);
                    continue;
                }
                _ => {}
            }
            if name == "ee_test_signals" {
                continue;
            }
            let audio = matches!(
                class,
                "Audio/Sink" | "Audio/Source" | "Audio/Source/Virtual" | "Audio/Duplex" | "Stream/Output/Audio" | "Stream/Input/Audio"
            );
            if !audio {
                continue;
            }
            if class.starts_with("Stream/")
                && let Some(lg) = prop(p, "node.link-group")
            {
                link_groups.entry(lg.to_string()).or_default().push(id);
                continue;
            }
            singles.push(id);
        }

        // Linked stream pairs (loopbacks). Anything else in a link group is shown as single streams.
        for (lg, ids) in link_groups {
            let class = |id: &u32| prop(&g.nodes[id], "media.class").unwrap_or_default().to_string();
            let ins: Vec<u32> = ids.iter().copied().filter(|i| class(i) == "Stream/Input/Audio").collect();
            let outs: Vec<u32> = ids.iter().copied().filter(|i| class(i) == "Stream/Output/Audio").collect();
            if ins.len() == 1 && outs.len() == 1 {
                let (i, o) = (ins[0], outs[0]);
                let pi = &g.nodes[&i];
                let po = &g.nodes[&o];
                let ours = prop(pi, LOOPBACK_PROP).or(prop(po, LOOPBACK_PROP)).map(str::to_string);
                let module_id = prop(pi, "pulse.module.id").and_then(|s| s.parse().ok());
                let title = node_title(po);
                u.push(Unit {
                    key: match &ours {
                        Some(id) => format!("loopback:{id}"),
                        None => format!("linkgroup:{lg}"),
                    },
                    kind: UnitKind::Loopback { ours: ours.clone(), module_id },
                    subtitle: if ours.is_some() { "Loopback".into() } else { "Loopback (created outside Audio Router)".into() },
                    title,
                    in_node: Some(i),
                    out_node: Some(o),
                    nodes: vec![i, o],
                    device: None,
                    node_name: prop(po, "node.name").unwrap_or_default().to_string(),
                });
            } else {
                singles.extend(ids);
            }
        }

        for id in singles {
            let p = &g.nodes[&id];
            let class = prop(p, "media.class").unwrap_or_default();
            let device = prop(p, "device.id").and_then(|s| s.parse::<u32>().ok()).filter(|d| g.devices.contains_key(d));
            let name = prop(p, "node.name").unwrap_or_default().to_string();
            let (kind, in_node, out_node) = match class {
                "Audio/Sink" if device.is_some() => (UnitKind::HwSink, Some(id), None),
                "Audio/Sink" => (UnitKind::VirtSink, Some(id), None),
                "Audio/Source" | "Audio/Duplex" if device.is_some() => (UnitKind::HwSource, None, Some(id)),
                "Audio/Source" | "Audio/Source/Virtual" | "Audio/Duplex" => (UnitKind::VirtSource, None, Some(id)),
                "Stream/Output/Audio" => (UnitKind::AppOut, None, Some(id)),
                _ => (UnitKind::AppIn, Some(id), None),
            };
            let (title, subtitle) = if kind.is_app_stream() {
                (
                    prop(p, "application.name").map(str::to_string).unwrap_or_else(|| node_title(p)),
                    prop(p, "media.name").unwrap_or(if kind == UnitKind::AppOut { "Playback" } else { "Recording" }).to_string(),
                )
            } else {
                let sub = match &kind {
                    UnitKind::HwSink => "Output device",
                    UnitKind::HwSource => "Input device",
                    UnitKind::VirtSink => "Virtual output",
                    _ => "Virtual input",
                };
                (node_title(p), sub.to_string())
            };
            u.push(Unit {
                key: if kind.is_app_stream() { format!("stream:{id}") } else { format!("node:{name}") },
                kind,
                title,
                subtitle,
                in_node,
                out_node,
                nodes: vec![id],
                device,
                node_name: name,
            });
        }

        if !ee_out.is_empty() {
            let by_name = |n: &str| ee_out.iter().copied().find(|i| prop(&g.nodes[i], "node.name") == Some(n));
            let in_node = by_name("easyeffects_sink");
            let out_node = by_name("ee_soe_output_level").or_else(|| chain_end(g, &ee_out, in_node));
            u.push(Unit {
                key: "ee:output".into(),
                kind: UnitKind::EeOut,
                title: "EasyEffects".into(),
                subtitle: "Output effects".into(),
                in_node,
                out_node,
                nodes: ee_out,
                device: None,
                node_name: "easyeffects_sink".into(),
            });
        }
        if !ee_in.is_empty() {
            let out_node = ee_in.iter().copied().find(|i| prop(&g.nodes[i], "node.name") == Some("easyeffects_source"));
            u.push(Unit {
                key: "ee:input".into(),
                kind: UnitKind::EeIn,
                title: "EasyEffects (mic)".into(),
                subtitle: "Input effects".into(),
                // The input chain is fed by EasyEffects itself from its configured input device.
                in_node: None,
                out_node,
                nodes: ee_in,
                device: None,
                node_name: "easyeffects_source".into(),
            });
        }

        // Unit-level edges.
        let mut edges: BTreeMap<(usize, usize), UnitEdge> = BTreeMap::new();
        for l in g.links.values() {
            let (Some(&a), Some(&b)) = (u.by_node.get(&l.out_node), u.by_node.get(&l.in_node)) else { continue };
            if a == b {
                continue;
            }
            let e = edges.entry((a, b)).or_insert(UnitEdge { from: a, to: b, link_ids: vec![], managed: true });
            e.link_ids.push(l.id);
            e.managed &= l.managed;
        }
        u.edges = edges.into_values().collect();
        u
    }

    fn push(&mut self, unit: Unit) {
        let idx = self.list.len();
        for n in &unit.nodes {
            self.by_node.insert(*n, idx);
        }
        self.list.push(unit);
    }

    #[cfg(test)]
    pub fn edge(&self, from: usize, to: usize) -> Option<&UnitEdge> {
        self.edges.iter().find(|e| e.from == from && e.to == to)
    }
}

/// Last node of a filter chain: a member whose outputs leave the group.
fn chain_end(g: &LiveGraph, members: &[u32], start: Option<u32>) -> Option<u32> {
    members.iter().copied().find(|m| Some(*m) != start && g.links.values().any(|l| l.out_node == *m && !members.contains(&l.in_node)))
}

/// Port pairs for connecting `out_node` to `in_node`, mapping channels.
pub fn map_ports(g: &LiveGraph, out_node: u32, in_node: u32) -> Vec<(u32, u32)> {
    let ports = |node: u32, dir: PortDir| -> Vec<&Port> {
        let mut v: Vec<&Port> =
            g.ports.values().filter(|p| p.node_id == node && p.dir == dir && !(dir == PortDir::Out && p.monitor)).collect();
        v.sort_by_key(|p| p.id);
        v
    };
    let outs = ports(out_node, PortDir::Out);
    let ins = ports(in_node, PortDir::In);
    if outs.is_empty() || ins.is_empty() {
        return vec![];
    }
    if outs.len() == 1 {
        return ins.iter().map(|i| (outs[0].id, i.id)).collect();
    }
    if ins.len() == 1 {
        return outs.iter().map(|o| (o.id, ins[0].id)).collect();
    }
    let by_channel: Vec<(u32, u32)> =
        outs.iter().filter_map(|o| ins.iter().find(|i| !o.channel.is_empty() && i.channel == o.channel).map(|i| (o.id, i.id))).collect();
    if !by_channel.is_empty() {
        return by_channel;
    }
    outs.iter().zip(ins.iter()).map(|(o, i)| (o.id, i.id)).collect()
}

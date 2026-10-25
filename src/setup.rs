//! The saved setup: nodes (selectors), groups, placements and links. Stored as TOML.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Setup {
    #[serde(default)]
    pub nodes: Vec<SetupNode>,
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub placements: Vec<Placement>,
    #[serde(default)]
    pub links: Vec<SetupLink>,
    /// Canvas positions of live units that are not part of the setup, by unit key.
    #[serde(default)]
    pub layout: BTreeMap<String, [f64; 2]>,
    #[serde(default)]
    pub next_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetupNode {
    pub id: String,
    pub label: String,
    pub selector: Selector,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SelectorKind {
    Exact,
    Model,
    Vendor,
    Name,
    Role,
}

impl SelectorKind {
    pub const HARDWARE: [SelectorKind; 4] = [SelectorKind::Exact, SelectorKind::Model, SelectorKind::Vendor, SelectorKind::Name];

    pub fn label(self) -> &'static str {
        match self {
            SelectorKind::Exact => "This exact device",
            SelectorKind::Model => "Any device of this model",
            SelectorKind::Vendor => "Any device from this vendor",
            SelectorKind::Name => "Name matches a pattern",
            SelectorKind::Role => "Built-in role",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            SelectorKind::Exact => "exact device",
            SelectorKind::Model => "same model",
            SelectorKind::Vendor => "same vendor",
            SelectorKind::Name => "name pattern",
            SelectorKind::Role => "role",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Sink,
    Source,
    Any,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Selector {
    pub kind: SelectorKind,
    pub direction: Direction,
    /// For hardware selectors: only nodes whose name ends with this profile (e.g. "analog-stereo").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Identity of the device this node was created from (kept so the kind can be changed later).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceIdent>,
    /// Regex for `name` selectors, matched against node name and description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// For `role` selectors: "easyeffects:output" or "easyeffects:input".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdent {
    pub bus: String,
    pub vendor_id: String,
    pub product_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bus_path: Option<String>,
    /// Last seen device description, for display only.
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub rect: [f64; 4],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Placement {
    pub id: String,
    pub node: String,
    pub pos: [f64; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// A connection between two setup endpoints (node or group ids).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupLink {
    pub id: String,
    pub from: String,
    pub to: String,
}

impl Setup {
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
        base.join("audiorouter").join("setup.toml")
    }

    pub fn load(path: &Path) -> anyhow::Result<Setup> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Setup::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn new_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    pub fn node(&self, id: &str) -> Option<&SetupNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn node_mut(&mut self, id: &str) -> Option<&mut SetupNode> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    pub fn group(&self, id: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }

    pub fn is_group(&self, id: &str) -> bool {
        self.group(id).is_some()
    }

    /// Setup node ids that are members of a group (through any of their placements).
    pub fn group_members(&self, group: &str) -> Vec<String> {
        let mut v: Vec<String> = self.placements.iter().filter(|p| p.group.as_deref() == Some(group)).map(|p| p.node.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn has_link(&self, from: &str, to: &str) -> bool {
        self.links.iter().any(|l| l.from == from && l.to == to)
    }

    pub fn add_link(&mut self, from: &str, to: &str) -> bool {
        if from == to || self.has_link(from, to) {
            return false;
        }
        let id = self.new_id("l");
        self.links.push(SetupLink { id, from: from.into(), to: to.into() });
        true
    }

    /// Remove a placement; when it was the node's last one, remove the node and its links too.
    pub fn remove_placement(&mut self, placement: &str) {
        let Some(node) = self.placements.iter().find(|p| p.id == placement).map(|p| p.node.clone()) else { return };
        self.placements.retain(|p| p.id != placement);
        if !self.placements.iter().any(|p| p.node == node) {
            self.remove_node(&node);
        }
    }

    pub fn remove_node(&mut self, node: &str) {
        self.placements.retain(|p| p.node != node);
        self.links.retain(|l| l.from != node && l.to != node);
        self.nodes.retain(|n| n.id != node);
    }

    pub fn remove_group(&mut self, group: &str) {
        self.groups.retain(|g| g.id != group);
        self.links.retain(|l| l.from != group && l.to != group);
        for p in &mut self.placements {
            if p.group.as_deref() == Some(group) {
                p.group = None;
            }
        }
    }

    /// Does any node use an EasyEffects role?
    pub fn needs_easyeffects(&self) -> bool {
        self.nodes
            .iter()
            .any(|n| n.selector.kind == SelectorKind::Role && n.selector.role.as_deref().is_some_and(|r| r.starts_with("easyeffects:")))
    }

    /// Endpoint display name (node label or group name).
    pub fn endpoint_name(&self, id: &str) -> String {
        if let Some(n) = self.node(id) {
            n.label.clone()
        } else if let Some(g) = self.group(id) {
            g.name.clone()
        } else {
            id.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_toml() {
        let mut s = Setup::default();
        let id = s.new_id("n");
        s.nodes.push(SetupNode {
            id: id.clone(),
            label: "Headphones".into(),
            selector: Selector {
                kind: SelectorKind::Model,
                direction: Direction::Sink,
                profile: Some("analog-stereo".into()),
                device: Some(DeviceIdent {
                    bus: "usb".into(),
                    vendor_id: "0x1234".into(),
                    product_id: "0x5678".into(),
                    ..Default::default()
                }),
                pattern: None,
                role: None,
            },
        });
        s.placements.push(Placement { id: "p1".into(), node: id.clone(), pos: [10.0, 20.0], group: None });
        s.layout.insert("node:foo".into(), [1.0, 2.0]);
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Setup = toml::from_str(&text).unwrap();
        assert_eq!(back.nodes[0].selector.kind, SelectorKind::Model);
        assert_eq!(back.placements[0].pos, [10.0, 20.0]);
        assert_eq!(back.next_id, 1);
    }

    #[test]
    fn removing_last_placement_removes_node_and_links() {
        let mut s = Setup::default();
        for id in ["n1", "n2"] {
            s.nodes.push(SetupNode {
                id: id.into(),
                label: id.into(),
                selector: Selector {
                    kind: SelectorKind::Name,
                    direction: Direction::Any,
                    profile: None,
                    device: None,
                    pattern: Some(".*".into()),
                    role: None,
                },
            });
        }
        s.placements.push(Placement { id: "p1".into(), node: "n1".into(), pos: [0.0, 0.0], group: None });
        s.placements.push(Placement { id: "p2".into(), node: "n1".into(), pos: [0.0, 0.0], group: None });
        s.placements.push(Placement { id: "p3".into(), node: "n2".into(), pos: [0.0, 0.0], group: None });
        s.add_link("n1", "n2");
        s.remove_placement("p1");
        assert_eq!(s.links.len(), 1, "duplicate still exists");
        s.remove_placement("p2");
        assert!(s.links.is_empty());
        assert!(s.node("n1").is_none());
    }
}

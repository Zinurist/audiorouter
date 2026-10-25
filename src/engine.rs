//! GTK-thread orchestrator: owns the setup and the latest live graph, runs the reconciler,
//! tracks pending/failed commands and computes the problems shown to the user.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command as Proc;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::glib;

use crate::backend::{self, Backend, BackendCmd, BackendEvent};
use crate::easyeffects;
use crate::model::{LOOPBACK_PROP, LiveGraph, UnitKind, profile_of};
use crate::reconcile::{self, Analysis, Command, device_ident};
use crate::setup::{Direction, LoopbackDef, Placement, Selector, SelectorKind, Setup, SetupNode};

/// A link that has not shown up this long after creating it counts as failed.
const PENDING_TIMEOUT: Duration = Duration::from_secs(4);
/// Wait this long before retrying a failed command.
const RETRY_AFTER: Duration = Duration::from_secs(10);
/// How long EasyEffects gets to create its nodes after being launched.
const EE_START_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub severity: Severity,
    pub text: String,
}

/// How a setup link is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkUi {
    /// Every expansion exists.
    Ok,
    /// Some expansions exist.
    Partial,
    /// Nothing to connect yet (an endpoint matches no device).
    Waiting,
    /// Being created right now.
    Pending,
    Failed,
}

/// Something the user can connect: a setup node or group, or a live unit not in the setup yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Setup(String),
    Unit(String),
}

pub struct Engine {
    pub setup: Setup,
    path: PathBuf,
    pub live: LiveGraph,
    pub analysis: Analysis,
    backend: Option<Backend>,
    pub connected: bool,
    pending: HashMap<(u32, u32), Instant>,
    failed: HashMap<(u32, u32), Instant>,
    destroying: HashMap<u32, Instant>,
    loopback_attempts: HashMap<String, Instant>,
    loopback_errors: HashMap<String, String>,
    ee_launched: Option<Instant>,
    ee_error: Option<String>,
    ee_setting: Option<bool>,
    pub problems: Vec<Problem>,
    reconcile_scheduled: bool,
    on_change: Vec<Rc<dyn Fn()>>,
    on_toast: Option<ToastFn>,
}

pub type EngineRef = Rc<RefCell<Engine>>;
pub type ToastFn = Rc<dyn Fn(&str)>;

impl Engine {
    /// Start the engine. With `AUDIOROUTER_FIXTURE=<pw-dump.json>` it runs offline on that
    /// snapshot and only logs the commands it would send.
    pub fn start() -> EngineRef {
        let path = std::env::var_os("AUDIOROUTER_SETUP").map(PathBuf::from).unwrap_or_else(Setup::default_path);
        let (setup, load_error) = match Setup::load(&path) {
            Ok(s) => (s, None),
            Err(e) => (Setup::default(), Some(format!("Could not read {}: {e}", path.display()))),
        };
        let fixture = std::env::var_os("AUDIOROUTER_FIXTURE");
        let (tx, rx) = async_channel::unbounded();
        let backend = if fixture.is_none() { Some(backend::spawn(tx)) } else { None };
        let mut live = LiveGraph::default();
        if let Some(f) = &fixture {
            live = std::fs::read_to_string(f).map_err(anyhow::Error::from).and_then(|s| LiveGraph::from_pw_dump(&s)).unwrap_or_else(|e| {
                eprintln!("fixture: {e}");
                LiveGraph::default()
            });
        }
        let engine = Rc::new(RefCell::new(Engine {
            setup,
            path,
            live,
            analysis: Analysis::default(),
            backend,
            connected: fixture.is_some(),
            pending: HashMap::new(),
            failed: HashMap::new(),
            destroying: HashMap::new(),
            loopback_attempts: HashMap::new(),
            loopback_errors: HashMap::new(),
            ee_launched: None,
            ee_error: None,
            ee_setting: easyeffects::process_all_outputs_setting(),
            problems: vec![],
            reconcile_scheduled: false,
            on_change: vec![],
            on_toast: None,
        }));
        if let Some(e) = load_error {
            engine.borrow_mut().problems.push(Problem { severity: Severity::Error, text: e });
        }

        let weak = Rc::downgrade(&engine);
        glib::spawn_future_local(async move {
            while let Ok(ev) = rx.recv().await {
                let Some(engine) = weak.upgrade() else { return };
                // Coalesce bursts: only the latest graph matters.
                let mut ev = ev;
                while let Ok(next) = rx.try_recv() {
                    if let BackendEvent::Error(ref m) = ev {
                        log_backend_error(m);
                    }
                    ev = next;
                }
                match ev {
                    BackendEvent::Graph(g) => {
                        let mut e = engine.borrow_mut();
                        e.live = g;
                        e.connected = true;
                    }
                    BackendEvent::Disconnected => engine.borrow_mut().connected = false,
                    BackendEvent::Error(m) => log_backend_error(&m),
                }
                schedule_reconcile(&engine);
            }
        });

        let weak = Rc::downgrade(&engine);
        glib::timeout_add_seconds_local(1, move || {
            let Some(engine) = weak.upgrade() else { return glib::ControlFlow::Break };
            tick(&engine);
            glib::ControlFlow::Continue
        });
        schedule_reconcile(&engine);
        engine
    }

    pub fn subscribe(&mut self, f: Rc<dyn Fn()>) {
        self.on_change.push(f);
    }

    pub fn set_toast_handler(&mut self, f: Rc<dyn Fn(&str)>) {
        self.on_toast = Some(f);
    }

    pub fn is_settled(&self) -> bool {
        self.pending.is_empty() && self.destroying.is_empty()
    }

    pub fn save(&mut self) {
        if let Err(e) = self.setup.save(&self.path) {
            self.toast(&format!("Could not save setup: {e}"));
        }
    }

    pub fn toast(&self, msg: &str) {
        match &self.on_toast {
            Some(f) => f(msg),
            None => eprintln!("{msg}"),
        }
    }

    pub fn link_ui(&self, link_id: &str) -> LinkUi {
        let ports: Vec<(u32, u32)> =
            self.analysis.desired.iter().filter(|d| d.setup_link == link_id).map(|d| (d.out_port, d.in_port)).collect();
        if ports.iter().any(|p| self.failed.contains_key(p)) {
            return LinkUi::Failed;
        }
        if ports.iter().any(|p| self.pending.contains_key(p)) {
            return LinkUi::Pending;
        }
        match self.analysis.link_states.get(link_id) {
            Some(st) if st.pairs.is_empty() => LinkUi::Waiting,
            Some(st) if st.present == st.pairs.len() => LinkUi::Ok,
            Some(st) if st.present > 0 => LinkUi::Partial,
            Some(_) => LinkUi::Pending,
            None => LinkUi::Waiting,
        }
    }

    fn unit_endpoint_error(&self, key: &str) -> Option<String> {
        let unit = self.analysis.units.list.iter().find(|u| u.key == key)?;
        match &unit.kind {
            UnitKind::AppOut if self.analysis.ee_grabs_outputs => Some(EE_GRAB_MSG.into()),
            UnitKind::AppOut | UnitKind::AppIn => {
                Some("Applications follow the system default device. Right-click an output and choose “Use as default”.".into())
            }
            UnitKind::Loopback { ours: None, .. } => {
                Some("This loopback was created outside Audio Router. Add a loopback from the menu instead, then remove this one.".into())
            }
            _ => None,
        }
    }

    /// Create a setup node for a live unit (at the given canvas position). Returns the node id.
    pub fn adopt(&mut self, key: &str, pos: [f64; 2]) -> Result<String, String> {
        if let Some(err) = self.unit_endpoint_error(key) {
            return Err(err);
        }
        let unit = self.analysis.units.list.iter().find(|u| u.key == key).ok_or("This device is gone")?.clone();
        let selector = match &unit.kind {
            UnitKind::HwSink | UnitKind::HwSource => {
                let props = unit.device.and_then(|d| self.live.devices.get(&d)).ok_or("Device information is missing")?;
                Selector {
                    kind: SelectorKind::Model,
                    direction: if unit.kind == UnitKind::HwSink { Direction::Sink } else { Direction::Source },
                    profile: profile_of(&unit.node_name),
                    device: Some(device_ident(props)),
                    pattern: None,
                    role: None,
                }
            }
            UnitKind::VirtSink | UnitKind::VirtSource => Selector {
                kind: SelectorKind::Name,
                direction: if unit.kind == UnitKind::VirtSink { Direction::Sink } else { Direction::Source },
                profile: None,
                device: None,
                pattern: Some(format!("^{}$", regex::escape(&unit.node_name))),
                role: None,
            },
            UnitKind::EeOut | UnitKind::EeIn => Selector {
                kind: SelectorKind::Role,
                direction: Direction::Any,
                profile: None,
                device: None,
                pattern: None,
                role: Some(if unit.kind == UnitKind::EeOut { "easyeffects:output" } else { "easyeffects:input" }.into()),
            },
            UnitKind::Loopback { ours: Some(id), .. } => Selector {
                kind: SelectorKind::Role,
                direction: Direction::Any,
                profile: None,
                device: None,
                pattern: None,
                role: Some(format!("loopback:{id}")),
            },
            _ => return Err("This node cannot be added to the setup".into()),
        };
        let id = self.setup.new_id("n");
        self.setup.nodes.push(SetupNode { id: id.clone(), label: unit.title.clone(), selector });
        let pid = self.setup.new_id("p");
        self.setup.placements.push(Placement { id: pid, node: id.clone(), pos, group: None });
        self.setup.layout.remove(key);
        Ok(id)
    }

    pub fn can_output(&self, setup_id: &str) -> bool {
        match self.setup.node(setup_id) {
            Some(n) => match n.selector.kind {
                SelectorKind::Role => true,
                _ => n.selector.direction != Direction::Sink,
            },
            None => self.setup.is_group(setup_id),
        }
    }

    pub fn can_input(&self, setup_id: &str) -> bool {
        match self.setup.node(setup_id) {
            Some(n) => match n.selector.kind {
                SelectorKind::Role => n.selector.role.as_deref() != Some("easyeffects:input"),
                _ => n.selector.direction != Direction::Source,
            },
            None => self.setup.is_group(setup_id),
        }
    }

    /// Connect two endpoints. Live units are added to the setup first.
    pub fn connect(&mut self, from: &Endpoint, to: &Endpoint, from_pos: [f64; 2], to_pos: [f64; 2]) -> Result<(), String> {
        for e in [from, to] {
            if let Endpoint::Unit(k) = e
                && let Some(err) = self.unit_endpoint_error(k)
            {
                return Err(err);
            }
        }
        let mut resolve = |e: &Endpoint, pos| match e {
            Endpoint::Setup(id) => Ok(id.clone()),
            Endpoint::Unit(k) => self.adopt(k, pos),
        };
        let from = resolve(from, from_pos)?;
        let to = resolve(to, to_pos)?;
        if from == to {
            return Err("A node cannot be connected to itself".into());
        }
        if !self.can_output(&from) {
            return Err(format!("“{}” has no output", self.setup.endpoint_name(&from)));
        }
        if !self.can_input(&to) {
            return Err(format!("“{}” has no input", self.setup.endpoint_name(&to)));
        }
        if !self.setup.add_link(&from, &to) {
            return Err("Already connected".into());
        }
        Ok(())
    }

    /// Remove live links that are not part of the setup (e.g. made by another tool).
    pub fn remove_live_links(&mut self, from_unit: usize, to_unit: usize, link_ids: &[u32]) -> Result<(), String> {
        let units = &self.analysis.units.list;
        if self.analysis.ee_grabs_outputs
            && units.get(from_unit).is_some_and(|u| u.kind == UnitKind::AppOut)
            && units.get(to_unit).is_some_and(|u| u.kind == UnitKind::EeOut)
        {
            return Err(EE_GRAB_MSG.into());
        }
        let Some(b) = &self.backend else { return Err("Offline (fixture mode)".into()) };
        for id in link_ids {
            b.send(BackendCmd::Destroy(*id));
            self.destroying.insert(*id, Instant::now());
        }
        Ok(())
    }

    pub fn add_loopback(&mut self, name: &str, pos: [f64; 2]) {
        let id = self.setup.new_id("lb");
        self.setup.loopbacks.push(LoopbackDef { id: id.clone(), name: name.into() });
        let nid = self.setup.new_id("n");
        self.setup.nodes.push(SetupNode {
            id: nid.clone(),
            label: name.into(),
            selector: Selector {
                kind: SelectorKind::Role,
                direction: Direction::Any,
                profile: None,
                device: None,
                pattern: None,
                role: Some(format!("loopback:{id}")),
            },
        });
        let pid = self.setup.new_id("p");
        self.setup.placements.push(Placement { id: pid, node: nid, pos, group: None });
    }

    pub fn unload_module(&self, module: u32) {
        run_pactl(vec!["unload-module".into(), module.to_string()], None);
    }

    pub fn set_default(&self, node_name: &str, sink: bool) {
        let cmd = if sink { "set-default-sink" } else { "set-default-source" };
        run_pactl(vec![cmd.into(), node_name.into()], None);
    }

    fn compute_problems(&mut self) {
        let mut p = vec![];
        if !self.connected {
            p.push(Problem { severity: Severity::Error, text: "Not connected to PipeWire".into() });
        }
        if let Some(e) = &self.ee_error {
            p.push(Problem { severity: Severity::Error, text: e.clone() });
        }
        for e in self.loopback_errors.values() {
            p.push(Problem { severity: Severity::Error, text: e.clone() });
        }
        for l in &self.setup.links {
            if self.link_ui(&l.id) == LinkUi::Failed {
                p.push(Problem {
                    severity: Severity::Error,
                    text: format!("Could not connect “{}” → “{}”", self.setup.endpoint_name(&l.from), self.setup.endpoint_name(&l.to)),
                });
            }
        }
        for n in &self.setup.nodes {
            if n.selector.kind == SelectorKind::Name && n.selector.pattern.as_deref().is_some_and(|re| regex::Regex::new(re).is_err()) {
                p.push(Problem { severity: Severity::Error, text: format!("“{}”: invalid name pattern", n.label) });
            } else if self.analysis.node_matches.get(&n.id).is_none_or(|m| m.is_empty()) && !self.is_role_starting(n) {
                p.push(Problem { severity: Severity::Warning, text: format!("“{}” is not connected", n.label) });
            }
        }
        self.problems = p;
    }

    fn is_role_starting(&self, n: &SetupNode) -> bool {
        n.selector.role.as_deref().is_some_and(|r| {
            (r.starts_with("easyeffects:") && self.ee_launched.is_some_and(|t| t.elapsed() < EE_START_TIMEOUT))
                || r.strip_prefix("loopback:").is_some_and(|id| self.loopback_attempts.get(id).is_some_and(|t| t.elapsed() < RETRY_AFTER))
        })
    }

    fn notify(&self) {
        for f in self.on_change.clone() {
            f();
        }
    }
}

const EE_GRAB_MSG: &str = "EasyEffects is set to process all output streams, so app audio always goes through it. To change this, adjust EasyEffects' settings (disable “Process all output streams” or exclude the app).";

fn log_backend_error(m: &str) {
    // "File exists" just means the link is already there.
    if !m.contains("File exists") {
        eprintln!("audiorouter: {m}");
    }
}

pub fn schedule_reconcile(engine: &EngineRef) {
    let mut e = engine.borrow_mut();
    if e.reconcile_scheduled {
        return;
    }
    e.reconcile_scheduled = true;
    let weak = Rc::downgrade(engine);
    glib::timeout_add_local_once(Duration::from_millis(120), move || {
        if let Some(engine) = weak.upgrade() {
            engine.borrow_mut().reconcile_scheduled = false;
            reconcile_now(&engine);
        }
    });
}

/// Call after editing the setup: saves, reconciles and redraws.
pub fn setup_changed(engine: &EngineRef) {
    engine.borrow_mut().save();
    reconcile_now(engine);
}

pub fn reconcile_now(engine: &EngineRef) {
    let mut guard = engine.borrow_mut();
    let e = &mut *guard;
    e.analysis = reconcile::analyze(&e.setup, &e.live, e.ee_setting);
    if debug_enabled() {
        eprintln!(
            "[debug] reconcile: connected={} synced={} nodes={} links={} units={} desired={}",
            e.connected,
            e.live.synced,
            e.live.nodes.len(),
            e.live.links.len(),
            e.analysis.units.list.len(),
            e.analysis.desired.len()
        );
    }
    if e.live.synced && e.connected {
        let live_ports: std::collections::HashSet<(u32, u32)> = e.live.links.values().map(|l| (l.out_port, l.in_port)).collect();
        e.pending.retain(|p, _| !live_ports.contains(p));
        e.failed.retain(|p, t| !live_ports.contains(p) && t.elapsed() < RETRY_AFTER);
        e.destroying.retain(|id, t| e.live.links.contains_key(id) && t.elapsed() < PENDING_TIMEOUT);
        e.loopback_errors.retain(|id, _| e.setup.loopbacks.iter().any(|l| &l.id == id));

        let cmds = reconcile::plan(&e.setup, &e.live, &e.analysis);
        for cmd in cmds {
            execute(e, cmd, engine);
        }

        if e.setup.needs_easyeffects() && !e.analysis.ee_present {
            match e.ee_launched {
                None => {
                    e.ee_launched = Some(Instant::now());
                    if let Err(err) = easyeffects::launch() {
                        e.ee_error = Some(format!("EasyEffects could not be started: {err}"));
                    }
                }
                Some(t) if t.elapsed() > EE_START_TIMEOUT && e.ee_error.is_none() => {
                    e.ee_error = Some("EasyEffects was started but its nodes did not appear".into());
                }
                Some(t) if t.elapsed() > RETRY_AFTER * 3 => e.ee_launched = None,
                _ => {}
            }
        } else if e.analysis.ee_present {
            e.ee_error = None;
            e.ee_launched = None;
        }
    }
    e.compute_problems();
    drop(guard);
    engine.borrow().notify();
}

fn debug_enabled() -> bool {
    std::env::var_os("AUDIOROUTER_DEBUG").is_some()
}

fn execute(e: &mut Engine, cmd: Command, engine: &EngineRef) {
    if debug_enabled() {
        eprintln!("[debug] {cmd:?}");
    }
    if e.backend.is_none() {
        // Offline fixture mode.
        eprintln!("[fixture] would run: {cmd:?}");
        return;
    }
    match cmd {
        Command::CreateLink { out_node, out_port, in_node, in_port } => {
            let key = (out_port, in_port);
            if e.pending.contains_key(&key) || e.failed.contains_key(&key) {
                return;
            }
            e.pending.insert(key, Instant::now());
            e.backend.as_ref().unwrap().send(BackendCmd::CreateLink { out_node, out_port, in_node, in_port });
        }
        Command::DestroyLink(id) => {
            if e.destroying.contains_key(&id) {
                return;
            }
            e.destroying.insert(id, Instant::now());
            e.backend.as_ref().unwrap().send(BackendCmd::Destroy(id));
        }
        Command::CreateLoopback { id, name } => {
            if e.loopback_attempts.get(&id).is_some_and(|t| t.elapsed() < RETRY_AFTER) {
                return;
            }
            e.loopback_attempts.insert(id.clone(), Instant::now());
            let desc = name.replace(['"', '\\'], "");
            let args = vec![
                "load-module".into(),
                "module-loopback".into(),
                format!("sink_input_properties=\"node.autoconnect=false {LOOPBACK_PROP}={id} node.description=\\\"{desc}\\\"\""),
                format!("source_output_properties=\"node.autoconnect=false {LOOPBACK_PROP}={id} node.description=\\\"{desc} (input)\\\"\""),
            ];
            let weak = Rc::downgrade(engine);
            run_pactl(
                args,
                Some(Box::new(move |res| {
                    let Some(engine) = weak.upgrade() else { return };
                    let mut e = engine.borrow_mut();
                    match res {
                        Ok(()) => {
                            e.loopback_errors.remove(&id);
                        }
                        Err(err) => {
                            e.loopback_errors.insert(id.clone(), format!("Could not create loopback “{name}”: {err}"));
                        }
                    }
                })),
            );
        }
        Command::UnloadModule(m) => {
            if e.destroying.contains_key(&m) {
                return;
            }
            e.destroying.insert(m, Instant::now());
            run_pactl(vec!["unload-module".into(), m.to_string()], None);
        }
    }
}

type PactlDone = Box<dyn FnOnce(Result<(), String>)>;

/// Run pactl on a worker thread; the callback runs on the GTK thread.
fn run_pactl(args: Vec<String>, done: Option<PactlDone>) {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let res = match Proc::new("pactl").args(&args).output() {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => Err(format!("pactl: {e}")),
        };
        let _ = tx.send_blocking(res);
    });
    glib::spawn_future_local(async move {
        if let Ok(res) = rx.recv().await {
            if let Err(e) = &res {
                eprintln!("audiorouter: pactl failed: {e}");
            }
            if let Some(done) = done {
                done(res);
            }
        }
    });
}

fn tick(engine: &EngineRef) {
    let mut changed = false;
    {
        let mut e = engine.borrow_mut();
        let expired: Vec<(u32, u32)> = e.pending.iter().filter(|(_, t)| t.elapsed() > PENDING_TIMEOUT).map(|(k, _)| *k).collect();
        for k in expired {
            e.pending.remove(&k);
            e.failed.insert(k, Instant::now());
            changed = true;
        }
        if e.failed.values().any(|t| t.elapsed() >= RETRY_AFTER) || e.problems.iter().any(|p| p.severity == Severity::Error) {
            changed = true;
        }
        // Problems that depend on elapsed time (EasyEffects start-up, loopback creation).
        let before = e.problems.clone();
        e.compute_problems();
        changed |= before != e.problems;
    }
    if changed {
        reconcile_now(engine);
    }
}

//! PipeWire thread: mirrors the registry into a `LiveGraph` and executes link commands.
//!
//! Runs the PipeWire main loop on its own thread. Graph snapshots go to the GTK thread over an
//! `async_channel`; commands come back over a `pipewire::channel`. Reconnects automatically if
//! the PipeWire daemon restarts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use pipewire as pw;
use pw::proxy::{Listener, ProxyT};
use pw::types::ObjectType;

use crate::model::{Link, LiveGraph, MANAGED_LINK_PROP, PortDir, Props, port_from_props};

#[derive(Debug)]
pub enum BackendEvent {
    Graph(LiveGraph),
    Error(String),
    Disconnected,
}

#[derive(Debug)]
pub enum BackendCmd {
    CreateLink { out_node: u32, out_port: u32, in_node: u32, in_port: u32 },
    Destroy(u32),
}

#[derive(Clone)]
pub struct Backend {
    tx: std::sync::Arc<std::sync::Mutex<Option<pw::channel::Sender<BackendCmd>>>>,
}

impl Backend {
    pub fn send(&self, cmd: BackendCmd) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(cmd);
        }
    }
}

pub fn spawn(events: async_channel::Sender<BackendEvent>) -> Backend {
    let backend = Backend { tx: Default::default() };
    let slot = backend.tx.clone();
    std::thread::Builder::new()
        .name("pipewire".into())
        .spawn(move || {
            pw::init();
            loop {
                let (tx, rx) = pw::channel::channel();
                *slot.lock().unwrap() = Some(tx);
                if let Err(e) = run(&events, rx) {
                    let _ = events.send_blocking(BackendEvent::Error(format!("PipeWire: {e}")));
                }
                *slot.lock().unwrap() = None;
                if events.send_blocking(BackendEvent::Disconnected).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("spawn pipewire thread");
    backend
}

fn dict_props(d: Option<&pw::spa::utils::dict::DictRef>) -> Props {
    d.map(|d| d.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()).unwrap_or_default()
}

type Bound = (Box<dyn ProxyT>, Vec<Box<dyn Listener>>);

#[derive(Default)]
struct State {
    graph: LiveGraph,
    proxies: HashMap<u32, Bound>,
    created_links: Vec<pw::link::Link>,
}

fn run(events: &async_channel::Sender<BackendEvent>, rx: pw::channel::Receiver<BackendCmd>) -> Result<(), pw::Error> {
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;
    let state = Rc::new(RefCell::new(State::default()));

    let publish = {
        let events = events.clone();
        let state = Rc::downgrade(&state);
        Rc::new(move || {
            if let Some(s) = state.upgrade() {
                let s = s.borrow();
                if s.graph.synced {
                    let _ = events.send_blocking(BackendEvent::Graph(s.graph.clone()));
                }
            }
        })
    };

    // Two roundtrips: the first enumerates globals (and binds them), the second delivers the
    // info events of the bound objects. Only then is the graph complete.
    let pending_sync = Rc::new(std::cell::Cell::new(Some((core.sync(0)?, true))));
    let _core_listener = core
        .add_listener_local()
        .done({
            let state = state.clone();
            let publish = publish.clone();
            let core = core.downgrade();
            move |id, seq| {
                let Some((expected, first)) = pending_sync.get() else { return };
                if id != pw::core::PW_ID_CORE || seq != expected {
                    return;
                }
                if first {
                    if let Some(core) = core.upgrade()
                        && let Ok(seq) = core.sync(0)
                    {
                        pending_sync.set(Some((seq, false)));
                    }
                } else {
                    pending_sync.set(None);
                    state.borrow_mut().graph.synced = true;
                    publish();
                }
            }
        })
        .error({
            let mainloop = mainloop.downgrade();
            let events = events.clone();
            move |id, _seq, _res, message| {
                if id == pw::core::PW_ID_CORE {
                    if let Some(ml) = mainloop.upgrade() {
                        ml.quit();
                    }
                } else {
                    let _ = events.send_blocking(BackendEvent::Error(message.to_string()));
                }
            }
        })
        .register();

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let registry = registry.downgrade();
            let state = state.clone();
            let publish = publish.clone();
            move |obj| {
                let Some(registry) = registry.upgrade() else { return };
                let id = obj.id;
                let st = Rc::downgrade(&state);
                let publish = publish.clone();
                let bound: Option<(Box<dyn ProxyT>, Box<dyn Listener>)> = match obj.type_ {
                    ObjectType::Node => {
                        let Ok(node) = registry.bind::<pw::node::Node, _>(obj) else { return };
                        let l = node
                            .add_listener_local()
                            .info(move |info| {
                                // Props are only sent when they changed.
                                if let Some(s) = st.upgrade()
                                    && info.change_mask().contains(pw::node::NodeChangeMask::PROPS)
                                {
                                    s.borrow_mut().graph.nodes.insert(id, dict_props(info.props()));
                                    publish();
                                }
                            })
                            .register();
                        Some((Box::new(node), Box::new(l)))
                    }
                    ObjectType::Port => {
                        let Ok(port) = registry.bind::<pw::port::Port, _>(obj) else { return };
                        let l = port
                            .add_listener_local()
                            .info(move |info| {
                                if let Some(s) = st.upgrade()
                                    && info.change_mask().contains(pw::port::PortChangeMask::PROPS)
                                {
                                    let dir = if info.direction() == pw::spa::utils::Direction::Input { PortDir::In } else { PortDir::Out };
                                    let p = port_from_props(id, dir, &dict_props(info.props()));
                                    s.borrow_mut().graph.ports.insert(id, p);
                                    publish();
                                }
                            })
                            .register();
                        Some((Box::new(port), Box::new(l)))
                    }
                    ObjectType::Link => {
                        let Ok(link) = registry.bind::<pw::link::Link, _>(obj) else { return };
                        let l = link
                            .add_listener_local()
                            .info(move |info| {
                                if let Some(s) = st.upgrade() {
                                    let mut s = s.borrow_mut();
                                    let managed = if info.change_mask().contains(pw::link::LinkChangeMask::PROPS) {
                                        dict_props(info.props()).get(MANAGED_LINK_PROP).is_some_and(|v| v == "true" || v == "1")
                                    } else {
                                        s.graph.links.get(&id).is_some_and(|l| l.managed)
                                    };
                                    let link = Link {
                                        id,
                                        out_node: info.output_node_id(),
                                        out_port: info.output_port_id(),
                                        in_node: info.input_node_id(),
                                        in_port: info.input_port_id(),
                                        managed,
                                    };
                                    s.graph.links.insert(id, link);
                                    drop(s);
                                    publish();
                                }
                            })
                            .register();
                        Some((Box::new(link), Box::new(l)))
                    }
                    ObjectType::Device => {
                        let Ok(dev) = registry.bind::<pw::device::Device, _>(obj) else { return };
                        let l = dev
                            .add_listener_local()
                            .info(move |info| {
                                if let Some(s) = st.upgrade()
                                    && info.change_mask().contains(pw::device::DeviceChangeMask::PROPS)
                                {
                                    s.borrow_mut().graph.devices.insert(id, dict_props(info.props()));
                                    publish();
                                }
                            })
                            .register();
                        Some((Box::new(dev), Box::new(l)))
                    }
                    ObjectType::Metadata if obj.props.and_then(|p| p.get("metadata.name")) == Some("default") => {
                        let Ok(md) = registry.bind::<pw::metadata::Metadata, _>(obj) else { return };
                        let l = md
                            .add_listener_local()
                            .property(move |subject, key, _type, value| {
                                if let Some(s) = st.upgrade() {
                                    let mut s = s.borrow_mut();
                                    let m = &mut s.graph.metadata;
                                    match (key, value) {
                                        (None, _) => m.retain(|(subj, _), _| *subj != subject),
                                        (Some(k), None) => {
                                            m.remove(&(subject, k.to_string()));
                                        }
                                        (Some(k), Some(v)) => {
                                            m.insert((subject, k.to_string()), v.to_string());
                                        }
                                    }
                                    drop(s);
                                    publish();
                                }
                                0
                            })
                            .register();
                        Some((Box::new(md), Box::new(l)))
                    }
                    _ => None,
                };
                if let Some((proxy, listener)) = bound {
                    state.borrow_mut().proxies.insert(id, (proxy, vec![listener]));
                }
            }
        })
        .global_remove({
            let state = state.clone();
            let publish = publish.clone();
            move |id| {
                let mut s = state.borrow_mut();
                s.proxies.remove(&id);
                s.graph.remove(id);
                drop(s);
                publish();
            }
        })
        .register();

    let _cmd_receiver = rx.attach(mainloop.loop_(), {
        let core = core.clone();
        let registry = registry.clone();
        let state = state.clone();
        let events = events.clone();
        move |cmd| match cmd {
            BackendCmd::CreateLink { out_node, out_port, in_node, in_port } => {
                let props = pw::properties::properties! {
                    "link.output.node" => out_node.to_string(),
                    "link.output.port" => out_port.to_string(),
                    "link.input.node" => in_node.to_string(),
                    "link.input.port" => in_port.to_string(),
                    "object.linger" => "true",
                    MANAGED_LINK_PROP => "true",
                };
                match core.create_object::<pw::link::Link>("link-factory", &props) {
                    Ok(link) => {
                        let mut s = state.borrow_mut();
                        s.created_links.push(link);
                        // Lingering links survive their proxy; keep only recent proxies around.
                        if s.created_links.len() > 256 {
                            s.created_links.drain(..128);
                        }
                    }
                    Err(e) => {
                        let _ = events.send_blocking(BackendEvent::Error(format!("Could not create link: {e}")));
                    }
                }
            }
            BackendCmd::Destroy(id) => {
                registry.destroy_global(id);
            }
        }
    });

    mainloop.run();
    Ok(())
}

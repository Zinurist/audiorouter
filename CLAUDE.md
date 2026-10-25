# Audio Router

A native Linux desktop app that shows the machine's audio setup as a **graph of nodes** (inputs, effects, outputs) and lets the user rewire it by connecting nodes. The setup is kept in sync with the live audio system, including devices that are plugged in later, and it is **re-applied automatically at login**.

Binary: `audiorouter`. Application ID: `io.github.audiorouter.AudioRouter`.

**Simplicity over completeness.** Supporting every PipeWire feature is not a goal. When a feature would make the main workflow harder to use, leave it out.

> **Keep this file current.** It is the source of truth for the architecture. When you change the design, add a module, change a dependency, change the config format, change a CLI flag or make a design decision, update the matching section here in the same change. If the code and this file disagree, fix one of them before you finish.
>
> **Keep machine-specific details out of this file.** Do not add device names, serials, hostnames or paths from the developer's own machine. The app must work on any Linux system that meets the requirements below. Use generic examples, such as "a USB headphone adapter".

## Status

First working version. All features described here are implemented.

- **Verified:** unit tests against a `pw-dump` fixture (`cargo test`); the backend and reconciler against a private PipeWire instance (link creation, group fan-out, hotplug auto-connect, removal, loopback properties); window rendering (`AUDIOROUTER_SCREENSHOT`).
- **Not verified automatically:** mouse interaction (dragging, connecting, context menus, dialogs). Test these by hand after UI changes.

## Requirements and limitations

- Linux with **PipeWire**, a session manager (**WirePlumber**) and **pipewire-pulse** (`pactl` must be installed; it is used for loopbacks and default devices). Plain PulseAudio and JACK-only setups are not supported.
- GTK 4.14+ and libadwaita 1.6+. The app targets GNOME and works on Wayland and X11.
- EasyEffects is optional. If it is installed, the app has special support for it (see below).
- **Application streams** (browser, games, …) are display-only. They cannot be part of the setup or be rerouted, because WirePlumber routes them to the default device. The user changes routing for them with "Use as default output" on a device.

## Core use cases

1. **See the real signal flow.** Every source, intermediate node and sink is shown with the links that **actually** exist. Requested targets can differ from actual links: EasyEffects, for example, can silently redirect streams.
2. **Rewire by connecting nodes.** Drag from the dot on a box's right edge onto another box or group. Select a line and press Delete (or right-click it) to disconnect.
3. **Intermediate nodes:** loopbacks (for example, to hear a TV that is plugged into a line-in or capture device) and the EasyEffects output chain.
4. **Groups.** Audio sent to a group goes to every node in it.
5. **Flexible device matching** by selector (exact device, model, vendor or name regex), so renamed nodes and replacement devices of the same model keep working.
6. **Clear change feedback.** The UI shows the live state. A change appears as an animated pending line until PipeWire confirms it, and turns red with a banner message if it fails.
7. **Apply on login and keep applying** (`--background`), including starting EasyEffects. Failures show a red banner and a notification; they are never silent.

## Background: how a typical setup looks in PipeWire

This is the kind of setup that motivated the app. It is described generically.

- Hardware sink: a USB headphone adapter (the final output). Hardware sources: a USB capture device with a TV connected, and an unused microphone.
- EasyEffects runs an output effect chain (for example, a loudness EQ) and has "process all output streams" enabled.
- The TV used to be made audible with `pactl load-module module-loopback source=<capture> sink=<headphones>`, run from an autostart entry after a `sleep`.

```
TV capture device ──▶ loopback (input.loopback-*) ─▶ (output.loopback-*) ─┐
App streams ────────────────────────────────────────────────────────────┤
                                                                          ▼
                                                     easyeffects_sink (virtual sink)
                                                                          │ monitor ports
                                                                          ▼
                                              ee_soe_* filter chain (gate, compressor, EQ, limiter, …)
                                                                          │
                                                                          ▼
                                                        hardware headphone sink
```

Facts the code relies on:

- EasyEffects redirects streams by writing `target.object` (the `object.serial` of `easyeffects_sink`) into the `default` metadata for each stream. A stream's own `target.object` property can still name a different device, so the app always reads **real links**.
- `pactl load-module module-loopback` creates two nodes, `input.loopback-*` (Stream/Input/Audio) and `output.loopback-*` (Stream/Output/Audio). They share `node.link-group` and carry `pulse.module.id`. The app shows them as **one** loopback box.
- EasyEffects nodes: `easyeffects_sink`, `easyeffects_source` and `ee_soe_*` / `ee_sie_*` filters in `node.group` `ee_sink_group` / `ee_source_group`. The app collapses each chain into **one** box. The output chain's last node is `ee_soe_output_level`; if that is missing, it is found from the links. `ee_test_signals` is ignored.
- EasyEffects stores its output device **by node name** (`~/.config/easyeffects/db/easyeffectsrc`, `[StreamOutputs] outputDevice`). The "process all outputs" setting is `processAllOutputs` in the same section. If the key is absent, the default applies, and the app infers it from the metadata.
- ALSA node names end in a profile (`….analog-stereo`, `….mono-fallback`, `….hdmi-stereo`). The part before it (such as `-00`) can change between boots.

## Core model

The setup has **nodes**, **groups** and **links**. On the canvas, each node is drawn as one or more **placements**. The live graph is collapsed into **units** (one box each): hardware sink/source, virtual sink/source, app stream, loopback, EasyEffects output, EasyEffects input.

### Nodes and selectors

A setup node is a selector plus a direction. It matches **zero, one or several** units.

| Kind | Matches on | Use |
|---|---|---|
| `exact` | bus + vendor + product + `device.serial` (or `device.bus-path` if there is no serial) | Two identical devices that must stay apart |
| `model` | `device.bus` + `device.vendor.id` + `device.product.id` | **Default for hardware.** Replacement units of the same model work with no change |
| `vendor` | bus + vendor | Any device from one maker |
| `name` | Regex on the node name or description | Virtual nodes; anything else. **Default for virtual sinks and sources** (`^<escaped node name>$`) |
| `role` | `easyeffects:output`, `easyeffects:input`, `loopback:<id>` | Nodes that EasyEffects or the app creates |

- Hardware selectors also filter by direction and an optional **profile** (the node name's last segment, for example `analog-stereo`). Device properties come from the PipeWire Device object (`device.id` on the node).
- Selectors never match app streams.
- A node that matches several units fans links out to all of them and shows "N connected". A node that matches nothing shows "Not connected" (a warning); its links wait and are created when a matching device appears.
- There is no separate "dynamic group" type: a broad selector already behaves like a group that fills itself.

### Groups, placements and duplicates

- A group is a named box with an input and an output socket. Linking into a group fans out to all members; linking out of a group mixes all members.
- **Membership is geometric:** a placement belongs to the smallest group box that contains the centre of its header. Membership is recomputed after every drag, resize or group move. Moving a group moves its members with it. Groups do not nest.
- A node can have several placements ("Duplicate" in the context menu), each in at most one group. That is how one node belongs to several groups.
- **Links belong to nodes and groups, never to placements**, so every placement shows every link of its node. Removing the last placement removes the node, its links and its loopback.

### Links: desired vs live, managed vs foreign

- A setup link (`from` → `to`, node or group IDs) expands to the cartesian product of matched units. It then becomes port links with automatic channel mapping: same channel names; mono fans out to all channels; otherwise channels are paired by index. Monitor ports are never used as outputs.
- **Managed** live links are created by the app with `object.linger = true` and `audiorouter.managed = true`. **Foreign** links come from WirePlumber, EasyEffects or other tools.
- Canvas drawing:
  - setup links: solid accent (all present), orange (partial), grey dotted (waiting for a device), animated dashes (pending), red (failed);
  - foreign links not explained by the setup: grey dashed;
  - managed links that are no longer wanted: orange dashed (briefly, while they are removed).
- Live units that no setup node matches are drawn as dashed boxes, in auto-layout columns: sources and apps | loopbacks | EasyEffects | sinks. Connecting to one of them, or using "Add to setup", adopts it into the setup with the default selector. Their dragged positions are saved in `layout`.

### Reconciliation (`reconcile::plan`)

The reconciler:
- creates missing desired port links;
- removes managed links that are no longer desired, but only when both ends belong to known units (a half-known graph must never cause removals);
- creates missing loopbacks and unloads app-created loopbacks that are no longer in the setup;
- **EasyEffects output exception:** when the setup routes the EasyEffects output, it also removes EasyEffects' *own* links from its output to other targets. EasyEffects otherwise keeps linking to its saved device (by name), which would duplicate the audio. When the setup does not route EasyEffects, its own routing is left alone.
- never touches other foreign links, unmanaged nodes or EasyEffects' internal chain.

The engine (`engine.rs`) runs the reconciler 120 ms after any live or setup change. A created link counts as pending until it appears; after 4 s it is marked failed and retried 10 s later. Destroys and loopback creation are rate-limited in the same way. GUI edits change the setup, which is **saved immediately** and applied; there is no separate "apply" step.

### EasyEffects decisions

- **Detection:** units with `node.group` `ee_sink_group` / `ee_source_group`.
- **"Grabs all outputs"** = EasyEffects is present and its `processAllOutputs` setting is true. If the setting is unset, the app looks for app streams whose metadata `target.object` points at `easyeffects_sink`. When it grabs:
  - its box says "Processes all app audio";
  - connecting an app stream elsewhere, or deleting an app stream → EasyEffects link, is refused with a warning toast that tells the user to change it in EasyEffects' settings.
- **Loopbacks created by the app are not grabbed:** they use `node.autoconnect = false`, so WirePlumber, and therefore EasyEffects' redirect, never links them. The setup decides where they go, for example loopback → EasyEffects → headphones.
- **Launch:** if any setup node has an EasyEffects role and no EasyEffects units exist, the app runs `easyeffects --service-mode` (`--gapplication-service` for versions without `--service-mode`), or the Flatpak `com.github.wwmm.easyeffects`. If the nodes have not appeared after 15 s, a red error is shown. It retries about every 30 s.
- The EasyEffects input chain (`EeIn`) has no input socket, because EasyEffects feeds it from its own input-device setting. Only its output (`easyeffects_source`) can be connected.

### Loopbacks

Loopbacks are created with:

```
pactl load-module module-loopback \
  sink_input_properties="node.autoconnect=false audiorouter.loopback=<id> node.description=\"<name>\"" \
  source_output_properties="node.autoconnect=false audiorouter.loopback=<id> node.description=\"<name> (input)\""
```

The module lives in pipewire-pulse, so it outlives the app. A loopback created by the app is recognised by `audiorouter.loopback`; matching uses `role = "loopback:<id>"`, and removal uses `pactl unload-module <pulse.module.id>`. Loopbacks created outside the app are shown, can be unloaded from the context menu, and cannot be adopted (the user creates a new one instead).

## Architecture

**Rust + gtk4-rs + libadwaita.** The canvas is a `gtk::DrawingArea` drawn with cairo and pangocairo. The PipeWire native API is used through the `pipewire` crate (0.10).

```
src/
  main.rs          # adw::Application (single instance), --background, debug render/screenshot hooks
  model.rs         # LiveGraph (raw PipeWire objects), pw-dump parser, Units (collapsing), port mapping
  setup.rs         # Setup data model + TOML load/save, ids, placement/node/group removal rules
  reconcile.rs     # Selector matching, link expansion, EasyEffects grab detection, plan() → Commands
  backend.rs       # PipeWire thread: registry → LiveGraph snapshots; executes CreateLink/Destroy
  engine.rs        # GTK-thread orchestrator: reconcile scheduling, pending/failed tracking, pactl, problems
  easyeffects.rs   # Read processAllOutputs, launch EasyEffects
  autostart.rs     # ~/.config/autostart/audiorouter.desktop (Exec=<current exe> --background)
  ui/
    scene.rs       # Geometry (items, groups, edges) from engine state + cairo drawing; no widgets
    canvas.rs      # DrawingArea + gestures (drag, connect, resize), context menus, membership refresh
    dialogs.rs     # Prompt, confirm, message, node settings (selector editing)
    window.rs      # Header bar, problem banner (red/orange), toasts, help, status subtitle
```

- `model`, `setup` and `reconcile` contain no GTK or PipeWire types. Test them with fixtures.
- **Backend thread:**
  - Binds Node, Port, Link, Device and the `default` Metadata, and keeps their proxies and listeners alive in a map.
  - Sends a full `LiveGraph` clone after every change; the engine drains the channel and keeps only the latest.
  - Commands arrive over a `pipewire::channel`.
  - If the PipeWire core fails, the thread reconnects after 2 s.
- **Backend gotchas** (already handled; keep them in mind):
  - `info` events only carry props when `change_mask` contains `PROPS`. Never overwrite props otherwise, or nodes lose their media class for a moment.
  - The graph is only `synced` after **two** core syncs. The first enumerates and binds globals; the second delivers their info. Reconciling before that makes the reconciler create duplicate links ("File exists").
- **Process model:**
  - Single instance. `--background` holds the application with no window. Running `audiorouter` again opens the window in that instance.
  - Without `--background`, closing the window quits the app. With it, closing only hides the window.
  - In background mode, any problems still present 25 s after start-up cause a notification, and the window is presented.

## Saved setup

`$XDG_CONFIG_HOME/audiorouter/setup.toml` (overridable with `AUDIOROUTER_SETUP`). It is written atomically on every edit. There is one setup; profiles are out of scope.

```toml
next_id = 7
[[nodes]]
id = "n1"
label = "Headphones"
[nodes.selector]
kind = "model"            # exact | model | vendor | name | role
direction = "sink"        # sink | source | any
profile = "analog-stereo" # optional
[nodes.selector.device]   # kept so the kind can be changed later
bus = "usb"
vendor_id = "0x1234"
product_id = "0x5678"
serial = "…"              # optional
bus_path = "…"            # optional
description = "USB headphone adapter"
# name selectors: pattern = "^regex$"; role selectors: role = "easyeffects:output"

[[groups]]
id = "g2"
name = "Speakers"
rect = [x, y, w, h]

[[placements]]
id = "p3"
node = "n1"
pos = [x, y]
group = "g2"              # derived from geometry; optional

[[links]]
id = "l4"
from = "n5"               # node or group id
to = "g2"

[[loopbacks]]
id = "lb6"
name = "TV"

[layout]                  # positions of live units that are not in the setup
"node:alsa_output.…" = [x, y]
```

## CLI

- `audiorouter`: open the window. If an instance is already running, its window is opened instead.
- `audiorouter --background` (`-b`): no window. Apply the setup and keep it applied. Used by autostart; enable it with "Start at Login" in the app menu.
- Debug environment variables:
  - `AUDIOROUTER_SETUP=<file>`: use a different setup file.
  - `AUDIOROUTER_FIXTURE=<pw-dump.json>`: run offline on a `pw-dump` snapshot; commands are only logged.
  - `AUDIOROUTER_RENDER=<out.png>`: render the canvas offline and exit. Use it with `AUDIOROUTER_FIXTURE`; `AUDIOROUTER_DARK=1` selects the dark palette.
  - `AUDIOROUTER_SCREENSHOT=<out.png>`: open the window, save a screenshot after 2.5 s and quit.
  - `AUDIOROUTER_DEBUG=1`: log every reconcile pass and command.

## Build, install and test

- System packages (Fedora names): `gtk4-devel`, `libadwaita-devel`, `pipewire-devel`, `clang-devel`. Debian/Ubuntu: `libgtk-4-dev`, `libadwaita-1-dev`, `libpipewire-0.3-dev`, `libclang-dev`.
- `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt` (`rustfmt.toml`: width 140).
- `scripts/install.sh` builds a release binary and installs it to `~/.local/bin` with a desktop entry (`data/`). Install before enabling "Start at Login", because the autostart entry records the current executable's path.
- **Never change the developer's live audio setup while testing.**
  - Unit tests use `tests/fixtures/*.json`: trimmed, anonymised `pw-dump` output.
  - For end-to-end tests, run a **private PipeWire instance** with a short runtime dir (Unix socket paths must stay under 108 characters):
    ```sh
    export XDG_RUNTIME_DIR=/tmp/<short> PIPEWIRE_RUNTIME_DIR=/tmp/<short> PULSE_SERVER=unix:/tmp/<short>/pulse/native
    pipewire & pipewire -c pipewire-pulse.conf &
    pw-cli create-node adapter '{ factory.name=support.null-audio-sink node.name=hp media.class=Audio/Sink object.linger=true audio.position=[FL FR] }'
    # Without WirePlumber, adapters have no ports until configured:
    pw-cli set-param <id> PortConfig '{ direction: Input, mode: dsp, format: { mediaType: audio, mediaSubtype: raw, format: F32P, rate: 48000, channels: 2, position: [ FL, FR ] } }'
    AUDIOROUTER_DEBUG=1 AUDIOROUTER_SETUP=/tmp/<short>/setup.toml audiorouter --background
    ```
  - In that private instance there is no session manager, so loopback ports never appear and links to loopbacks cannot be tested there.

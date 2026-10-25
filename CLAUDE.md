# Audio Router

A native Linux desktop app that shows the machine's audio setup as a **graph of nodes** (inputs, effects, outputs) and lets the user rewire it by connecting nodes. The setup is kept in sync with the live audio system, including devices that are plugged in later, and it is **re-applied automatically at login**.

Binary: `audiorouter`. Application ID: `io.github.audiorouter.AudioRouter` (debug builds: `….Devel`, so `cargo run` does not attach to an installed instance that is running).

**Simplicity over completeness.** Supporting every PipeWire feature is not a goal. When a feature would make the main workflow harder to use, leave it out.

> **Keep this file current.** It is the source of truth for the architecture. When you change the design, add a module, change a dependency, change the config format, change a CLI flag or make a design decision, update the matching section here in the same change. If the code and this file disagree, fix one of them before you finish.
>
> **Keep machine-specific details out of this file.** Do not add device names, serials, hostnames or paths from the developer's own machine. The app must work on any Linux system that meets the requirements below. Use generic examples, such as "a USB headphone adapter".

## Status

First working version. All features described here are implemented.

- **Verified:** unit tests against a `pw-dump` fixture (`cargo test`); the backend and reconciler against a private PipeWire instance (link creation, group fan-out, hotplug auto-connect, removal, loopback properties incl. `media.name`); window rendering (`AUDIOROUTER_SCREENSHOT`).
- **Not verified automatically:** mouse interaction (dragging, connecting, context menus, dialogs). Test these by hand after UI changes.

## Requirements and limitations

- Linux with **PipeWire**, a session manager (**WirePlumber**) and **pipewire-pulse** (`pactl` must be installed; it is used for loopbacks and default devices). Plain PulseAudio and JACK-only setups are not supported.
- GTK 4.14+ and libadwaita 1.6+. The app targets GNOME and works on Wayland and X11.
- The app always uses libadwaita's dark style (`ColorScheme::ForceDark`, set at startup). The canvas palette (`scene::Palette`) follows `StyleManager::is_dark`, so the light palette is only kept for the offline renderer.
- EasyEffects is optional. If it is installed, the app has special support for it (see below).
- **Application streams** (browser, games, …) are display-only. They cannot be part of the setup or be rerouted, because WirePlumber routes them to the default device. The user changes routing for them with "Use as default output" on a device.

## Core use cases

1. **See the real signal flow.** Every source, intermediate node and sink is shown with the links that **actually** exist. Requested targets can differ from actual links: EasyEffects, for example, can silently redirect streams.
2. **Rewire by connecting nodes.** Drag from the dot on a box's right edge onto another box or group. Select a line and press Delete (or right-click it) to disconnect.
   - **Unbounded canvas, no scrollbars.** Dragging empty space (or a group's body) pans; touchpad scrolling pans; the mouse wheel and pinch zoom around the pointer (25–300 %). A header-bar button next to the menu shows the zoom (only when it is not 100 %) and resets it; Ctrl+0 / Ctrl++ / Ctrl+- do the same. Scene geometry is in world coordinates; `scene::View` (pan, zoom) maps to the screen. The view is not saved.
3. **Hear inputs** (for example, a TV plugged into a capture device) by connecting the input to an output or to EasyEffects. Loopbacks are an implementation detail the user never has to think about (see below). The EasyEffects output chain is the intermediate node.
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
- `pactl load-module module-loopback` creates two nodes, `input.loopback-*` (Stream/Input/Audio) and `output.loopback-*` (Stream/Output/Audio). They share `node.link-group` and carry `pulse.module.id`. The app shows loopbacks created outside the app as **one** box and hides its own (see Loopbacks).
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
| `role` | `easyeffects:output`, `easyeffects:input` | EasyEffects' chains |

- Hardware selectors also filter by direction and an optional **profile** (the node name's last segment, for example `analog-stereo`). Device properties come from the PipeWire Device object (`device.id` on the node).
- Selectors never match app streams or the app's own hidden loopbacks.
- A node that matches several units fans links out to all of them and shows "N connected". A node that matches nothing shows "Not connected" (a warning); its links wait and are created when a matching device appears.
- There is no separate "dynamic group" type: a broad selector already behaves like a group that fills itself.

### Groups, placements and duplicates

- A group is a named box with an input and an output socket. Linking into a group fans out to all members; linking out of a group mixes all members.
- **Membership is geometric:** a placement belongs to the smallest group box that contains the centre of its header. Membership is recomputed after every drag, resize or group move. Moving a group moves its members with it. Groups do not nest.
- A node can have several placements ("Duplicate" in the context menu), each in at most one group. That is how one node belongs to several groups.
- **Links belong to nodes and groups, never to placements**, so every placement shows every link of its node. Removing the last placement removes the node and its links (its hidden loopback then goes away too).

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
- creates missing hidden loopbacks, and unloads app-created loopbacks that are no longer wanted or whose name no longer matches the node label;
- **EasyEffects output exception:** when the setup routes the EasyEffects output, it also removes EasyEffects' *own* links from its output to other targets. EasyEffects otherwise keeps linking to its saved device (by name), which would duplicate the audio. When the setup does not route EasyEffects, its own routing is left alone.
- never touches other foreign links, unmanaged nodes or EasyEffects' internal chain.

The engine (`engine.rs`) runs the reconciler 120 ms after any live or setup change. A created link counts as pending until it appears; after 4 s it is marked failed and retried 10 s later. Destroys and loopback creation are rate-limited in the same way. GUI edits change the setup, which is **saved immediately** and applied; there is no separate "apply" step.

### EasyEffects decisions

- **Detection:** units with `node.group` `ee_sink_group` / `ee_source_group`.
- **"Grabs all outputs"** = EasyEffects is present and its `processAllOutputs` setting is true. If the setting is unset, the app looks for app streams whose metadata `target.object` points at `easyeffects_sink`. When it grabs:
  - its box says "Processes all app audio";
  - connecting an app stream elsewhere, or deleting an app stream → EasyEffects link, is refused with a warning toast that tells the user to change it in EasyEffects' settings.
- **Loopbacks created by the app are not grabbed:** they use `node.autoconnect = false`, so WirePlumber, and therefore EasyEffects' redirect, never links them. The setup decides where they go, for example TV → EasyEffects → headphones.
- **Launch:** if any setup node has an EasyEffects role and no EasyEffects units exist, the app runs `easyeffects --service-mode` (`--gapplication-service` for versions without `--service-mode`), or the Flatpak `com.github.wwmm.easyeffects`. If the nodes have not appeared after 15 s, a red error is shown. It retries about every 30 s.
- The EasyEffects input chain (`EeIn`) has no input socket, because EasyEffects feeds it from its own input-device setting. Only its output (`easyeffects_source`) can be connected.

### Loopbacks (hidden, one per input)

Decision: **inputs never link to their targets directly, and loopbacks are not a user-facing concept.** The user connects an input (hardware source, virtual source or the EasyEffects mic output) to a target. The app routes it as `input device → hidden loopback → targets`. The loopback gives the route a buffer between devices with different clocks and a named stream in the system sound settings. There is no "add loopback" UI.

- **One loopback per input node** (setup node), not per link. It exists while the node feeds at least one link (directly or through a group it is in) **and** matches at least one input device. Otherwise it is unloaded. If the node matches several devices, they are all mixed into that one loopback.
- **Tied to the device through the selector, not the loopback.** The loopback is created without `source=`/`sink=`, with `node.autoconnect=false`. The reconciler links matched device → loopback input and loopback output → targets, so renamed or replaced devices keep working.
- **Name = node label.** It is set as `node.description` and `media.name`; the latter is what GNOME's sound settings show. The name is fixed at creation, so renaming the node unloads the loopback and the next pass recreates it (a brief gap in audio). `reconcile::loopback_name` strips quotes and backslashes.
- Creation command:
  ```
  pactl load-module module-loopback \
    sink_input_properties="node.autoconnect=false audiorouter.loopback=<node id> node.description=\"<name>\" media.name=\"<name>\"" \
    source_output_properties="node.autoconnect=false audiorouter.loopback=<node id> node.description=\"<name> (input)\" media.name=\"<name> (input)\""
  ```
- **Ours vs foreign:** the `audiorouter.loopback` property marks the app's loopbacks. Those are hidden on the canvas (`Analysis::hidden_units`); the setup link is drawn from the input box. Loopbacks **without** the mark (made by pactl, another tool, an old autostart entry) are shown as normal boxes, are never touched automatically, can be unloaded from their context menu, and cannot be added to the setup.
- If an input with a hidden loopback also feeds a foreign loopback, a warning says it may be heard twice.
- The module lives in pipewire-pulse, so it outlives the app; the mark lets the app find it again after a restart. Removal uses `pactl unload-module <pulse.module.id>`.
- While the loopback is being created, links from that input show as pending; a failed `pactl` call shows them as failed with a red banner.

## Architecture

**Rust + gtk4-rs + libadwaita.** The canvas is a `gtk::DrawingArea` drawn with cairo and pangocairo. The PipeWire native API is used through the `pipewire` crate (0.10). The tray icon uses `ksni` (StatusNotifierItem over D-Bus; `async-io` + `blocking` features, no tokio).

```
src/
  main.rs          # adw::Application (single instance), --background, tray/background/close-dialog wiring, debug hooks
  settings.rs      # App preferences (settings.toml): what the close button does
  model.rs         # LiveGraph (raw PipeWire objects), pw-dump parser, Units (collapsing), port mapping
  setup.rs         # Setup data model + TOML load/save, ids, placement/node/group removal rules
  reconcile.rs     # Selector matching, link expansion, EasyEffects grab detection, plan() → Commands
  backend.rs       # PipeWire thread: registry → LiveGraph snapshots; executes CreateLink/Destroy
  engine.rs        # GTK-thread orchestrator: reconcile scheduling, pending/failed tracking, pactl, problems
  easyeffects.rs   # Read processAllOutputs, launch EasyEffects
  autostart.rs     # ~/.config/autostart/audiorouter.desktop (Exec=<current exe> --background); refreshed at start-up if enabled
  icons.rs         # Embedded app + symbolic tray icon, written flat into $XDG_CACHE_HOME/audiorouter/icons (no hicolor/ subdir: the AppIndicator extension cannot find icons there without an index.theme)
  ui/
    scene.rs       # Geometry (items, groups, edges) from engine state + cairo drawing; no widgets
    canvas.rs      # DrawingArea + gestures (free drag, connect, resize; no grid snapping), pan/zoom, context menus, membership refresh
    dialogs.rs     # Prompt, confirm, message, node settings (selector editing)
    window.rs      # Header bar + menu, problem banner (red/orange), toasts, help, status subtitle
    tray.rs        # Tray icon (ksni thread → events on the GTK thread), tooltip = status, warning icon on errors
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
  - A **tray icon** is shown while the app runs, unless "Show in System Tray" (app menu, `show_tray`) is off (needs a StatusNotifierItem host: KDE, or GNOME with the AppIndicator extension). Clicking it, or its "Open Audio Router" item, shows the window; its "Quit" item quits. Without a tray icon (turned off, or no tray host) the app still works; going to the background then sends a notification that it is still running. The tray uses the app's own symbolic icon via `IconThemePath` (`dialog-warning-symbolic` from the theme on errors).
  - **Run in Background** (app menu) hides the window and holds the app. Starting `audiorouter` again attaches to the running instance and shows its window (GApplication single instance over D-Bus).
  - The **close button** does what `close_action` in `settings.toml` says: `ask` (default) shows a dialog with Quit / Run in Background and a "Remember my choice" check box; `background` runs in the background; `quit` quits. The menu's "When Closing the Window" submenu changes it (that is how a remembered choice is undone). Quit (Ctrl+Q) always quits. Close behaviour does **not** depend on "Start at Login".
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

[layout]                  # positions of live units that are not in the setup
"node:alsa_output.…" = [x, y]
```

### App settings

`$XDG_CONFIG_HOME/audiorouter/settings.toml` holds preferences that are not part of the audio setup (written atomically when changed):

```toml
close_action = "ask"   # ask | background | quit
show_tray = true
```

## CLI

- `audiorouter`: open the window. If an instance is already running, its window is opened instead.
- `audiorouter --background` (`-b`): no window. Apply the setup and keep it applied. Used by autostart; enable it with "Start at Login" in the app menu.
- `audiorouter --version` (`-v`): print `audiorouter X.Y` (handled locally, never forwarded to a running instance).
- Debug environment variables:
  - `AUDIOROUTER_SETUP=<file>`: use a different setup file.
  - `AUDIOROUTER_FIXTURE=<pw-dump.json>`: run offline on a `pw-dump` snapshot; commands are only logged.
  - `AUDIOROUTER_RENDER=<out.png>`: render the canvas offline and exit. Use it with `AUDIOROUTER_FIXTURE`; `AUDIOROUTER_DARK=1` selects the dark palette.
  - `AUDIOROUTER_SCREENSHOT=<out.png>`: open the window, save a screenshot after 2.5 s and quit.
  - `AUDIOROUTER_DEBUG=1`: log every reconcile pass and command.

## Build, install and test

- `README.md` is the short user-facing description (what it does, screenshot, build). Keep it short and keep the vibe-coding disclaimer. `docs/screenshot.png` is taken with `AUDIOROUTER_SCREENSHOT` on the test fixture plus a demo setup, never on a real system.
- System packages (Fedora names): `gtk4-devel`, `libadwaita-devel`, `pipewire-devel`, `clang-devel`. Debian/Ubuntu: `libgtk-4-dev`, `libadwaita-1-dev`, `libpipewire-0.3-dev`, `libclang-dev`.
- `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt` (`rustfmt.toml`: width 140).
- Icons live in `data/`: `io.github.audiorouter.AudioRouter.svg` (128 px app icon: waveform source routed to two outputs on a dark tile) and `…-symbolic.svg` (16 px, **filled shapes only**, because symbolic recoloring replaces fills and would turn stroked lines into blobs). Both are embedded with `include_str!`; the window uses the app icon by name (`set_default_icon_name`) after adding the cache dir to GTK's icon search path.
- `./install.sh` builds a release binary and copies it to `~/Applications/AudioRouter/audiorouter`, plus the app icon as `audiorouter.svg` (used by the autostart entry). The binary is copied and then renamed, so a running instance does not block it. It creates **no** desktop entry. Install before enabling "Start at Login", because the autostart entry records the current executable's path. Enabling it from a cargo build (`…/target/debug|release/…`) is refused with a message.
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
  - In that private instance there is no session manager, so loopback ports never appear and links through loopbacks cannot be tested there (loopback creation and its properties can).

## CI, releases and versioning

- **Versioning:** two numbers, `X.Y` (tags and releases `vX.Y`). Cargo needs three, so `Cargo.toml` says `X.Y.0` (patch always 0); `--version` prints `X.Y`. `scripts/set-version.sh X.Y` updates `Cargo.toml` and `Cargo.lock`.
- **Releasing:** run `scripts/set-version.sh X.Y` and commit, tag `vX.Y` on `main`, push the tag, then review and publish the **draft** release by hand.
- **`.github/workflows/ci.yml`** (push to `main`, PRs, manual): `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- **`.github/workflows/build.yml`** (reusable or manual): release build, packaged as `audiorouter-<label>-x86_64-linux.tar.gz` (binary, `audiorouter.svg`, README), uploaded as artifact `dist`.
- **`.github/workflows/release.yml`** (tag `vX.Y`, or manual for a dry run): checks that the tag is `vX.Y` and matches `Cargo.toml`, runs `build.yml` with label `X.Y`, then creates a draft release. Its notes are `.github/RELEASE_DISCLAIMER.md` followed by GitHub's generated changelog since the previous `vX.Y` tag.
- All jobs run in a **`fedora:43` container**: Ubuntu runners' libadwaita is older than 1.6. The release binary therefore needs glibc 2.42+ (Fedora 43 or newer, or a similarly recent distribution).
- The disclaimer exists twice, in `.github/RELEASE_DISCLAIMER.md` and in the README. Keep them in sync.
- Workflows can be linted with `actionlint` (e.g. `podman run --rm -v "$PWD":/repo:Z -w /repo docker.io/rhysd/actionlint`).

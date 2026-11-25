# Discovery 20 — the Rust/Tauri engine (`the Tauri predecessor`)

Path prefix: `upstream/` = `the Tauri predecessor checkout`. Workspace members:
`crates/mh-protocol`, `crates/mh-device`, `app/src-tauri` (`upstream/Cargo.toml:1-3`; package
`hubdeck` v0.2.3, edition 2024, release `panic = "abort"`, root `Cargo.toml:5-8, 27-34`).

## 1. Crates and process model

- `mh-protocol`: pure encoding/decoding, no I/O (`crates/mh-protocol/src/lib.rs:1-2`). Public:
  `ModuleKind`/`VENDOR_ID = 0x2516` (`module.rs:3-14`), report builders (`report.rs`), input parser
  (`input.rs`), descriptor parser (`descriptor.rs`), and `safety.rs` (§6).
- `mh-device`: hidapi 2.6 transport (`upstream/Cargo.toml:13`; Linux uses `linux-static-libusb`
  because vendor HID gets no hidraw node, `crates/mh-device/Cargo.toml:17-20`). `Hub::start()`
  spawns scanner thread `mh-scan` (`hub.rs:59`) and one reader thread per module `mh-read-<kind>`
  (`hub.rs:264`); `RESCAN = 1 s`, `IDLE_RESCAN = 15 s` (`hub.rs:20-21`). Read error ⇒ module
  removed, `Event::Disconnected` (`hub.rs:287-295`); return ⇒ re-init + redraw; measured 5 s for
  keys to come back (PROTOCOL.md:174). `Module::initialize` does the software-mode handshake with
  `MODE_SETTLE = 600 ms` and a double software-mode command before the first read
  (`module.rs:12, 191-222`); `release()` hands keys/dial back to hardware mode (`module.rs:227-240`).
- **No daemon either**: the Tauri app owns the device in-process. `main.rs:5-35` dispatches
  `--mcp`/`--mcp-deck`/`--guard`/`cli …` subcommands; otherwise `hubdeck_lib::run()`
  (`app/src-tauri/src/lib.rs:344-611`). Threads: `deck` (the mh-device hub, `deck.rs:467-470`),
  engine worker `engine` (`engine/mod.rs:836, 1986`), `foreground` (`:840`), `media` (`:862`),
  `monitor` (`:863`), `plugin-server`/`api-http`/`plugin-watch` (`engine/plugins.rs:361-375`),
  per-screen `keys pictures`/`dial pictures` (`mod.rs:1475-1535`), `leds` (`mod.rs:1650`).
  "All HID I/O runs on blocking threads, never on the async runtime or the UI thread"
  (ARCHITECTURE.md:59-60).
- Single instance: `tauri_plugin_single_instance` + file lock `%TEMP%/hubdeck-deck.lock`, second
  copy exits after 10 s (`lib.rs:196-233, 352-357`). State: in-process `Arc<Mutex<Shared>>`
  (`engine/mod.rs:586`), persisted as JSON profiles + `settings.json` (atomic temp+rename,
  `engine/profile.rs:882-909`).
- Guard child process (`hubdeck --guard`, `src/guard.rs:30-44`): liveness via an anonymous stdin
  pipe; on unclean exit it re-opens the hub to hand keys/dial back to firmware and restarts the
  app, max 3 restarts/10 min, writes `last-crash.json` (`guard.rs:15-17, 57-149`).
- Test copy: `HUBDECK_AUTOMATION=<dir>` runs a second instance with virtual modules
  (`src/automation.rs:4-45`, `deck.rs:474-491`).

## 2. Local API / CLI / MCP — status: implemented

- HTTP + WebSocket servers on `127.0.0.1` **ephemeral ports**, started in
  `engine/plugins.rs:336-377`: WS `TcpListener::bind("127.0.0.1:0")` (`plugins.rs:346`) with the
  port written to `<data>/api-port` (`:354`); HTTP via `tiny_http` with port in `api-http-port`
  (`:355-358`); threads `plugin-server` (`:373`) and `api-http` (`:361`). Auth: per-install UUIDv4
  token in `<data>/api-token` (`:341-344`), HTTP `Authorization: Bearer <token>` → 401 otherwise
  (`:272-283`), WS checks `token` in the `register` message (`:751-757`); body cap
  `MAX_BODY = 16_000_000` (`:266`). One JSON object per WS frame with an `"event"` field.
- Routes (`engine/api.rs:86-180`): `GET /api/catalog|modules|modules/<id>/check|apps|profiles|…|
  leds|states|presses|deck/keys|…`, `POST /api/profiles…|controls|lights|pages|images|animations|
  state|apps/announce|quit|plugins/install|…` plus test-only `/api/test/*`. `GET /api/leds` returns
  `{shown, sent}` — the engine's own tracking of what was sent, since the device does not ack
  (api.rs:135-143).
- CLI: `hubdeck cli {status|profiles|profile|key|lights|doctor}` (`engine/cli.rs:21-105`,
  docs/CLI.md) — a thin HTTP client of the running app, reading `api-token`/`api-http-port` per
  call (`engine/mcp.rs:39-56`, raw HTTP/1.1 with manual chunked decoding `mcp.rs:58-90`).
- MCP: `hubdeck --mcp` stdio JSON-RPC 2.0 (`engine/mcp.rs:108-147`), tools at `mcp.rs:293-507`
  (`list_modules, list_actions, set_control, add_image, add_animation, preview_control,
  activate_profile, …`), each one HTTP call to the running app; `--mcp-deck` adds
  `notify/show_status/ask_physical/read_controls` (`engine/deck_mcp.rs:17-84`); resource
  `hubdeck://design-guide`. Tested incl. a real HTTP round trip (`mcp.rs:1300-1430`). Note: both
  implementations share the same shape — files-in-data-dir for port+token — which works but is
  racy across users/installs (see 50-risks).

## 3. Window watching (foreground)

- `engine/foreground.rs:1-5`: Windows = `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` + a 250 ms
  `GetForegroundWindow` poll (`:14-155`); **elsewhere: poll every 250 ms**
  (`active_win_pos_rs::get_active_window()`, `:157-195`, `:163, 180`). On Linux that crate pulls
  in `xcb` (X11) + `hyprland` + `kdotool` (Cargo.lock) — no native Wayland path. OPEN: behaviour
  on pure Wayland (inferred from deps, not from its source).
- Effect: `Msg::Foreground{exe,title}` → `Worker::on_foreground` (`engine/mod.rs:2764-2800`) →
  profile switch only: if `settings.auto_switch`, pick the profile whose `apps` match
  (`profile_for_app`), else home (`mod.rs:1276-1285`); skipped while locked/calibrating/pinned.
  Additionally `engine/auto.rs:158-204` can auto-create a profile the first time an app appears.
- So the Rust watcher changes **whole profiles**, never individual keys — coarser than the Python
  one, which can run arbitrary `on_focus` actions.

## 4. Profile format and action model

- Profile = one JSON file `<app data>/profiles/<id>.json`, `"version": 1`
  (`engine/profile.rs:10, 222-281`): `id, name, apps[], controls: BTreeMap<String,Binding>,
  pages[], edge/panel/effects/color, …`. Control ids `keys.0..14`, `dial`, `knobs.N`, `faders.N`,
  `rollers.N`, gestures `:hold`/`:double`, per-page `<page>/keys.N` (`api.rs:222`,
  `profile.rs:421-427`). `Binding {action, params, look, …}` (`profile.rs:167-188`);
  `Look` (`profile.rs:14-146`) carries label/icon/image/colours/LED rules. Export/import `.mhz`
  (`engine/mhz.rs`). Bundled presets are hardcoded Rust builders (`engine/presets.rs`,
  `KINDS` at `:587`).
- Resolution: input → `Worker::on_input` (`mod.rs:2233`) → gestures (`HOLD_AFTER = 500 ms`,
  `DOUBLE_WITHIN = 300 ms`, `engine/gestures.rs:11-13`) → `fire()` (`mod.rs:2390`) → binding
  lookup → special cases `core.games/toggle/multi/modes` → plugin actions (id contains `/`,
  `plugins.rs:38-40, 838`) → else `Executor::run` (`engine/exec.rs:175`), a big
  `match (binding.action, input)` at `exec.rs:193-705` (`core.hotkey` via enigo, Linux via
  `/dev/uinput` `engine/uinput.rs`; `core.run` spawns a process; `obs.*`/`ha.*` over websockets…).
- **The action vocabulary is hardcoded in `engine/actions.rs::catalog()`** (`actions.rs:100`):
  `ActionDef{id,name,group,description,inputs,params,icon}` with namespaces `core.*` (consts
  `actions.rs:1232-1288`), `obs.*`, `streamlabs.*`, `twitch.*`, `spotify.*`, `ha.*`, `midi.*`,
  `vjoy.*`, `claude.*`; plugin actions are appended dynamically (`actions.rs:1350-1360`).

## 5. Plugin host

- One OS process per plugin folder in `<app data>/plugins/<folder>/` (`engine/plugins.rs:310-333`),
  launched with env `HUBDECK_PORT`/`HUBDECK_PLUGIN`/`HUBDECK_TOKEN` (`plugins.rs:390-401`), Windows
  `CREATE_NO_WINDOW` + job object `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (`plugins.rs:180-205`).
  Watchdog thread `plugin-watch` polls every 2 s, restarts after 3 s up to `MAX_RESTARTS = 5`
  (`plugins.rs:454-495`); orphans killed at start (`:529-558`).
- Transport: WebSocket to the same API port, first message
  `{"event":"register","plugin":<id>,"token":<token>}` (`plugins.rs:734-760`). Host→plugin events
  `willAppear/willDisappear/keyDown/keyUp/dialRotate/slide/state/error` (`plugins.rs:14-20`);
  plugin→host `setTitle, setImage, setState, showFeedback, setLed, setPanelImage, switchProfile,
  getState, playGames, log` handled in `Worker::on_plugin` (`mod.rs:3371-3490`). API clients
  register as `{"plugin":"api"}` and receive every control input as `{"event":"input",…}`
  (`mod.rs:2346-2352`). Protocol documented in `docs/PLUGINS.md`.
- Manifest `manifest.json` (`plugins.rs:49-66`): `{id, name, version, command, actions[],
  permissions[]}`, action params typed `text|multiline|number|select|app|target|hotkey`
  (`:68-92`); ids restricted to `[A-Za-z0-9._-]≤80` (`safe_id`, `:98-100`). **A plugin author
  writes `plugin.js` + `manifest.json`** — see `plugins/example-counter/` (Node 22 built-in
  WebSocket, registers from env vars, handles `keyDown`, replies `setTitle`).
- **No sandboxing**: `permissions` is declared but only displayed — "it does not sandbox plugins"
  (`plugins.rs:58-59`, docs/PLUGINS.md). Only containment is process kill + the Windows job object.
- Stream Deck compat: `<uuid>.sdPlugin` folders with Elgato-style launch args
  (`-port/-pluginUUID/-registerEvent/-info`, `engine/sdplugins.rs:646-649`), dialect translation
  (`sdplugins.rs:471, 542`), settings HTML served from a local file server (`sdplugins.rs:815-820`),
  install from `.streamDeckPlugin` zip (`:634-672`); catalog `sdcatalog.json` hosted remotely
  (`sdcatalog.rs:14-27`). 53 checked / 44 run on Windows (docs/STREAMDECK-PLUGINS.md:181).
- Reverse contract for other apps: apps expose `GET /hubdeck/status`, SSE `GET /hubdeck/events`,
  `GET /hubdeck/manifest` and announce via `POST /api/apps/announce` (docs/APP-CONTRACT.md;
  `engine/states.rs:148-209`, poll 5 s or SSE; `scripts/example-app.py`).

## 6. Frame path and safety guards

- Sizes: key 72×72, dial 240×240, full panel 480×272 (`engine/render.rs:100-101, 760-761`).
  Draw: SVG → resvg/usvg → tiny-skia; panel = 15 cached tiles keyed by SVG hash
  (`render.rs:1836-1846`); optional wgpu compute-shader canvas under the keys
  (`engine/gpu.rs`, `shaders/*.wgsl`).
- Encode: always JPEG (`jpeg-encoder` + simd), `KEY_QUALITY = 98`, `DIAL_QUALITY = 90`,
  chroma forced 4:4:4 (`render.rs:1831-1832, 1160-1169`); **standard Huffman tables only** —
  optimized tables break the module decoder (PROTOCOL.md:87-88).
- Chunking: `image_chunks` (`crates/mh-protocol/src/report.rs:132-153`), header
  `02 <cmd> chunk(LE16) len(LE16) index last`, chunk = output_len − 8 = 312 B on keys; cmds
  `0x36` per-key, `0x35` panel, `0x45` dial (`report.rs:154-165`).
- Pipelined writes: Windows `overlapped.rs` (own overlapped handle, `DEPTH = 8` in flight,
  `TIMEOUT_MS = 1000`, `:21-23`), macOS `pictures.rs` + `pictures_macos.m`
  (`IOHIDDeviceSetReportWithCallback`, `DEPTH = 8`, `:13-15`); `HUBDECK_PIPELINE=0` disables;
  `Module::send_all` refuses forbidden reports (`module.rs:103-106`).
- Queue/backpressure: per screen `PictureSender = Mutex<Option<Picture>> + Condvar + busy`
  (`engine/mod.rs:1313-1335`): "a picture is whole, so **only the newest waiting one is sent**"
  (`mod.rs:1303-1310`) — latest-frame-wins with at most one in flight + one waiting; failures
  recorded and the module redrawn (`mod.rs:1458-1466`). LEDs on a separate `leds` thread
  (`mod.rs:1609-1686`). Engine tick 40 ms; dial frame ≥ `DIAL_FRAME = 36 ms`, moving keys every
  `KEYS_MOVING_EVERY = 50 ms`, canvas every `CANVAS_EVERY = 41 667 µs` ≈ 24 fps — "what the key
  screen shows at most (counted on film)" (`mod.rs:139-144, 1979-2026`).
- Measured: device displays ≤ **~24-25 pictures/s** though it accepts ~32 fps sent
  (PROTOCOL.md:93-97, 243-246); USB ~**2.1 MB/s** caps big panels (PROTOCOL.md:257-266);
  PERF-AUDIT-0.2.md: release soak 11.0 % CPU, keys 16.1 pictures/s, JPEG encode 2.7-4.1 ms at q98
  (`:33-41`); WebView2 window 450-800 MB (`:62`).
- Safety: `mh-protocol/src/safety.rs` `FORBIDDEN` (report-id, cmd-byte) list (`:8-32`),
  `is_forbidden` (`:34-36`), enforced on the raw path too (`report.rs:28-30`, `module.rs:104-106`,
  `Err(Error::Forbidden)`) — the commands are **unrepresentable**, matching BRIEF's requirement.
  Never sends backlight 0 (bricks the dial on macOS until reseat; screens-off = black JPEGs,
  PROTOCOL.md:61, `mod.rs:3009`). Reads on keys/dial refused outside software mode
  (`Error::HardwareMode`, `module.rs:158-166`). Assistant guard `engine/guard.rs`: `REFUSED`
  actions (`core.run`, `ha.service` unless power mode), risky launch targets, destructive hotkeys
  only on `:hold` (`:12-81`). UIPI detection `engine/elevated.rs`.

## 7. Device readback

Feature-report reads exist (`Query` enum `report.rs:172-184`; `Module::query` `module.rs:158-169`;
`diagnostic_reads` `report.rs:210-232`) surfaced at `GET /api/modules/<id>/check`
(`api.rs:100-108` → `src/module_check.rs:113-130`); fader positions also arrive as input events.
No per-write ack; `GET /api/leds` returns `{shown, sent}` from engine-side tracking. Serial read at
connect (`hub.rs:241-245`). `engine/recorder.rs` (Windows macro recorder) and `presses.rs` (press
counts in `presses.json`) are app-side records, not device readback.

## 8. OS-specific code

`cfg(target_os)`/`cfg(windows)` in ~35 files. Windows: WinEvent foreground hook, `overlapped.rs`,
`notify.rs` device-change notifications, `pnputil /restart-device` for wedged modules (PROTOCOL.md:192-197),
job objects, UIPI. macOS: shared hidapi open (`MH_HID_SHARED`), IOUSBHost re-enumeration
(`recover.rs` + `reenumerate_macos.m`), SET_IDLE for rollers (`recover.rs:15-18`), pipelined
pictures (`pictures_macos.m`), `location.rs` via `locationID`. Linux: libusb hidapi, 1 s poll
rescan (no notifications — `notify.rs` is Windows-only, `hub.rs:149-155`), uinput keystrokes,
`busctl`/`loginctl` for idle/lock (`engine/idle.rs:29-97`). **No named pipes anywhere** — all IPC
is loopback TCP + files in the data dir (only pipe is the guard's anonymous stdin, `src/guard.rs:7-10`).

## 9. Tests

`test.log` at repo root: `cargo test` — 360 unit tests in `hubdeck_lib`, all ok (≈402 `#[test]`
across the workspace, inline `#[cfg(test)]` in ~40 engine files: api.rs, plugins.rs, cli.rs,
mcp.rs, guard.rs, exec.rs…). Hardware timing test `crates/mh-device/tests/output_timing.rs`
(`#[ignore]`, needs a deck). `.testshots/` holds live-run camera artifacts. No virtual-device
conformance harness equivalent to Python's `tests/fakes/virtual_hub.py` — `HUBDECK_AUTOMATION`
(`src/automation.rs`) gives a second copy virtual modules but tests don't assert on
protocol-level writes the way the Python acceptance tests do. OPEN: confirm.

## What this means for the new base

Preserve: mh-protocol's unrepresentable-forbidden-commands pattern (it already meets BRIEF's
"unrepresentable, not merely unused"), software-mode handshake + settle, pipelined writes
(8 in flight) as the throughput lever, latest-frame-wins `PictureSender`, guard-process handback,
per-install token on loopback API. Replace/discard: Tauri shell + WebView2 memory, profile JSON
authored by UI, hardcoded `actions.rs` catalog as the only vocabulary, port/token-in-files discovery
(a stable socket path is R1/R7), no-sandbox plugin host (R8 requires capability grants), 250 ms
polling foreground watcher on non-Windows.

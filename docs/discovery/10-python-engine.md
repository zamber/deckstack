# Discovery 10 — the Python engine (`the Python predecessor`)

Path prefix: `hubdeck/` = `the Python predecessor checkout`. All line numbers verified against the checkout.

## 1. Architecture and process model

One process, no daemon. Entry points are console scripts `hubdeck = hubdeck.cli:main` and
`hubdeck-tray = hubdeck.tray:main` (`hubdeck/pyproject.toml:43-46`); in bundled builds
`src/hubdeck/_bundle.py:13-29` dispatches: no args → tray, args → CLI.

- `hubdeck run` (`hubdeck/src/hubdeck/cli.py:128-141`) takes a single-instance lock
  (`instance.acquire()`; POSIX `flock` on `<app_dir>/.lock` in `platforms/posix.py:51-62`,
  Windows `CreateMutexW` in `platforms/windows/__init__.py:71-80`) and runs `Engine.run()` on
  the main thread (macOS: on a `"hubdeck-engine"` thread with AppKit on main,
  `platforms/macos/__init__.py:139-160`).
- `hubdeck-tray` (`tray.py:63-168`) runs the same engine on a daemon thread
  (`tray.py:155`) plus a pystray icon; `--replace` hands over from a running older copy via the
  control channel (`instance.py:38-56`).
- Engine main loop: `run()` → `while not stop: step(); sleep(LOOP_SLEEP)` with
  `LOOP_SLEEP = 0.01` (`engine.py:34, 427-444`). `step()` (`engine.py:465-514`) drains the command
  queue, checks config mtimes every `RELOAD_CHECK_SECONDS = 1` (`engine.py:33, 469-472`), scans for
  modules, polls device events, ticks gestures, drains the plugin-state queue, expires timed face
  overrides, polls focus, advances GIF frames, updates lighting (30 fps cap, `lighting.py:19`),
  reasserts screens, and paints if dirty.
- All public API calls (`goto_page`, `run_action`, `set_face`, …) just enqueue onto the engine
  thread via `Engine.call()` (`engine.py:173-175`); reads are single lookups. Threads:
  `hubdeck-engine`, `hubdeck-control` (`control.py:84`), `hubdeck-focus` (`focus.py:37`),
  `hubdeck-gui` (`gui/server.py:147`), `hubdeck-download` per image URL (`render/images.py:113`),
  `hubdeck-<plugin>` per `ThreadedPlugin` (`plugins/base.py:183`), `hubdeck-python-N` per
  `{python:}` press (`plugins/python.py:53`).

## 2. Device / HID layer

- `device/hid.py`: `VID = 0x2516` (`hid.py:20`); `MODULE_TYPES` (`hid.py:48-62`) maps PIDs
  (keys `0x01FE`, dial `0x0206`, M0 family base `0x01F6`/knobs `0x0202`/rollers `0x0204`/faders
  `0x0200`) to output length (320 B screen / 64 B M0), input tag base (`0x34`/`0x44`/`0x2A`) and
  the safety lists. `HidModule` opens **every** HID path for the device non-blocking and remembers
  whichever handle accepts each report type (`hid.py:199-231`) because Windows exposes several
  paths per module. Input frame `01 <tag> <len u16 LE> <payload>` parsed by `parse_frame`
  (`hid.py:134-141`).
- Read model: **polled, not a reader thread** — `DeviceManager.poll()` (`device/manager.py:66-79`)
  reads all pending non-blocking reports on the engine thread. `DeviceError` from all handles ⇒
  module lost (`_module_lost`, `engine.py:1422-1435`): fires `up` on held keys, defers reopen by
  `RETRY_SECONDS = 3` (`manager.py:15`).
- Hot-plug: `DeviceManager.scan()` (`manager.py:36-53`) re-enumerates at most every 3 s when a
  wanted kind is missing; reconnect ⇒ `_init_module` (`engine.py:1343-1362`): software mode on,
  brightness rewritten, full repaint.
- Image upload: `build_chunked_reports(cmd, data, index, report_len=320)` (`hid.py:112-122`) →
  output reports `02 <cmd> <chunk u16 LE> <len u16 LE> <index> <last> <≤312 B data>`; keys cmd
  `0x36` JPEG 72×72 q90 (`device/m4315.py:30-72`), dial cmd `0x45` JPEG 240×240 q85
  (`device/modules.py:35-65`), sent via `write_many` (`hid.py:246-258`).
- LEDs (M0): `02 2C/2D/2E <led_module> <idx> …` mode/colour/brightness, always `save=0`
  (`device/modules.py:81-92`), gamma 2.2 (`lighting.py:53-56`). Screen backlight feature
  `03 34` (keys) / `03 44` (dial). Software mode first: `03 3A 01` / `03 49 01`
  (`engine.py:1343-1352`).
- Safety policy: `check_write` (`hid.py:85-104`) — per-module `deny_*` lists are absolute
  (firmware/serial/factory/recovery bytes, `hid.py:52-62`), anything not in `safe_*` raises
  `UnsafeCommand` unless `unverified=True`. Denylist mirrors `docs/protocol/README.md:46-57`.
- Readback: **no ack for writes** — `write`/`write_many` only check the return count
  (`hid.py:235-258`). Reads exist via `get_feature` (`hid.py:260-283`); get-feature returns the
  *saved* LED state, not the live one (`docs/masterhub-protocol.md:219-220`).
- `device/fake.py`: `FakeKeyModule`/`FakeControls` implement the device protocol in memory for
  unit tests (`fake.py:7-97`); the HID-protocol-level virtual hub used by acceptance tests is
  `tests/fakes/virtual_hub.py` (see §9).

## 3. Config / preset model

- `config/loader.py`: YAML 1.2 booleans (only `true`/`false`) and `!secret` from `secrets.yaml` or
  `HUBDECK_<NAME>` env (`loader.py:26-33, 60-70`); `includes:` files may hold only `sections:`
  (`loader.py:230-258`); then pydantic validation + cross-reference checks (`loader.py:73-211`).
- `config/schema.py`: pydantic v2, `extra="forbid"` (`schema.py:66-67`). Root `Config`
  (`schema.py:730+`) with `device`, `modules`, `defaults` (incl. `reassert_seconds: 5`
  `schema.py:685`, `led_gamma: 2.2` `:726`, `animation_fps: 30` `:727`), `plugins`, `lighting`,
  `auto_switch`, `pages` / `sections` / `presets`. Presets merge `extends` chains with recursive
  dict overlay, `None` deletes (`schema.py:604-613`), materialized by `Config.for_preset`
  (`schema.py:650-705`).
- Hot reload: mtime poll of config + secrets + includes (`ConfigSource.changed`,
  `loader.py:282-311`), engine checks every 1 s; a broken file is logged and the old config keeps
  running (`engine.py:539-554`, `loader.py:297-300`). Config changes only — code changes need a
  restart (`CLAUDE.md:89`). The GUI edits files with ruamel round-trip preserving comments
  (`gui/doc.py`).

## 4. Plugin model

- Registry: `plugins/__init__.py:22-28` maps 19 names (`page, preset, flow, style, keyboard,
  launch, python, discord, audio, system, media, vars, shortcuts, spotify, ha, huemux, hwinfo,
  rambox, backlight`) to classes. Interface: `actions` / `states` / `values` dicts,
  `invoke`/`adjust`/`set`, `watch`/`watch_states`; `ThreadedPlugin` runs on its own thread
  (`plugins/base.py:112-272`). `page`/`preset`/`flow`/`style` are executed by the engine itself
  (`engine.py:1028-1060`).
- In-process by default. Opt-in isolation: `plugins/<name>.host: isolated` or
  `HUBDECK_REMOTE_PLUGINS=1` runs `remote = True` plugins (ha, huemux) as
  `python -m hubdeck.plugin_runner <name>` over a socketpair with newline-delimited JSON-RPC
  (`engine.py:656-684`, `plugins/host/remote.py`); dead child respawns with backoff max 3
  (`plugins/host/remote.py:29-31`); `Plugin.manifest` declares pydantic arg models, `wire()` emits
  JSON Schema for foreign plugins (`plugins/host/manifest.py`, `protocol.py`). CLAUDE.md:34 notes
  `ctx.deck` was not proxied to remote plugins at that point (docs/plans/plugin-host.md:33-36 says
  `_RemoteDeck` landed — the two docs disagree; OPEN).
- `plugins/python.py`: `{python: code}` is `exec(compile(...))` in the hubdeck process, one thread
  per press, one shared namespace with `deck` and `log` (`python.py:40-60`); syntax checked at
  config load (`:46-51`). This is the "python-in-yaml" the brief rejects (BRIEF.md R8).

## 5. Action model

- Vocabulary hardcoded in two places: the `REGISTRY` (`plugins/__init__.py:22-28`) and the alias
  table `ALIASES` in `config/schema.py:55-67` (`page→page.goto`, `keys→keyboard.keys`,
  `type→keyboard.type`, `media→keyboard.media`, `run→launch.run`, `open→launch.open`,
  `focus→launch.focus`, `python→python.exec`, `preset→preset.goto`, `if→flow.if`,
  `unless→flow.unless`, plus `style`, `light`). `parse_action` (`schema.py:77-102`) normalizes
  `"plugin.action"`, `{plugin.action: arg}`, `{plugin, name, arg}` into `Action(plugin, name, arg)`.
- Resolution: engine `_invoke` (`engine.py:1028-1073`) looks up `self.plugins[action.plugin]` and
  calls `plugin.invoke(name, arg)`; `flow.if/unless` recurse via `_invoke_flow`
  (`engine.py:1088-1112`, depth cap 10). Gestures (`on_down/on_up/on_long_press/on_double_press`)
  resolved in `gestures.py`. Load-time validation `loader._check_actions` (`loader.py:184-211`);
  runtime check for `hubdeck send` is `engine.check_runnable` (`engine.py:366-405`).
- States: plugins publish via `ctx.set_state` → `engine.emit_state` → `_state_q`
  (`engine.py:363-364, 1116-1131`); the engine computes what to watch from the config
  (`cfg.iter_targets()`/`state_refs()`, `schema.py:946-1020`) and passes it to
  `watch`/`watch_states` (`engine.py:630-654`). Value states are keyed `value:<name>:<arg>`, muted
  `muted:<name>:<arg>` (`plugins/base.py:36-44`). On-demand reads wait up to `ASK_SECONDS = 2.0`
  off the engine thread (`engine.py:336-344`).
- The `deck` object handed to python actions and plugins is the thread-safe API in `deck.py`
  (per-control objects `deck.keys[n]`, `deck.dial`, `deck.knobs[n]`, `deck.rollers[n]`,
  `deck.faders[n]`, `deck.base`; runtime face/light overrides live in `Engine.overrides` /
  `light_overrides`).

## 6. Window watcher

- `focus.py`: `FocusWatcher` polls `reader.read()` every `POLL_SECONDS = 0.2` on thread
  `hubdeck-focus` (`focus.py:12, 28-54`). The engine applies a 0.3 s settle
  (`FOCUS_SETTLE = 0.3`, `engine.py:41, 810-815`) then `_focus_changed` (`engine.py:817-840`):
  updates `page.app`/`page.title` states, computes `cfg.match_page(window)` (best rank
  app+title > title > app; `schema.py` `WindowMatch`), skips `auto_switch.ignore`, and
  `_follow_focus()` (`engine.py:842-854`) swaps pages (saving the manual stack in `_saved_stack`),
  runs `on_focus` actions.
- What it can change: page selection per `match` rules, the `page.auto/app/title` states, and any
  `on_focus` action list — i.e. anything an action can do. It cannot change arbitrary bindings
  except through actions.
- Linux reader (`platforms/linux/focus.py`): **KDE/KWin only** — loads a KWin JS script over the
  session D-Bus (jeepney) reporting `(resourceClass, caption, pid)`; non-KDE raises `Unsupported`
  (`focus.py:99-103`) and the engine degrades gracefully (`engine.py:453-462`). Windows:
  `GetForegroundWindow` + process name + `GetWindowTextW` (`platforms/windows/focus.py:47-95`).
  macOS: `NSWorkspace.frontmostApplication` + AX title via ctypes, needs Accessibility
  (`platforms/macos/focus.py:27-141`).

## 7. Image / frame path

- Rendering is PIL-only, 4× supersampled then LANCZOS-downscaled (`render/faces.py:16, 158-208`):
  bg, image (cover/contain), fill gradient, gauge ring, Tabler icon from bundled webfont
  (`render/tabler.py`), label/text auto-fit, accent colour refreshed every 2 s.
- `render/images.py`: `ImageStore` with URL downloads on background threads, 15 s timeout,
  **10 MB cap** (`MAX_DOWNLOAD`, `images.py:21-22`), sha1-keyed cache, atomic replace
  (`images.py:76-129`); GIFs capped at `MAX_FRAMES = 300`, min frame 0.02 s (`images.py:22-23`),
  played at `defaults.animation_fps` (default 30, `schema.py:727`; engine advances frames in
  `step()` at `engine.py:500-503`).
- Paint: `_paint` on the engine thread only when dirty/forced (`engine.py:1325-1341`); per key
  `face_for` merges defaults → config → state overlays → runtime overrides (`engine.py:1170-1268`);
  JPEG encode cached (`_jpeg_cache`, 4096 entries, `engine.py:1302-1317`); `_write` skips
  unchanged faces via the `shown` map (`engine.py:1319-1323`).
- **Backpressure: none by design.** There is no frame queue — writes are synchronous on the engine
  thread and unchanged faces are skipped. A slow HID write blocks the engine loop (and therefore
  control handling); `device.reassert_seconds` (default 5) force-repaints everything
  (`schema.py:685`, `engine.py:506-509`). Max sustainable rate is whatever the engine thread
  sustains; protocol-level ceilings are in `docs/discovery/40-hardware-and-protocol.md`.
- `hubdeck send image` stores pictures in `cache/sent/` downscaled to 480 px (`control.py:146-176`,
  `MAX_IMAGE` = 20 MB `control.py:36`).

## 8. Control channel (the existing "local API")

- No daemon, no Unix socket. `ControlServer.start()` binds `("127.0.0.1", 0)` — a random loopback
  port — on thread `hubdeck-control`, one JSON line per connection, one JSON line reply, 5 s
  handler timeout (`control.py:69-93`). Port + per-run token (`secrets.token_urlsafe(24)`) + pid are
  written atomically (mode `0o600`) to `control.json` in the app dir (`control.py:51, 81-90`).
  Auth: `hmac.compare_digest(req["token"], self.token)` (`control.py:76`); max request 64 KB
  (`MAX_REQUEST`, `control.py:35`).
- Commands (`control.py:101-123`): `action` (any config action, validated by `check_runnable`,
  queued to the engine thread), `set` (vars), `get` (read a state, waits up to `ASK_SECONDS`),
  `image`, `quit`. `hubdeck send …` (`cli.py:185-216`) is a thin client over this
  (`control.send`, `control.py:178-193`).
- GUI: stdlib HTTP server on `127.0.0.1` (default port 8765 for standalone `hubdeck gui`,
  `cli.py:243`; in-tray it runs in-process, `tray.py:102-108`), per-run token as
  `X-Hubdeck-Token` header/`?t=`, Host header must be 127.0.0.1/localhost (DNS-rebinding guard)
  (`gui/server.py:556-566`), routes listed at `gui/server.py:568-648`. `net/websocket.py` is a
  WebSocket **client** only (plugins ha/huemux/discord), not a server.

## 9. Tests and acceptance harness

- `pyproject.toml:52-53` `testpaths = ["tests"]`; **461 test functions across 37 files** (grep
  count). `tests/conftest.py:11-50` injects a `RecorderPlugin` into the registry as `rec`.
- **Virtual device at the HID-protocol level**: `tests/fakes/virtual_hub.py` impersonates the
  `hid` module and implements the real report protocol (M4315 `03 3a/34`, chunked `02 36`
  320-byte, input `01 34 …`; dial `03 49/44`, `02 45`, tag `0x44`; M0 `02 2c/2d/2e`, tag `0x2a`),
  reassembles chunked JPEGs into `images[key]`, and records writes so tests assert on
  device-level traffic (`virtual_hub.py:17-130, 196-246` — `VirtualHub.patch` swaps `hid` and
  `factories()` returns the **real** drivers). `tests/fakes/ws_server.py` is a real RFC 6455
  server for plugin tests.
- `tests/test_acceptance.py` (10 tests): full engine + real drivers + virtual hub + fake HueMux —
  knob turn → lamp + LED with gamma 2.2, dim/wake, preset switching, face colours following lamp
  state (`test_acceptance.py:112-313`). Required loop per `CLAUDE.md:101-103`: unit tests →
  acceptance on the virtual hub → manual run on real hardware; if they disagree, the virtual
  model is wrong, fix it first.

## 10. Packaging

- Linux AppImage (`packaging/linux/build.py`): python-build-standalone CPython 3.13.16 (pinned
  sha256, `build.py:33-34`) + wheel + manylinux deps into `opt/app`, pruned, hash-based bytecode,
  AppRun execs `python3 -B -m hubdeck._bundle` (`build.py:52-58`), `appimagetool` (`build.py:118-124`).
- Windows (`packaging/windows/`): PBS Python + win wheels, two zig-compiled launchers
  (`hubdeck.exe`/`hubdeck-tray.exe`, `launcher.c:17-21`), MSI via wixl 0.106; installer starts
  `hubdeck-tray.exe --replace` for live upgrade (`packaging/windows/hubdeck.wxs:37, 58-61`).
- macOS (`packaging/macos/`): `.app` with zig launcher + rcodesign signing levels, built on Linux
  in Docker/CI. Release: version only in `hubdeck.__version__`; `vX.Y.Z` tags → GitLab + GitHub
  (`packaging/release.py`).

## 11. Hard-won vs accidental

- **Hard-won (keep):** the protocol layer + denylist (`device/hid.py`, `docs/protocol/`),
  software-mode handshake discipline, chunked-JPEG details, LED gamma 2.2, saved-vs-live readback,
  fader soft-takeover (`engine.py:973-1006`, `TAKEOVER_WINDOW = 4` `engine.py:39`), hot-plug
  handling, the virtual-hub acceptance harness, `platforms/` seam with `Unsupported`.
- **Accidental (the brief replaces):** single-process monolith with the action vocabulary
  hardcoded in `REGISTRY`/`ALIASES`, `{python: …}` exec-in-process (`plugins/python.py`), no frame
  queue/backpressure on the paint path, control channel as random-port TCP + file
  (`control.py`) instead of a stable socket, mtime-polling reload, GUI coupled to the engine.

## What this means for the new base

Constraints the design must satisfy: preserve the HID framing/software-mode/denylist semantics and
the virtual-hub test strategy; keep the `platforms/`-style OS seam (Linux is primary per BRIEF R9);
replace the in-process plugin/action model with a process/API contract (R6), the random-port
control file with a stable daemon socket (R1/R7), and add bounded queues + latest-frame-wins on
the frame path (R5) — today a slow write blocks the engine loop at `engine.py:465-514`.

# Component: ds-focus (window/focus sender — external program)

R4: the window watcher is an ordinary program that calls the CLI. The daemon contains no
watcher (D17).

- **Language:** Rust (boring, fast startup for session autostart). Backends behind a
  `ds-platform` trait, selected at runtime: Linux KDE/Wayland → KWin D-Bus script
  (the proven path, `hubdeck/src/hubdeck/platforms/linux/focus.py`); Linux other
  Wayland → `wlr-foreign-toplevel` (ext-foreign-toplevel-v1) — OPEN O1 until measured on
  Bazzite; X11 → xcb; Windows → `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` +
  `GetForegroundWindow` (`upstream/.../engine/foreground.rs:14-155`); macOS →
  NSWorkspace + AX (`hubdeck/src/hubdeck/platforms/macos/focus.py`).
- **Behaviour:** poll/hook → settle 300 ms (precedent `FOCUS_SETTLE = 0.3`,
  `hubdeck/src/hubdeck/engine.py:41,810-815`) → `ds emit app/focus/changed
  {"app":"…","title":"…","pid":…}` → exit. Nothing else. Recoloring, page switching,
  binding changes are bindings/subscribers on `app/focus/changed`, e.g.:
  ```sh
  ds bind app/focus/changed --when /focus/app eq code --do 'set /controls/keys/3/color #00ff00'
  ```
- **Why external:** neither existing watcher has a native Wayland path (discovery 30,
  O1); keeping it swappable per DE is the only way R9 (Linux-first) works.
- **Public surface:** `ds-focus [--backend auto|kwin|wlr|x11|win|macos] [--interval 250ms]`;
  emits only. Config in `/settings/senders/focus`.
- **Failure modes:** backend unsupported → logs + exits 3 with the list of available
  backends (precedent: `Unsupported`, `hubdeck/src/hubdeck/platforms/base.py:172-283`);
  daemon unreachable → retry with backoff, exit 6 after 60 s.
- **Dependencies:** ds-client only.
- **Test command:** conformance `tests/conformance/focus.sh` runs ds-focus against a
  fake event source (`--inject file.ndjson`) and asserts the emitted topics on
  `ds watch app/focus`.
- **Minimum working example:**
  ```sh
  ds-focus --backend auto &
  ds watch app/focus --ndjson
  # in another terminal: focus a window → one event line appears
  ```

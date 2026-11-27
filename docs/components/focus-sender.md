# Component: ds-focus (window/focus sender — external program)

R4: the window watcher is an ordinary program that calls the CLI. The daemon contains no
watcher (D17).

- **Binary:** `bins/ds-focus` (Rust, package `ds-focus`), built by the workspace glob
  `bins/*`. Depends on `ds-client` (control-plane client) plus the two backend libraries
  below; no shell-outs, no config eval.
- **Backends** (selected at runtime by `--backend auto|kwin|wlr|x11`, autodetect rule in
  `bins/ds-focus/src/backend.rs::detect`):
  | backend | mechanism | availability (measured on this Bazzite KDE Plasma 6 session) |
  |---|---|---|
  | `kwin` | D-Bus `org.kde.KWin` at `/KWin`, interface `org.kde.KWin`, method `queryWindowInfo` → `a{sv}` (the active-window interface — chosen over `/Scripting` `org.kde.kwin.Scripting` because scripting needs a script file on disk, while `queryWindowInfo` is a plain pollable method; both verified present via `busctl --user introspect`). Keys read: `resourceClass`/`class` (wm-class), `caption`/`name` (title), `pid`. Implemented with `zbus` (blocking). | available whenever the session bus owns `org.kde.KWin` (Wayland **and** X11 KDE); `queryWindowInfo` answering `UserCancel` means "no active window right now" and is reported as no-focus, not an error |
  | `wlr` | Wayland `zwlr_foreign_toplevel_manager_v1` (wlr-foreign-toplevel; `wayland-client` + `wayland-protocols-wlr`), activated toplevel from the handle `state` event (`activated`) | available on wlroots compositors (sway/Hyprland/river). **Not** available on KDE (KWin advertises no foreign-toplevel global — verified with `wayland-info` on this session) |
  | `x11` | accepted as a flag value but not built in the Linux alpha (Wayland/KDE-first, `docs/40-platforms.md` §1) | selecting it exits 3 with the list of backends available on the session |
  | `auto` | KDE session + `org.kde.KWin` owned → `kwin`; else Wayland + zwlr global → `wlr`; else none | on this Bazzite session auto selects `kwin` |

  O1 measurement note (closes the "which protocol" part of O1): `ext-foreign-toplevel-list-v1`
  (staging) has **no activation state** — its handle events are only `title`/`app_id`/
  `identifier`/`done`/`closed` — so it cannot drive focus detection on its own;
  `zwlr_foreign_toplevel_manager_v1` is the wlr-foreign-toplevel protocol that carries
  `activated` and is therefore the wlr backend.
- **Payload:** one event per settled focus change on the bus topic `app/focus/changed`
  (`docs/contracts/actions.md` §1), emitted with the IPC `emit` op (mutate scope):
  `{"app":"<wm-class>","title":"<title>"}` plus `"pid":<n>` when the backend knows it
  (KWin does, wlr does not) — the shape `schemas/state.json` `/focus` mirrors. The daemon
  accepts the event with `pid` absent or null and rejects only a present non-integer
  `pid` (DS-114). The exact
  request is `{"op":"emit","topic":"app/focus/changed","payload":{…}}`, schema-validated
  client-side against `schemas/ipc-message.json` (unit-tested in `focus.rs`).
  ds-focus **never writes `/focus` directly**; the daemon's router mirrors the payload
  into `/focus` in the same transaction that dispatches bindings (S7, DS-044).
- **Settle / dedupe:** a live backend is polled every `--interval` (default 250 ms); a
  candidate focus must stay unchanged for `--settle` (default **300 ms**, the
  `FOCUS_SETTLE = 0.3` precedent) before it is emitted, and identical consecutive
  payloads are deduped (emit-storm guard). The watch loop wakes at the earlier of the
  next poll tick and the settle deadline (`focus.rs::Settler::next_wake`).
- **Latency budget:** the emit is issued at the settle deadline, i.e. within the **500 ms**
  budget measured from the settled change (worst-case end-to-end from the physical focus
  change: interval 250 ms + settle 300 ms + emit RPC; the RPC duration is measured and
  logged per emit, and a > 500 ms emit logs a warn). Every emit is logged at `info`
  (`{"level":"info",…,"msg":"emit app/focus/changed payload=… subscribers=N emit_ms=…"}`).
- **Failure modes:** unsupported/unavailable backend → error log + **exit 3** listing the
  backends available on the session (`backend.rs::SelectError::message`); daemon
  unreachable → reconnect with backoff (100 ms doubling to 2 s) for **60 s**, then
  **exit 6** (E_UNREACHABLE) with the `ds daemon start` hint; auth/scope errors keep
  their IPC exit code (4). SIGTERM/SIGINT exit 0 cleanly (signal-hook, like ds-daemon).
- **CLI:** `ds-focus [--backend auto|kwin|wlr|x11] [--interval 250ms] [--settle 300ms]
  [--once] [--inject <json|file.ndjson>] [--socket PATH] [--token-file PATH]
  [--data-dir DIR] [--quiet] [-h|--help] [-V|--version]`. `--once` emits the current
  focus once (no settle wait) and exits. Optional overrides can live in
  `/settings/senders/focus` (`{"backend":…,"interval_ms":…,"settle_ms":…}`); CLI flags win.
- **Test hook:** `--inject <json|file.ndjson>` bypasses the backend entirely (no
  compositor needed): an inline JSON object, an array of objects, or a path to an
  `.ndjson` file with one payload object per line. Payloads are emitted verbatim,
  immediately (deduped against the last emitted payload), then ds-focus exits 0.
- **Conformance:** `tests/conformance/focus_sender.sh` (sh only, always cleans up) boots
  ds-vdev + ds-daemon on a temp dir and asserts `ds-focus --inject '{"app":"code","title":"x"}'`
  → the event on `ds watch /app/focus/changed --ndjson` with exactly that payload and no
  `rev`, the info-level emit log, and the `/focus` mirror via `ds get /focus --json` when
  the daemon build carries it (DS-044); it also replays an ndjson file in order.
- **Minimum working example:**
  ```sh
  ds-focus --backend auto &                 # logs "backend kwin selected (…)" on KDE
  ds watch /app/focus/changed --ndjson
  # in another terminal: focus a window → one event line appears
  ```

Out of scope for this binary: the daemon-side `/focus` mirror (DS-044), bindings
(DS-041/042), session autostart (DS-064), Windows/macOS backends (post-alpha).

# Procedure: observing module position / orientation signals (DS-112 prep)

Status: **prep only** for ticket DS-112
(`docs/tickets/2026-11-25-module-position-orientation.md`). This document and the
scripts it references change **no** daemon code, schemas, contracts, or tests. The
`(slot, orientation)` identity remap itself is the ticket's implementation work and
is deliberately not done here.

Audience: the engineer who later runs the hardware observation session on the real
MasterHub deck. Everything in Part A runs **now** against the local `ds-vdev` virtual
device and is the rehearsal for Part B.

Safety: never run anything with `DS_HARDWARE=1` unattended; the hardware session is
supervised. Both scripts refuse to start when `DS_HARDWARE=1` is set. All local
commands below run against a private temp dir and never touch the default daemon
socket (`$XDG_RUNTIME_DIR/deckstack/daemon.sock`) or any real deck.

## 0. The signals this procedure observes

| Signal | Wire / state form | Where decoded |
|---|---|---|
| Base orientation | input tag `0x2B`, payload `01` = flat/horizontal, `00` = upright/vertical; also readable via feature `0x35` (`35 00 vv`) | `crates/ds-device/src/hid/reader.rs` → event topic `deck/device/<serial>/orientation` with payload `{"orientation":"horizontal"\|"vertical"}` (`crates/ds-daemon/src/router/mod.rs:283`) |
| Module slot | USB port path relative to the base's top hub, e.g. `1-4-3` (`upstream/PROTOCOL.md:107-127`); any module fits any of the **26 connectors** | `schemas/state.json` `$defs.device.slot`; daemon writes `connection` / `last_seen_rev` only today |
| Connect / disconnect | hot-plug events | topics `deck/device/<serial>/connected` \| `disconnected` |
| Device state | `/devices/<serial>` = `{kind, model, pid, slot, connection, last_seen_rev}` | `ds get /devices --json` (`docs/contracts/state.md`) |
| Frame / write oracle | reassembled frame sha256, report log | `ds-vdevctl frames` / `ds-vdevctl writes` (vdev only) |

Known current behaviour (observed in the Part A rehearsal, 2026-11-27): on the
`virtual:` transport the daemon logs `N module(s) handshaken` but `/devices` stays
empty (`{"value": {}}`); serials are `virtual-<pid hex>` (base = `virtual-01f6`).
On real hardware `/devices/<serial>` is populated at connect — the hardware session
must confirm the `slot` field's real values (`1-4-3` style).

## Part A — vdev rehearsal (run now, locally)

One command, self-cleaning, exit 0 = green:

```sh
sh tools/observe-orientation.sh rehearse          # fresh temp dir, full dry-run
sh tools/observe-orientation.sh rehearse --keep   # keep stack + logs for interactive poking
sh tools/observe-orientation.sh rehearse --dir /tmp/ds112-a  # fixed dir, stack left running
```

What `rehearse` does and asserts (all logged with UTC timestamps):

1. boots `ds-vdev` + `ds-daemon` on `virtual:<dir>/vdev.sock` with a private data dir;
2. attaches `tools/watch-deck-events.sh` on `/deck/*` → `<dir>/events.log`;
3. injects base orientation **upright** then **flat** via `ds-vdevctl inject`
   (`{"module":"base","orientation":"vertical"}` / `"horizontal"`) and asserts the
   `deck/device/virtual-01f6/orientation` events appear with the right payload;
4. pushes a key frame (`ds set /controls/keys/0/image '{"kind":"color","value":"#336699"}'`)
   and records `ds-vdevctl frames key:0` sha256;
5. simulates a **replug**: arms `ds-vdevctl throttle off 1`, forces a write past the
   budget (asserts the `deck/device/<serial>/disconnected` event), restarts the
   daemon on the same vdev (fresh connection = fresh write budget), asserts
   re-handshake (`… module(s) handshaken` in `daemon.log`) and that the re-pushed
   `key:0` frame sha256 is **intact**;
6. prints the captured event log and a `PASS/FAIL` summary.

Reference output (green run, 2026-11-27): `8 passed, 0 failed`, with events like

```
2026-11-27T19:56:01.251Z {"op":"event","path":"/deck/device/virtual-01f6/orientation","payload":{"orientation":"vertical"},…}
2026-11-27T19:56:02.297Z {"op":"event","path":"/deck/device/virtual-01f6/orientation","payload":{"orientation":"horizontal"},…}
2026-11-27T19:56:04.355Z {"op":"event","path":"/deck/device/virtual-01fe/disconnected","payload":{},…}
```

Interactive building blocks (all idempotent, all `--help`-documented; default dir
`${DS112_DIR:-/tmp/ds112-vdev}`):

```sh
sh tools/observe-orientation.sh start  --dir /tmp/ds112      # boot vdev+daemon (no-op if healthy)
sh tools/observe-orientation.sh status --dir /tmp/ds112
sh tools/observe-orientation.sh watch  --dir /tmp/ds112 --seconds 30   # append /deck/* events to events.log
sh tools/observe-orientation.sh orient --dir /tmp/ds112 upright         # base rotate (tag 0x2B)
sh tools/observe-orientation.sh snapshot --dir /tmp/ds112               # /devices + reports/writes/frames
sh tools/observe-orientation.sh replug --dir /tmp/ds112                 # drop + reconnect + sha check
sh tools/observe-orientation.sh move   --dir /tmp/ds112                 # rotation + replug analogue
sh tools/observe-orientation.sh stop   --dir /tmp/ds112 [--remove]
```

Raw equivalents (what the scripts wrap), for the record:

```sh
ds-vdev --socket /tmp/vdev.sock &                       # virtual deck
ds daemon start --device virtual:/tmp/vdev.sock --data-dir /tmp/data
ds --socket /tmp/data/daemon.sock --data-dir /tmp/data watch /deck/* --ndjson
ds-vdevctl --socket /tmp/vdev.sock inject ev.ndjson     # ev.ndjson: {"module":"base","orientation":"vertical"}
ds-vdevctl --socket /tmp/vdev.sock throttle off 1       # arm hot-unplug after 1 accepted write
ds --socket … set /controls/keys/1/image '{"kind":"color","value":"#aa0000"}'   # trip the drop
ds-vdevctl --socket /tmp/vdev.sock frames key:0         # sha256 oracle
ds --socket … --data-dir /tmp/data get /devices --json
```

`move` is the honest analogue available today: base rotation (`orient`) + replug.
The vdev has **no slot-change knob yet** — simulating "same module, different
connector" is part of the DS-112 implementation (ticket req. 5), not this prep.

## Part B — hardware observation procedure (run later, on the real deck)

Preconditions: daemon running against `hid:` (`ds status` shows `device=hid:…`),
udev permissions in place (`docs/agents.md`), a capture dir, and the event watcher
already attached **before** touching any connector:

```sh
mkdir -p /tmp/ds112-hw && cd /tmp/ds112-hw
sh tools/watch-deck-events.sh \
    --socket "$XDG_RUNTIME_DIR/deckstack/daemon.sock" \
    --data-dir "$XDG_RUNTIME_DIR/deckstack" \
    --log /tmp/ds112-hw/events.log --pattern '/deck/*' &   # keep running for the whole session
```

At every step, capture into `/tmp/ds112-hw/`:

1. `ds watch /deck/* --ndjson` output (the watcher above) — orientation /
   connected / disconnected events with `ts_ms`;
2. `ds get /devices --json` → `devices-step<N>.json` — serial, `slot` (USB port
   path, e.g. `1-4-3`), `connection`, `last_seen_rev` per module;
3. `ds status` → `status-step<N>.txt`;
4. `tail -50 <daemon data-dir>/daemon.log` → `daemon-step<N>.log` (look for
   `connected hid:…`, `module(s) handshaken`, `re-pushing N cached target(s)`,
   `disconnected`, reconnect within the ≤ 5 s budget — `docs/90-verification.md`);
5. `udevadm info /dev/hidrawN` + `lsusb -t` → `usb-step<N>.txt` — the physical port
   path to correlate with the `slot` string;
6. app-level note: which `/controls/…` visuals still respond after the move
   (`ds get /controls/knobs/0` etc.).

Steps (perform in order; record observed vs expected in a copy of the table):

| Step | Manual action on the deck | Expected signal to capture |
|---|---|---|
| B1 | Baseline: all 26 connectors as shipped, base flat | `devices-step1.json` with `slot` per module; no orientation event yet (or one `horizontal` if the base reports on connect) |
| B2 | Rotate base flat → upright | `deck/device/<base-serial>/orientation` payload `{"orientation":"vertical"}` (tag `0x2B` payload `00`); **no USB hot-plug event** — layout change is purely logical (6×4 vs 4×6) |
| B3 | Rotate base back upright → flat | orientation event `{"orientation":"horizontal"}` (payload `01`) |
| B4 | Reseat the knob module in the **same** connector (unplug, wait 3 s, replug) | `deck/device/<serial>/disconnected` then `connected` for the **same serial**; `slot` unchanged; daemon re-handshakes and re-pushes within ≤ 5 s |
| B5 | Move the knob module to a **different** connector (any of the 26) | same serial, **different `slot`** (port path); this is the case the ticket says currently mis-identifies the module as "new" — capture `/devices` before/after and whether the old subtree stops responding |
| B6 | With base upright, move a second module (e.g. faders) to another connector | combination of orientation + slot change; capture both event streams and `/devices` |
| B7 | Unplug a module and leave it out > 30 s | `disconnected`, module marked unhealthy, `/devices/<serial>/connection` state; daemon stays responsive (`ds status` ok) — the Salon hang from the ticket must NOT reproduce |
| B8 | Replug everything back to the B1 layout | all serials back at their B1 `slot`s; orientation back to `horizontal` |

For B5/B6 also capture the raw input report if a userspace HID tap is available
(`usbhid-dump`/`hidraw` read of tag `0x2B` and feature `0x35` reply `35 00 vv`) so
the payload mapping (`01`=flat, `00`=upright) is confirmed against
`docs/discovery/40-hardware-and-protocol.md` §3.

After the session: file the captured `/tmp/ds112-hw/` bundle against DS-112
(observed `slot` format, orientation payload direction, replug timing vs the ≤ 5 s
budget, and any identity breakage in B5/B6) — that evidence feeds requirements 1–5
of the ticket.

## Files

- `tools/observe-orientation.sh` — vdev stack lifecycle + simulation + `rehearse` (this doc, Part A).
- `tools/watch-deck-events.sh` — timestamped `/deck/*` event logger for any daemon (vdev rehearsal and the Part B hardware session).
- No changes to `crates/`, `schemas/`, `docs/contracts/`, or tests.

## Consumer-facing goal of the mapping (added 2026-11-27, SALON-001 session)

The point of mapping connector ↔ USB port path ↔ orientation is not only identity:
deckstack should expose, per device, **in what orientation it faces the user** (base
flat/vertical × module rotation at its connector), so consumers can rotate screen
content, remap inputs, or abstract the physical layout away behind one API. All
possible configurations must be **representable in state** and **testable via the
virtual device** across permutations (module kind × connector × base orientation),
not hardcoded to the salon deck's current layout. Hot-swappable: a reseat/rotation
must adjust the logical view on the fly, without an app restart (see DS-120 for the
current restart-on-unplug defect).

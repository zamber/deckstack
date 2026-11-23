# Component: ds-vdev (virtual device)

A software MasterHub at the HID-report level for CI and development (R10, D16). Port of
`hubdeck/tests/fakes/virtual_hub.py:17-130,196-246` to Rust, exposed as a socket server
so the real `ds-device` code path (chunking, framing, safety) runs unchanged.

- **Language:** Rust. Why: the acceptance suite must drive the real drivers, not a
  parallel model (`hubdeck/CLAUDE.md:101-103` — if real and virtual disagree, fix the
  model).
- **Implements:** VID `0x2516` modules — keys M4315 (`0x01FE`, 320 B out, tag `0x34`),
  dial M131E (`0x0206`, tag `0x44`), and the **full M0 family** (`0x01F6` base,
  `0x0200` faders, `0x0202` knobs, `0x0204` rollers — rollers included explicitly, m7:
  64 B out, tag `0x2A`, input `01 2A … 01 02 r1 r2`) — `docs/discovery/40` §1. Software-mode features `03 3A/49`, backlight
  `03 34/44`, solid `03 36/37/46`, chunked images `02 36/35/45` (312 B chunks, reassembled
  into `frames[target]`), LEDs `02 2C/2D/2E`, input injection (`01 34 … 02 0F bitmap`,
  dial `01 44 …`, M0 `01 2A …`), feature reads incl. serial (`0x30/0x3A/0x4A`),
  fader values `0x33`, orientation `0x35`.
- **Enforces the safety rule too:** refuses forbidden reports with the same
  `is_forbidden` list (`upstream/crates/mh-protocol/src/safety.rs:8-32`) so tests can
  assert the daemon never emits them.
- **Public surface:** `ds-vdev --socket /tmp/vdev.sock [--modules keys,dial,m0]
  [--inject events.ndjson] [--throttle-kbps N] [--drop-after N]`. Inspection is the
  separate **`ds-vdevctl` binary** (m10/S10 — there is no test-only verb in
  the production `ds` CLI): `ds-vdevctl --socket S frames <target>` → sha256 + bytes of
  each reassembled frame (the golden-frame oracle); `ds-vdevctl … writes` → full report
  log (bytes, chunk index, timestamps, per-write return counts); `ds-vdevctl … inject
  events.ndjson` → scripted input with timestamps; `ds-vdevctl … reports` → the module
  reports of the deck layout; `ds-vdevctl … throttle <kbps|off> [drop-after|off]` →
  runtime throttle knobs. Targets for `frames`: `panel`, `key:N` (N < 15), `dial`.
- **Failure modes / knobs:** `--throttle-kbps` simulates the ~2.1 MB/s device cap
  (`upstream/PROTOCOL.md:265`) to prove backpressure; `--drop-after` simulates
  hot-unplug; `--inject` replays input events with timestamps. All knobs are also settable
  at runtime on the control socket (`throttle …`, see below) and from the Rust API
  (`Hub::set_throttle` / `Hub::set_credits`). Exact semantics under
  §"Throttle and backpressure knobs (DS-035)".
- **Dependencies:** ds-protocol (mh-protocol) for report building/parsing.
- **Test command:** `cargo test -p ds-vdev`; the conformance suite
  (`python tests/conformance/run.py`) starts ds-vdev itself.
- **Minimum working example:**
  ```sh
  ds-vdev --socket /tmp/vdev.sock &
  ds daemon start --device virtual:/tmp/vdev.sock
  ds set /controls/keys/0/image --file examples/key.png
  ds-vdevctl --socket /tmp/vdev.sock frames key:0    # sha256 of the reassembled frame
  ds-vdevctl --socket /tmp/vdev.sock writes | tail -3
  ```

## Socket protocol (normative — ds-device ↔ ds-vdev)

Pinned by DS-014; the Rust implementation is `crates/ds-device/src/frame.rs` (host side) and the
server side lands with DS-017. Every message on the Unix socket is one frame:

```
u32 LE body_len | u8 kind | u16 LE pid | payload[body_len - 3]
```

- `pid` is the USB product id of the module the message is about (vendor id `0x2516` always);
  `pid = 0` means "the connection itself" (only `CLOSE`). `body_len` is in `3..=4096`.
- Host → device kinds: `0x01` **write** (output report bytes, report id `0x02` first, padded to
  the module's output report length — 320 B keys/dial, 64 B M0), `0x02` **feature set**
  (`0x03 …`, padded to 32 B), `0x03` **feature get** (payload = `[feature_id]`), `0x04` **close**
  (no payload).
- Device → host kinds: `0x81` **input** (raw input report bytes, id `0x01` first — this is the
  `--inject` path), `0x82` **ack** (payload = `u32 LE` byte count accepted, the `hid_write`
  return; sent in request order, possibly delayed by `--throttle-kbps`), `0x83` **answer**
  (feature report answer bytes, report id first — the reply to `0x03`), `0x84` **error**
  (payload = UTF-8 message, e.g. a refused forbidden report).
- One ack per write/feature-set, one answer per feature-get; input reports arrive
  asynchronously and may interleave with acks/answers. The host matches them by `pid`.

## Control socket (normative for ds-vdevctl ↔ ds-vdev)

`pid = 0` is reserved for CLOSE on the device socket, so inspection/control cannot ride it.
Instead `ds-vdev` also binds a sibling line-based control socket at `<socket>.ctl` (DS-017).
One request per connection — a single line — answered with `OK`, then zero or more body
lines, then a line containing only `.`; failures are a single line `ERR <message>`.
Requests (first word = command):

```
ping
frames [target]            # target: panel | key:N | dial; body: "<target> <sha256> <bytes>B seq=<n>"
reports                    # one line per served module (pid, report lengths, tags, state)
writes [n]                 # last n report log entries (seq, pid, kind, len, refused, at_ms)
throttle <kbps|off> [drop-after|off] [credits=<burst>/<refill>/<period_ms>[@<seed>]|off]
inject <event-json>        # one ndjson event (see below), broadcast as an 0x81 INPUT frame
```

`throttle` replies `throttle kbps=<n|off> drop_after=<n|off> credits=<b>/<r>/<ms>[@<seed>] remaining=<n>|off`
(the `remaining` field is the credit bucket's current fill; it is only present while a
credit window is armed).

`--inject events.ndjson` / `ds-vdevctl inject` replay scripts of one JSON object per line,
`{"module": "keys", "keys": [0, 3], "at_ms": 120}` (also `faders`, `knob`/`knob_delta`,
`knob_press`/`pressed`, `roller`/`roller_delta`, `dial_delta`, `dial_pressed`,
`orientation`); the hub encodes each into the module's raw input report
(`01 <tag> <len u16 LE> <payload…>`, 64 B) and broadcasts it to every connected host.

## Throttle and backpressure knobs (DS-035)

All slowdown knobs live in `crates/ds-vdev/src/throttle.rs` and are deterministic: the
default is no throttling at all, and every delay is a pure function of the knob values and
the write timeline (a seed only perturbs the credit-refill schedule, reproducibly).
They are reachable from the CLI (`ds-vdev --throttle-kbps N --drop-after N`), from
`ds-vdevctl` (`throttle <kbps|off> [drop-after|off] [credits=…|off]` on the control
socket) and from the Rust API (`Throttle`, `HubConfig::throttle`, `Hub::set_throttle`,
`CreditWindow`, `Hub::set_credits`, `Hub::credits`, `Hub::credit_remaining`).

- **`kbps` (`--throttle-kbps N`)** — the simulated device write cap in kbit/s
  (1 kbit/s = 125 B/s; the real cap is ~2.1 MB/s ≈ 16800 kbit/s). The ack of a
  WRITE/FEATURE_SET is delayed by `payload_len / (kbps * 125)` seconds — the time the real
  device would need for those bytes. **Acks are delayed, never reordered and never
  duplicated: exactly one ack per write, in request order** (the host matches them by pid).
  Feature gets are never delayed.
- **`drop-after` (`--drop-after N`)** — hot-unplug simulation. The write budget is **per
  device connection**: the write that pushes a connection past `N` accepted writes gets no
  ack and the connection is closed (the host sees `Error::Closed`/`Error::Timeout` on the
  in-flight send and one `DeviceEvent::Disconnected` from `recv` — the signal the daemon's
  device task uses to mark the module unhealthy). The already-accepted writes keep their
  acks, and the unplugging write is not entered in the write log. **A new connection (a
  reconnect) starts with a fresh budget of N** — the replugged device accepts again, which
  is what lets a reconnect re-push the latest frame.
- **`credits=<burst>/<refill>/<period_ms>[@<seed>]`** — credit-exhaustion mode (armed at
  runtime via the control socket or `Hub::set_credits`; `credits=off` disarms). The device
  keeps a token bucket of write credits: it starts full (`burst`), every accepted **output**
  write (WRITE) consumes one, and the bucket refills by `refill` (capped at `burst`) every
  `period_ms`. When the bucket is empty, **the ack is held back until the next refill** —
  the sender's write *waits* (backpressure) instead of being dropped or buffered without
  bound; when credits return, writes proceed again at full speed until the bucket empties.
  Feature traffic (handshake, feature sets/gets) is never charged or held, and the control
  socket is never blocked by a held write. With `@<seed>` each refill window gets a
  deterministic jitter of up to `period_ms/8` derived from the seed (same seed + same write
  timeline ⇒ identical hold durations); without a seed the schedule is fixed.

**In-flight frames are never torn.** Every write is one whole report; the hub's write log
(`Hub::writes`, each record now carries `chunk = Some((index, last))` for image chunks) is
the oracle: after a drop the device never sees a partial chunk sequence — the next
connection's transfer restarts at chunk 0 and completes with `last=1`, and only completed
transfers ever land in `frames[<target>]` (reassembled sha256 intact).

**Tests:** `cargo test -p ds-vdev --test backpressure` (`tests/bench/backpressure.rs`) drives
the real `ds-device` `VirtualTransport` (and the daemon `FrameService` for the
queue-accounting assertions) against an in-process `VdevServer` and proves: ack ordering +
one-ack-per-write under kbps shaping; credit exhaustion → held writes, fast control plane,
recovery on refill, no unbounded buffering; drop-after → clean `Closed`/`Timeout` teardown,
frame service drop+count, `Disconnected`, reconnect re-push with intact sha; the write-log
no-torn-sequence oracle; a 32 fps bench against a throttled vdev asserting
`dropped == sent - accepted` and a bounded queue (1 in flight + 1 waiting); and a soak
(default 60 s, `DS_BACKPRESSURE_SOAK_SECS` to shorten for CI; the bench window is
`DS_BACKPRESSURE_BENCH_SECS`) at a throttled rate with RSS growth < 100 MB recorded via
`--nocapture` and control-socket ping p99 ≤ 10 ms.

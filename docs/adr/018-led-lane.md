# ADR-018 — LED lane: latest-wins coalescing, dedicated writer thread, per-module caps

Status: accepted (implemented on branch `led-coalesce`). Deciders: plan author per the
LED-latency ticket (Salon micro-app colour lag).

## Context

Measured on a vdev + private daemon (pre-fix): a 20 Hz Salon-style feed (base LED + three
per-index knob paths + a roller path) lost pushes and lagged 1–2 s. 100 base-LED writes
issued in 0.97 s (300 reports planned) had only 276 reports arrived after 21 s. Causes,
all in `crates/ds-daemon/src/device.rs` + `state.rs`:

1. the state→device forwarder `try_send`s into a bounded 64-slot FIFO, so a burst drops the
   *newest* push and then replays stale ones;
2. the single device task sleeps inside `led.acquire()` on one global 30 reports/s bucket, so
   input reads and image pushes wait behind LED pacing;
3. `BaseLed` re-sent mode+colour+brightness (3 reports) on every colour-only change;
4. every per-index knob path planned the same module-wide `LedTarget::All` report;
5. `/controls/rollers/*/led` planned `LedColor{Rollers}`, which mh-protocol cannot represent
   (`m0_led_address` supports Base/Faders/Knobs only) — every write logged "no led report".

## Decision

- **Latest-wins lane per LED target** (`crates/ds-daemon/src/led.rs`): LED-plane state writes
  (`/controls/base/led`, `/controls/{knobs,faders,rollers}/<n>/led`,
  `/controls/{keys/<n>,dial}/brightness`) never enter the bounded channel. They are stored
  under a per-target key, replacing any pending value (counted `superseded`). The newest
  value is never dropped and never queued behind a stale one. The state plane's `rev`
  semantics are untouched; non-LED pushes keep the bounded channel + drop counting.
- **Dedicated LED writer thread** owns the per-module token buckets and is the only code that
  may sleep on LED pacing (`acquire()`). The device task handles input reads and image pushes
  and never touches an LED limiter, so LED pacing cannot block them. The transport is shared
  as `Arc<dyn Transport>` (`Transport: Send + Sync`, writes serialized internally).
- **Per-module rate caps, no global bucket and no cap raise**: each module kind gets its own
  bucket at `/policy/led_rate_per_s` (default 30/s); faders are capped at 25/s on hardware
  evidence (`upstream/PROTOCOL.md:118-124`: the faders' firmware takes ~25 LED reports/s,
  not 50, and the module fell off USB twice at ~50/s while at 10/s it stayed connected for
  hours). Base+knobs on a fader-less deck therefore sustain 60 reports/s aggregate (the
  20 fps × 3 reports target) without any single module exceeding 30/s.
- **Report dedup**: the last mode/colour/brightness successfully sent per module is cached;
  a colour-only change sends exactly one `02 2D` report; per-index paths targeting the same
  module-wide LED collapse into one send (the lane key is per module kind).
- **Rollers**: the M032LE has no LED command — the official M0 SDK accepts LED targets
  0 = base (index 0 only), 1 = faders 0-4/`FF`, 2 = knobs 0-2/`FF` and nothing else
  (`upstream/PROTOCOL.md:38`, `mh_protocol::report::m0_led_address`). No command is
  invented: `/controls/rollers/*/led` stays valid state (schema unchanged) but plans to
  nothing and logs a one-shot warning.

## Alternatives

- Raise the single global bucket to 60/s — rejected: it is exactly the "blindly raise the
  cap" move; the faders' USB drops at ~50/s show a global raise is unsafe, and it would still
  sleep the device task.
- Keep LED writes on the device task with `try_acquire` + a pending map — rejected: it fixes
  the sleep but keeps LED and image I/O on one thread, so a burst of image chunks still delays
  LED sends behind them.
- Coalesce in the state task and keep sending on the channel — rejected: the bounded FIFO
  can still drop the newest push when full; the lane has no capacity limit to overflow.

## Consequences

Positive: newest colour always reaches the device (measured below), no stale backlog, LED
pacing structurally cannot block input/image paths, 2 reports per 20 Hz colour frame
instead of 6+. Negative: one more thread and a second forwarding path in the state task;
roller LED writes are silent no-ops (warned once) — apps animating rollers will not see
light there (the M032LE has no addressable LEDs at all per the SDK).

## Measurements (vdev, `ds-vdev --socket …` + private daemon, debug binaries)

- Bench `tests/bench/led_latency.py --check`, 20 Hz × 5 s feed (base + knobs/0..2), 4 runs:
  p95 lag 18.9–19.5 ms, max 20.0–20.8 ms, newest-colour lag 18.1–19.6 ms, 100/100 frames
  delivered to both base and knobs (200 colour reports = 2/frame), 20.0 effective fps,
  zero "no led report". Pre-fix baseline: 276/300 reports after 21 s on a 100-write burst,
  500-write feed lost pushes (channel full), 74 "no led report" errors.
- Burst soak: 200 base writes in 1.96 s → 134 colour reports sent (coalesced), newest
  arrival 33.8 ms after its issue, max 38.8 ms — no backlog.
- 30 Hz × 10 s on base+knobs (30/s per module = the cap): 250/250 frames delivered, lag
  p95 20.1 ms, max 21.9 ms.
- Faders at ~30 Hz with the 25/s cap: 150/150 frames delivered (5 per-index paths coalesced
  to 1 report/frame), lag p95 10.3 ms, newest 9.3 ms.
- The vdev proves host-side delivery only; the real-deck LED soak (hours at 30/s per module)
  remains a phase-2 hardware item (OPEN O2).

## References

`crates/ds-daemon/src/led.rs`, `device.rs` (`led_lane_key`, rollers arm of `plan_push`),
`state.rs` forwarder, `docs/contracts/streaming.md` §5, `docs/components/device-driver.md`
invariant 4, `upstream/PROTOCOL.md:34-38,118-124`, `tests/bench/led_latency.py`.
Decision log D39.

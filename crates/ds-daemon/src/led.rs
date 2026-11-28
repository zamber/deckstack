//! The LED lane: latest-wins coalescing per LED target between state commits and device
//! writes, plus the dedicated LED writer thread that owns all LED pacing.
//!
//! Why this exists (the measured 1–2 s LED lag on the Salon micro-app): the state→device
//! channel is a bounded FIFO with `try_send`, so a burst dropped the *newest* colour push and
//! replayed stale ones; the single device task slept inside `led.acquire()` on one global
//! 30 reports/s bucket, so input reads and image pushes waited behind LED pacing; `BaseLed`
//! re-sent mode+colour+brightness on every colour-only change; and every per-index knob path
//! produced the same module-wide report.
//!
//! Design (same discipline as the frame plane, `docs/contracts/streaming.md` §3):
//!
//! - **Latest-wins, newest never dropped.** LED-plane pushes never enter the bounded channel.
//!   [`LedLane::submit`] stores the push under its target key, *replacing* any pending one
//!   (counted as `superseded`). There is no queue of stale colours to replay: at most one
//!   pending value per LED target exists, and it is always the newest.
//! - **No sleeping on the device task.** A dedicated writer thread ([`LedLane::spawn_writer`])
//!   owns the LED token buckets and is the only place that calls `acquire()` (which may sleep)
//!   for LED writes. The device task keeps handling input reads and image pushes and never
//!   touches an LED limiter, so LED pacing cannot block them.
//! - **Per-module rate caps, not one global bucket.** Each module kind gets its own token
//!   bucket: `/policy/led_rate_per_s` (default 30/s) for base/knobs/keys/dial, and
//!   [`FADER_LED_RATE_PER_S`] (25/s) for faders — the faders' firmware takes about 25 LED
//!   reports a second and the module dropped off USB at ~50/s
//!   (`upstream/PROTOCOL.md:118-124`), so 60/s aggregate (3 reports × 20 fps across
//!   base + knobs on a fader-less deck) is delivered while faders stay under their cap.
//! - **Report dedup.** The last successfully sent mode/colour/brightness per module is
//!   cached; a colour-only change sends exactly one `02 2D` report, and per-index paths that
//!   target the same module-wide LED collapse into one send (the lane key is per module kind).
//!
//! The state plane is untouched: `rev` semantics, the bounded channel for non-LED pushes and
//! their drop counting all stay as they were.

use crate::device::{self, DeviceAction, DevicePush};
use crate::lifecycle::{self, Shutdown};
use ds_device::rate::{RateLimiter, DEFAULT_LED_RATE_PER_S};
use ds_device::transport::{module_id_for_send, Transport};
use ds_protocol::{LedMode, LedTarget, ModuleKind};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Fader LED writes are capped at 25/s (hardware evidence, `upstream/PROTOCOL.md:118-124`:
/// the faders' firmware takes about 25 LED reports a second, not 50, and the module fell off
/// USB twice while its LEDs changed at ~50/s; at 10/s it stayed connected for hours).
pub const FADER_LED_RATE_PER_S: u32 = 25;

/// How often the LED writer re-checks shutdown / transport while idle.
const WAIT_SLICE: Duration = Duration::from_millis(50);

/// The last LED-plane value actually sent to one module (report dedup cache).
#[derive(Debug, Default, Clone)]
struct LastSent {
    mode: Option<LedMode>,
    rgb: Option<[u8; 3]>,
    brightness: Option<u8>,
}

struct Inner {
    /// Latest pending push per LED target key — replacing is the whole point (latest-wins).
    pending: Mutex<HashMap<String, DevicePush>>,
    cv: Condvar,
    /// The live transport, installed by the device task once the device is open.
    transport: Mutex<Option<Arc<dyn Transport>>>,
    shutdown: Shutdown,
    /// Pending pushes replaced by a newer value for the same target.
    superseded: AtomicU64,
    /// LED/control reports actually sent on the wire.
    sent: AtomicU64,
    /// Pushes skipped because the value equals the last one sent to that module.
    deduped: AtomicU64,
    /// Sends that failed (logged and dropped, counted — never retried blindly).
    send_errors: AtomicU64,
    /// Bumped on every (re)connect ([`LedLane::invalidate`]): the writer clears its dedup
    /// cache when it changes, so a fresh connection receives the current LED values again
    /// even when they are unchanged since the last send (DS-120 re-push).
    generation: AtomicU64,
}

/// The latest-wins LED lane between the state task and the device.
#[derive(Clone)]
pub struct LedLane {
    inner: Arc<Inner>,
}

impl LedLane {
    /// A lane that stops with `shutdown`.
    pub fn new(shutdown: Shutdown) -> Self {
        Self {
            inner: Arc::new(Inner {
                pending: Mutex::new(HashMap::new()),
                cv: Condvar::new(),
                transport: Mutex::new(None),
                shutdown,
                superseded: AtomicU64::new(0),
                sent: AtomicU64::new(0),
                deduped: AtomicU64::new(0),
                send_errors: AtomicU64::new(0),
                generation: AtomicU64::new(0),
            }),
        }
    }

    /// Store the newest push for `key`, replacing any pending one (never queued behind it).
    /// Never blocks, never drops the newest value.
    pub fn submit(&self, key: String, push: DevicePush) {
        let mut p = self.inner.pending.lock().expect("led lane poisoned");
        if p.insert(key, push).is_some() {
            self.inner.superseded.fetch_add(1, Ordering::Relaxed);
        }
        drop(p);
        self.inner.cv.notify_one();
    }

    /// Install (or clear, with `None`) the live transport the writer sends through.
    pub fn set_transport(&self, t: Option<Arc<dyn Transport>>) {
        *self.inner.transport.lock().expect("led lane poisoned") = t;
        self.inner.cv.notify_one();
    }

    /// Invalidate the writer's report-dedup cache: call on every (re)connect so the new
    /// connection is re-sent the current LED values even if unchanged (DS-120 re-push).
    pub fn invalidate(&self) {
        self.inner.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Spawn the dedicated LED writer thread. It owns the per-module LED token buckets and is
    /// the only LED code path that may sleep on pacing.
    pub fn spawn_writer(self) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("ds-led-writer".into())
            .spawn(move || run_writer(self.inner))
            .expect("spawn ds-led-writer thread")
    }

    /// Pending pushes right now (one per target key at most).
    pub fn pending_len(&self) -> usize {
        self.inner.pending.lock().expect("led lane poisoned").len()
    }

    /// Replaced-pending count (latest-wins coalescing).
    pub fn superseded_count(&self) -> u64 {
        self.inner.superseded.load(Ordering::Relaxed)
    }

    /// Reports sent on the wire.
    pub fn sent_count(&self) -> u64 {
        self.inner.sent.load(Ordering::Relaxed)
    }

    /// Pushes skipped by report dedup (value unchanged since the last send).
    pub fn deduped_count(&self) -> u64 {
        self.inner.deduped.load(Ordering::Relaxed)
    }

    /// Failed sends (counted, logged).
    pub fn send_error_count(&self) -> u64 {
        self.inner.send_errors.load(Ordering::Relaxed)
    }
}

/// The per-module LED token bucket: `/policy/led_rate_per_s` default for every module except
/// faders, which are capped lower on hardware evidence (see [`FADER_LED_RATE_PER_S`]).
fn limiter_for(kind: ModuleKind) -> RateLimiter {
    let per_s = match kind {
        ModuleKind::Faders => FADER_LED_RATE_PER_S,
        _ => DEFAULT_LED_RATE_PER_S,
    };
    RateLimiter::new(per_s)
}

fn run_writer(inner: Arc<Inner>) {
    let mut limiters: HashMap<ModuleKind, RateLimiter> = ModuleKind::ALL
        .into_iter()
        .map(|k| (k, limiter_for(k)))
        .collect();
    let mut last: HashMap<ModuleKind, LastSent> = HashMap::new();
    let mut last_gen = inner.generation.load(Ordering::Relaxed);
    loop {
        if inner.shutdown.get() {
            return;
        }
        // a new transport generation means a fresh connection: forget the dedup cache so
        // the current values are re-sent (DS-120 re-push after a hot-plug reconnect)
        let gen = inner.generation.load(Ordering::Relaxed);
        if gen != last_gen {
            last.clear();
            last_gen = gen;
        }
        // Wait until there is something pending AND a transport to send it through. The
        // pending map is drained wholesale: each key holds only the newest value, so a burst
        // of commits collapses to one send per target.
        let batch: Vec<(String, DevicePush)> = {
            let mut p = inner.pending.lock().expect("led lane poisoned");
            loop {
                if inner.shutdown.get() {
                    return;
                }
                if !p.is_empty() && inner.transport.lock().expect("led lane poisoned").is_some() {
                    break;
                }
                let (g, _) = inner
                    .cv
                    .wait_timeout(p, WAIT_SLICE)
                    .expect("led lane poisoned");
                p = g;
            }
            std::mem::take(&mut *p).into_iter().collect()
        };
        let Some(t) = inner.transport.lock().expect("led lane poisoned").clone() else {
            continue;
        };
        for (key, push) in batch {
            let DevicePush::State { path, value } = push;
            let Some(action) = device::plan_push(&path, &value) else {
                // out of scope (e.g. rollers have no LED command): already warned once by
                // plan_push; nothing to send
                lifecycle::log(
                    "debug",
                    "ds-daemon::led",
                    &format!("no device action for {path} (lane key {key}); dropped"),
                );
                continue;
            };
            if let Err(e) = send_led(t.as_ref(), &action, &mut limiters, &mut last, &inner) {
                inner.send_errors.fetch_add(1, Ordering::Relaxed);
                lifecycle::log(
                    "error",
                    "ds-daemon::led",
                    &format!("led push {path} failed: {e} (counted, dropped)"),
                );
            }
        }
    }
}

/// Send one LED-plane action, paced by the per-module bucket, deduplicated against the last
/// value actually sent. `acquire()` may sleep here — this thread owns LED pacing and nothing
/// else waits on it.
fn send_led(
    t: &dyn Transport,
    action: &DeviceAction,
    limiters: &mut HashMap<ModuleKind, RateLimiter>,
    last: &mut HashMap<ModuleKind, LastSent>,
    inner: &Inner,
) -> Result<(), String> {
    match action {
        DeviceAction::BaseLed {
            mode,
            rgb,
            brightness,
        } => {
            let module = module_id_for_send(t, ModuleKind::Base).map_err(|e| e.to_string())?;
            let lim = limiters
                .get(&ModuleKind::Base)
                .expect("base limiter exists");
            let e = last.entry(ModuleKind::Base).or_default();
            // a colour-only change sends exactly the colour report
            if e.mode != Some(*mode) {
                let r = ds_protocol::led_mode(ModuleKind::Base, LedTarget::All, *mode)
                    .ok_or("no led mode report")?;
                lim.acquire();
                t.send(&module, &r).map_err(|e| e.to_string())?;
                e.mode = Some(*mode);
                inner.sent.fetch_add(1, Ordering::Relaxed);
            }
            if e.rgb != Some(*rgb) {
                let r = ds_protocol::led_color(ModuleKind::Base, LedTarget::All, *rgb)
                    .ok_or("no led report")?;
                lim.acquire();
                t.send(&module, &r).map_err(|e| e.to_string())?;
                e.rgb = Some(*rgb);
                inner.sent.fetch_add(1, Ordering::Relaxed);
            }
            if e.brightness != Some(*brightness) {
                let r = ds_protocol::led_brightness(ModuleKind::Base, LedTarget::All, *brightness)
                    .ok_or("no led report")?;
                lim.acquire();
                t.send(&module, &r).map_err(|e| e.to_string())?;
                e.brightness = Some(*brightness);
                inner.sent.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
        DeviceAction::LedColor {
            kind,
            mode,
            rgb,
            brightness,
        } => {
            let module = module_id_for_send(t, *kind).map_err(|e| e.to_string())?;
            let lim = limiters.get(kind).expect("limiter exists per kind");
            let e = last.entry(*kind).or_default();
            // only the changed mode/colour/brightness is sent, exactly like the BaseLed
            // branch above; a fully unchanged push is one deduped no-op
            if e.mode == Some(*mode) && e.rgb == Some(*rgb) && e.brightness == Some(*brightness) {
                inner.deduped.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            if e.mode != Some(*mode) {
                let r = ds_protocol::led_mode(*kind, LedTarget::All, *mode)
                    .ok_or("no led mode report")?;
                lim.acquire();
                t.send(&module, &r).map_err(|e| e.to_string())?;
                e.mode = Some(*mode);
                inner.sent.fetch_add(1, Ordering::Relaxed);
            }
            if e.rgb != Some(*rgb) {
                let r =
                    ds_protocol::led_color(*kind, LedTarget::All, *rgb).ok_or("no led report")?;
                lim.acquire();
                t.send(&module, &r).map_err(|e| e.to_string())?;
                e.rgb = Some(*rgb);
                inner.sent.fetch_add(1, Ordering::Relaxed);
            }
            if e.brightness != Some(*brightness) {
                let r = ds_protocol::led_brightness(*kind, LedTarget::All, *brightness)
                    .ok_or("no led report")?;
                lim.acquire();
                t.send(&module, &r).map_err(|e| e.to_string())?;
                e.brightness = Some(*brightness);
                inner.sent.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
        DeviceAction::Backlight { kind, percent } => {
            let module = module_id_for_send(t, *kind).map_err(|e| e.to_string())?;
            let lim = limiters.get(kind).expect("limiter exists per kind");
            let e = last.entry(*kind).or_default();
            if e.brightness == Some(*percent) {
                inner.deduped.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            let r = ds_protocol::backlight(*kind, *percent).ok_or("no backlight report")?;
            lim.acquire();
            t.send(&module, &r).map_err(|e| e.to_string())?;
            e.brightness = Some(*percent);
            inner.sent.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        DeviceAction::Image { .. } => Err("image actions do not take the LED lane".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DevicePush;
    use ds_protocol::Report;
    use mh_device::ModuleId;
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    struct MockTransport {
        sent: StdMutex<Vec<(ModuleId, Vec<u8>)>>,
    }

    impl Transport for MockTransport {
        fn send(&self, module: &ModuleId, report: &Report) -> ds_device::Result<usize> {
            let bytes = report.bytes().to_vec();
            self.sent
                .lock()
                .unwrap()
                .push((module.clone(), bytes.clone()));
            Ok(bytes.len())
        }
        fn query(
            &self,
            _module: &ModuleId,
            _feature_id: u8,
            _timeout: Duration,
        ) -> ds_device::Result<Vec<u8>> {
            Ok(vec![])
        }
        fn recv(&self, _timeout: Duration) -> ds_device::Result<ds_device::transport::DeviceEvent> {
            Err(ds_device::Error::Timeout(Duration::from_millis(1)))
        }
        fn close(&self) -> ds_device::Result<()> {
            Ok(())
        }
        fn module_id(&self, kind: ModuleKind) -> Option<ModuleId> {
            Some(ds_device::vtransport::virtual_module_id(kind))
        }
    }

    fn lane_with_mock() -> (LedLane, Arc<MockTransport>, Shutdown) {
        let shutdown = Shutdown::manual();
        let lane = LedLane::new(shutdown.clone());
        let mock = Arc::new(MockTransport {
            sent: StdMutex::new(Vec::new()),
        });
        let m2 = Arc::clone(&mock);
        lane.set_transport(Some(Arc::new(MockTransportArc(m2))));
        (lane, mock, shutdown)
    }

    struct MockTransportArc(Arc<MockTransport>);
    impl Transport for MockTransportArc {
        fn send(&self, module: &ModuleId, report: &Report) -> ds_device::Result<usize> {
            self.0.send(module, report)
        }
        fn query(&self, m: &ModuleId, f: u8, t: Duration) -> ds_device::Result<Vec<u8>> {
            self.0.query(m, f, t)
        }
        fn recv(&self, t: Duration) -> ds_device::Result<ds_device::transport::DeviceEvent> {
            self.0.recv(t)
        }
        fn close(&self) -> ds_device::Result<()> {
            self.0.close()
        }
        fn module_id(&self, kind: ModuleKind) -> Option<ModuleId> {
            Some(ds_device::vtransport::virtual_module_id(kind))
        }
    }

    fn wait_until(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        f()
    }

    #[test]
    fn latest_wins_replaces_pending_and_the_newest_is_sent() {
        let (lane, mock, shutdown) = lane_with_mock();
        let writer = lane.clone().spawn_writer();
        // three rapid colour commits for the same target: only the newest may be sent
        for hex in ["#010203", "#040506", "#070809"] {
            lane.submit(
                "led:knobs".into(),
                DevicePush::State {
                    path: "/controls/knobs/0/led".into(),
                    value: json!({"mode": "static", "color": hex, "brightness": 100}),
                },
            );
        }
        assert!(
            wait_until(Duration::from_secs(2), || lane.sent_count() >= 1),
            "the newest colour reached the wire"
        );
        std::thread::sleep(Duration::from_millis(150));
        let sent = mock.sent.lock().unwrap();
        let knob_reports: Vec<&Vec<u8>> = sent
            .iter()
            .filter(|(m, b)| m.kind == ModuleKind::Knobs && b[1] == 0x2D)
            .map(|(_, b)| b)
            .collect();
        assert_eq!(knob_reports.len(), 1, "exactly one colour report was sent");
        assert_eq!(
            &knob_reports[0][4..7],
            &[0x07, 0x08, 0x09],
            "it is the newest"
        );
        assert!(lane.superseded_count() >= 2, "replacements counted");
        drop(sent);
        shutdown.trigger();
        writer.join().unwrap();
    }

    #[test]
    fn base_led_colour_only_change_sends_one_report() {
        let (lane, mock, shutdown) = lane_with_mock();
        let writer = lane.clone().spawn_writer();
        let base = |hex: &str| {
            lane.submit(
                "base-led".into(),
                DevicePush::State {
                    path: "/controls/base/led".into(),
                    value: json!({"mode": "static", "color": hex, "brightness": 100}),
                },
            )
        };
        base("#010203"); // first write: mode + colour + brightness
        assert!(wait_until(Duration::from_secs(2), || {
            mock.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, _)| m.kind == ModuleKind::Base)
                .count()
                >= 3
        }));
        base("#040506"); // colour-only change: exactly one more report (the colour)
        assert!(wait_until(Duration::from_secs(2), || {
            mock.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, _)| m.kind == ModuleKind::Base)
                .count()
                >= 4
        }));
        std::thread::sleep(Duration::from_millis(150));
        let sent = mock.sent.lock().unwrap();
        let base_reports: Vec<&(ModuleId, Vec<u8>)> = sent
            .iter()
            .filter(|(m, _)| m.kind == ModuleKind::Base)
            .collect();
        assert_eq!(base_reports.len(), 4, "3 on first write + 1 colour-only");
        assert_eq!(base_reports[3].1[1], 0x2D, "the extra report is the colour");
        assert_eq!(&base_reports[3].1[4..7], &[0x04, 0x05, 0x06]);
        shutdown.trigger();
        writer.join().unwrap();
    }

    #[test]
    fn send_led_for_knobs_sends_mode_colour_brightness_then_only_changes() {
        let shutdown = Shutdown::manual();
        let lane = LedLane::new(shutdown);
        let mock = MockTransport {
            sent: StdMutex::new(Vec::new()),
        };
        let mut limiters: HashMap<ModuleKind, RateLimiter> = ModuleKind::ALL
            .into_iter()
            .map(|k| (k, limiter_for(k)))
            .collect();
        let mut last: HashMap<ModuleKind, LastSent> = HashMap::new();
        let push = |mode: &str, hex: &str, brightness: u64| {
            device::plan_push(
                "/controls/knobs/0/led",
                &json!({"mode": mode, "color": hex, "brightness": brightness}),
            )
            .expect("a knobs led push plans to an action")
        };
        // first write: mode + colour + brightness, in wire order, on the knobs module
        // (group 2, LedTarget::All = index 0xFF, Static = 1)
        let a = push("static", "#070809", 100);
        send_led(&mock, &a, &mut limiters, &mut last, &lane.inner).unwrap();
        {
            let sent = mock.sent.lock().unwrap();
            let knobs: Vec<&Vec<u8>> = sent
                .iter()
                .filter(|(m, _)| m.kind == ModuleKind::Knobs)
                .map(|(_, b)| b)
                .collect();
            assert_eq!(knobs.len(), 3, "mode + colour + brightness: {knobs:?}");
            assert_eq!(
                &knobs[0][..],
                &[0x02, 0x2C, 2, 0xFF, 0x01, 0x00],
                "02 2C static mode"
            );
            assert_eq!(
                &knobs[1][..],
                &[0x02, 0x2D, 2, 0xFF, 0x07, 0x08, 0x09, 0x00],
                "02 2D colour"
            );
            assert_eq!(
                &knobs[2][..],
                &[0x02, 0x2E, 2, 0xFF, 100],
                "02 2E brightness"
            );
        }
        // identical push: deduped, nothing sent
        send_led(&mock, &a, &mut limiters, &mut last, &lane.inner).unwrap();
        assert_eq!(
            mock.sent.lock().unwrap().len(),
            3,
            "an unchanged value sends nothing"
        );
        assert_eq!(lane.deduped_count(), 1, "the skip is counted as deduped");
        // brightness-only change: exactly one 02 2E report
        let a2 = push("static", "#070809", 55);
        send_led(&mock, &a2, &mut limiters, &mut last, &lane.inner).unwrap();
        {
            let sent = mock.sent.lock().unwrap();
            assert_eq!(sent.len(), 4, "only the changed brightness is sent");
            assert_eq!(&sent[3].1[..], &[0x02, 0x2E, 2, 0xFF, 55]);
        }
        // mode-only change (rainbow = 9): exactly one 02 2C report
        let a3 = push("rainbow", "#070809", 55);
        send_led(&mock, &a3, &mut limiters, &mut last, &lane.inner).unwrap();
        {
            let sent = mock.sent.lock().unwrap();
            assert_eq!(sent.len(), 5, "only the changed mode is sent");
            assert_eq!(&sent[4].1[..], &[0x02, 0x2C, 2, 0xFF, 0x09, 0x00]);
        }
    }

    #[test]
    fn rollers_led_is_a_no_op_and_not_an_error() {
        let (lane, mock, shutdown) = lane_with_mock();
        let writer = lane.clone().spawn_writer();
        lane.submit(
            "led:rollers".into(),
            DevicePush::State {
                path: "/controls/rollers/0/led".into(),
                value: json!({"mode": "static", "color": "#aabbcc", "brightness": 50}),
            },
        );
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            lane.sent_count(),
            0,
            "rollers have no LED command: nothing sent"
        );
        assert_eq!(lane.send_error_count(), 0, "and no error is raised");
        assert!(
            mock.sent.lock().unwrap().is_empty(),
            "no report left the host"
        );
        shutdown.trigger();
        writer.join().unwrap();
    }

    #[test]
    fn fader_limiter_is_capped_at_25_per_s() {
        assert_eq!(limiter_for(ModuleKind::Faders).rate_per_s(), 25.0);
        assert_eq!(limiter_for(ModuleKind::Base).rate_per_s(), 30.0);
        assert_eq!(limiter_for(ModuleKind::Knobs).rate_per_s(), 30.0);
    }
}

//! The resilient device task (DS-023): hot-plug rescan, reconnect, re-push of the current
//! target image, and unresponsive-device handling.
//!
//! This is the DS-015 device task grown a device lifecycle ([`ds_device::hotplug`]): it owns
//! the [`Transport`] and reacts to the device coming and going instead of giving up after
//! the first open:
//!
//! - **initial rescan/enumeration** — the task starts in `reconnecting` and scans until the
//!   device URI opens; on success it runs the DS-022 handshake of every module
//!   ([`hotplug::handshake_all`], bounded per module);
//! - **periodic/event-driven rescan** — while connected it rescans on a tick (≤ 1 s busy,
//!   device-driver invariant 8) and compares the enumerated module set (`hid:` URIs), so a
//!   hot-plugged module triggers a reconnect; a `Disconnected` event or a `Closed`/io error
//!   on any send/recv does the same immediately;
//! - **reconnect + re-push** — after a reconnect the task re-pushes the **current** image of
//!   every display target and the current LED colors from a latest-frame-wins cache (only
//!   the newest push per target is kept; replacing one is counted). A cached Panel image
//!   and cached Key images are mutually exclusive (DS-111): caching one drops the other
//!   kind, so a reconnect never replays stale key tiles over a newer bulk panel (or a
//!   stale panel over a newer key). Re-push is a pure transport operation: it reads no
//!   state document and never bumps `rev` (S1), the latest-frame-wins semantics live in
//!   the cache, mirroring the frame service's queue;
//! - **unresponsive devices** — every send/query is bounded (the transport's own write/query
//!   timeout plus a host-side deadline, [`hotplug::timed`]); on a timeout the module is
//!   marked unhealthy, logged, and no further sends go to it (drops counted), and the
//!   handshake is retried on the next rescan tick instead of hanging the task. A watchdog
//!   probes liveness with one bounded firmware read per tick (≤ 100 ms, roadmap phase-2).
//!
//! The control plane and the state task stay independent: pushes arrive over a bounded
//! channel (overflow drops and is counted by [`Handle::push`]), all device I/O happens on
//! this task's thread — no lock is ever held across it — and a failed push is logged and
//! dropped, never stalling the state task or the IPC.

use crate::device::{self, DeviceAction, DevicePush, ImageSource, ImageTarget};
use crate::frames::{self, FrameTarget};
use crate::lifecycle::{self, Shutdown};
use crate::router::{self, Bus};
use ds_device::error::{Error, Result as DevResult};
use ds_device::handshake::{HandshakeConfig, ModuleState};
use ds_device::hotplug::{self, IoFault, Lifecycle, Phase, RescanPolicy};
use ds_device::module::module_info;
use ds_device::rate::RateLimiter;
use ds_device::slot::DeviceKey;
use ds_device::transport::{DeviceEvent, Transport};
use ds_device::uri::DeviceUri;
use ds_protocol::{LedTarget, ModuleKind, Orientation};
use mh_device::ModuleId;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Capacity of the device-push channel (same bound as the DS-095 device task; overflow
/// drops and is counted, never blocks the state task).
pub const TASK_CHANNEL_CAP: usize = device::DEVICE_CHANNEL_CAP;

/// How long the task waits on the push channel before re-checking shutdown/lifecycle.
pub const POLL: Duration = Duration::from_millis(100);

/// Short transport poll while connected, so pushes arriving during a read are handled fast.
const TRANSPORT_POLL: Duration = Duration::from_millis(5);

/// Watchdog tick: the liveness check runs at most every tick and at least this often when
/// the device is idle (the roadmap's ≤ 100 ms liveness check).
pub const WATCHDOG_TICK: Duration = Duration::from_millis(100);

/// The device must show I/O at least this often, else the watchdog probes it.
pub const IDLE_BEFORE_PROBE: Duration = Duration::from_secs(1);

/// Host-side bound on one whole push (the transport bounds each single write; this bounds
/// the chunked transfer around it).
pub const SEND_BOUND: Duration = Duration::from_secs(5);

/// Bound of one liveness probe (firmware feature read).
pub const PROBE_TIMEOUT: Duration = hotplug::DEFAULT_PROBE_TIMEOUT;

/// Image-chunk write pacing (the device's ~2.1 MB/s cap, same as the DS-095 task).
pub const IMAGE_WRITE_RATE_PER_S: u32 = device::IMAGE_WRITE_RATE_PER_S;

/// Tunables of the device task; [`Default`] is the hardware-verified policy.
#[derive(Debug, Clone)]
pub struct TaskConfig {
    /// Handshake timings/bounds of every (re-)connect.
    pub handshake: HandshakeConfig,
    /// Rescan policy (down/busy/idle/backoff).
    pub policy: RescanPolicy,
    /// Consecutive bounded timeouts before the device is declared unresponsive.
    pub max_failures: u32,
    /// Host-side bound on one whole push.
    pub send_bound: Duration,
    /// Bound of one liveness probe.
    pub probe_timeout: Duration,
    /// Watchdog tick.
    pub watchdog_tick: Duration,
    /// Idle time before the watchdog probes.
    pub idle_before_probe: Duration,
    /// Push-channel wait while connected.
    pub poll: Duration,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            handshake: HandshakeConfig::default(),
            policy: RescanPolicy::default(),
            max_failures: hotplug::DEFAULT_MAX_FAILURES,
            send_bound: SEND_BOUND,
            probe_timeout: PROBE_TIMEOUT,
            watchdog_tick: WATCHDOG_TICK,
            idle_before_probe: IDLE_BEFORE_PROBE,
            poll: POLL,
        }
    }
}

/// Handle of the device task for producers (the state task): bounded, non-blocking.
pub struct Handle {
    tx: SyncSender<DevicePush>,
    /// Pushes dropped because the channel was full.
    pub dropped: Arc<AtomicU64>,
    /// Cached pushes superseded by a newer one for the same target (latest-frame-wins).
    pub superseded: Arc<AtomicU64>,
    /// Pushes not sent because their module is unhealthy.
    pub unhealthy_drops: Arc<AtomicU64>,
}

impl Handle {
    /// Queue one state change for the device. Returns false when the bounded channel was
    /// full (the push is dropped and counted — never blocks).
    pub fn push(&self, push: DevicePush) -> bool {
        match self.tx.try_send(push) {
            Ok(()) => true,
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// A clone of the raw sender (the state task holds one).
    pub fn sender(&self) -> SyncSender<DevicePush> {
        self.tx.clone()
    }

    /// Pushes dropped so far (channel full).
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Pushes superseded in the re-push cache so far.
    pub fn superseded_count(&self) -> u64 {
        self.superseded.load(Ordering::Relaxed)
    }

    /// Pushes not sent because their module was unhealthy.
    pub fn unhealthy_drop_count(&self) -> u64 {
        self.unhealthy_drops.load(Ordering::Relaxed)
    }
}

/// Human-readable device status for `daemon status` (same type as the DS-095 task).
pub type Status = device::DeviceStatus;

/// Open the transport for a device URI (when given) and run the resilient device loop on a
/// new thread. Same shape as `device::spawn`, so the daemon can swap the two with a
/// one-line change.
///
/// `device` is `None` for `--device none` (the task idles, accepting and dropping pushes,
/// counted). `bus` is the event bus: input reports and connect/disconnect are published on
/// it.
pub fn spawn(
    device: Option<DeviceUri>,
    shutdown: Shutdown,
    bus: Bus,
) -> (Handle, JoinHandle<()>, Status) {
    spawn_with(device, shutdown, bus, TaskConfig::default(), None)
}

/// A transport factory: one rescan attempt's `open` (the real one dispatches on the URI;
/// tests inject fakes that disappear or stop answering).
pub type Opener = Box<dyn Fn() -> DevResult<Box<dyn Transport>> + Send>;

/// Testable entry point: `open` overrides the transport factory (unit tests inject fake
/// transports that disappear or stop answering).
pub fn spawn_with(
    device: Option<DeviceUri>,
    shutdown: Shutdown,
    bus: Bus,
    cfg: TaskConfig,
    open: Option<Opener>,
) -> (Handle, JoinHandle<()>, Status) {
    let (tx, rx) = std::sync::mpsc::sync_channel(TASK_CHANNEL_CAP);
    let dropped = Arc::new(AtomicU64::new(0));
    let superseded = Arc::new(AtomicU64::new(0));
    let unhealthy_drops = Arc::new(AtomicU64::new(0));
    let handle = Handle {
        tx,
        dropped: Arc::clone(&dropped),
        superseded: Arc::clone(&superseded),
        unhealthy_drops: Arc::clone(&unhealthy_drops),
    };
    let status: Status = Arc::new(Mutex::new(None));
    let status2 = Arc::clone(&status);
    let opener = open.unwrap_or_else(|| {
        let uri = device.clone();
        Box::new(move || match &uri {
            Some(uri) => ds_device::open(uri),
            None => Err(Error::Device("no device URI".into())),
        })
    });
    let thread = std::thread::Builder::new()
        .name("ds-device-task".into())
        .spawn(move || {
            let max_failures = cfg.max_failures;
            Task {
                uri: device,
                open: opener,
                cfg,
                rx,
                shutdown,
                bus,
                status: status2,
                lifecycle: Lifecycle::new(max_failures),
                transport: None,
                modules: Vec::new(),
                keys: Vec::new(),
                orientation: Orientation::Horizontal,
                unhealthy: HashSet::new(),
                latest: HashMap::new(),
                led: RateLimiter::new(ds_device::rate::DEFAULT_LED_RATE_PER_S),
                image: RateLimiter::new(IMAGE_WRITE_RATE_PER_S),
                decoder: router::InputDecoder::new(),
                last_io: Instant::now(),
                next_rescan: Instant::now(),
                next_probe: Instant::now(),
                rescan_failures: 0,
                superseded: Arc::clone(&superseded),
                unhealthy_drops: Arc::clone(&unhealthy_drops),
            }
            .run()
        })
        .expect("spawn ds-device-task thread");
    (handle, thread, status)
}

/// Panel and key image actions are mutually exclusive in the re-push cache (DS-111):
/// caching a Panel image drops every cached `Key(_)` image and caching a Key image drops
/// a cached Panel, so a reconnect can never replay stale key tiles over a newer bulk
/// panel (or a stale panel over a newer key). The dial is a separate screen and is not
/// affected. Returns how many cached actions were dropped.
fn drop_excluded_images(
    latest: &mut HashMap<String, DeviceAction>,
    action: &DeviceAction,
) -> usize {
    let DeviceAction::Image { target, .. } = action else {
        return 0;
    };
    let excluded: fn(&str) -> bool = match target {
        ImageTarget::Panel => |k| k.starts_with("image:Key("),
        ImageTarget::Key(_) => |k| k == "image:Panel",
        ImageTarget::Dial => return 0,
    };
    let before = latest.len();
    latest.retain(|k, _| !excluded(k));
    before - latest.len()
}

/// The per-target re-push cache key of an action (latest-frame-wins per target).
fn cache_key(action: &DeviceAction) -> String {
    match action {
        DeviceAction::Image { target, .. } => format!("image:{target:?}"),
        DeviceAction::LedColor { kind, .. } => format!("led:{kind:?}"),
        DeviceAction::Backlight { kind, .. } => format!("backlight:{kind:?}"),
        DeviceAction::BaseLed { .. } => "base-led".to_string(),
    }
}

/// The module kind an action writes to.
fn action_kind(action: &DeviceAction) -> ModuleKind {
    match action {
        DeviceAction::Image { target, .. } => target.module(),
        DeviceAction::LedColor { kind, .. } => *kind,
        DeviceAction::Backlight { kind, .. } => *kind,
        DeviceAction::BaseLed { .. } => ModuleKind::Base,
    }
}

/// Encode + chunk + send one action to `module` through `t`, paced by the rate limits.
/// Typed errors (not strings) so the lifecycle can classify timeouts vs gone.
fn send_action_typed(
    t: &dyn Transport,
    module: &ModuleId,
    action: &DeviceAction,
    led: &RateLimiter,
    image: &RateLimiter,
) -> DevResult<()> {
    match action {
        DeviceAction::Backlight { kind, percent } => {
            let report = ds_protocol::backlight(*kind, *percent)
                .ok_or_else(|| Error::Device("no backlight report".into()))?;
            led.acquire();
            t.send(module, &report).map(|_| ())
        }
        DeviceAction::BaseLed {
            mode,
            rgb,
            brightness,
        } => {
            for report in [
                ds_protocol::led_mode(ModuleKind::Base, LedTarget::All, *mode),
                ds_protocol::led_color(ModuleKind::Base, LedTarget::All, *rgb),
                ds_protocol::led_brightness(ModuleKind::Base, LedTarget::All, *brightness),
            ]
            .into_iter()
            .flatten()
            {
                led.acquire();
                t.send(module, &report)?;
            }
            Ok(())
        }
        DeviceAction::LedColor {
            kind,
            mode,
            rgb,
            brightness,
        } => {
            for report in [
                ds_protocol::led_mode(*kind, LedTarget::All, *mode),
                ds_protocol::led_color(*kind, LedTarget::All, *rgb),
                ds_protocol::led_brightness(*kind, LedTarget::All, *brightness),
            ]
            .into_iter()
            .flatten()
            {
                led.acquire();
                t.send(module, &report)?;
            }
            Ok(())
        }
        DeviceAction::Image { target, source } => {
            let ft: FrameTarget = target.frame_target();
            let jpeg = match source {
                ImageSource::Color { rgb } => frames::encode::encode_solid(ft, *rgb),
                ImageSource::Colors(colors) => match target {
                    // panel-only bulk visual (DS-111): compose the 480x272 panel and push
                    // it as one 0x35; the schema rejects it on keys/dial, this is the
                    // daemon-side backstop
                    ImageTarget::Panel => frames::encode::encode_panel_colors(colors),
                    other => {
                        return Err(Error::Device(format!(
                            "the kind:\"colors\" visual is panel-only, not {other:?}"
                        )))
                    }
                },
                ImageSource::File { path } => {
                    let wire = device::read_image_file(path)
                        .map_err(|e| Error::Device(format!("E_FORMAT: {e}")))?;
                    frames::encode::encode_frame(ft, &wire)
                        .map_err(|e| Error::Device(format!("{}: {e}", e.code())))?
                }
            };
            let out_len = module_info(target.module()).out_len;
            let reports = frames::chunk::reports(ft, &jpeg, out_len);
            if reports.is_empty() {
                return Err(Error::Device(format!(
                    "image chunker produced no reports for {target:?}"
                )));
            }
            for report in &reports {
                image.acquire();
                t.send(module, report)?;
            }
            Ok(())
        }
    }
}

struct Task {
    uri: Option<DeviceUri>,
    open: Box<dyn Fn() -> DevResult<Box<dyn Transport>> + Send>,
    cfg: TaskConfig,
    rx: Receiver<DevicePush>,
    shutdown: Shutdown,
    bus: Bus,
    status: Status,
    lifecycle: Lifecycle,
    transport: Option<Box<dyn Transport>>,
    /// Handshaken modules of the live connection.
    modules: Vec<ModuleState>,
    /// DS-112: the (slot, orientation) identity keys of the live connection, recorded at
    /// connect — the rescan comparison is keyed by position, not by serial alone.
    keys: Vec<(ModuleId, DeviceKey)>,
    /// DS-112: the current base orientation (part of every module's identity key); updated
    /// from base orientation input reports.
    orientation: Orientation,
    /// Modules currently unhealthy (no sends to them; handshake retried on rescan).
    unhealthy: HashSet<ModuleId>,
    /// Latest push per target — the re-push source after a reconnect (latest-frame-wins).
    latest: HashMap<String, DeviceAction>,
    led: RateLimiter,
    image: RateLimiter,
    decoder: router::InputDecoder,
    last_io: Instant,
    next_rescan: Instant,
    next_probe: Instant,
    rescan_failures: u32,
    superseded: Arc<AtomicU64>,
    unhealthy_drops: Arc<AtomicU64>,
}

impl Task {
    fn log(&self, level: &str, msg: &str) {
        lifecycle::log(level, "ds-daemon::device_task", msg);
    }

    fn set_status(&self, s: String) {
        if let Ok(mut g) = self.status.lock() {
            *g = Some(s);
        }
    }

    fn device_serial(&self) -> String {
        // the transport owns module identity: prefer the serial of its keys module
        // (hid: -> the enumerated serial); the virtual id is only the no-transport fallback
        self.transport
            .as_ref()
            .and_then(|t| t.module_id(ModuleKind::Keys))
            .map(|m| m.serial)
            .unwrap_or_else(|| ds_device::vtransport::virtual_module_id(ModuleKind::Keys).serial)
    }

    fn run(mut self) {
        if self.uri.is_none() {
            self.set_status("none".to_string());
            self.log("info", "no device (--device none)");
            // Accept and drop pushes (counted) until shutdown — the control plane and the
            // state task stay fully independent of the missing hardware.
            while !self.shutdown.get() {
                match self.rx.recv_timeout(self.cfg.poll) {
                    Ok(_) => {
                        self.superseded.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            self.log("info", "device task stopped (no device)");
            return;
        }
        self.log(
            "info",
            &format!(
                "starting hot-plug device task for {}",
                self.uri.as_ref().unwrap()
            ),
        );
        loop {
            if self.shutdown.get() {
                break;
            }
            let now = Instant::now();
            match self.lifecycle.phase() {
                Phase::Reconnecting => {
                    if now >= self.next_rescan {
                        self.try_connect();
                    }
                    // Keep draining the push channel so nothing is lost while the device is
                    // down (the channel stays bounded); once a connect succeeded in this
                    // iteration, pushes go straight through again.
                    match self.rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(push) => {
                            if self.lifecycle.accepts_sends() {
                                self.handle_push(&push);
                                while let Ok(push) = self.rx.try_recv() {
                                    self.handle_push(&push);
                                }
                            } else {
                                self.cache_push(&push);
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                Phase::Unresponsive => {
                    // Retry the handshake of the unhealthy modules on every rescan tick
                    // (bounded), never hang; a gone transport drops to reconnecting.
                    if now >= self.next_rescan {
                        self.retry_unhealthy();
                    }
                    match self.rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(push) => self.cache_push(&push),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                Phase::Detected | Phase::Handshaken | Phase::Healthy => {
                    self.service_connected();
                }
            }
        }
        if let Some(t) = self.transport.take() {
            let _ = t.close();
        }
        self.log("info", "device task stopped");
    }

    /// One rescan cycle: open + handshake all modules; on success install the transport,
    /// publish the connect event and re-push the current targets.
    fn try_connect(&mut self) {
        let uri = self.uri.clone().expect("uri checked by run()");
        match (self.open)() {
            Ok(t) => {
                self.lifecycle.detect();
                // The transport owns module identity: handshake exactly the modules it has
                // open (hid: -> the enumerated ids, virtual: -> the virtual ids), never a
                // hardcoded id.
                let ids: Vec<ModuleId> = ModuleKind::ALL
                    .into_iter()
                    .filter_map(|kind| t.module_id(kind))
                    .collect();
                let report = hotplug::handshake_all(&*t, &ids, self.cfg.handshake);
                if report.ok.is_empty() && !ids.is_empty() {
                    // every module failed its handshake: unresponsive, not gone (unless a
                    // fault says the transport died — then drop it and rescan)
                    let gone = report.unresponsive.iter().any(|(_, f)| *f == IoFault::Gone);
                    if gone {
                        let _ = t.close();
                        self.lifecycle.io_fault(IoFault::Gone);
                        self.set_status("reconnecting (transport gone during handshake)".into());
                        self.rescan_failures += 1;
                        self.next_rescan =
                            Instant::now() + self.cfg.policy.backoff(self.rescan_failures);
                        return;
                    }
                    self.lifecycle.unresponsive();
                    self.set_status("unresponsive (no module completed the handshake)".into());
                    self.next_rescan =
                        Instant::now() + self.cfg.policy.backoff(self.rescan_failures);
                    let _ = t.close();
                    return;
                }
                // DS-112: record the (slot, orientation) identity keys of the live connection
                // before the transport is stored (the rescan compares against these).
                self.keys = hotplug::identity_keys(&*t, self.orientation);
                self.transport = Some(t);
                self.unhealthy = report
                    .unresponsive
                    .iter()
                    .map(|(id, _)| id.clone())
                    .collect();
                self.modules = report.ok;
                let phase = if self.unhealthy.is_empty() {
                    self.lifecycle.handshaken();
                    self.lifecycle.io_ok()
                } else {
                    self.lifecycle.handshaken()
                };
                let fw: Vec<String> = self
                    .modules
                    .iter()
                    .map(|m| {
                        format!(
                            "{} fw={}",
                            m.id.kind.name(),
                            m.firmware.as_deref().unwrap_or("?")
                        )
                    })
                    .collect();
                self.set_status(format!("connected {uri} ({})", phase.name()));
                self.log(
                    "info",
                    &format!(
                        "connected to {uri}: {} module(s) handshaken [{}], {} unhealthy",
                        self.modules.len(),
                        fw.join(", "),
                        self.unhealthy.len()
                    ),
                );
                self.bus.publish(router::device_event(
                    &self.device_serial(),
                    "connected",
                    serde_json::json!({"device": uri.to_string()}),
                ));
                self.last_io = Instant::now();
                self.rescan_failures = 0;
                self.next_rescan = Instant::now() + self.cfg.policy.busy;
                self.next_probe = Instant::now() + self.cfg.watchdog_tick;
                // Re-push the current target images / LED colors after the (re)connect.
                self.repush();
            }
            Err(e) => {
                self.rescan_failures = self.rescan_failures.saturating_add(1);
                self.next_rescan = Instant::now() + self.cfg.policy.backoff(self.rescan_failures);
                self.set_status(format!("reconnecting: {e}"));
                self.log(
                    "warn",
                    &format!(
                        "rescan failed ({e}); next attempt in {:?}",
                        self.cfg.policy.backoff(self.rescan_failures)
                    ),
                );
            }
        }
    }

    /// Re-handshake the unhealthy modules on the current transport (bounded per module).
    /// With no transport left, go back to a full rescan.
    fn retry_unhealthy(&mut self) {
        let Some(t) = &self.transport else {
            self.lifecycle.on_rescan();
            self.next_rescan = Instant::now() + self.cfg.policy.backoff(self.rescan_failures);
            return;
        };
        let ids: Vec<ModuleId> = self.unhealthy.iter().cloned().collect();
        if ids.is_empty() {
            self.lifecycle.io_ok();
            self.next_rescan = Instant::now() + self.cfg.policy.busy;
            return;
        }
        let report = hotplug::handshake_all(&**t, &ids, self.cfg.handshake);
        let gone = report.unresponsive.iter().any(|(_, f)| *f == IoFault::Gone);
        for st in &report.ok {
            self.unhealthy.remove(&st.id);
            self.log(
                "info",
                &format!(
                    "module {} recovered on rescan (fw={})",
                    st.id,
                    st.firmware.as_deref().unwrap_or("?")
                ),
            );
        }
        for (id, fault) in &report.unresponsive {
            self.log(
                "warn",
                &format!(
                    "module {} still unresponsive ({:?}); retry on next rescan",
                    id, fault
                ),
            );
        }
        if gone {
            self.on_gone("transport gone while retrying the handshake");
            return;
        }
        if self.unhealthy.is_empty() {
            self.lifecycle.io_ok();
            self.set_status(format!("connected (recovered, {})", Phase::Healthy.name()));
            self.log("info", "all modules healthy again after rescan");
            self.next_rescan = Instant::now() + self.cfg.policy.busy;
            // the device may have lost state while unresponsive: re-push the current targets
            self.repush();
        } else {
            self.set_status(format!(
                "unresponsive: {} module(s) down",
                self.unhealthy.len()
            ));
            self.next_rescan = Instant::now() + self.cfg.policy.backoff(self.rescan_failures);
        }
    }

    /// The connected loop: pushes (hot path), device events, watchdog, periodic rescan.
    fn service_connected(&mut self) {
        // 1. pushes
        match self.rx.recv_timeout(self.cfg.poll) {
            Ok(push) => {
                self.handle_push(&push);
                while let Ok(push) = self.rx.try_recv() {
                    self.handle_push(&push);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.shutdown.trigger();
                return;
            }
        }
        // 2. device events
        if let Some(t) = &self.transport {
            let r = t.recv(TRANSPORT_POLL);
            let phase = self.lifecycle.on_recv(&r);
            match r {
                Ok(DeviceEvent::Input { module, input }) => {
                    let events = self.decoder.decode(&module, &input);
                    let n = events.len();
                    for ev in events {
                        self.bus.publish(ev);
                    }
                    // DS-112: the base orientation is part of the identity key; a rotation
                    // re-keys every module and re-pushes the cached targets (idempotent).
                    if let ds_protocol::Input::Orientation(o) = input {
                        if o != self.orientation {
                            self.orientation = o;
                            self.log(
                                "info",
                                &format!(
                                    "base orientation -> {}; re-keying by (slot, orientation)",
                                    ds_device::orientation_name(o)
                                ),
                            );
                            if let Some(t) = &self.transport {
                                self.keys = hotplug::identity_keys(&**t, self.orientation);
                            }
                            self.repush();
                        }
                    }
                    self.last_io = Instant::now();
                    self.log(
                        "info",
                        &format!(
                            "input from {:?} ({}): {:?} -> {n} bus event(s) [{}]",
                            module.kind,
                            module.serial,
                            input,
                            phase.name()
                        ),
                    );
                }
                Ok(DeviceEvent::Disconnected) => {
                    self.on_gone("device reported disconnect");
                    return;
                }
                Err(Error::Timeout(_)) => {}
                Err(e) => {
                    if hotplug::classify(&e) == IoFault::Gone {
                        self.on_gone(&format!("transport error: {e}"));
                        return;
                    }
                    self.log("warn", &format!("device error: {e}"));
                }
            }
        }
        // 3. watchdog: bounded liveness probe when the device has been idle
        let now = Instant::now();
        if now >= self.next_probe {
            self.next_probe = now + self.cfg.watchdog_tick;
            if now.duration_since(self.last_io) > self.cfg.idle_before_probe {
                self.liveness_probe();
                if self.lifecycle.phase() == Phase::Reconnecting {
                    return; // the probe found the transport gone
                }
            }
        }
        // 4. periodic rescan for hot-plugged modules (hid enumerates; virtual is one socket)
        if now >= self.next_rescan {
            self.periodic_rescan();
            self.next_rescan = Instant::now() + self.cfg.policy.busy;
        }
    }

    /// One bounded liveness probe on a handshaken module (≤ 100 ms firmware read).
    fn liveness_probe(&mut self) {
        let Some(t) = &self.transport else {
            return;
        };
        let Some(state) = self.modules.first() else {
            return;
        };
        match hotplug::timed(self.cfg.probe_timeout, || {
            hotplug::probe(&**t, state, self.cfg.probe_timeout)
        }) {
            Ok(_) => {
                self.lifecycle.io_ok();
                self.last_io = Instant::now();
            }
            Err(e) => match hotplug::classify(&e) {
                IoFault::Gone => self.on_gone(&format!("liveness probe: {e}")),
                fault => {
                    let phase = self.lifecycle.io_fault(fault);
                    self.log(
                        "warn",
                        &format!("liveness probe timed out ({e}); phase={}", phase.name()),
                    );
                    if phase == Phase::Unresponsive {
                        self.set_status("unresponsive (liveness probe)".into());
                        for m in &self.modules {
                            self.unhealthy.insert(m.id.clone());
                        }
                        self.next_rescan =
                            Instant::now() + self.cfg.policy.backoff(self.rescan_failures);
                    }
                }
            },
        }
    }

    /// Periodic rescan while connected. DS-112: the identity key is `(slot, orientation)`
    /// in addition to the serial, so the rescan compares the live transport's placement keys
    /// with the keys recorded at connect — a module moved to another connector (or a base
    /// rotation, which re-keys without any USB event) triggers a reconnect, which
    /// re-handshakes and re-pushes keyed by position. For `hid:` URIs the enumerated module
    /// set (serials + slots from sysfs) is compared too — a hot-plugged or unplugged module.
    fn periodic_rescan(&mut self) {
        let scheme = self.uri.as_ref().map(|u| u.scheme().to_string());
        let Some(scheme) = scheme else {
            return;
        };
        // DS-112: re-read the base orientation (feature 0x35) — the tilt switch heartbeats
        // ~2 reports/s on hardware; polling also recovers a broadcast lost in a reconnect
        // window, and a rotation re-keys every module's position.
        let polled = self.transport.as_ref().and_then(|t| {
            let base = t.module_id(ModuleKind::Base)?;
            let ans = t.query(&base, 0x35, Duration::from_millis(500)).ok()?;
            let o = ans.get(1..).and_then(ds_protocol::parse_orientation)?;
            Some((base.serial, o))
        });
        if let Some((serial, o)) = polled {
            if o != self.orientation {
                self.orientation = o;
                self.log(
                    "info",
                    &format!(
                        "base orientation -> {}; re-keying by (slot, orientation)",
                        ds_device::orientation_name(o)
                    ),
                );
                self.bus.publish(router::device_event(
                    &serial,
                    "orientation",
                    serde_json::json!({"orientation": ds_device::orientation_name(o)}),
                ));
                if let Some(t) = &self.transport {
                    self.keys = hotplug::identity_keys(&**t, self.orientation);
                }
                self.repush();
            }
        }
        let changes = self
            .transport
            .as_ref()
            .map(|t| hotplug::placement_changes(&**t, self.orientation, &self.keys))
            .unwrap_or_default();
        if !changes.is_empty() {
            let desc: Vec<String> = changes
                .iter()
                .map(|(id, key)| format!("{}:{}", id.kind.name(), key))
                .collect();
            self.log(
                "warn",
                &format!(
                    "rescan: placement changed ({}); reconnecting keyed by position",
                    desc.join(", ")
                ),
            );
            self.on_gone("rescan detected a changed (slot, orientation) placement");
            return;
        }
        if scheme != "hid" {
            return;
        }
        match ds_device::hid::enumerate() {
            Ok(list) => {
                let mut now: HashSet<ModuleId> = list.iter().map(|m| m.module_id.clone()).collect();
                let current: HashSet<ModuleId> = self
                    .modules
                    .iter()
                    .map(|m| m.id.clone())
                    .chain(self.unhealthy.iter().cloned())
                    .collect();
                now.retain(|id| current.contains(id));
                if now.len() != current.len() || list.len() != current.len() {
                    self.log(
                        "warn",
                        &format!(
                            "rescan: deck changed ({} enumerated vs {} open); reconnecting",
                            list.len(),
                            current.len()
                        ),
                    );
                    self.on_gone("rescan detected a changed module set");
                }
            }
            Err(e) => self.log("warn", &format!("rescan enumeration failed: {e}")),
        }
    }

    /// Handle one push: cache it (latest-frame-wins) and send it when the device is ready.
    fn handle_push(&mut self, push: &DevicePush) {
        let DevicePush::State { path, value } = push;
        let Some(action) = device::plan_push(path, value) else {
            self.log("debug", &format!("out of scope, ignored: {path} = {value}"));
            return;
        };
        self.cache_action(&action);
        if !self.lifecycle.accepts_sends() {
            self.log(
                "debug",
                &format!(
                    "device not ready ({}); cached {path} for re-push",
                    self.lifecycle.phase().name()
                ),
            );
            return;
        }
        self.send_action(&action, path);
    }

    /// Cache a push as the current value of its target (replacing = superseded, counted).
    fn cache_push(&mut self, push: &DevicePush) {
        let DevicePush::State { path, value } = push;
        if let Some(action) = device::plan_push(path, value) {
            self.cache_action(&action);
        }
    }

    fn cache_action(&mut self, action: &DeviceAction) {
        // DS-111: a panel image and key images are mutually exclusive in the cache
        let dropped = drop_excluded_images(&mut self.latest, action);
        if dropped > 0 {
            self.superseded.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        let key = cache_key(action);
        if self.latest.insert(key, action.clone()).is_some() {
            self.superseded.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Send one cached action to its module, with the bounded send deadline and fault
    /// handling (timeout -> module unhealthy; gone -> reconnect).
    fn send_action(&mut self, action: &DeviceAction, path: &str) {
        let kind = action_kind(action);
        let module = self
            .modules
            .iter()
            .find(|m| m.id.kind == kind)
            .map(|m| m.id.clone());
        let Some(module) = module else {
            self.log(
                "debug",
                &format!("no handshaken module for {kind:?}; dropped push {path}"),
            );
            self.unhealthy_drops.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if self.unhealthy.contains(&module) {
            self.unhealthy_drops.fetch_add(1, Ordering::Relaxed);
            self.log(
                "warn",
                &format!("module {module} is unhealthy; not sending {path} (counted)"),
            );
            return;
        }
        let Some(t) = &self.transport else {
            return;
        };
        let start = Instant::now();
        let r = hotplug::timed(self.cfg.send_bound, || {
            send_action_typed(&**t, &module, action, &self.led, &self.image)
        });
        match r {
            Ok(()) => {
                self.lifecycle.io_ok();
                self.last_io = Instant::now();
                self.log("debug", &format!("pushed {action:?} for {path}"));
            }
            Err(e) => {
                let fault = hotplug::classify(&e);
                match fault {
                    IoFault::Gone => self.on_gone(&format!("push {path} failed: {e}")),
                    IoFault::Timeout => {
                        self.unhealthy.insert(module.clone());
                        let phase = self.lifecycle.io_fault(IoFault::Timeout);
                        self.log(
                            "error",
                            &format!(
                                "push {path} to {module} timed out after {:?} (bound {:?}); module marked unhealthy, phase={}",
                                start.elapsed(),
                                self.cfg.send_bound,
                                phase.name()
                            ),
                        );
                        self.set_status(format!(
                            "unresponsive: {module} ({} failures)",
                            self.lifecycle.failures()
                        ));
                        if phase == Phase::Unresponsive {
                            self.next_rescan =
                                Instant::now() + self.cfg.policy.backoff(self.rescan_failures);
                        }
                    }
                    IoFault::Other => {
                        let phase = self.lifecycle.io_fault(IoFault::Other);
                        self.log(
                            "error",
                            &format!("push {path} failed: {e} (phase={})", phase.name()),
                        );
                    }
                }
            }
        }
    }

    /// Re-push every cached target (current image / LED color) after a (re)connect.
    /// Latest-frame-wins: only the newest value per target exists in the cache. This is a
    /// pure transport operation — no state read, no `rev` bump (S1).
    fn repush(&mut self) {
        if self.latest.is_empty() {
            return;
        }
        let mut keys: Vec<&String> = self.latest.keys().collect();
        keys.sort();
        self.log(
            "info",
            &format!("re-pushing {} cached target(s) after connect", keys.len()),
        );
        let actions: Vec<(String, DeviceAction)> = keys
            .into_iter()
            .map(|k| (k.clone(), self.latest[k].clone()))
            .collect();
        for (key, action) in actions {
            if !self.lifecycle.accepts_sends() {
                self.log(
                    "debug",
                    &format!(
                        "re-push of {key} skipped (phase {})",
                        self.lifecycle.phase().name()
                    ),
                );
                continue;
            }
            let kind = action_kind(&action);
            let unhealthy = self
                .modules
                .iter()
                .find(|m| m.id.kind == kind)
                .map(|m| self.unhealthy.contains(&m.id))
                .unwrap_or(true);
            if unhealthy {
                self.unhealthy_drops.fetch_add(1, Ordering::Relaxed);
                self.log(
                    "warn",
                    &format!("re-push of {key} skipped: module {kind:?} unhealthy"),
                );
                continue;
            }
            self.send_action(&action, &format!("re-push {key}"));
            if self.lifecycle.phase() == Phase::Reconnecting {
                return; // the re-push found the transport gone; the rescan loop takes over
            }
        }
    }

    /// The transport is gone: close it, publish the event, and go rescan (replug ≤ 5 s).
    fn on_gone(&mut self, reason: &str) {
        self.log(
            "warn",
            &format!("device gone: {reason}; closing and rescanning"),
        );
        if let Some(t) = self.transport.take() {
            let _ = t.close();
        }
        self.modules.clear();
        self.unhealthy.clear();
        self.lifecycle.io_fault(IoFault::Gone);
        self.set_status("disconnected".to_string());
        self.bus.publish(router::device_event(
            &self.device_serial(),
            "disconnected",
            serde_json::json!({"reason": reason}),
        ));
        // fast rescan first — the replug budget is ≤ 5 s end to end
        self.next_rescan = Instant::now() + self.cfg.policy.down;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::ImageTarget;
    use ds_device::vtransport::virtual_module_id;
    use ds_protocol::{LedMode, Report};
    use ds_vdev::{HubConfig, Target as VdevTarget, VdevServer};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    // ---------------------------------------------------------------- fake transport

    struct FakeTransport {
        sent: StdMutex<Vec<(ModuleId, Vec<u8>)>>,
        send_err: StdMutex<Option<Error>>,
        query_err: StdMutex<Option<Error>>,
        events: StdMutex<VecDeque<DevResult<DeviceEvent>>>,
        closed: StdMutex<bool>,
    }

    impl FakeTransport {
        fn new() -> Arc<Self> {
            Arc::new(FakeTransport {
                sent: StdMutex::new(Vec::new()),
                send_err: StdMutex::new(None),
                query_err: StdMutex::new(None),
                events: StdMutex::new(VecDeque::new()),
                closed: StdMutex::new(false),
            })
        }

        fn fail_sends(&self, e: Error) {
            *self.send_err.lock().unwrap() = Some(e);
        }

        fn fail_queries(&self, e: Error) {
            *self.query_err.lock().unwrap() = Some(e);
        }

        fn clear_faults(&self) {
            *self.send_err.lock().unwrap() = None;
            *self.query_err.lock().unwrap() = None;
        }

        fn disconnect(&self) {
            self.events
                .lock()
                .unwrap()
                .push_back(Ok(DeviceEvent::Disconnected));
        }

        /// The image reports (0x02 0x36 …) sent to the keys module, in order.
        fn key_image_reports(&self) -> Vec<Vec<u8>> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, b)| {
                    m.kind == ModuleKind::Keys && b.len() > 2 && b[0] == 0x02 && b[1] == 0x36
                })
                .map(|(_, b)| b.clone())
                .collect()
        }

        fn send_count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
    }

    fn clone_err(e: &Error) -> Error {
        match e {
            Error::Timeout(d) => Error::Timeout(*d),
            Error::Closed => Error::Closed,
            Error::Io(io) => Error::Io(std::io::Error::new(io.kind(), io.to_string())),
            other => Error::Device(other.to_string()),
        }
    }

    impl Transport for FakeTransport {
        fn send(&self, module: &ModuleId, report: &Report) -> DevResult<usize> {
            if let Some(e) = self.send_err.lock().unwrap().as_ref() {
                return Err(clone_err(e));
            }
            let bytes = report.bytes().to_vec();
            self.sent
                .lock()
                .unwrap()
                .push((module.clone(), bytes.clone()));
            Ok(bytes.len())
        }

        fn query(&self, _module: &ModuleId, _fid: u8, _t: Duration) -> DevResult<Vec<u8>> {
            if let Some(e) = self.query_err.lock().unwrap().as_ref() {
                return Err(clone_err(e));
            }
            // firmware answer shaped like the hardware/vdev one
            let mut payload = vec![0x29, 0x63, 0x1C, 0x5D];
            payload.extend_from_slice(b"01.00.00.00");
            let mut ans = vec![0x03, 0x38, payload.len() as u8];
            ans.extend_from_slice(&payload);
            Ok(ans)
        }

        fn recv(&self, _t: Duration) -> DevResult<DeviceEvent> {
            match self.events.lock().unwrap().pop_front() {
                Some(e) => e,
                None => Err(Error::Timeout(Duration::from_millis(1))),
            }
        }

        fn close(&self) -> DevResult<()> {
            *self.closed.lock().unwrap() = true;
            Ok(())
        }
    }

    fn fast_cfg() -> TaskConfig {
        TaskConfig {
            handshake: HandshakeConfig {
                settle: Duration::from_millis(1),
                re_settle: Duration::from_millis(1),
                retry_gap: Duration::from_millis(1),
                attempts: 2,
                query_timeout: Duration::from_millis(50),
            },
            policy: RescanPolicy {
                down: Duration::from_millis(50),
                busy: Duration::from_millis(100),
                idle: Duration::from_secs(15),
                max_backoff: Duration::from_millis(500),
            },
            max_failures: 2,
            send_bound: Duration::from_secs(2),
            probe_timeout: Duration::from_millis(50),
            watchdog_tick: Duration::from_millis(20),
            idle_before_probe: Duration::from_millis(80),
            poll: Duration::from_millis(20),
        }
    }

    fn push_color(handle: &Handle, path: &str, hex: &str) -> bool {
        handle.push(DevicePush::State {
            path: path.into(),
            value: json!({"kind": "color", "value": hex}),
        })
    }

    fn expected_key_image_reports(rgb: [u8; 3]) -> Vec<Vec<u8>> {
        let ft = ImageTarget::Key(0).frame_target();
        let jpeg = frames::encode::encode_solid(ft, rgb);
        frames::chunk::reports(ft, &jpeg, module_info(ModuleKind::Keys).out_len)
            .iter()
            .map(|r| r.bytes().to_vec())
            .collect()
    }

    /// The last `n` image reports (0x02 0x36 …) accepted for the keys module.
    fn last_key_image_reports(fake: &FakeTransport, n: usize) -> Vec<Vec<u8>> {
        let all = fake.key_image_reports();
        all[all.len().saturating_sub(n)..].to_vec()
    }

    fn wait_until(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        f()
    }

    fn test_uri() -> DeviceUri {
        DeviceUri::parse("virtual:/tmp/ds023-fake.sock").unwrap()
    }

    // ---------------------------------------------------------------- tests

    #[test]
    fn reconnect_repushes_the_latest_frame_to_the_new_transport() {
        let fake1 = FakeTransport::new();
        let fake2 = FakeTransport::new();
        let fake3 = FakeTransport::new();
        let opens = Arc::new(AtomicU64::new(0));
        let f1 = Arc::clone(&fake1);
        let f2 = Arc::clone(&fake2);
        let f3 = Arc::clone(&fake3);
        let op = Arc::clone(&opens);
        let opener: Opener = Box::new(move || {
            let n = op.fetch_add(1, Ordering::SeqCst);
            Ok(match n {
                0 => Box::new(FakeTransportArc(f1.clone())),
                1 => Box::new(FakeTransportArc(f2.clone())),
                _ => Box::new(FakeTransportArc(f3.clone())),
            })
        });
        let shutdown = Shutdown::manual();
        let (handle, thread, status) = spawn_with(
            Some(test_uri()),
            shutdown.clone(),
            Bus::new(),
            fast_cfg(),
            Some(opener),
        );

        // wait for the first connect (handshake done, status connected)
        assert!(wait_until(Duration::from_secs(5), || {
            status
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.starts_with("connected"))
        }));
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        // push colour A to key:0 -> image chunks on fake1
        assert!(push_color(&handle, "/controls/keys/0/image", "#3cc86e"));
        assert!(wait_until(Duration::from_secs(5), || {
            !fake1.key_image_reports().is_empty()
        }));
        assert_eq!(
            fake1.key_image_reports(),
            expected_key_image_reports([0x3c, 0xc8, 0x6e])
        );

        // the device disappears; the task must reconnect (fake2) and re-push the cache
        fake1.disconnect();
        assert!(wait_until(Duration::from_secs(5), || {
            opens.load(Ordering::SeqCst) >= 2
        }));
        assert!(wait_until(Duration::from_secs(5), || {
            !fake2.key_image_reports().is_empty()
        }));
        assert_eq!(
            fake2.key_image_reports(),
            expected_key_image_reports([0x3c, 0xc8, 0x6e]),
            "re-push after reconnect delivers the current frame"
        );
        assert!(*fake1.closed.lock().unwrap(), "old transport closed");

        // push a newer colour B while connected to fake2 (latest-frame-wins in the cache)
        assert!(push_color(&handle, "/controls/keys/0/image", "#ff8800"));
        let want_b = expected_key_image_reports([0xff, 0x88, 0x00]);
        assert!(wait_until(Duration::from_secs(5), || {
            last_key_image_reports(&fake2, want_b.len()) == want_b
        }));
        assert!(handle.superseded_count() >= 1, "superseded push counted");

        // second disappearance: fake3 must get the LATEST frame (B), not A
        fake2.disconnect();
        assert!(wait_until(Duration::from_secs(5), || {
            opens.load(Ordering::SeqCst) >= 3
        }));
        assert!(wait_until(Duration::from_secs(5), || {
            last_key_image_reports(&fake3, want_b.len()) == want_b
        }));

        shutdown.trigger();
        thread.join().unwrap();
    }

    #[test]
    fn unresponsive_module_stops_sends_and_recovers_on_rescan() {
        let fake = FakeTransport::new();
        let f = Arc::clone(&fake);
        let opens = Arc::new(AtomicU64::new(0));
        let op = Arc::clone(&opens);
        let opener: Opener = Box::new(move || {
            op.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeTransportArc(f.clone())))
        });
        let shutdown = Shutdown::manual();
        let (handle, thread, status) = spawn_with(
            Some(test_uri()),
            shutdown.clone(),
            Bus::new(),
            fast_cfg(),
            Some(opener),
        );
        assert!(wait_until(Duration::from_secs(5), || {
            status
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.starts_with("connected"))
        }));

        // the device stops answering sends and queries (bounded timeouts, no hang)
        fake.fail_sends(Error::Timeout(Duration::from_millis(30)));
        fake.fail_queries(Error::Timeout(Duration::from_millis(30)));
        assert!(push_color(&handle, "/controls/keys/0/image", "#112233"));
        // the push and then the watchdog probes hit the bounded timeout -> unresponsive
        assert!(wait_until(Duration::from_secs(5), || {
            status
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.contains("unresponsive"))
        }));
        let sent_at_fault = fake.key_image_reports().len();
        // further pushes are cached but NOT sent to the unhealthy module (counted)
        assert!(push_color(&handle, "/controls/keys/0/image", "#445566"));
        assert!(wait_until(Duration::from_millis(500), || {
            handle.unhealthy_drop_count() >= 1
        }));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            fake.key_image_reports().len(),
            sent_at_fault,
            "no image sends to an unhealthy module (handshake retries excepted)"
        );

        // the device recovers; the next rescan re-handshakes and sends flow again
        fake.clear_faults();
        assert!(wait_until(Duration::from_secs(5), || {
            status
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.starts_with("connected"))
        }));
        let want = expected_key_image_reports([0x44, 0x55, 0x66]);
        assert!(
            wait_until(Duration::from_secs(5), || {
                last_key_image_reports(&fake, want.len()) == want
            }),
            "latest cached frame pushed after recovery"
        );

        shutdown.trigger();
        thread.join().unwrap();
    }

    #[test]
    fn vdev_replug_repushes_the_latest_frame_within_five_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("vdev.sock");
        let vdev_a = VdevServer::start(&sock, HubConfig::default()).unwrap();
        let shutdown = Shutdown::manual();
        let uri = DeviceUri::parse(&format!("virtual:{}", sock.display())).unwrap();
        let (handle, thread, status) =
            spawn_with(Some(uri), shutdown.clone(), Bus::new(), fast_cfg(), None);

        // connect + handshake against the real virtual device
        assert!(wait_until(Duration::from_secs(10), || {
            status
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.starts_with("connected"))
        }));

        // push colour A; the vdev reassembles it into the key:0 frame
        assert!(push_color(&handle, "/controls/keys/0/image", "#3cc86e"));
        assert!(
            wait_until(Duration::from_secs(10), || {
                vdev_a.hub().frame(VdevTarget::Key(0)).is_some()
            }),
            "no frame on the vdev after the push"
        );
        let entry_a = vdev_a.hub().frame(VdevTarget::Key(0)).unwrap();
        let sha_a = ds_vdev::sha256_hex(&frames::encode::encode_solid(
            ImageTarget::Key(0).frame_target(),
            [0x3c, 0xc8, 0x6e],
        ));
        assert_eq!(entry_a.sha256, sha_a);

        // push colour B (the latest frame), then simulate the hot-unplug: the next accepted
        // write kills the connection (vdev --drop-after), and the server is replaced on the
        // same socket path so the rescan lands on the new one
        assert!(push_color(&handle, "/controls/keys/0/image", "#ff8800"));
        let replug_start = Instant::now();
        vdev_a.hub().set_throttle(ds_vdev::Throttle {
            kbps: None,
            drop_after: Some(1),
        });
        vdev_a.shutdown();
        let vdev_b = VdevServer::start(&sock, HubConfig::default()).unwrap();

        // the task must reconnect to vdev_b and re-push the LATEST frame (B) within 5 s
        let got = wait_until(Duration::from_secs(5), || {
            vdev_b.hub().frame(VdevTarget::Key(0)).is_some()
        });
        let elapsed = replug_start.elapsed();
        assert!(got, "no re-pushed frame on the new vdev within 5 s");
        assert!(
            elapsed <= Duration::from_secs(5),
            "replug took {elapsed:?}, over the 5 s budget"
        );
        let entry_b = vdev_b.hub().frame(VdevTarget::Key(0)).unwrap();
        let want_b = ds_vdev::sha256_hex(&frames::encode::encode_solid(
            ImageTarget::Key(0).frame_target(),
            [0xff, 0x88, 0x00],
        ));
        assert_eq!(entry_b.sha256, want_b, "re-push delivered the latest frame");
        assert_ne!(entry_a.sha256, entry_b.sha256);

        shutdown.trigger();
        thread.join().unwrap();
        drop(vdev_b);
    }

    /// A slot-aware fake: the (slot, orientation) identity of each kind can be changed at
    /// runtime, like `ds-vdevctl move` does on the virtual deck.
    struct SlotFake {
        inner: Arc<FakeTransport>,
        slots: Arc<StdMutex<HashMap<ModuleKind, ds_device::slot::Slot>>>,
    }

    impl Transport for SlotFake {
        fn send(&self, module: &ModuleId, report: &Report) -> DevResult<usize> {
            self.inner.send(module, report)
        }
        fn query(&self, module: &ModuleId, fid: u8, t: Duration) -> DevResult<Vec<u8>> {
            self.inner.query(module, fid, t)
        }
        fn recv(&self, t: Duration) -> DevResult<DeviceEvent> {
            self.inner.recv(t)
        }
        fn close(&self) -> DevResult<()> {
            self.inner.close()
        }
        fn module_id(&self, kind: ModuleKind) -> Option<ModuleId> {
            Some(virtual_module_id(kind))
        }
        fn module_slot(&self, kind: ModuleKind) -> Option<ds_device::slot::Slot> {
            self.slots.lock().unwrap().get(&kind).cloned()
        }
    }

    #[test]
    fn placement_change_on_rescan_triggers_reconnect_and_repush_keyed_by_position() {
        // One slot-aware fake per "physical deck"; the opener hands out successive fakes so
        // a reconnect lands on a fresh transport, like a replugged vdev. The slot map is
        // SHARED with the test so a "move" on the live connection is visible to the rescan.
        let slots1 = Arc::new(StdMutex::new(
            [(
                ModuleKind::Keys,
                ds_device::slot::Slot::parse("1-3-4").unwrap(),
            )]
            .into_iter()
            .collect(),
        ));
        let slots2 = Arc::new(StdMutex::new(
            [(
                ModuleKind::Keys,
                ds_device::slot::Slot::parse("3-1").unwrap(),
            )]
            .into_iter()
            .collect(),
        ));
        let fake1 = FakeTransport::new();
        let fake2 = FakeTransport::new();
        let opens = Arc::new(AtomicU64::new(0));
        let f1 = Arc::clone(&fake1);
        let f2 = Arc::clone(&fake2);
        let s1 = Arc::clone(&slots1);
        let s2 = Arc::clone(&slots2);
        let op = Arc::clone(&opens);
        let opener: Opener = Box::new(move || {
            let n = op.fetch_add(1, Ordering::SeqCst);
            Ok(match n {
                0 => Box::new(SlotFake {
                    inner: f1.clone(),
                    slots: s1.clone(),
                }),
                _ => Box::new(SlotFake {
                    inner: f2.clone(),
                    slots: s2.clone(),
                }),
            })
        });
        let shutdown = Shutdown::manual();
        let (handle, thread, status) = spawn_with(
            Some(test_uri()),
            shutdown.clone(),
            Bus::new(),
            fast_cfg(),
            Some(opener),
        );
        assert!(wait_until(Duration::from_secs(5), || {
            status
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.starts_with("connected"))
        }));
        // identity keys recorded at connect: keys at (1-3-4, horizontal)
        // push a frame; it lands on fake1
        assert!(push_color(&handle, "/controls/keys/0/image", "#3cc86e"));
        let want = expected_key_image_reports([0x3c, 0xc8, 0x6e]);
        assert!(wait_until(Duration::from_secs(5), || {
            last_key_image_reports(&fake1, want.len()) == want
        }));

        // MOVE the keys module to another connector on the SAME transport: the periodic
        // rescan compares (slot, orientation) keys, sees the change, and reconnects
        slots1.lock().unwrap().insert(
            ModuleKind::Keys,
            ds_device::slot::Slot::parse("3-1").unwrap(),
        );
        assert!(
            wait_until(Duration::from_secs(5), || opens.load(Ordering::SeqCst) >= 2),
            "placement change did not trigger a reconnect"
        );
        // the re-push after the reconnect delivers the same frame to the new transport,
        // keyed by position (idempotent latest-frame-wins)
        assert!(wait_until(Duration::from_secs(5), || {
            last_key_image_reports(&fake2, want.len()) == want
        }));

        shutdown.trigger();
        thread.join().unwrap();
    }

    /// Adapter so a `Arc<FakeTransport>` can be boxed as a `Transport`.
    struct FakeTransportArc(Arc<FakeTransport>);

    impl Transport for FakeTransportArc {
        fn send(&self, module: &ModuleId, report: &Report) -> DevResult<usize> {
            self.0.send(module, report)
        }
        fn query(&self, module: &ModuleId, fid: u8, t: Duration) -> DevResult<Vec<u8>> {
            self.0.query(module, fid, t)
        }
        fn recv(&self, t: Duration) -> DevResult<DeviceEvent> {
            self.0.recv(t)
        }
        fn close(&self) -> DevResult<()> {
            self.0.close()
        }
        fn module_id(&self, kind: ModuleKind) -> Option<ModuleId> {
            // the fake stands in for a whole virtual deck: every kind resolves to its
            // virtual id, exactly like VirtualTransport
            Some(virtual_module_id(kind))
        }
    }

    #[test]
    fn none_device_idles_and_counts_dropped_pushes() {
        let shutdown = Shutdown::manual();
        let (handle, thread, status) =
            spawn_with(None, shutdown.clone(), Bus::new(), fast_cfg(), None);
        assert!(push_color(&handle, "/controls/keys/0/image", "#ffffff"));
        assert!(wait_until(Duration::from_secs(2), || handle
            .superseded_count()
            >= 1));
        shutdown.trigger();
        thread.join().unwrap();
        assert_eq!(status.lock().unwrap().as_deref(), Some("none"));
    }

    #[test]
    fn cache_key_and_action_kind_cover_both_action_shapes() {
        let img = DeviceAction::Image {
            target: ImageTarget::Key(3),
            source: ImageSource::Color { rgb: [1, 2, 3] },
        };
        let led = DeviceAction::LedColor {
            kind: ModuleKind::Knobs,
            mode: LedMode::Static,
            rgb: [4, 5, 6],
            brightness: 100,
        };
        assert!(cache_key(&img).starts_with("image:"));
        assert!(cache_key(&led).starts_with("led:"));
        assert_eq!(action_kind(&img), ModuleKind::Keys);
        assert_eq!(action_kind(&led), ModuleKind::Knobs);
        assert_eq!(virtual_module_id(ModuleKind::Keys).kind, ModuleKind::Keys);
    }

    #[test]
    fn panel_and_key_images_are_mutually_exclusive_in_the_repush_cache() {
        let panel = DeviceAction::Image {
            target: ImageTarget::Panel,
            source: ImageSource::Colors([[1, 2, 3]; 15]),
        };
        let key = |n: u8| DeviceAction::Image {
            target: ImageTarget::Key(n),
            source: ImageSource::Color { rgb: [9, 9, 9] },
        };
        let dial = DeviceAction::Image {
            target: ImageTarget::Dial,
            source: ImageSource::Color { rgb: [5, 5, 5] },
        };
        let led = DeviceAction::LedColor {
            kind: ModuleKind::Knobs,
            mode: LedMode::Static,
            rgb: [4, 5, 6],
            brightness: 100,
        };
        let mut latest: HashMap<String, DeviceAction> = HashMap::new();

        // key images cached, then a panel image arrives: all key images are dropped
        for n in [0u8, 3, 14] {
            assert_eq!(drop_excluded_images(&mut latest, &key(n)), 0);
            latest.insert(cache_key(&key(n)), key(n));
        }
        latest.insert(cache_key(&dial), dial.clone());
        latest.insert(cache_key(&led), led.clone());
        assert_eq!(drop_excluded_images(&mut latest, &panel), 3);
        assert!(!latest.keys().any(|k| k.starts_with("image:Key(")));
        assert!(
            !latest.contains_key("image:Panel"),
            "panel not inserted yet"
        );
        latest.insert(cache_key(&panel), panel.clone());
        assert!(latest.contains_key("image:Dial"), "the dial is unaffected");
        assert!(
            latest.contains_key("led:Knobs"),
            "led actions are unaffected"
        );

        // a key image cached afterwards drops the cached panel
        assert_eq!(drop_excluded_images(&mut latest, &key(7)), 1);
        assert!(!latest.contains_key("image:Panel"));
        latest.insert(cache_key(&key(7)), key(7));
        assert!(latest.contains_key("image:Dial"));

        // non-image actions never drop anything
        assert_eq!(drop_excluded_images(&mut latest, &led), 0);
        assert_eq!(drop_excluded_images(&mut latest, &dial), 0);
        assert!(latest.contains_key("image:Key(7)"));
    }
}

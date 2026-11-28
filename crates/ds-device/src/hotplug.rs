//! Hot-plug lifecycle: rescan, reconnect, re-handshake and unresponsive-device detection
//! (DS-023) on top of any [`Transport`].
//!
//! The daemon's device task owns one transport and drives it through [`Lifecycle`]:
//!
//! ```text
//! Reconnecting --open ok--> Detected --handshake ok--> Handshaken --first io ok--> Healthy
//! Healthy      --send/query timeout x max_failures--> Unresponsive
//! Unresponsive --rescan tick: re-handshake--> (ok) Handshaken/Healthy  (gone) Reconnecting
//! any          --Disconnected / Closed / Io--> Reconnecting --rescan--> Detected ...
//! ```
//!
//! Rescans are poll-based with backoff (never event-only — USB enumeration races,
//! `docs/components/device-driver.md` invariant 8: rescan ≤ 1 s busy / 15 s idle, replug
//! ≤ 5 s end to end). Every send/query is bounded: the transport bounds it internally
//! (write timeout / query timeout) and [`timed`] adds a host-side deadline on top, so a
//! wedged module surfaces as [`Error::Timeout`] instead of hanging the task. A module whose
//! handshake or I/O times out is *unresponsive*: the caller stops sending to it and retries
//! the handshake on the next rescan ([`handshake_all`]).
//!
//! This module never blocks indefinitely and never touches daemon state: reconnect and
//! re-push are pure transport operations, so no state `rev` is ever bumped (S1).

use std::time::{Duration, Instant};

use ds_protocol::{ModuleKind, Orientation, Query};
use mh_device::ModuleId;

use crate::error::{Error, Result};
use crate::handshake::{self, HandshakeConfig, ModuleState};
use crate::slot::DeviceKey;
use crate::transport::{DeviceEvent, Transport};
use crate::uri::DeviceUri;
use crate::vtransport::virtual_module_id;

/// Rescan interval while no transport is open (the replug budget is ≤ 5 s, so the scan
/// runs well inside it).
pub const DEFAULT_RESCAN_DOWN: Duration = Duration::from_millis(500);
/// Rescan tick while connected (device-driver invariant 8: ≤ 1 s busy).
pub const DEFAULT_RESCAN_BUSY: Duration = Duration::from_secs(1);
/// Rescan tick while connected and idle (device-driver invariant 8: 15 s idle).
pub const DEFAULT_RESCAN_IDLE: Duration = Duration::from_secs(15);
/// Cap of the rescan backoff after repeated failed attempts.
pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(5);
/// Host-side bound on one send/query on top of the transport's own timeouts.
pub const DEFAULT_SEND_BOUND: Duration = Duration::from_secs(5);
/// Bound of one liveness probe (the roadmap's ≤ 100 ms liveness check).
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(100);
/// Consecutive send/query timeouts before a device is declared unresponsive.
pub const DEFAULT_MAX_FAILURES: u32 = 3;

/// The phases of the device lifecycle state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// A transport was opened (device detected), handshake not run yet.
    Detected,
    /// The software-mode handshake completed for at least one module.
    Handshaken,
    /// Normal operation: sends and queries succeed.
    Healthy,
    /// Repeated bounded timeouts: stop sending, retry the handshake on the next rescan.
    Unresponsive,
    /// No usable transport: rescan (open + handshake) until the device comes back.
    Reconnecting,
}

impl Phase {
    /// Stable lowercase name for logs and status strings.
    pub fn name(self) -> &'static str {
        match self {
            Phase::Detected => "detected",
            Phase::Handshaken => "handshaken",
            Phase::Healthy => "healthy",
            Phase::Unresponsive => "unresponsive",
            Phase::Reconnecting => "reconnecting",
        }
    }
}

/// How a transport error is read by the lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoFault {
    /// The bounded send/query deadline expired — the module is unresponsive, not gone.
    Timeout,
    /// The connection is gone (closed / io error / disconnect event) — reconnect.
    Gone,
    /// Anything else (forbidden, malformed, device-reported) — counted, never retried blindly.
    Other,
}

/// Classify a transport error into the lifecycle's [`IoFault`].
pub fn classify(err: &Error) -> IoFault {
    match err {
        Error::Timeout(_) => IoFault::Timeout,
        Error::Closed | Error::Io(_) => IoFault::Gone,
        _ => IoFault::Other,
    }
}

/// Rescan timing: poll with backoff, never event-only (USB enumeration races).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RescanPolicy {
    /// Interval while no transport is open (must keep the replug budget ≤ 5 s).
    pub down: Duration,
    /// Tick while connected and busy (≤ 1 s, device-driver invariant 8).
    pub busy: Duration,
    /// Tick while connected and idle (15 s, device-driver invariant 8).
    pub idle: Duration,
    /// Cap of the backoff after repeated failed rescans.
    pub max_backoff: Duration,
}

impl Default for RescanPolicy {
    fn default() -> Self {
        Self {
            down: DEFAULT_RESCAN_DOWN,
            busy: DEFAULT_RESCAN_BUSY,
            idle: DEFAULT_RESCAN_IDLE,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }
}

impl RescanPolicy {
    /// The delay before the next rescan after `failures` consecutive failed attempts:
    /// exponential from `down`, capped at `max_backoff` (overflow-safe).
    pub fn backoff(&self, failures: u32) -> Duration {
        let shift = failures.min(8);
        self.down
            .checked_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
            .unwrap_or(self.max_backoff)
            .min(self.max_backoff)
    }
}

/// The device lifecycle state machine: pure transitions, no I/O, no locks — the device task
/// drives it with the outcomes of its transport calls.
#[derive(Debug)]
pub struct Lifecycle {
    phase: Phase,
    failures: u32,
    max_failures: u32,
    changed_at: Instant,
}

impl Lifecycle {
    /// A fresh lifecycle starts in [`Phase::Reconnecting`]: nothing is open yet, the first
    /// rescan is due immediately.
    pub fn new(max_failures: u32) -> Self {
        Self {
            phase: Phase::Reconnecting,
            failures: 0,
            max_failures: max_failures.max(1),
            changed_at: Instant::now(),
        }
    }

    /// The current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Consecutive send/query timeouts since the last successful I/O.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// How long the machine has been in the current phase.
    pub fn since_change(&self) -> Duration {
        self.changed_at.elapsed()
    }

    /// Whether sends may be attempted now (the transport is open and not unresponsive).
    pub fn accepts_sends(&self) -> bool {
        matches!(self.phase, Phase::Handshaken | Phase::Healthy)
    }

    fn set(&mut self, phase: Phase) {
        self.phase = phase;
        self.changed_at = Instant::now();
    }

    /// A transport was opened: the device is detected.
    pub fn detect(&mut self) -> Phase {
        self.failures = 0;
        self.set(Phase::Detected);
        self.phase
    }

    /// The handshake completed (for at least one module).
    pub fn handshaken(&mut self) -> Phase {
        self.set(Phase::Handshaken);
        self.phase
    }

    /// Declare the device unresponsive outright (e.g. every module failed its handshake).
    pub fn unresponsive(&mut self) -> Phase {
        self.set(Phase::Unresponsive);
        self.phase
    }

    /// One successful send/query/recv: healthy, failures reset.
    pub fn io_ok(&mut self) -> Phase {
        self.failures = 0;
        if !matches!(self.phase, Phase::Detected | Phase::Reconnecting) {
            self.set(Phase::Healthy);
        }
        self.phase
    }

    /// One failed send/query, classified by [`classify`].
    pub fn io_fault(&mut self, fault: IoFault) -> Phase {
        match fault {
            IoFault::Gone => {
                self.failures = 0;
                self.set(Phase::Reconnecting);
            }
            IoFault::Timeout | IoFault::Other => {
                self.failures = self.failures.saturating_add(1);
                if self.failures >= self.max_failures {
                    self.set(Phase::Unresponsive);
                }
            }
        }
        self.phase
    }

    /// The outcome of one `recv` call on the transport. A recv timeout only means "no event
    /// right now" — it is not a device fault and does not count toward unresponsiveness.
    pub fn on_recv(&mut self, r: &Result<DeviceEvent>) -> Phase {
        match r {
            Ok(DeviceEvent::Input { .. }) => self.io_ok(),
            Ok(DeviceEvent::Disconnected) => {
                self.failures = 0;
                self.set(Phase::Reconnecting);
                self.phase
            }
            Err(Error::Timeout(_)) => self.phase,
            Err(e) => self.io_fault(classify(e)),
        }
    }

    /// A rescan tick: an unresponsive device goes back to scanning so the next rescan
    /// retries its handshake instead of hanging the task.
    pub fn on_rescan(&mut self) -> Phase {
        if matches!(self.phase, Phase::Unresponsive | Phase::Reconnecting) {
            self.set(Phase::Reconnecting);
        }
        self.phase
    }
}

/// Run `f` (one send/query) under a host-side deadline `bound`. The transport's own write
/// timeout bounds the call itself; if it returns after the deadline anyway (a slow device,
/// not a dead one), the outcome is reported as [`Error::Timeout`] so the caller treats the
/// module as unresponsive instead of trusting a stale success.
pub fn timed<T>(bound: Duration, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let start = Instant::now();
    let out = f()?;
    if start.elapsed() > bound {
        return Err(Error::Timeout(bound));
    }
    Ok(out)
}

/// The module ids a device URI addresses.
///
/// `virtual:<path>` is one socket for the whole deck, so every kind is addressed by its
/// virtual module id. `hid:` enumerates the attached modules; `hid:<os-path>` addresses
/// exactly the enumerated module at that path.
pub fn module_ids_for(uri: &DeviceUri) -> Result<Vec<ModuleId>> {
    match uri.scheme() {
        "virtual" => Ok(ModuleKind::ALL
            .iter()
            .map(|kind| virtual_module_id(*kind))
            .collect()),
        "hid" => {
            let modules = crate::hid::enumerate()?;
            let rest = uri.rest();
            if rest.is_empty() {
                Ok(modules.iter().map(|m| m.module_id.clone()).collect())
            } else {
                Ok(modules
                    .iter()
                    .filter(|m| m.path.to_string_lossy() == rest)
                    .map(|m| m.module_id.clone())
                    .collect())
            }
        }
        other => Err(Error::UnsupportedScheme(other.to_string())),
    }
}

/// One rescan attempt: open the device the URI addresses.
pub fn open_device(uri: &DeviceUri) -> Result<Box<dyn Transport>> {
    crate::open(uri)
}

/// The DS-112 identity keys of every module on `t`: each handshaken module paired with its
/// `(slot, orientation)` key. Modules whose transport reports no slot (a test double, or a
/// hid enumeration without a port path) fall back to serial-only identity and get no key —
/// the serial comparison still covers them.
pub fn identity_keys(t: &dyn Transport, orientation: Orientation) -> Vec<(ModuleId, DeviceKey)> {
    ModuleKind::ALL
        .iter()
        .filter_map(|kind| {
            let id = t.module_id(*kind)?;
            let slot = t.module_slot(*kind)?;
            Some((id.clone(), DeviceKey::new(slot, orientation)))
        })
        .collect()
}

/// Whether the identity key set of `t` still matches `known` — the rescan comparison of
/// DS-112: keyed by `(slot, orientation)` in addition to the serial, so a reseated or
/// rotated module is recognized by its position, not by appearing as a new device.
/// Returns the keys that changed (empty = the placement is unchanged).
pub fn placement_changes(
    t: &dyn Transport,
    orientation: Orientation,
    known: &[(ModuleId, DeviceKey)],
) -> Vec<(ModuleId, DeviceKey)> {
    let current = identity_keys(t, orientation);
    current
        .into_iter()
        .filter(|(id, key)| !known.iter().any(|(kid, kkey)| kid == id && kkey == key))
        .collect()
}

/// Run the DS-022 handshake of one module, bounded by `cfg` (attempts × timeouts). An
/// `Err` here is the "unresponsive module" signal — never followed by more I/O on it.
pub fn handshake_module<T: Transport + ?Sized>(
    t: &T,
    id: &ModuleId,
    cfg: HandshakeConfig,
) -> Result<ModuleState> {
    Ok(handshake::pending(t, id.clone(), cfg).handshake()?.state())
}

/// The result of handshaking every module of a connection: the initialized modules plus the
/// unresponsive ones (with their fault class). Bounded overall: each module's handshake is
/// bounded by `cfg`, so this never hangs the caller.
#[derive(Debug)]
pub struct HandshakeReport {
    /// Modules that completed the handshake.
    pub ok: Vec<ModuleState>,
    /// Modules that did not, with the classified fault (`Timeout` = unresponsive,
    /// `Gone` = the transport died during the handshake).
    pub unresponsive: Vec<(ModuleId, IoFault)>,
}

impl HandshakeReport {
    /// Whether every addressed module completed the handshake.
    pub fn all_ok(&self) -> bool {
        self.unresponsive.is_empty()
    }
}

/// Handshake every module in `ids` over `t`. Modules that fail are reported, not retried
/// here — the next rescan retries them ([`Lifecycle::on_rescan`]).
pub fn handshake_all<T: Transport + ?Sized>(
    t: &T,
    ids: &[ModuleId],
    cfg: HandshakeConfig,
) -> HandshakeReport {
    let mut ok = Vec::new();
    let mut unresponsive = Vec::new();
    for id in ids {
        match handshake_module(t, id, cfg) {
            Ok(state) => ok.push(state),
            Err(e) => unresponsive.push((id.clone(), classify(&e))),
        }
    }
    HandshakeReport { ok, unresponsive }
}

/// One bounded liveness probe: a firmware feature read on a handshaken module (the modules
/// answer in ~45 ms on hardware; the bound here is the roadmap's ≤ 100 ms liveness check).
pub fn probe<T: Transport + ?Sized>(
    t: &T,
    state: &ModuleState,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let feature_id = ds_protocol::query_id(state.mode, Query::Firmware).ok_or_else(|| {
        Error::Device(format!("{} has no firmware query id", state.id.kind.name()))
    })?;
    t.query(&state.id, feature_id, timeout)
}

/// One connected device: the open transport plus the handshake report of its modules.
pub struct Connection {
    /// The open transport.
    pub transport: Box<dyn Transport>,
    /// The handshake report of [`module_ids_for`]'s modules.
    pub report: HandshakeReport,
}

/// One full rescan cycle: open the device at `uri`, then handshake every module it
/// addresses. The handshake is bounded per module, so a dead device fails fast and a
/// half-alive one still yields its healthy modules.
pub fn connect(uri: &DeviceUri, cfg: HandshakeConfig) -> Result<Connection> {
    let transport = open_device(uri)?;
    let ids = module_ids_for(uri)?;
    let report = handshake_all(&*transport, &ids, cfg);
    Ok(Connection { transport, report })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::DeviceEvent;
    use ds_protocol::{Report, SoftwareMode};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A programmable fake transport: sends/queries/events are scripted, every sent report
    /// is recorded.
    struct FakeTransport {
        sent: Mutex<Vec<(ModuleId, Vec<u8>)>>,
        send_err: Mutex<Option<Error>>,
        query_err: Mutex<Option<Error>>,
        query_answer: Mutex<Vec<u8>>,
        events: Mutex<VecDeque<Result<DeviceEvent>>>,
    }

    impl FakeTransport {
        fn new() -> Self {
            FakeTransport {
                sent: Mutex::new(Vec::new()),
                send_err: Mutex::new(None),
                query_err: Mutex::new(None),
                query_answer: Mutex::new(firmware_answer()),
                events: Mutex::new(VecDeque::new()),
            }
        }

        fn fail_sends_with(&self, e: Error) {
            *self.send_err.lock().unwrap() = Some(e);
        }

        fn fail_queries_with(&self, e: Error) {
            *self.query_err.lock().unwrap() = Some(e);
        }

        fn clear_faults(&self) {
            *self.send_err.lock().unwrap() = None;
            *self.query_err.lock().unwrap() = None;
        }

        fn push_event(&self, e: Result<DeviceEvent>) {
            self.events.lock().unwrap().push_back(e);
        }

        fn sent_to(&self, kind: ModuleKind) -> Vec<Vec<u8>> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, _)| m.kind == kind)
                .map(|(_, b)| b.clone())
                .collect()
        }
    }

    /// A firmware feature answer shaped like the hardware/vdev one (`mh-protocol`
    /// `parse_firmware`: `[0x03, fid, len, 4 opaque bytes, ascii version…]`).
    fn firmware_answer() -> Vec<u8> {
        let mut payload = vec![0x29, 0x63, 0x1C, 0x5D];
        payload.extend_from_slice(b"01.00.00.00");
        let mut ans = vec![0x03, 0x38, payload.len() as u8];
        ans.extend_from_slice(&payload);
        ans
    }

    impl Transport for FakeTransport {
        fn send(&self, module: &ModuleId, report: &Report) -> Result<usize> {
            let bytes = report.bytes().to_vec();
            self.sent
                .lock()
                .unwrap()
                .push((module.clone(), bytes.clone()));
            if let Some(e) = self.send_err.lock().unwrap().as_ref() {
                return Err(clone_err(e));
            }
            Ok(bytes.len())
        }

        fn query(
            &self,
            _module: &ModuleId,
            _feature_id: u8,
            _timeout: Duration,
        ) -> Result<Vec<u8>> {
            if let Some(e) = self.query_err.lock().unwrap().as_ref() {
                return Err(clone_err(e));
            }
            Ok(self.query_answer.lock().unwrap().clone())
        }

        fn recv(&self, _timeout: Duration) -> Result<DeviceEvent> {
            match self.events.lock().unwrap().pop_front() {
                Some(e) => e,
                None => Err(Error::Timeout(Duration::from_millis(1))),
            }
        }

        fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    /// `Error` is not `Clone`; rebuild the scripted one by kind.
    fn clone_err(e: &Error) -> Error {
        match e {
            Error::Timeout(d) => Error::Timeout(*d),
            Error::Closed => Error::Closed,
            Error::Io(io) => Error::Io(std::io::Error::new(io.kind(), io.to_string())),
            other => Error::Device(other.to_string()),
        }
    }

    /// A handshake config with protocol-sized attempts but millisecond waits, so the tests
    /// exercise the real state machine without sleeping the hardware timings.
    fn fast_cfg() -> HandshakeConfig {
        HandshakeConfig {
            settle: Duration::from_millis(1),
            re_settle: Duration::from_millis(1),
            retry_gap: Duration::from_millis(1),
            attempts: 2,
            query_timeout: Duration::from_millis(50),
        }
    }

    fn keys_id() -> ModuleId {
        virtual_module_id(ModuleKind::Keys)
    }

    #[test]
    fn lifecycle_chain_runs_detected_handshaken_healthy_unresponsive_reconnecting() {
        let mut lc = Lifecycle::new(2);
        assert_eq!(lc.phase(), Phase::Reconnecting);
        assert!(!lc.accepts_sends());

        assert_eq!(lc.detect(), Phase::Detected);
        assert_eq!(lc.handshaken(), Phase::Handshaken);
        assert!(lc.accepts_sends());
        assert_eq!(lc.io_ok(), Phase::Healthy);

        // bounded timeouts: the first one is tolerated, the max-th declares unresponsive
        assert_eq!(lc.io_fault(IoFault::Timeout), Phase::Healthy);
        assert_eq!(lc.failures(), 1);
        assert_eq!(lc.io_fault(IoFault::Timeout), Phase::Unresponsive);
        assert!(!lc.accepts_sends(), "unresponsive stops sends");

        // the next rescan retries the handshake instead of hanging
        assert_eq!(lc.on_rescan(), Phase::Reconnecting);
        assert_eq!(lc.detect(), Phase::Detected);
        assert_eq!(lc.handshaken(), Phase::Handshaken);
        assert_eq!(lc.io_ok(), Phase::Healthy);

        // a disconnect event from any state goes back to reconnecting
        assert_eq!(
            lc.on_recv(&Ok(DeviceEvent::Disconnected)),
            Phase::Reconnecting
        );
        // a gone fault does the same
        lc.detect();
        assert_eq!(lc.io_fault(IoFault::Gone), Phase::Reconnecting);
        assert_eq!(lc.failures(), 0);
    }

    #[test]
    fn lifecycle_with_fake_transport_drives_the_full_chain() {
        let fake = FakeTransport::new();
        let mut lc = Lifecycle::new(2);
        let id = keys_id();

        // rescan: open (fake) + handshake + probe all succeed
        lc.detect();
        let state = handshake_module(&fake, &id, fast_cfg()).expect("handshake");
        assert_eq!(state.id, id);
        assert_eq!(lc.handshaken(), Phase::Handshaken);
        probe(&fake, &state, Duration::from_millis(50)).expect("probe answers");
        assert_eq!(lc.io_ok(), Phase::Healthy);

        // input reports count as liveness
        fake.push_event(Ok(DeviceEvent::Input {
            module: id.clone(),
            input: ds_protocol::Input::Keys([true; 15]),
        }));
        let ev = fake.recv(Duration::from_millis(10)).unwrap();
        assert_eq!(lc.on_recv(&Ok(ev)), Phase::Healthy);

        // the device stops answering: bounded timeouts, then unresponsive
        fake.fail_sends_with(Error::Timeout(Duration::from_millis(50)));
        fake.fail_queries_with(Error::Timeout(Duration::from_millis(50)));
        for _ in 0..2 {
            let report = ds_protocol::fill_screen(ModuleKind::Keys, [9, 9, 9]).unwrap();
            let r = fake.send(&id, &report);
            assert!(matches!(r, Err(Error::Timeout(_))));
            lc.io_fault(classify(r.as_ref().unwrap_err()));
        }
        assert_eq!(lc.phase(), Phase::Unresponsive);
        assert!(!lc.accepts_sends());

        // the device goes away entirely
        fake.push_event(Ok(DeviceEvent::Disconnected));
        let ev = fake.recv(Duration::from_millis(10)).unwrap();
        assert_eq!(lc.on_recv(&Ok(ev)), Phase::Reconnecting);

        // next rescan: re-handshake succeeds, back to healthy
        fake.clear_faults();
        assert_eq!(lc.on_rescan(), Phase::Reconnecting);
        lc.detect();
        let state = handshake_module(&fake, &id, fast_cfg()).expect("re-handshake");
        assert_eq!(lc.handshaken(), Phase::Handshaken);
        probe(&fake, &state, Duration::from_millis(50)).expect("probe after re-handshake");
        assert_eq!(lc.io_ok(), Phase::Healthy);
    }

    #[test]
    fn handshake_all_reports_unresponsive_modules_without_hanging() {
        let fake = FakeTransport::new();
        fake.fail_queries_with(Error::Timeout(Duration::from_millis(10)));
        let ids: Vec<ModuleId> = ModuleKind::ALL
            .iter()
            .map(|k| virtual_module_id(*k))
            .collect();
        let start = Instant::now();
        let report = handshake_all(&fake, &ids, fast_cfg());
        let elapsed = start.elapsed();
        // the M0 family needs no reads and handshakes instantly; keys/dial time out on the
        // confirming read and are reported unresponsive, bounded by cfg
        assert!(!report.all_ok());
        assert!(report
            .unresponsive
            .iter()
            .any(|(id, f)| id.kind == ModuleKind::Keys && *f == IoFault::Timeout));
        assert!(report
            .unresponsive
            .iter()
            .any(|(id, _)| id.kind == ModuleKind::Dial));
        assert!(report.ok.iter().any(|s| s.id.kind == ModuleKind::Faders));
        assert!(
            elapsed < Duration::from_secs(5),
            "handshake_all hung: {elapsed:?}"
        );
    }

    #[test]
    fn handshake_failure_on_a_gone_transport_is_classified_gone() {
        let fake = FakeTransport::new();
        fake.fail_sends_with(Error::Closed);
        let report = handshake_all(&fake, &[keys_id()], fast_cfg());
        assert_eq!(report.unresponsive.len(), 1);
        assert_eq!(report.unresponsive[0].1, IoFault::Gone);
        assert!(report.ok.is_empty());
    }

    #[test]
    fn classify_maps_transport_errors() {
        assert_eq!(
            classify(&Error::Timeout(Duration::from_millis(1))),
            IoFault::Timeout
        );
        assert_eq!(classify(&Error::Closed), IoFault::Gone);
        assert_eq!(
            classify(&Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pipe"
            ))),
            IoFault::Gone
        );
        assert_eq!(classify(&Error::Forbidden), IoFault::Other);
        assert_eq!(classify(&Error::Device("wedged".into())), IoFault::Other);
    }

    #[test]
    fn rescan_policy_backoff_grows_and_is_capped() {
        let p = RescanPolicy::default();
        assert_eq!(p.backoff(0), p.down);
        assert!(p.backoff(1) > p.down);
        assert_eq!(p.backoff(100), p.max_backoff);
        assert!(
            p.backoff(u32::MAX) <= p.max_backoff,
            "overflow must not panic"
        );
        assert!(p.down <= Duration::from_secs(1), "replug budget <= 5 s");
        assert_eq!(p.busy, DEFAULT_RESCAN_BUSY);
        assert_eq!(p.idle, DEFAULT_RESCAN_IDLE);
    }

    #[test]
    fn module_ids_for_virtual_covers_the_whole_deck() {
        let uri = DeviceUri::parse("virtual:/tmp/vdev.sock").unwrap();
        let ids = module_ids_for(&uri).unwrap();
        assert_eq!(ids.len(), ModuleKind::ALL.len());
        for kind in ModuleKind::ALL {
            assert!(ids.iter().any(|i| i.kind == kind), "{kind:?}");
        }
        assert!(module_ids_for(&DeviceUri::parse("usb:/dev/x").unwrap()).is_err());
    }

    #[test]
    fn timed_marks_an_overrun_as_a_bounded_timeout() {
        let overran = timed(Duration::from_millis(1), || {
            std::thread::sleep(Duration::from_millis(5));
            Ok(7u32)
        });
        assert!(
            matches!(overran, Err(Error::Timeout(d)) if d == Duration::from_millis(1)),
            "{overran:?}"
        );
        assert_eq!(timed(Duration::from_secs(1), || Ok(2u32)).unwrap(), 2);
        // the inner error propagates unchanged
        assert!(matches!(
            timed(Duration::from_secs(1), || Err::<u32, _>(Error::Closed)),
            Err(Error::Closed)
        ));
    }

    #[test]
    fn open_device_refuses_a_missing_virtual_socket() {
        let uri = DeviceUri::parse("virtual:/tmp/definitely-missing-ds023.sock").unwrap();
        assert!(matches!(open_device(&uri), Err(Error::Io(_))));
    }

    #[test]
    fn probe_uses_the_handshaken_module_and_its_timeout() {
        let fake = FakeTransport::new();
        let state = handshake_module(&fake, &keys_id(), fast_cfg()).unwrap();
        let ans = probe(&fake, &state, Duration::from_millis(50)).unwrap();
        assert!(ds_protocol::parse_firmware(&ans).is_some());
        fake.fail_queries_with(Error::Timeout(Duration::from_millis(5)));
        assert!(matches!(
            probe(&fake, &state, Duration::from_millis(5)),
            Err(Error::Timeout(_))
        ));
        // a software-mode proof is required: SoftwareMode is minted by the handshake only
        let mode = SoftwareMode::m0(ModuleKind::Base).unwrap();
        assert!(ds_protocol::query_id(mode, Query::Firmware).is_some());
    }
}

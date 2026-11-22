//! The software-mode handshake and module init on a connected transport (DS-022).
//!
//! Keys and dial only accept commands and answer reads in software mode; reading them in
//! hardware mode wedges the module until it is power-cycled (`upstream/PROTOCOL.md`,
//! verified 2026-10-09). This module drives the required sequence over any
//! [`Transport`] — real HID (DS-021) or the virtual one — mirroring the pinned
//! `mh-device` `Module::initialize()` and the DS-014/DS-021 virtual path:
//!
//! 1. send the software-mode-on report (`03 3A 01` keys, `03 49 01` dial) — a write that
//!    must be acked (the virtual transport waits for the ack up to its write timeout, the
//!    HID transport's `hid_write` is synchronous),
//! 2. settle ≥ [`MODE_SETTLE`] (600 ms),
//! 3. send the same report again (the double command before the first read — 200 ms was not
//!    always enough on a module just handed back by another app, 600 ms + double was never
//!    seen to wedge),
//! 4. wait [`MODE_RE_SETTLE`] (200 ms), then confirm with one firmware feature read,
//! 5. on no usable answer, bounded retries: hand the module back (`… 00`), wait
//!    [`RETRY_GAP`], and start over — at most [`DEFAULT_ATTEMPTS`] attempts, then a clear
//!    error (this is the "unresponsive module" signal DS-023 picks up). **A failed send is
//!    never followed by a read.**
//!
//! The M0 family (base, faders, knobs, rollers) has no software mode and answers at any
//! time: the handshake mints its [`SoftwareMode`] token without sending anything.
//!
//! Type state: [`pending`] gives a [`PendingModule`] whose reads are refused; only
//! [`PendingModule::handshake`] produces an [`InitModule`], and only
//! [`InitTransport::init`] (handshake + init of every connected module) produces the safe
//! post-init [`InitTransport`], which refuses operations on any module that never completed
//! the handshake. All writes go through the [`safe`] builders on top of ds-protocol, and
//! every report passes the forbidden-command guard again before it leaves the host.

use std::collections::HashMap;
use std::time::Duration;

use ds_protocol::{LedMode, LedTarget, ModuleKind, Query, Report, Rgb, SoftwareMode};
use mh_device::ModuleId;

use crate::error::{Error, Result};
use crate::module::{module_info, ModuleInfo};
use crate::report::ensure_allowed;
use crate::transport::Transport;

/// Time a module gets to switch to software mode before the second command (`mh-device`
/// `MODE_SETTLE`; 600 ms — 200 ms once wedged a module on macOS).
pub const MODE_SETTLE: Duration = Duration::from_millis(600);
/// Wait between the second software-mode command and the first read (200 ms, `PROTOCOL.md`).
pub const MODE_RE_SETTLE: Duration = Duration::from_millis(200);
/// Wait between handing a module back (off) and the next attempt's on (`mh-device` 500 ms).
pub const RETRY_GAP: Duration = Duration::from_millis(500);
/// Handshake attempts before the module is declared unresponsive (`mh-device`: 3).
pub const DEFAULT_ATTEMPTS: u32 = 3;
/// Timeout of one confirming feature read (the modules answer in ~45 ms on hardware).
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Timings and bounds of the handshake; [`Default`] is the hardware-verified sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandshakeConfig {
    /// Settle after the first software-mode-on report (≥ [`MODE_SETTLE`]).
    pub settle: Duration,
    /// Wait between the second on report and the confirming read ([`MODE_RE_SETTLE`]).
    pub re_settle: Duration,
    /// Wait between the off report of a failed attempt and the next on ([`RETRY_GAP`]).
    pub retry_gap: Duration,
    /// Total attempts before the module is declared unresponsive ([`DEFAULT_ATTEMPTS`]).
    pub attempts: u32,
    /// Timeout of one feature read during the handshake ([`DEFAULT_QUERY_TIMEOUT`]).
    pub query_timeout: Duration,
}

impl Default for HandshakeConfig {
    fn default() -> Self {
        Self {
            settle: MODE_SETTLE,
            re_settle: MODE_RE_SETTLE,
            retry_gap: RETRY_GAP,
            attempts: DEFAULT_ATTEMPTS,
            query_timeout: DEFAULT_QUERY_TIMEOUT,
        }
    }
}

/// The states of the per-module handshake state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Nothing sent yet.
    Start,
    /// First software-mode-on report sent (and acked); settling now.
    ModeSent,
    /// Settle time elapsed.
    Settled,
    /// Second software-mode-on report sent (double command before the first read).
    Resent,
    /// Confirming with a firmware feature read.
    Confirming,
    /// Handed back to hardware mode before the next attempt.
    Retry,
    /// The off report of the retry was sent.
    OffSent,
    /// Handshake complete; the [`SoftwareMode`] token is minted.
    Done,
    /// A send failed or the attempts are exhausted — no read follows a failed send.
    Failed,
}

/// The software-mode handshake of one module over a transport.
///
/// Drive it with [`Handshake::run`] (sleeps the protocol waits) or step it manually with
/// [`Handshake::step`] (no sleeps — the unit tests do that against a mock transport).
pub struct Handshake<'a, T: Transport + ?Sized> {
    transport: &'a T,
    id: ModuleId,
    info: ModuleInfo,
    cfg: HandshakeConfig,
    phase: Phase,
    attempt: u32,
    mode: Option<SoftwareMode>,
    firmware: Option<String>,
}

impl<'a, T: Transport + ?Sized> Handshake<'a, T> {
    /// A handshake for `id` on `transport` with the given timings.
    pub fn new(transport: &'a T, id: ModuleId, cfg: HandshakeConfig) -> Self {
        Self {
            transport,
            info: module_info(id.kind),
            id,
            cfg,
            phase: Phase::Start,
            attempt: 0,
            mode: None,
            firmware: None,
        }
    }

    /// The module this handshake is for.
    pub fn id(&self) -> &ModuleId {
        &self.id
    }

    /// The module descriptor (kind, pid, report lengths) the handshake identified.
    pub fn info(&self) -> &ModuleInfo {
        &self.info
    }

    /// The current state.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// How many confirming reads have failed so far.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The firmware version the confirming read returned, once the handshake is done.
    pub fn firmware(&self) -> Option<&str> {
        self.firmware.as_deref()
    }

    /// Guard + send one report of the handshake sequence.
    fn send(&self, report: &Report) -> Result<usize> {
        ensure_allowed(report.bytes())?;
        self.transport.send(&self.id, report)
    }

    /// Count one attempt that got no usable answer: retry, or fail for good.
    fn retry_or_fail(&mut self, cause: Error) -> Result<bool> {
        self.attempt += 1;
        if self.attempt >= self.cfg.attempts {
            self.phase = Phase::Failed;
            return Err(cause);
        }
        self.phase = Phase::Retry;
        Ok(false)
    }

    /// Perform the next state transition. Returns `Ok(true)` once the handshake is done;
    /// does not sleep — [`Handshake::run`] inserts the protocol waits.
    pub fn step(&mut self) -> Result<bool> {
        match self.phase {
            Phase::Start => match ds_protocol::enter_software_mode(self.id.kind) {
                Some((on, mode)) => {
                    self.mode = Some(mode);
                    match self.send(&on) {
                        Ok(_) => self.phase = Phase::ModeSent,
                        Err(e) => {
                            // a failed send is never followed by a read (mh-device)
                            self.phase = Phase::Failed;
                            return Err(e);
                        }
                    }
                }
                None => {
                    // the M0 family has no software mode and answers at any time
                    let mode = SoftwareMode::m0(self.id.kind).ok_or_else(|| {
                        Error::Device(format!("{} is neither screen nor M0", self.id))
                    })?;
                    self.mode = Some(mode);
                    self.phase = Phase::Done;
                    return Ok(true);
                }
            },
            Phase::ModeSent => self.phase = Phase::Settled,
            Phase::Settled => {
                let (on, _) = ds_protocol::enter_software_mode(self.id.kind).ok_or_else(|| {
                    Error::Device(format!("{} lost its handshake report", self.id))
                })?;
                match self.send(&on) {
                    Ok(_) => self.phase = Phase::Resent,
                    Err(e) => {
                        self.phase = Phase::Failed;
                        return Err(e);
                    }
                }
            }
            Phase::Resent => self.phase = Phase::Confirming,
            Phase::Confirming => {
                let Some(mode) = self.mode else {
                    self.phase = Phase::Failed;
                    return Err(Error::Device(format!(
                        "{} handshake lost its mode token",
                        self.id
                    )));
                };
                let Some(feature_id) = ds_protocol::query_id(mode, Query::Firmware) else {
                    self.phase = Phase::Failed;
                    return Err(Error::Device(format!(
                        "{} has no firmware query id",
                        self.id.kind.name()
                    )));
                };
                match self
                    .transport
                    .query(&self.id, feature_id, self.cfg.query_timeout)
                {
                    Ok(answer) => match ds_protocol::parse_firmware(&answer) {
                        Some(fw) => {
                            self.firmware = Some(fw);
                            self.phase = Phase::Done;
                            return Ok(true);
                        }
                        None => {
                            let cause = Error::Device(format!(
                                "{} answered feature {feature_id:#04x} without a firmware version",
                                self.id
                            ));
                            return self.retry_or_fail(cause);
                        }
                    },
                    // a read timeout is a flaky-USB symptom: bounded retry, then the error
                    Err(e @ Error::Timeout(_)) => return self.retry_or_fail(e),
                    Err(e) => {
                        self.phase = Phase::Failed;
                        return Err(e);
                    }
                }
            }
            Phase::Retry => match ds_protocol::leave_software_mode(self.id.kind) {
                Some(off) => match self.send(&off) {
                    Ok(_) => self.phase = Phase::OffSent,
                    Err(e) => {
                        self.phase = Phase::Failed;
                        return Err(e);
                    }
                },
                None => self.phase = Phase::OffSent,
            },
            Phase::OffSent => self.phase = Phase::Start,
            Phase::Done => return Ok(true),
            Phase::Failed => {
                return Err(Error::Device(format!(
                    "{} ({}) did not complete the software-mode handshake",
                    self.id.kind.name(),
                    self.id.serial
                )))
            }
        }
        Ok(false)
    }

    /// Run the handshake to completion, sleeping the protocol waits between steps.
    ///
    /// Returns the [`SoftwareMode`] proof token that gates feature reads on the module.
    pub fn run(&mut self) -> Result<SoftwareMode> {
        loop {
            match self.phase {
                Phase::ModeSent => std::thread::sleep(self.cfg.settle),
                Phase::Confirming => std::thread::sleep(self.cfg.re_settle),
                Phase::OffSent => std::thread::sleep(self.cfg.retry_gap),
                _ => {}
            }
            if self.step()? {
                return self.mode.ok_or_else(|| {
                    Error::Device(format!(
                        "{} handshake finished without a mode token",
                        self.id
                    ))
                });
            }
        }
    }
}

/// A connected module whose handshake has not run yet.
///
/// Reads are refused here: on keys/dial a feature read in hardware mode wedges the module
/// until power-cycled, so the only way forward is [`PendingModule::handshake`].
pub struct PendingModule<'a, T: Transport + ?Sized> {
    transport: &'a T,
    id: ModuleId,
    cfg: HandshakeConfig,
}

/// Wrap one connected module before its handshake.
pub fn pending<'a, T: Transport + ?Sized>(
    transport: &'a T,
    id: ModuleId,
    cfg: HandshakeConfig,
) -> PendingModule<'a, T> {
    PendingModule { transport, id, cfg }
}

impl<'a, T: Transport + ?Sized> PendingModule<'a, T> {
    /// The module this wrapper is for.
    pub fn id(&self) -> &ModuleId {
        &self.id
    }

    /// Refused before the handshake: a read now could wedge keys/dial for good.
    pub fn query(&self, query: Query) -> Result<Vec<u8>> {
        Err(Error::Device(format!(
            "{query:?} refused on {}: the software-mode handshake has not completed",
            self.id
        )))
    }

    /// Run the software-mode handshake and module init; yields the safe post-init handle.
    pub fn handshake(self) -> Result<InitModule<'a, T>> {
        let mut hs = Handshake::new(self.transport, self.id.clone(), self.cfg);
        let mode = hs.run()?;
        Ok(InitModule {
            transport: self.transport,
            id: self.id,
            info: *hs.info(),
            cfg: self.cfg,
            mode,
            firmware: hs.firmware().map(str::to_string),
        })
    }
}

/// One module after the handshake and init: reads are allowed (behind the [`SoftwareMode`]
/// proof), and every write goes through a [`safe`] builder plus the forbidden guard.
pub struct InitModule<'a, T: Transport + ?Sized> {
    transport: &'a T,
    id: ModuleId,
    info: ModuleInfo,
    cfg: HandshakeConfig,
    mode: SoftwareMode,
    firmware: Option<String>,
}

impl<'a, T: Transport + ?Sized> InitModule<'a, T> {
    /// The module this handle is for.
    pub fn id(&self) -> &ModuleId {
        &self.id
    }

    /// The identified module descriptor (kind, pid, report lengths).
    pub fn info(&self) -> &ModuleInfo {
        &self.info
    }

    /// The software-mode proof minted by the handshake.
    pub fn mode(&self) -> SoftwareMode {
        self.mode
    }

    /// The firmware version read back during the handshake, when the module answered one.
    pub fn firmware(&self) -> Option<&str> {
        self.firmware.as_deref()
    }

    /// The init state of this module.
    pub fn state(&self) -> ModuleState {
        ModuleState {
            id: self.id.clone(),
            info: self.info,
            mode: self.mode,
            firmware: self.firmware.clone(),
        }
    }

    /// Feature read, only expressible behind the handshake's [`SoftwareMode`] proof.
    pub fn query(&self, query: Query) -> Result<Vec<u8>> {
        let feature_id = ds_protocol::query_id(self.mode, query).ok_or_else(|| {
            Error::Device(format!("{} does not answer {query:?}", self.id.kind.name()))
        })?;
        self.transport
            .query(&self.id, feature_id, self.cfg.query_timeout)
    }

    /// Send one report built by a [`safe`] builder; the forbidden guard runs again here.
    pub fn send(&self, report: &Report) -> Result<usize> {
        ensure_allowed(report.bytes())?;
        self.transport.send(&self.id, report)
    }

    /// LED color (`02 2D … save=0`).
    pub fn set_led_color(&self, target: LedTarget, rgb: Rgb) -> Result<usize> {
        let r = safe::led_color(self.id.kind, target, rgb)?;
        self.send(&r)
    }

    /// LED mode (`02 2C … save=0`).
    pub fn set_led_mode(&self, target: LedTarget, mode: LedMode) -> Result<usize> {
        let r = safe::led_mode(self.id.kind, target, mode)?;
        self.send(&r)
    }

    /// LED brightness, clamped into 0..=100 (`02 2E …`).
    pub fn set_led_brightness(&self, target: LedTarget, percent: u8) -> Result<usize> {
        let r = safe::led_brightness(self.id.kind, target, percent)?;
        self.send(&r)
    }

    /// LED animation speed, clamped into 0..=100 (`02 30 …`, M0 family only).
    pub fn set_led_speed(&self, percent: u8) -> Result<usize> {
        let r = safe::led_speed(self.id.kind, percent)?;
        self.send(&r)
    }

    /// Screen backlight 1..=100 — `0` is refused on every module (it wedges the screens;
    /// "screen off" is a black image).
    pub fn set_backlight(&self, percent: u8) -> Result<usize> {
        let r = safe::backlight(self.id.kind, percent)?;
        self.send(&r)
    }

    /// Fill the whole screen with one color (keys/dial).
    pub fn fill_screen(&self, rgb: Rgb) -> Result<usize> {
        let r = safe::fill_screen(self.id.kind, rgb)?;
        self.send(&r)
    }

    /// Fill one key (keys module only).
    pub fn fill_key(&self, index: u8, rgb: Rgb) -> Result<usize> {
        if self.id.kind != ModuleKind::Keys {
            return Err(Error::Device(format!(
                "fill_key goes to the keys module, not {}",
                self.id.kind.name()
            )));
        }
        let r = safe::fill_key(index, rgb)?;
        self.send(&r)
    }

    /// One key's 72x72 JPEG, chunked (`02 36 …`), keys module only.
    pub fn send_key_image(&self, index: u8, jpeg: &[u8]) -> Result<()> {
        if self.id.kind != ModuleKind::Keys {
            return Err(Error::Device(format!(
                "key images go to the keys module, not {}",
                self.id.kind.name()
            )));
        }
        for r in safe::key_image(index, jpeg)? {
            self.send(&r)?;
        }
        Ok(())
    }

    /// One 480x272 JPEG across the whole panel (`02 35 …`), keys module only.
    pub fn send_panel_image(&self, jpeg: &[u8]) -> Result<()> {
        if self.id.kind != ModuleKind::Keys {
            return Err(Error::Device(format!(
                "panel images go to the keys module, not {}",
                self.id.kind.name()
            )));
        }
        for r in safe::panel_image(jpeg)? {
            self.send(&r)?;
        }
        Ok(())
    }

    /// One 240x240 JPEG on the dial screen (`02 45 …`), dial module only.
    pub fn send_dial_image(&self, jpeg: &[u8]) -> Result<()> {
        if self.id.kind != ModuleKind::Dial {
            return Err(Error::Device(format!(
                "dial images go to the dial module, not {}",
                self.id.kind.name()
            )));
        }
        for r in safe::dial_image(jpeg)? {
            self.send(&r)?;
        }
        Ok(())
    }
}

/// The init state of one connected module: identity (kind/pid) + handshake result.
#[derive(Debug, Clone)]
pub struct ModuleState {
    /// The module, identified by kind and serial (never by USB string).
    pub id: ModuleId,
    /// The descriptor its pid mapped to.
    pub info: ModuleInfo,
    /// The software-mode proof minted by the handshake.
    pub mode: SoftwareMode,
    /// Firmware version read back during init, when the module answered one.
    pub firmware: Option<String>,
}

/// The safe post-init transport: every module of [`InitTransport::init`] completed the
/// handshake; operations on a module that did not are refused.
pub struct InitTransport<'a, T: Transport + ?Sized> {
    transport: &'a T,
    cfg: HandshakeConfig,
    modules: HashMap<ModuleId, ModuleState>,
}

impl<'a, T: Transport + ?Sized> InitTransport<'a, T> {
    /// Handshake + init every module in `ids`. Fails (and yields no transport) if any
    /// module does not complete the handshake — that module is unresponsive, not initialized.
    pub fn init(transport: &'a T, ids: &[ModuleId], cfg: HandshakeConfig) -> Result<Self> {
        let mut modules = HashMap::new();
        for id in ids {
            let module = pending(transport, id.clone(), cfg).handshake()?;
            modules.insert(id.clone(), module.state());
        }
        Ok(Self {
            transport,
            cfg,
            modules,
        })
    }

    /// The underlying transport.
    pub fn transport(&self) -> &'a T {
        self.transport
    }

    /// The initialized modules, sorted by identity.
    pub fn modules(&self) -> Vec<ModuleId> {
        let mut ids: Vec<ModuleId> = self.modules.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// The init state of one module, when it completed the handshake.
    pub fn state(&self, id: &ModuleId) -> Option<&ModuleState> {
        self.modules.get(id)
    }

    /// The safe handle of one module — refused when it never completed the handshake.
    pub fn module(&self, id: &ModuleId) -> Result<InitModule<'a, T>> {
        let Some(state) = self.modules.get(id) else {
            return Err(Error::Device(format!(
                "{id} has not completed the software-mode handshake; refusing the operation"
            )));
        };
        Ok(InitModule {
            transport: self.transport,
            id: id.clone(),
            info: state.info,
            cfg: self.cfg,
            mode: state.mode,
            firmware: state.firmware.clone(),
        })
    }

    /// Send one safe-builder report to a module (refused before its handshake completed).
    pub fn send(&self, id: &ModuleId, report: &Report) -> Result<usize> {
        self.module(id)?.send(report)
    }

    /// Feature read on a module (refused before its handshake completed).
    pub fn query(&self, id: &ModuleId, query: Query) -> Result<Vec<u8>> {
        self.module(id)?.query(query)
    }
}

/// Safe report builders on top of ds-protocol (DS-010): key/panel/dial images, LED
/// color/mode/brightness/speed, backlight, screen fills. Every returned report passed the
/// forbidden-command guard ([`ds_protocol::is_forbidden`]) — builder output can never trip
/// it, and the check here is the second line of defense on the way to the transport.
///
/// Backlight `0` is refused on every module (it wedges the screens — "off" is a black
/// image), brightness values are clamped into 0..=100, and key indices stay in 0..15.
pub mod safe {
    use super::*;

    fn guard(report: &Report) -> Result<()> {
        ensure_allowed(report.bytes())?;
        if report.is_forbidden() {
            return Err(Error::Forbidden);
        }
        Ok(())
    }

    fn one(report: Option<Report>, what: &str) -> Result<Report> {
        let report = report
            .ok_or_else(|| Error::Device(format!("{what} is not available on this module")))?;
        guard(&report)?;
        Ok(report)
    }

    fn many(reports: Vec<Report>, what: &str) -> Result<Vec<Report>> {
        if reports.is_empty() {
            return Err(Error::Device(format!("{what} produced no reports")));
        }
        for r in &reports {
            guard(r)?;
        }
        Ok(reports)
    }

    /// One key's 72x72 JPEG, chunked as `02 36 …` at the keys module's output length.
    /// `index` must be < 15.
    pub fn key_image(index: u8, jpeg: &[u8]) -> Result<Vec<Report>> {
        many(
            ds_protocol::key_image(index, jpeg, module_info(ModuleKind::Keys).out_len),
            "key image",
        )
    }

    /// One 480x272 JPEG across the whole 15-key panel, chunked as `02 35 …`.
    pub fn panel_image(jpeg: &[u8]) -> Result<Vec<Report>> {
        many(
            ds_protocol::panel_image(jpeg, module_info(ModuleKind::Keys).out_len),
            "panel image",
        )
    }

    /// One 240x240 JPEG on the dial screen, chunked as `02 45 …`.
    pub fn dial_image(jpeg: &[u8]) -> Result<Vec<Report>> {
        many(
            ds_protocol::dial_image(jpeg, module_info(ModuleKind::Dial).out_len),
            "dial image",
        )
    }

    /// LED color, never persisted (`save=0` always).
    pub fn led_color(kind: ModuleKind, target: LedTarget, rgb: Rgb) -> Result<Report> {
        one(ds_protocol::led_color(kind, target, rgb), "LED color")
    }

    /// LED mode, never persisted (`save=0` always).
    pub fn led_mode(kind: ModuleKind, target: LedTarget, mode: LedMode) -> Result<Report> {
        one(ds_protocol::led_mode(kind, target, mode), "LED mode")
    }

    /// LED brightness, clamped into 0..=100 on the wire.
    pub fn led_brightness(kind: ModuleKind, target: LedTarget, percent: u8) -> Result<Report> {
        one(
            ds_protocol::led_brightness(kind, target, percent.min(100)),
            "LED brightness",
        )
    }

    /// LED animation speed, clamped into 0..=100 (M0 family only).
    pub fn led_speed(kind: ModuleKind, percent: u8) -> Result<Report> {
        one(ds_protocol::led_speed(kind, percent.min(100)), "LED speed")
    }

    /// Screen backlight in 1..=100 percent (clamped at 100). **`0` is refused on every
    /// module** — it wedges the screens until reseat; "screen off" is a black image.
    pub fn backlight(kind: ModuleKind, percent: u8) -> Result<Report> {
        if percent == 0 {
            return Err(Error::Device(format!(
                "backlight 0 is refused on {} (it wedges the screen); send a black image instead",
                kind.name()
            )));
        }
        one(ds_protocol::backlight(kind, percent.min(100)), "backlight")
    }

    /// Fill the whole screen of keys/dial with one color.
    pub fn fill_screen(kind: ModuleKind, rgb: Rgb) -> Result<Report> {
        one(ds_protocol::fill_screen(kind, rgb), "screen fill")
    }

    /// Fill one key (0..15) with a color (keys module).
    pub fn fill_key(index: u8, rgb: Rgb) -> Result<Report> {
        one(ds_protocol::fill_key(index, rgb), "key fill")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::DeviceEvent;
    use std::sync::Mutex;

    /// One feature-report answer of the mock: bytes, or a read timeout.
    enum MockAnswer {
        Bytes(Vec<u8>),
        Timeout,
    }

    #[derive(Default)]
    struct MockState {
        sent: Vec<(ModuleId, Vec<u8>)>,
        queries: Vec<(ModuleId, u8)>,
    }

    /// A transport that records every report and query, acks writes, and answers feature
    /// reads from a fixed table — the DS-014/DS-021 virtual path's semantics, in-process.
    struct MockTransport {
        state: Mutex<MockState>,
        fail_sends: bool,
        answers: HashMap<u8, MockAnswer>,
    }

    impl MockTransport {
        fn new(answers: HashMap<u8, MockAnswer>) -> Self {
            Self {
                state: Mutex::new(MockState::default()),
                fail_sends: false,
                answers,
            }
        }

        fn sent(&self) -> Vec<(ModuleId, Vec<u8>)> {
            self.state.lock().unwrap().sent.clone()
        }

        fn queries(&self) -> Vec<(ModuleId, u8)> {
            self.state.lock().unwrap().queries.clone()
        }
    }

    impl Transport for MockTransport {
        fn send(&self, module: &ModuleId, report: &Report) -> Result<usize> {
            self.state
                .lock()
                .unwrap()
                .sent
                .push((module.clone(), report.bytes().to_vec()));
            if self.fail_sends {
                return Err(Error::Timeout(Duration::from_secs(5)));
            }
            Ok(report.bytes().len())
        }

        fn query(&self, module: &ModuleId, feature_id: u8, timeout: Duration) -> Result<Vec<u8>> {
            self.state
                .lock()
                .unwrap()
                .queries
                .push((module.clone(), feature_id));
            match self.answers.get(&feature_id) {
                Some(MockAnswer::Bytes(b)) => Ok(b.clone()),
                Some(MockAnswer::Timeout) | None => Err(Error::Timeout(timeout)),
            }
        }

        fn recv(&self, timeout: Duration) -> Result<DeviceEvent> {
            Err(Error::Timeout(timeout))
        }

        fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    fn id(kind: ModuleKind) -> ModuleId {
        ModuleId {
            kind,
            serial: format!("MOCK-{}", kind.name()),
        }
    }

    /// Zero waits, short query timeout — the state machine is driven without real sleeps.
    fn test_cfg() -> HandshakeConfig {
        HandshakeConfig {
            settle: Duration::ZERO,
            re_settle: Duration::ZERO,
            retry_gap: Duration::ZERO,
            attempts: 3,
            query_timeout: Duration::from_millis(50),
        }
    }

    /// A hardware-captured firmware answer (`PROTOCOL.md`): `[id, len, 4 bytes, "02.00.01.00"]`.
    fn firmware_answer(feature_id: u8) -> Vec<u8> {
        let mut a = vec![feature_id, 0x0F, 0x29, 0x63, 0x1c, 0x5d];
        a.extend_from_slice(b"02.00.01.00");
        a
    }

    fn on_report(kind: ModuleKind) -> Vec<u8> {
        ds_protocol::enter_software_mode(kind)
            .unwrap()
            .0
            .bytes()
            .to_vec()
    }

    fn off_report(kind: ModuleKind) -> Vec<u8> {
        ds_protocol::leave_software_mode(kind)
            .unwrap()
            .bytes()
            .to_vec()
    }

    #[test]
    fn happy_path_keys_sends_the_required_sequence_then_confirms() {
        let mut answers = HashMap::new();
        answers.insert(0x38, MockAnswer::Bytes(firmware_answer(0x38)));
        let mock = MockTransport::new(answers);
        let keys = id(ModuleKind::Keys);
        let mut hs = Handshake::new(&mock, keys.clone(), test_cfg());
        let mode = hs.run().unwrap();

        assert_eq!(mode.kind(), ModuleKind::Keys);
        assert_eq!(ds_protocol::query_id(mode, Query::Firmware), Some(0x38));
        assert_eq!(hs.phase(), Phase::Done);
        assert_eq!(hs.firmware(), Some("02.00.01.00"));
        // exactly two on-reports (`03 3A 01`), nothing else, then one firmware read
        let sent = mock.sent();
        assert_eq!(sent.len(), 2);
        for (_, bytes) in &sent {
            assert_eq!(bytes, &[0x03, 0x3A, 0x01]);
        }
        assert_eq!(mock.queries(), vec![(keys.clone(), 0x38)]);
    }

    #[test]
    fn happy_path_steps_through_the_phases_in_order() {
        let mut answers = HashMap::new();
        answers.insert(0x48, MockAnswer::Bytes(firmware_answer(0x48)));
        let mock = MockTransport::new(answers);
        let dial = id(ModuleKind::Dial);
        let mut hs = Handshake::new(&mock, dial.clone(), test_cfg());

        assert_eq!(hs.phase(), Phase::Start);
        assert!(!hs.step().unwrap());
        assert_eq!(hs.phase(), Phase::ModeSent);
        assert!(!hs.step().unwrap());
        assert_eq!(hs.phase(), Phase::Settled);
        assert!(!hs.step().unwrap());
        assert_eq!(hs.phase(), Phase::Resent);
        assert!(!hs.step().unwrap());
        assert_eq!(hs.phase(), Phase::Confirming);
        assert!(hs.step().unwrap());
        assert_eq!(hs.phase(), Phase::Done);

        let sent = mock.sent();
        assert_eq!(sent.len(), 2);
        for (_, bytes) in &sent {
            assert_eq!(bytes, &[0x03, 0x49, 0x01]); // dial handshake
        }
        assert_eq!(mock.queries(), vec![(dial, 0x48)]);
    }

    #[test]
    fn m0_modules_need_no_reports_and_are_ready_immediately() {
        let mock = MockTransport::new(HashMap::new());
        for kind in [
            ModuleKind::Base,
            ModuleKind::Faders,
            ModuleKind::Knobs,
            ModuleKind::Rollers,
        ] {
            let mut hs = Handshake::new(&mock, id(kind), test_cfg());
            let mode = hs.run().unwrap();
            assert_eq!(mode.kind(), kind);
            assert_eq!(hs.phase(), Phase::Done);
        }
        assert!(mock.sent().is_empty());
        assert!(mock.queries().is_empty());
    }

    #[test]
    fn a_failed_send_is_never_followed_by_a_read() {
        let mock = MockTransport {
            state: Mutex::new(MockState::default()),
            fail_sends: true,
            answers: HashMap::new(),
        };
        let keys = id(ModuleKind::Keys);
        let mut hs = Handshake::new(&mock, keys, test_cfg());
        let err = hs.run().unwrap_err();
        assert!(matches!(err, Error::Timeout(_)));
        assert_eq!(hs.phase(), Phase::Failed);
        // the on-report was attempted exactly once, and no query was ever issued
        assert_eq!(mock.sent().len(), 1);
        assert_eq!(mock.sent()[0].1, on_report(ModuleKind::Keys));
        assert!(mock.queries().is_empty());
    }

    #[test]
    fn no_answer_retries_bounded_then_fails_with_a_clear_error() {
        // the module acks the writes but its firmware read carries no version (wedged-ish)
        let mut answers = HashMap::new();
        answers.insert(0x38, MockAnswer::Bytes(vec![0x38, 0x00]));
        let mock = MockTransport::new(answers);
        let keys = id(ModuleKind::Keys);
        let cfg = HandshakeConfig {
            attempts: 2,
            ..test_cfg()
        };
        let mut hs = Handshake::new(&mock, keys.clone(), cfg);
        let err = hs.run().unwrap_err();
        assert!(matches!(err, Error::Device(_)), "{err:?}");
        assert!(err.to_string().contains("firmware"), "{err}");
        assert_eq!(hs.phase(), Phase::Failed);
        // attempt 1: on, on; retry: off; attempt 2: on, on — then give up
        let sent: Vec<Vec<u8>> = mock.sent().iter().map(|(_, b)| b.clone()).collect();
        assert_eq!(
            sent,
            vec![
                on_report(ModuleKind::Keys),
                on_report(ModuleKind::Keys),
                off_report(ModuleKind::Keys),
                on_report(ModuleKind::Keys),
                on_report(ModuleKind::Keys),
            ]
        );
        assert_eq!(mock.queries(), vec![(keys.clone(), 0x38), (keys, 0x38)]);
    }

    #[test]
    fn a_read_timeout_is_retried_bounded_then_surfaces() {
        let mut answers = HashMap::new();
        answers.insert(0x38, MockAnswer::Timeout);
        let mock = MockTransport::new(answers);
        let cfg = HandshakeConfig {
            attempts: 2,
            ..test_cfg()
        };
        let mut hs = Handshake::new(&mock, id(ModuleKind::Keys), cfg);
        let err = hs.run().unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{err:?}");
        assert_eq!(mock.queries().len(), 2);
        assert_eq!(mock.sent().len(), 5); // on, on, off, on, on
    }

    #[test]
    fn reads_are_refused_before_the_handshake_and_on_unknown_modules() {
        let mut answers = HashMap::new();
        answers.insert(0x38, MockAnswer::Bytes(firmware_answer(0x38)));
        answers.insert(0x2E, MockAnswer::Bytes(firmware_answer(0x2E)));
        let mock = MockTransport::new(answers);
        let keys = id(ModuleKind::Keys);
        let dial = id(ModuleKind::Dial);

        // pre-init: the pending wrapper refuses the read and never touches the transport
        let p = pending(&mock, keys.clone(), test_cfg());
        assert!(matches!(p.query(Query::Firmware), Err(Error::Device(_))));
        assert!(mock.queries().is_empty());

        // post-init transport: the handshaken module works, a foreign module is refused
        let init = InitTransport::init(&mock, std::slice::from_ref(&keys), test_cfg()).unwrap();
        assert!(init.state(&keys).is_some());
        assert!(init.state(&dial).is_none());
        assert!(matches!(init.module(&dial), Err(Error::Device(_))));
        let hs = ds_protocol::enter_software_mode(ModuleKind::Dial)
            .unwrap()
            .0;
        assert!(matches!(init.send(&dial, &hs), Err(Error::Device(_))));
        assert!(matches!(
            init.query(&dial, Query::Firmware),
            Err(Error::Device(_))
        ));
        // and nothing was sent to the dial
        assert!(mock.sent().iter().all(|(m, _)| m.kind == ModuleKind::Keys));
        // the handshaken module answers through the safe path
        let answer = init.query(&keys, Query::Firmware).unwrap();
        assert_eq!(
            ds_protocol::parse_firmware(&answer).as_deref(),
            Some("02.00.01.00")
        );
    }

    #[test]
    fn init_transport_identifies_modules_by_kind_and_pid() {
        let mut answers = HashMap::new();
        answers.insert(0x38, MockAnswer::Bytes(firmware_answer(0x38)));
        answers.insert(0x2E, MockAnswer::Bytes(firmware_answer(0x2E)));
        let mock = MockTransport::new(answers);
        let keys = id(ModuleKind::Keys);
        let base = id(ModuleKind::Base);
        let init = InitTransport::init(&mock, &[base.clone(), keys.clone()], test_cfg()).unwrap();

        let mut ids = init.modules();
        ids.sort();
        assert_eq!(ids, vec![base.clone(), keys.clone()]);
        let st = init.state(&keys).unwrap();
        assert_eq!(st.info.pid, 0x01FE);
        assert_eq!(st.info.kind, ModuleKind::Keys);
        assert_eq!(st.firmware.as_deref(), Some("02.00.01.00"));
        assert_eq!(st.mode.kind(), ModuleKind::Keys);
        let st = init.state(&base).unwrap();
        assert_eq!(st.info.pid, 0x01F6);
        assert_eq!(
            st.firmware, None,
            "M0 init sends no reports and reads nothing"
        );
        assert!(mock
            .sent()
            .iter()
            .all(|(_, b)| b == &on_report(ModuleKind::Keys)));
    }

    #[test]
    fn post_init_writes_go_through_the_safe_builders() {
        let mut answers = HashMap::new();
        answers.insert(0x38, MockAnswer::Bytes(firmware_answer(0x38)));
        answers.insert(0x3C, MockAnswer::Bytes(vec![0x3C, 0x01, 100]));
        let mock = MockTransport::new(answers);
        let keys = id(ModuleKind::Keys);
        let init = InitTransport::init(&mock, std::slice::from_ref(&keys), test_cfg()).unwrap();
        let module = init.module(&keys).unwrap();

        // backlight write (never 0), read back the state
        assert_eq!(module.set_backlight(80).unwrap(), 3);
        let answer = init.query(&keys, Query::Backlight).unwrap();
        assert_eq!(ds_protocol::parse_percent(&answer), Some(100));

        // a chunked panel image: every report guarded and sent
        let jpeg: Vec<u8> = (0..700u32).map(|i| (i % 256) as u8).collect();
        let before_image = mock.sent().len();
        module.send_panel_image(&jpeg).unwrap();
        let sent = mock.sent();
        let image_reports = &sent[before_image..]; // after the handshake + backlight reports
        assert!(image_reports.len() > 1);
        assert_eq!(
            &image_reports[0].1[..8],
            &[0x02, 0x35, 0x00, 0x00, 0x38, 0x01, 0x00, 0x00]
        );
        assert_eq!(image_reports.last().unwrap().1[7], 1);

        // a wrong-module image is refused before anything is sent
        let before = mock.sent().len();
        assert!(matches!(
            module.send_dial_image(&jpeg),
            Err(Error::Device(_))
        ));
        assert_eq!(mock.sent().len(), before);
    }

    #[test]
    fn every_safe_builder_passes_the_forbidden_guard() {
        // raw forbidden bytes are refused by the guard the builders are checked against
        assert!(matches!(
            ensure_allowed(&[0x02, 0x2A, 0x00, 0x00]),
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            ensure_allowed(&[0x03, 0x35, b'M', b'H']),
            Err(Error::Forbidden)
        ));

        let jpeg = [0xFFu8; 700];
        let images: Vec<Vec<Report>> = vec![
            safe::key_image(0, &jpeg).unwrap(),
            safe::key_image(14, &jpeg).unwrap(),
            safe::panel_image(&jpeg).unwrap(),
            safe::dial_image(&jpeg).unwrap(),
        ];
        for reports in &images {
            for r in reports {
                assert!(!r.is_forbidden());
                assert!(ensure_allowed(r.bytes()).is_ok());
            }
        }

        for kind in ModuleKind::ALL {
            for target in [LedTarget::All, LedTarget::Index(0)] {
                for mode in [
                    LedMode::Off,
                    LedMode::Static,
                    LedMode::Breathing,
                    LedMode::Rainbow,
                ] {
                    if let Ok(r) = safe::led_mode(kind, target, mode) {
                        assert!(!r.is_forbidden());
                        assert!(ensure_allowed(r.bytes()).is_ok());
                    }
                    if let Ok(r) = safe::led_color(kind, target, [1, 2, 3]) {
                        assert!(!r.is_forbidden());
                        assert!(ensure_allowed(r.bytes()).is_ok());
                    }
                    if let Ok(r) = safe::led_brightness(kind, target, 42) {
                        assert!(!r.is_forbidden());
                        assert!(ensure_allowed(r.bytes()).is_ok());
                    }
                }
            }
            if let Ok(r) = safe::led_speed(kind, 50) {
                assert!(!r.is_forbidden());
                assert!(ensure_allowed(r.bytes()).is_ok());
            }
            if let Ok(r) = safe::fill_screen(kind, [9, 9, 9]) {
                assert!(!r.is_forbidden());
                assert!(ensure_allowed(r.bytes()).is_ok());
            }
            for pct in [1u8, 20, 100] {
                if let Ok(r) = safe::backlight(kind, pct) {
                    assert!(!r.is_forbidden());
                    assert!(ensure_allowed(r.bytes()).is_ok());
                }
            }
        }
        if let Ok(r) = safe::fill_key(3, [1, 2, 3]) {
            assert!(!r.is_forbidden());
            assert!(ensure_allowed(r.bytes()).is_ok());
        }
    }

    #[test]
    fn backlight_zero_is_refused_and_brightness_stays_in_range() {
        // never backlight-0, on the dial and everywhere else
        for kind in ModuleKind::ALL {
            let err = safe::backlight(kind, 0).unwrap_err();
            assert!(matches!(err, Error::Device(_)), "{err}");
            assert!(err.to_string().contains("backlight 0"), "{err}");
        }
        // clamped at 100 on the wire
        assert_eq!(
            safe::backlight(ModuleKind::Dial, 250).unwrap().bytes(),
            &[0x03, 0x44, 100]
        );
        assert_eq!(
            safe::backlight(ModuleKind::Keys, 100).unwrap().bytes(),
            &[0x03, 0x34, 100]
        );
        assert_eq!(
            safe::led_brightness(ModuleKind::Faders, LedTarget::All, 255)
                .unwrap()
                .bytes(),
            &[0x02, 0x2E, 1, 0xFF, 100]
        );
        assert_eq!(
            safe::led_speed(ModuleKind::Knobs, 200).unwrap().bytes(),
            &[0x02, 0x30, 100]
        );
        // out-of-range key index produces no report
        assert!(safe::key_image(15, &[0xFF; 4]).is_err());
        // modules without a screen / without LEDs refuse the builder
        assert!(safe::backlight(ModuleKind::Faders, 50).is_err());
        assert!(safe::led_color(ModuleKind::Dial, LedTarget::All, [0; 3]).is_err());
    }
}

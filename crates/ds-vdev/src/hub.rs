//! The virtual MasterHub itself: report-level state shared by every connected host. One
//! [`Hub`] serves the module set of one deck layout; the socket server (`server.rs`) feeds it
//! decoded frames and sends its replies.
//!
//! Behaviour (mirrors `docs/components/virtual-device.md`):
//!
//! - every WRITE/FEATURE_SET is checked against the forbidden-command list and refused with
//!   an `0x84` error frame instead of an ack — so tests can assert the daemon never emits them;
//! - chunked image pushes (`02 35/36/45 …`) are reassembled per target with the ds-device
//!   [`Reassembler`] (sha256 computed on completion, stored as the target's latest frame);
//! - FEATURE_GET is answered from fixed, deterministic virtual-module data (firmware, serial,
//!   screen info, backlight, fader values, orientation);
//! - `inject` broadcasts `0x81` INPUT frames to every connected host.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ds_device::chunks::Reassembler;
use ds_device::frame::{kind, Frame};
use ds_device::module::{module_info, module_info_for_pid, ModuleInfo};
use ds_device::report::{view_report, ReportView};
use ds_device::slot::{Slot, BASE_MCU_SLOT, SLOT_FEATURE_ID};
use ds_protocol::{ModuleKind, Orientation};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::inject::{event_frame, InjectEvent};
use crate::target::Target;

/// Virtual firmware version reported by every module's firmware feature read.
pub const VIRTUAL_FIRMWARE: &str = "01.00.00.00";
/// Default backlight percent reported before the host sets one.
pub const DEFAULT_BACKLIGHT: u8 = 50;

/// The DS-112 reference (rot0) slot anchors per module kind, base flat, corner indicator in
/// the fixed standard position — the SALON-001 reference sweep
/// (`docs/discovery/salon-001/README.md`, connector-map-ref-labeled.csv):
/// knobs `1-2-3`, keys `2-3`, faders `1-1-3`, rollers `3-3`, dial `1-3-4`, base MCU
/// `1-4-4`.
pub fn reference_slot(kind: ModuleKind) -> Slot {
    let s = match kind {
        ModuleKind::Knobs => "1-2-3",
        ModuleKind::Keys => "2-3",
        ModuleKind::Faders => "1-1-3",
        ModuleKind::Rollers => "3-3",
        ModuleKind::Dial => "1-3-4",
        ModuleKind::Base => BASE_MCU_SLOT,
    };
    Slot::parse(s).expect("reference slots are valid port paths")
}

// The throttle/backpressure knobs (DS-035) live in `throttle.rs`; re-exported here so the
// `hub::Throttle` path (and `ds_vdev::Throttle`) keeps working.
pub use crate::throttle::{CreditState, CreditWindow, Throttle};

/// What to seed the hub with.
#[derive(Debug, Clone)]
pub struct HubConfig {
    /// The modules of the deck layout this hub emulates (default: all six kinds).
    pub modules: Vec<ModuleKind>,
    /// Initial throttle knobs.
    pub throttle: Throttle,
}

impl Default for HubConfig {
    fn default() -> Self {
        HubConfig {
            modules: ModuleKind::ALL.to_vec(),
            throttle: Throttle::none(),
        }
    }
}

/// The latest reassembled frame of one target — the golden-frame oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameEntry {
    /// Target name (`panel`, `key:N`, `dial`).
    pub target: String,
    /// sha256 of the reassembled bytes, lowercase hex.
    pub sha256: String,
    /// Size of the reassembled bytes.
    pub bytes: usize,
    /// Sequence number of the write that completed the transfer.
    pub seq: u64,
}

/// One accepted (or refused) host→device report, for `ds-vdevctl writes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRecord {
    pub seq: u64,
    pub pid: u16,
    pub kind: u8,
    pub len: usize,
    /// Whether the report was refused (forbidden / reassembly error) instead of acked.
    pub refused: bool,
    /// Milliseconds since the hub started.
    pub at_ms: u128,
    /// For image-chunk writes: `(chunk index, last)` — the write-log oracle for whole-frame
    /// transfers (DS-035: the device never sees a partial chunk sequence).
    pub chunk: Option<(u16, bool)>,
    /// The raw report bytes for short (non-chunk) writes, so `writes <n> full` can show the
    /// payload (LED reports are 8-9 meaningful bytes padded to the module's out_len).
    /// Empty for image chunks, which would dominate the log's memory.
    pub data: Vec<u8>,
}

/// Payload recorded for the write log: short reports only (image chunks are skipped to keep
/// the log bounded).
fn record_data(payload: &[u8], chunk: Option<(u16, bool)>) -> Vec<u8> {
    match chunk {
        Some(_) => Vec::new(),
        None if payload.len() <= 64 => payload.to_vec(),
        None => Vec::new(),
    }
}

/// What the connection loop should do after one request frame.
#[derive(Debug)]
pub struct Outcome {
    /// Reply frames to send, in order (ack/answer/error).
    pub replies: Vec<Frame>,
    /// Artificial delay before the replies (the `--throttle-kbps` knob).
    pub delay: Duration,
    /// Close the connection after this frame (CLOSE, or `--drop-after` hot-unplug).
    pub close: bool,
}

#[derive(Debug)]
struct StoredFrame {
    target: Target,
    sha256: [u8; 32],
    bytes: usize,
    seq: u64,
}

#[derive(Debug)]
struct HubState {
    modules: Vec<ModuleInfo>,
    reassembler: Reassembler,
    frames: HashMap<(u8, u8), StoredFrame>,
    writes: Vec<WriteRecord>,
    software_mode: HashMap<u16, bool>,
    backlight: HashMap<u16, u8>,
    /// DS-112: the current USB slot (port path) of each module, by pid — the SALON-001
    /// reference (rot0) anchors at boot, changed by `move` (a reseat in another connector).
    slots: HashMap<u16, Slot>,
    /// DS-112: the base's current orientation (flat = 0x01 horizontal, upright = 0x00
    /// vertical — a one-direction tilt switch).
    orientation: Orientation,
    throttle: Throttle,
    /// Credit-exhaustion window (DS-035), armed via `set_credits` / `throttle … credits=…`.
    credits: Option<CreditState>,
    seq: u64,
    accepted: u64,
    started: Instant,
}

/// The shared virtual hub.
#[derive(Debug)]
pub struct Hub {
    state: Mutex<HubState>,
    clients: Mutex<HashMap<usize, Sender<Frame>>>,
    next_client: Mutex<usize>,
    /// Set by `replug()`: every open device connection closes on its next poll, so the host
    /// sees a disconnect and its rescan reconnects (a hot replug). Cleared when the first
    /// new connection registers.
    replug_requested: AtomicBool,
}

impl Hub {
    /// A hub seeded by `config`.
    pub fn new(config: HubConfig) -> Self {
        Hub {
            state: Mutex::new(HubState {
                modules: config.modules.iter().map(|&k| module_info(k)).collect(),
                reassembler: Reassembler::new(),
                frames: HashMap::new(),
                writes: Vec::new(),
                software_mode: config
                    .modules
                    .iter()
                    .map(|&k| (module_info(k).pid, false))
                    .collect(),
                backlight: config
                    .modules
                    .iter()
                    .map(|&k| (module_info(k).pid, DEFAULT_BACKLIGHT))
                    .collect(),
                slots: config
                    .modules
                    .iter()
                    .map(|&k| (module_info(k).pid, reference_slot(k)))
                    .collect(),
                orientation: Orientation::Horizontal,
                throttle: config.throttle,
                credits: None,
                seq: 0,
                accepted: 0,
                started: Instant::now(),
            }),
            clients: Mutex::new(HashMap::new()),
            next_client: Mutex::new(0),
            replug_requested: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HubState> {
        self.state.lock().expect("hub state poisoned")
    }

    /// Register a device connection; returns its id and the queue of INPUT frames to send it.
    pub fn register_client(&self) -> (usize, Receiver<Frame>) {
        let (tx, rx) = channel();
        let mut next = self.next_client.lock().expect("clients poisoned");
        let id = *next;
        *next += 1;
        self.clients
            .lock()
            .expect("clients poisoned")
            .insert(id, tx);
        // A new device connection is a replug: the --drop-after write budget is per
        // connection, so a reconnect after a simulated unplug gets a fresh budget. It also
        // clears a pending replug request (the fresh connection is the replugged device).
        self.replug_requested.store(false, Ordering::SeqCst);
        self.lock().accepted = 0;
        (id, rx)
    }

    /// Unregister a device connection.
    pub fn unregister_client(&self, id: usize) {
        self.clients.lock().expect("clients poisoned").remove(&id);
    }

    /// The DS-112 slot of one module (by kind), if the hub serves it.
    pub fn slot(&self, kind: ModuleKind) -> Option<Slot> {
        self.lock().slots.get(&module_info(kind).pid).cloned()
    }

    /// All current module slots as `(kind, slot)` pairs, sorted by kind name
    /// (`ds-vdevctl slots`).
    pub fn slots(&self) -> Vec<(ModuleKind, Slot)> {
        let st = self.lock();
        let mut out: Vec<(ModuleKind, Slot)> = st
            .modules
            .iter()
            .filter_map(|info| st.slots.get(&info.pid).map(|s| (info.kind, s.clone())))
            .collect();
        out.sort_by_key(|(k, _)| k.name());
        out
    }

    /// "Move" a module to another connector: change its slot in place (same serial, same
    /// pid — a reseat). The host's next rescan sees the changed `(slot, orientation)`
    /// identity key. Returns the new position.
    pub fn set_slot(&self, kind: ModuleKind, slot: Slot) -> Result<u8, String> {
        let pid = module_info(kind).pid;
        let mut st = self.lock();
        if !st.slots.contains_key(&pid) {
            return Err(format!("module {} is not served by this hub", kind.name()));
        }
        // A connector seats exactly one module: reject moves onto an occupied slot
        // (the physical base would not allow two modules in one connector).
        if let Some(other) = st.slots.iter().find(|(p, s)| **s == slot && **p != pid) {
            return Err(format!(
                "slot {} is occupied by pid {:#06x}",
                slot.as_str(),
                other.0
            ));
        }
        let pos = slot.position();
        st.slots.insert(pid, slot);
        Ok(pos)
    }

    /// The base's current orientation.
    pub fn orientation(&self) -> Orientation {
        self.lock().orientation
    }

    /// Rotate the base: set the orientation and broadcast the orientation input report to
    /// every connected host (exactly what the hardware's tilt switch does, SALON-001
    /// finding 1 — the daemon also re-reads the orientation feature 0x35 on its rescan
    /// tick, so a broadcast lost in a reconnect window is recovered within one tick).
    /// Returns the new orientation.
    pub fn set_orientation(&self, o: Orientation) -> Orientation {
        {
            let mut st = self.lock();
            st.orientation = o;
        }
        let ev = InjectEvent {
            module: "base".into(),
            at_ms: 0,
            keys: None,
            faders: None,
            knob: None,
            knob_delta: None,
            knob_press: None,
            pressed: None,
            roller: None,
            roller_delta: None,
            dial_delta: None,
            dial_pressed: None,
            orientation: Some(match o {
                Orientation::Horizontal => "horizontal".to_string(),
                Orientation::Vertical => "vertical".to_string(),
            }),
        };
        let _ = self.inject(&ev);
        o
    }

    /// Hot replug: close every open device connection (the hosts see `Disconnected` and
    /// rescan; the socket stays bound, so the rescan lands on the same hub). Returns how
    /// many connections were dropped.
    pub fn replug(&self) -> usize {
        let n = self.clients.lock().expect("clients poisoned").len();
        self.replug_requested.store(true, Ordering::SeqCst);
        n
    }

    /// Whether a replug was requested and no fresh connection has registered since (the
    /// device-connection loop polls this and closes its stream).
    pub fn replug_requested(&self) -> bool {
        self.replug_requested.load(Ordering::SeqCst)
    }

    /// Broadcast one device→host frame (an injected input report) to every connected host;
    /// returns how many hosts received it.
    pub fn broadcast(&self, frame: Frame) -> usize {
        let clients = self.clients.lock().expect("clients poisoned");
        clients
            .values()
            .filter(|tx| tx.send(frame.clone()).is_ok())
            .count()
    }

    /// Inject one scripted event: encode it and broadcast the INPUT frame.
    pub fn inject(&self, event: &InjectEvent) -> Result<usize, String> {
        let frame = event_frame(event)?;
        Ok(self.broadcast(frame))
    }

    /// Handle one request frame from a host and say what to reply.
    pub fn handle_frame(&self, f: &Frame) -> Outcome {
        match f.kind {
            kind::WRITE | kind::FEATURE_SET => self.handle_write(f),
            kind::FEATURE_GET => Outcome {
                replies: vec![self.handle_query(f)],
                delay: Duration::ZERO,
                close: false,
            },
            kind::CLOSE => Outcome {
                replies: Vec::new(),
                delay: Duration::ZERO,
                close: true,
            },
            other => Outcome {
                replies: vec![error_frame(
                    f.pid,
                    format!("unexpected device-bound kind {other:#04x}"),
                )],
                delay: Duration::ZERO,
                close: false,
            },
        }
    }

    fn handle_write(&self, f: &Frame) -> Outcome {
        let mut st = self.lock();
        st.seq += 1;
        let seq = st.seq;
        let at_ms = st.started.elapsed().as_millis();

        // Safety rule enforced device-side too: forbidden reports get an error, never an ack.
        if ds_protocol::is_forbidden(&f.payload) {
            st.writes.push(WriteRecord {
                seq,
                pid: f.pid,
                kind: f.kind,
                len: f.payload.len(),
                refused: true,
                at_ms,
                chunk: None,
                data: record_data(&f.payload, None),
            });
            return Outcome {
                replies: vec![error_frame(
                    f.pid,
                    "refusing a forbidden report (firmware, serial number or factory command)"
                        .to_string(),
                )],
                delay: Duration::ZERO,
                close: false,
            };
        }
        if module_info_for_pid(f.pid).is_none() {
            st.writes.push(WriteRecord {
                seq,
                pid: f.pid,
                kind: f.kind,
                len: f.payload.len(),
                refused: true,
                at_ms,
                chunk: None,
                data: record_data(&f.payload, None),
            });
            return Outcome {
                replies: vec![error_frame(
                    f.pid,
                    format!("unknown module pid {:#06x}", f.pid),
                )],
                delay: Duration::ZERO,
                close: false,
            };
        }

        // --drop-after: simulate a hot-unplug — no ack, the connection just dies.
        st.accepted += 1;
        if st.throttle.drop_exceeded(st.accepted) {
            return Outcome {
                replies: Vec::new(),
                delay: Duration::ZERO,
                close: true,
            };
        }

        // --throttle-kbps paces the ack by the bytes of this report; the credit window
        // (DS-035) additionally holds the ack when its bucket is empty — output writes
        // only, feature traffic (handshake, queries) is never held or charged.
        let mut delay = st.throttle.delay_for(f.payload.len());
        if f.kind == kind::WRITE {
            let now = st.started.elapsed();
            if let Some(credits) = st.credits.as_mut() {
                delay += credits.take(now);
            }
        }
        let mut refused = false;
        let mut error = None;
        let mut chunk = None;

        if f.kind == kind::FEATURE_SET {
            if let ReportView::Feature { cmd, rest } = view_report(&f.payload) {
                match (cmd, rest.first()) {
                    (0x3A | 0x49, Some(&on)) => {
                        st.software_mode.insert(f.pid, on != 0);
                    }
                    (0x34 | 0x44, Some(&pct)) => {
                        st.backlight.insert(f.pid, pct);
                    }
                    _ => {}
                }
            }
        } else if let ReportView::ImageChunk(c) = view_report(&f.payload) {
            chunk = Some((c.chunk, c.last));
            // a transfer starts at chunk 0 (restarting any partial previous attempt)
            if c.chunk == 0 {
                st.reassembler.begin(c.cmd, c.index, None);
            }
            match st.reassembler.push(&f.payload) {
                Ok(Some(done)) => {
                    let (cmd, index) = (done.cmd, done.index);
                    let target = match cmd {
                        0x35 => Target::Panel,
                        0x36 => Target::Key(index),
                        _ => Target::Dial,
                    };
                    st.frames.insert(
                        (cmd, index),
                        StoredFrame {
                            target,
                            sha256: done.sha256,
                            bytes: done.data.len(),
                            seq,
                        },
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    refused = true;
                    error = Some(format!("chunk reassembly failed: {e}"));
                }
            }
        }

        st.writes.push(WriteRecord {
            seq,
            pid: f.pid,
            kind: f.kind,
            len: f.payload.len(),
            refused,
            at_ms,
            chunk,
            data: record_data(&f.payload, chunk),
        });

        let replies = if let Some(msg) = error {
            vec![error_frame(f.pid, msg)]
        } else {
            vec![Frame::new(
                kind::ACK,
                f.pid,
                (f.payload.len() as u32).to_le_bytes().to_vec(),
            )
            .expect("ack frame size is bounded")]
        };
        Outcome {
            replies,
            delay,
            close: false,
        }
    }

    fn handle_query(&self, f: &Frame) -> Frame {
        let Some(info) = module_info_for_pid(f.pid) else {
            return error_frame(f.pid, format!("unknown module pid {:#06x}", f.pid));
        };
        let Some(&feature_id) = f.payload.first() else {
            return error_frame(f.pid, "feature get needs a feature id byte".to_string());
        };
        match answer(&info, feature_id, &self.lock()) {
            Ok(mut data) => {
                data.resize(info.feature_len, 0); // feature reports are 32 B everywhere
                match Frame::new(kind::ANSWER, f.pid, data) {
                    Ok(frame) => frame,
                    Err(e) => error_frame(f.pid, format!("answer: {e}")),
                }
            }
            Err(msg) => error_frame(f.pid, msg),
        }
    }

    /// All stored frames as entries, sorted by target name (`ds-vdevctl frames`).
    pub fn frames(&self) -> Vec<FrameEntry> {
        let st = self.lock();
        let mut out: Vec<FrameEntry> = st
            .frames
            .values()
            .map(|f| FrameEntry {
                target: f.target.name(),
                sha256: f.sha256.iter().map(|b| format!("{b:02x}")).collect(),
                bytes: f.bytes,
                seq: f.seq,
            })
            .collect();
        out.sort_by(|a, b| a.target.cmp(&b.target));
        out
    }

    /// The stored frame of one target, if any.
    pub fn frame(&self, target: Target) -> Option<FrameEntry> {
        let (cmd, index) = target.wire_key();
        self.lock().frames.get(&(cmd, index)).map(|f| FrameEntry {
            target: f.target.name(),
            sha256: f.sha256.iter().map(|b| format!("{b:02x}")).collect(),
            bytes: f.bytes,
            seq: f.seq,
        })
    }

    /// Module report lines (`ds-vdevctl reports`): one line per served module, with the
    /// DS-112 slot + connector position appended.
    pub fn reports(&self) -> Vec<String> {
        let st = self.lock();
        st.modules
            .iter()
            .map(|info| {
                let pid = info.pid;
                let frames: Vec<String> = st
                    .frames
                    .values()
                    .filter(|f| {
                        let (cmd, _) = f.target.wire_key();
                        let module_pid = match cmd {
                            0x35 | 0x36 => module_info(ModuleKind::Keys).pid,
                            _ => module_info(ModuleKind::Dial).pid,
                        };
                        module_pid == pid
                    })
                    .map(|f| f.target.name())
                    .collect();
                let (slot, position) = match st.slots.get(&pid) {
                    Some(s) => (s.as_str().to_string(), s.position()),
                    None => ("?".to_string(), 0),
                };
                format!(
                    "{} pid={:#06x} out_len={} feature_len={} input_tag={:#04x} software_mode={} backlight={} frames=[{}] slot={} position={}",
                    info.kind.name(),
                    pid,
                    info.out_len,
                    info.feature_len,
                    info.input_tag,
                    st.software_mode.get(&pid).copied().unwrap_or(false),
                    st.backlight.get(&pid).copied().unwrap_or(DEFAULT_BACKLIGHT),
                    frames.join(","),
                    slot,
                    position,
                )
            })
            .collect()
    }

    /// The write log, newest last, capped at `limit` entries (`ds-vdevctl writes`).
    pub fn writes(&self, limit: usize) -> Vec<WriteRecord> {
        let st = self.lock();
        let start = st.writes.len().saturating_sub(limit);
        st.writes[start..].to_vec()
    }

    /// Current throttle knobs.
    pub fn throttle(&self) -> Throttle {
        self.lock().throttle
    }

    /// Set the throttle knobs at runtime (`ds-vdevctl throttle`).
    pub fn set_throttle(&self, throttle: Throttle) {
        self.lock().throttle = throttle;
    }

    /// The armed credit-exhaustion window, if any (DS-035).
    pub fn credits(&self) -> Option<CreditWindow> {
        self.lock().credits.as_ref().map(CreditState::window)
    }

    /// Arm or clear the credit-exhaustion window at runtime; arming resets the bucket to
    /// full. Reachable from `ds-vdevctl … throttle … credits=…` and the Rust API.
    pub fn set_credits(&self, window: Option<CreditWindow>) {
        let mut st = self.lock();
        st.credits = window.map(|w| CreditState::new(w, st.started.elapsed()));
    }

    /// Credits left in the bucket right now (`None` when the window is off).
    pub fn credit_remaining(&self) -> Option<u32> {
        self.lock().credits.as_ref().map(CreditState::remaining)
    }

    /// One line of the control protocol (`<socket>.ctl`, see `server.rs`).
    pub fn control(&self, line: &str) -> Result<Vec<String>, String> {
        let mut parts = line.trim().splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();
        match cmd {
            "ping" => Ok(vec!["pong".to_string()]),
            "frames" => {
                if rest.is_empty() {
                    Ok(self
                        .frames()
                        .iter()
                        .map(|f| format!("{} {} {}B seq={}", f.target, f.sha256, f.bytes, f.seq))
                        .collect())
                } else {
                    let target =
                        Target::parse(rest).ok_or_else(|| format!("unknown target '{rest}'"))?;
                    match self.frame(target) {
                        Some(f) => Ok(vec![format!(
                            "{} {} {}B seq={}",
                            f.target, f.sha256, f.bytes, f.seq
                        )]),
                        None => Err(format!("no frame for target '{rest}' yet")),
                    }
                }
            }
            "reports" => Ok(self.reports()),
            "slots" => Ok(self
                .slots()
                .iter()
                .map(|(kind, slot)| {
                    format!(
                        "{} pid={:#06x} slot={} position={}",
                        kind.name(),
                        module_info(*kind).pid,
                        slot.as_str(),
                        slot.position()
                    )
                })
                .collect()),
            "move" => {
                let mut fields = rest.split_whitespace();
                let module = fields
                    .next()
                    .ok_or_else(|| "usage: move <module> <slot>".to_string())?;
                let slot_s = fields
                    .next()
                    .ok_or_else(|| "usage: move <module> <slot>".to_string())?;
                let kind = ModuleKind::from_name(module)
                    .ok_or_else(|| format!("unknown module '{module}'"))?;
                let slot = Slot::parse(slot_s)
                    .ok_or_else(|| format!("bad slot '{slot_s}' (port path, e.g. 1-4-3)"))?;
                let pos = self.set_slot(kind, slot.clone())?;
                Ok(vec![format!(
                    "moved {} pid={:#06x} slot={} position={}",
                    kind.name(),
                    module_info(kind).pid,
                    slot.as_str(),
                    pos
                )])
            }
            "orientation" => {
                let arg = rest
                    .split_whitespace()
                    .next()
                    .ok_or_else(|| "usage: orientation <horizontal|vertical>".to_string())?;
                let o = match arg {
                    "horizontal" | "flat" => Orientation::Horizontal,
                    "vertical" | "upright" => Orientation::Vertical,
                    other => return Err(format!("unknown orientation '{other}'")),
                };
                let o = self.set_orientation(o);
                let byte = match o {
                    Orientation::Horizontal => 0x01,
                    Orientation::Vertical => 0x00,
                };
                Ok(vec![format!(
                    "orientation {} (0x{byte:02x}) broadcast to hosts",
                    match o {
                        Orientation::Horizontal => "horizontal",
                        Orientation::Vertical => "vertical",
                    }
                )])
            }
            "replug" => {
                let n = self.replug();
                Ok(vec![format!(
                    "replug requested; {n} connection(s) will close"
                )])
            }
            "writes" => {
                let mut fields = rest.split_whitespace();
                let limit = match fields.next() {
                    None => 50,
                    Some(n) => n
                        .parse::<usize>()
                        .map_err(|_| format!("bad writes limit '{n}'"))?,
                };
                // optional "full": append the raw payload of short (non-chunk) reports
                let full = fields.next() == Some("full");
                Ok(self
                    .writes(limit)
                    .iter()
                    .map(|w| {
                        let mut line = format!(
                            "#{} pid={:#06x} kind={:#04x} len={} at_ms={}{}",
                            w.seq,
                            w.pid,
                            w.kind,
                            w.len,
                            w.at_ms,
                            if w.refused { " refused" } else { "" }
                        );
                        if full && !w.data.is_empty() {
                            line.push_str(&format!(
                                " data={}",
                                w.data
                                    .iter()
                                    .map(|b| format!("{b:02x}"))
                                    .collect::<String>()
                            ));
                        }
                        line
                    })
                    .collect())
            }
            "clock" => {
                // hub-clock (at_ms, ms since hub start) + wall clock, so a test can map
                // write-log arrival times onto its own issue timestamps exactly
                let at_ms = self.lock().started.elapsed().as_millis();
                let epoch_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                Ok(vec![format!("at_ms={at_ms} epoch_ms={epoch_ms}")])
            }
            "throttle" => {
                let mut fields = rest.split_whitespace();
                let kbps = match fields.next() {
                    None => {
                        return Err(
                            "usage: throttle <kbps|off> [drop-after|off] [credits=…|off]"
                                .to_string(),
                        )
                    }
                    Some("off") => None,
                    Some(s) => Some(s.parse::<u32>().map_err(|_| format!("bad kbps '{s}'"))?),
                };
                let drop_after = match fields.next() {
                    None => self.throttle().drop_after,
                    Some("off") => None,
                    Some(s) => Some(
                        s.parse::<u32>()
                            .map_err(|_| format!("bad drop-after '{s}'"))?,
                    ),
                };
                // optional `credits=<burst>/<refill>/<period_ms>[@<seed>]` / `credits=off`
                // (DS-035 credit-exhaustion mode); unspecified leaves the window unchanged
                let mut credits = self.credits();
                for field in fields {
                    if field == "credits=off" {
                        credits = None;
                    } else if field.starts_with("credits=") {
                        credits = Some(CreditWindow::parse(field)?);
                    } else {
                        return Err(format!("bad throttle field '{field}'"));
                    }
                }
                let t = Throttle { kbps, drop_after };
                self.set_throttle(t);
                if let Some(w) = credits {
                    self.set_credits(Some(w));
                } else {
                    self.set_credits(None);
                }
                let remaining = self.credit_remaining();
                Ok(vec![format!(
                    "throttle kbps={} drop_after={} credits={}",
                    t.kbps.map_or("off".to_string(), |k| k.to_string()),
                    t.drop_after.map_or("off".to_string(), |n| n.to_string()),
                    match (credits, remaining) {
                        (Some(w), Some(r)) => format!(
                            "{}/{}/{}{} remaining={}",
                            w.burst,
                            w.refill,
                            w.refill_period.as_millis(),
                            if w.seed == 0 {
                                String::new()
                            } else {
                                format!("@{}", w.seed)
                            },
                            r
                        ),
                        _ => "off".to_string(),
                    }
                )])
            }
            "inject" => {
                if rest.is_empty() {
                    return Err("usage: inject <event-json>".to_string());
                }
                let ev: InjectEvent =
                    serde_json::from_str(rest).map_err(|e| format!("bad event json: {e}"))?;
                let n = self.inject(&ev)?;
                Ok(vec![format!(
                    "injected module={} at_ms={} clients={n}",
                    ev.module, ev.at_ms
                )])
            }
            other => Err(format!("unknown control command '{other}'")),
        }
    }
}

fn error_frame(pid: u16, msg: String) -> Frame {
    Frame::new(kind::ERROR, pid, msg.into_bytes()).expect("error frame size is bounded")
}

/// The deterministic feature-report answer of a virtual module: report id `0x03` first, then
/// `<feature_id> <len> <payload…>`, exactly what `hid_get_feature_report` returns on hardware
/// (see the captures in `mh-protocol`'s tests).
fn answer(info: &ModuleInfo, feature_id: u8, st: &HubState) -> Result<Vec<u8>, String> {
    let kind = info.kind;
    let payload: Vec<u8> = match (kind, feature_id) {
        (ModuleKind::Keys, 0x38) | (ModuleKind::Dial, 0x48) | (_, 0x2E) => {
            // firmware: 4 opaque bytes, then the ASCII version
            let mut p = vec![0x29, 0x63, 0x1C, 0x5D];
            p.extend_from_slice(VIRTUAL_FIRMWARE.as_bytes());
            p
        }
        (ModuleKind::Keys, 0x3A) | (ModuleKind::Dial, 0x4A) | (_, 0x30) => {
            // serial number: 20 ASCII bytes, deterministic per module
            let serial = format!("VDEV-{}-{:04X}", kind.name().to_uppercase(), info.pid);
            let mut p = serial.into_bytes();
            p.resize(20, 0);
            p
        }
        (ModuleKind::Faders, 0x33) => vec![0, 25, 50, 75, 100], // fader values 0..=100
        (ModuleKind::Keys, 0x3C) | (ModuleKind::Dial, 0x4C) => {
            vec![st
                .backlight
                .get(&info.pid)
                .copied()
                .unwrap_or(DEFAULT_BACKLIGHT)]
        }
        (ModuleKind::Keys, 0x3B) => {
            // screen info, as captured on hardware: 480x272
            vec![
                0x14, 0x03, 0x05, 0x0F, 0xE0, 0x01, 0x10, 0x01, 0x01, 0x48, 0x48,
            ]
        }
        (ModuleKind::Dial, 0x4B) => {
            // screen info, as captured on hardware: 240x240
            vec![
                0x14, 0x00, 0x00, 0x00, 0xF0, 0x00, 0xF0, 0x00, 0x01, 0xF0, 0xF0,
            ]
        }
        (ModuleKind::Base, 0x35) => {
            // DS-112: the base's current orientation (flat = 0x01 horizontal, upright =
            // 0x00 vertical), changed by `ds-vdevctl orientation`
            vec![match st.orientation {
                Orientation::Horizontal => 0x01,
                Orientation::Vertical => 0x00,
            }]
        }
        (_, SLOT_FEATURE_ID) => {
            // DS-112: the module's current USB slot (ASCII, padded like the serial read);
            // a vdev-only feature — on real HID the slot comes from the sysfs port path
            let slot = st
                .slots
                .get(&info.pid)
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| reference_slot(info.kind).as_str().to_string());
            let mut p = slot.into_bytes();
            p.resize(20, 0);
            p
        }
        // the rest of the M0-family diagnostic reads: deterministic filler payload
        (kind, 0x2D | 0x2F | 0x31 | 0x32 | 0x34 | 0x36) if kind.is_m0() => vec![0u8; 8],
        _ => {
            return Err(format!(
                "unknown feature id {feature_id:#04x} for {} (pid {:#06x})",
                kind.name(),
                info.pid
            ))
        }
    };
    let mut data = vec![0x03, feature_id, payload.len() as u8];
    data.extend_from_slice(&payload);
    Ok(data)
}

/// sha256 of `data` as lowercase hex — what `ds-vdevctl frames <target>` must match.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub use ds_device::chunks::sha256_of;

#[cfg(test)]
mod tests {
    use super::*;
    use ds_device::frame::kind;
    use ds_protocol::{self, ModuleKind};

    fn keys_pid() -> u16 {
        module_info(ModuleKind::Keys).pid
    }

    fn write_frame(pid: u16, payload: Vec<u8>) -> Frame {
        Frame::new(kind::WRITE, pid, payload).unwrap()
    }

    fn ack(outcome: &Outcome) -> u32 {
        assert_eq!(outcome.replies.len(), 1);
        assert_eq!(outcome.replies[0].kind, kind::ACK);
        u32::from_le_bytes(outcome.replies[0].payload[..4].try_into().unwrap())
    }

    #[test]
    fn reassembles_a_panel_push_and_stores_the_frame_sha() {
        let hub = Hub::new(HubConfig::default());
        let jpeg: Vec<u8> = (0..1000u32).map(|i| (i % 256) as u8).collect();
        let reports = ds_protocol::panel_image(&jpeg, 320);
        assert!(reports.len() > 1);
        for r in &reports {
            let out = hub.handle_frame(&write_frame(keys_pid(), r.bytes().to_vec()));
            assert_eq!(
                ack(&out),
                r.bytes().len() as u32,
                "every chunk is acked with its byte count"
            );
            assert!(!out.close);
        }
        let entry = hub.frame(Target::Panel).expect("panel frame stored");
        assert_eq!(entry.sha256, sha256_hex(&jpeg));
        assert_eq!(entry.bytes, jpeg.len());
        assert_eq!(entry.target, "panel");
    }

    #[test]
    fn sha_mismatch_is_rejected_by_the_reassembler() {
        // the verification path the host uses: expected digest vs reassembled digest
        let jpeg = vec![0x42u8; 700];
        let reports: Vec<Vec<u8>> = ds_protocol::key_image(0, &jpeg, 320)
            .iter()
            .map(|r| r.bytes().to_vec())
            .collect();
        let mut r = Reassembler::new();
        r.begin(0x36, 0, Some([0xEE; 32])); // wrong expected sha
        let mut result = Ok(None);
        for raw in &reports {
            result = r.push(raw);
            if result.is_err() {
                break;
            }
        }
        assert!(matches!(
            result,
            Err(ds_device::Error::Reassembly(
                ds_device::chunks::ReassemblyError::ShaMismatch { .. }
            ))
        ));
        // and with the right digest it passes
        let mut r = Reassembler::new();
        r.begin(0x36, 0, Some(sha256_of(&jpeg)));
        let mut done = None;
        for raw in &reports {
            done = r.push(raw).unwrap();
        }
        assert_eq!(done.unwrap().data, jpeg);
    }

    #[test]
    fn multiple_targets_are_kept_separate() {
        let hub = Hub::new(HubConfig::default());
        let panel: Vec<u8> = (0..500u32).map(|i| (i % 253) as u8).collect();
        let key3: Vec<u8> = (0..400u32).map(|i| (i % 249) as u8).collect();
        let dial: Vec<u8> = (0..300u32).map(|i| (i % 247) as u8).collect();
        for r in ds_protocol::panel_image(&panel, 320) {
            hub.handle_frame(&write_frame(keys_pid(), r.bytes().to_vec()));
        }
        for r in ds_protocol::key_image(3, &key3, 320) {
            hub.handle_frame(&write_frame(keys_pid(), r.bytes().to_vec()));
        }
        let dial_pid = module_info(ModuleKind::Dial).pid;
        for r in ds_protocol::dial_image(&dial, 320) {
            hub.handle_frame(&write_frame(dial_pid, r.bytes().to_vec()));
        }
        assert_eq!(hub.frame(Target::Panel).unwrap().sha256, sha256_hex(&panel));
        assert_eq!(hub.frame(Target::Key(3)).unwrap().sha256, sha256_hex(&key3));
        assert_eq!(hub.frame(Target::Dial).unwrap().sha256, sha256_hex(&dial));
        assert_eq!(hub.frames().len(), 3);
        // a second push to the same target replaces the frame
        let other = vec![0xAA; 100];
        for r in ds_protocol::key_image(3, &other, 320) {
            hub.handle_frame(&write_frame(keys_pid(), r.bytes().to_vec()));
        }
        assert_eq!(
            hub.frame(Target::Key(3)).unwrap().sha256,
            sha256_hex(&other)
        );
        assert_eq!(hub.frames().len(), 3);
    }

    #[test]
    fn malformed_chunk_gets_an_error_not_an_ack() {
        let hub = Hub::new(HubConfig::default());
        // out-of-order chunk with no transfer in progress
        let raw = [0x02u8, 0x36, 0x05, 0x00, 0x01, 0x00, 0x02, 0x00, 0xFF];
        let out = hub.handle_frame(&write_frame(keys_pid(), raw.to_vec()));
        assert_eq!(out.replies.len(), 1);
        assert_eq!(out.replies[0].kind, kind::ERROR);
        let msg = String::from_utf8_lossy(&out.replies[0].payload);
        assert!(msg.contains("reassembly"), "{msg}");
        assert!(hub.writes(10)[0].refused);
    }

    #[test]
    fn forbidden_reports_are_refused_device_side() {
        let hub = Hub::new(HubConfig::default());
        for raw in [&[0x02u8, 0x34, 0x00, 0x00][..], &[0x03, 0x35, b'M', b'H']] {
            let out = hub.handle_frame(&Frame::new(kind::WRITE, keys_pid(), raw.to_vec()).unwrap());
            assert_eq!(out.replies[0].kind, kind::ERROR);
            let msg = String::from_utf8_lossy(&out.replies[0].payload);
            assert!(msg.contains("forbidden"), "{msg}");
        }
    }

    #[test]
    fn feature_reports_round_trip() {
        let hub = Hub::new(HubConfig::default());
        for kind in ModuleKind::ALL {
            let info = module_info(kind);
            // software-mode handshake acks and flips the reported state
            if let Some((handshake, _mode)) = ds_protocol::enter_software_mode(kind) {
                let out = hub.handle_frame(
                    &Frame::new(kind::FEATURE_SET, info.pid, handshake.bytes().to_vec()).unwrap(),
                );
                // unpadded here; the transport pads feature reports to 32 B (proven in
                // tests/end_to_end.rs where the ack is 32)
                assert_eq!(ack(&out), handshake.bytes().len() as u32);
            }
            for (name, fid) in ds_protocol::diagnostic_reads(
                ds_protocol::SoftwareMode::m0(kind)
                    .unwrap_or_else(|| ds_protocol::enter_software_mode(kind).unwrap().1),
            ) {
                let out =
                    hub.handle_frame(&Frame::new(kind::FEATURE_GET, info.pid, vec![*fid]).unwrap());
                assert_eq!(out.replies.len(), 1, "{kind:?} {name}");
                let ans = &out.replies[0];
                assert_eq!(ans.kind, kind::ANSWER, "{kind:?} {name}");
                assert_eq!(ans.pid, info.pid);
                assert_eq!(
                    ans.payload.len(),
                    32,
                    "answers are padded to the feature length"
                );
                assert_eq!(ans.payload[0], 0x03, "report id first");
                assert_eq!(ans.payload[1], *fid, "feature id echoed");
            }
        }
        // firmware answer parses through ds-protocol (report id stripped)
        let out = hub.handle_frame(&Frame::new(kind::FEATURE_GET, keys_pid(), vec![0x38]).unwrap());
        let ans = &out.replies[0].payload[1..];
        assert_eq!(
            ds_protocol::parse_firmware(ans).as_deref(),
            Some(VIRTUAL_FIRMWARE)
        );
        // serial is deterministic per module
        let out = hub.handle_frame(&Frame::new(kind::FEATURE_GET, keys_pid(), vec![0x3A]).unwrap());
        assert_eq!(
            ds_protocol::parse_serial_number(&out.replies[0].payload[1..]).as_deref(),
            Some("VDEV-KEYS-01FE")
        );
        // screen geometry matches the module's real panel
        let out = hub.handle_frame(&Frame::new(kind::FEATURE_GET, keys_pid(), vec![0x3B]).unwrap());
        assert_eq!(
            ds_protocol::parse_screen_size(&out.replies[0].payload[1..]),
            Some((480, 272))
        );
        let dial_pid = module_info(ModuleKind::Dial).pid;
        let out = hub.handle_frame(&Frame::new(kind::FEATURE_GET, dial_pid, vec![0x4B]).unwrap());
        assert_eq!(
            ds_protocol::parse_screen_size(&out.replies[0].payload[1..]),
            Some((240, 240))
        );
        // fader values
        let faders_pid = module_info(ModuleKind::Faders).pid;
        let out = hub.handle_frame(&Frame::new(kind::FEATURE_GET, faders_pid, vec![0x33]).unwrap());
        assert_eq!(
            ds_protocol::parse_fader_values(&out.replies[0].payload[1..]),
            Some([0, 25, 50, 75, 100])
        );
        // backlight tracks the last feature set
        let bl = ds_protocol::backlight(ModuleKind::Keys, 80).unwrap();
        hub.handle_frame(&Frame::new(kind::FEATURE_SET, keys_pid(), bl.bytes().to_vec()).unwrap());
        let out = hub.handle_frame(&Frame::new(kind::FEATURE_GET, keys_pid(), vec![0x3C]).unwrap());
        assert_eq!(
            ds_protocol::parse_percent(&out.replies[0].payload[1..]),
            Some(80)
        );
        // unknown feature id -> error
        let out = hub.handle_frame(&Frame::new(kind::FEATURE_GET, keys_pid(), vec![0x77]).unwrap());
        assert_eq!(out.replies[0].kind, kind::ERROR);
    }

    #[test]
    fn ds112_slots_move_orientation_and_replug() {
        let hub = Hub::new(HubConfig::default());
        // boot: every module at its SALON-001 rot0 anchor
        assert_eq!(hub.slot(ModuleKind::Knobs).unwrap().as_str(), "1-2-3");
        assert_eq!(hub.slot(ModuleKind::Keys).unwrap().as_str(), "2-3");
        assert_eq!(hub.slot(ModuleKind::Faders).unwrap().as_str(), "1-1-3");
        assert_eq!(hub.slot(ModuleKind::Rollers).unwrap().as_str(), "3-3");
        assert_eq!(hub.slot(ModuleKind::Dial).unwrap().as_str(), "1-3-4");
        assert_eq!(
            hub.slot(ModuleKind::Base).unwrap().as_str(),
            ds_device::slot::BASE_MCU_SLOT
        );
        assert_eq!(hub.orientation(), Orientation::Horizontal);

        // the slot feature read answers the current slot (what VirtualTransport::module_slot parses)
        let out = hub.handle_frame(
            &Frame::new(kind::FEATURE_GET, keys_pid(), vec![SLOT_FEATURE_ID]).unwrap(),
        );
        let ans = &out.replies[0].payload;
        assert_eq!(ans[0], 0x03);
        assert_eq!(ans[1], SLOT_FEATURE_ID);
        assert_eq!(&ans[3..3 + 3], b"2-3");
        // base orientation feature read: flat = 0x01
        let base_pid = module_info(ModuleKind::Base).pid;
        let out = hub.handle_frame(&Frame::new(kind::FEATURE_GET, base_pid, vec![0x35]).unwrap());
        assert_eq!(out.replies[0].payload[3], 0x01);

        // move the knobs to another connector (slot change, same pid/serial)
        let pos = hub
            .set_slot(ModuleKind::Knobs, Slot::parse("1-4-3").unwrap())
            .unwrap();
        assert_eq!(pos, 78);
        assert_eq!(hub.slot(ModuleKind::Knobs).unwrap().as_str(), "1-4-3");
        let out = hub.handle_frame(
            &Frame::new(
                kind::FEATURE_GET,
                module_info(ModuleKind::Knobs).pid,
                vec![SLOT_FEATURE_ID],
            )
            .unwrap(),
        );
        assert_eq!(&out.replies[0].payload[3..3 + 5], b"1-4-3");
        assert!(Slot::parse("9-9").is_none());

        // control protocol: slots / move / orientation / replug
        let lines = hub.control("slots").unwrap();
        assert_eq!(lines.len(), 6);
        assert!(lines
            .iter()
            .any(|l| l.starts_with("knobs pid=0x0202 slot=1-4-3 position=78")));
        let lines = hub.control("move keys 3-1").unwrap();
        assert_eq!(lines[0], "moved keys pid=0x01fe slot=3-1 position=24");
        assert_eq!(hub.slot(ModuleKind::Keys).unwrap().as_str(), "3-1");
        assert!(hub.control("move keys nonsense").is_err());
        assert!(hub.control("move nope 1-1-1").is_err());
        // DS-112: a move onto another module's slot is refused (one module per connector)
        let taken = hub.control("slots").unwrap().join("\n");
        let first_slot = taken
            .lines()
            .find_map(|l| l.split_whitespace().find(|w| w.contains('-')))
            .unwrap()
            .to_string();
        let other = if first_slot == "1-4-3" { "keys" } else { "knobs" };
        assert!(hub
            .control(&format!("move {other} {first_slot}"))
            .is_err());
        let lines = hub.control("orientation vertical").unwrap();
        assert!(lines[0].contains("vertical"));
        assert_eq!(hub.orientation(), Orientation::Vertical);
        let out = hub.handle_frame(&Frame::new(kind::FEATURE_GET, base_pid, vec![0x35]).unwrap());
        assert_eq!(out.replies[0].payload[3], 0x00, "upright = 0x00");
        // the orientation change was broadcast as an input report to connected hosts
        let (id, rx) = hub.register_client();
        hub.control("orientation horizontal").unwrap();
        let f = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(f.kind, kind::INPUT);
        assert_eq!(f.pid, base_pid);
        assert!(matches!(
            ds_protocol::parse_input(ModuleKind::Base, &f.payload)[0],
            ds_protocol::Input::Orientation(Orientation::Horizontal)
        ));
        // replug: the flag is set, cleared by the next registration
        assert_eq!(hub.replug(), 1);
        assert!(hub.replug_requested());
        let _ = hub.register_client();
        assert!(!hub.replug_requested());
        hub.unregister_client(id);

        // reports carry slot + position
        let reports = hub.reports();
        assert!(
            reports.iter().any(|r| r.contains("slot=1-4-3 position=78")),
            "{reports:?}"
        );
    }

    #[test]
    fn reports_describe_the_deck_layout() {
        let hub = Hub::new(HubConfig::default());
        let reports = hub.reports();
        assert_eq!(reports.len(), 6);
        assert!(reports
            .iter()
            .any(|r| r.starts_with("keys pid=0x01fe out_len=320")));
        assert!(reports
            .iter()
            .any(|r| r.starts_with("faders pid=0x0200 out_len=64")));
    }

    #[test]
    fn throttle_knobs_and_control_protocol() {
        let hub = Hub::new(HubConfig {
            throttle: Throttle {
                kbps: Some(64),
                drop_after: None,
            },
            ..HubConfig::default()
        });
        assert_eq!(hub.throttle().delay_for(320), Duration::from_millis(40));
        let out = hub.handle_frame(&write_frame(keys_pid(), vec![0x02, 0x2D, 0, 0, 1, 2, 3, 0]));
        // 8 bytes at 64 kbit/s = 1 ms on the simulated device
        assert_eq!(out.delay, Duration::from_millis(1));

        let lines = hub.control("frames").unwrap();
        assert!(lines.is_empty());
        assert_eq!(hub.control("ping").unwrap(), vec!["pong".to_string()]);
        let lines = hub.control("throttle 128 7").unwrap();
        assert_eq!(
            hub.throttle(),
            Throttle {
                kbps: Some(128),
                drop_after: Some(7)
            }
        );
        assert!(lines[0].contains("kbps=128"));
        assert!(hub.control("throttle off off").is_ok());
        assert_eq!(hub.throttle(), Throttle::none());
        assert!(hub.control("bogus").is_err());
        assert!(hub.control("frames key:15").is_err());

        // drop-after closes the connection without an ack (one write was already accepted
        // above, so the cap of 2 is hit on the second write below)
        hub.set_throttle(Throttle {
            kbps: None,
            drop_after: Some(2),
        });
        let out = hub.handle_frame(&write_frame(keys_pid(), vec![0x02, 0x2D, 0, 0, 1, 2, 3, 0]));
        assert!(!out.close && !out.replies.is_empty());
        let out = hub.handle_frame(&write_frame(keys_pid(), vec![0x02, 0x2D, 0, 0, 1, 2, 3, 0]));
        assert!(out.close && out.replies.is_empty());
    }
}

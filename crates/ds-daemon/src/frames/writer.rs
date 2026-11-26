//! The frame writer threads (DS-031): one dedicated writer thread **per module** owns the
//! send side of the transport for that module (`docs/contracts/streaming.md` §3,
//! `docs/components/frame-service.md`).
//!
//! The keys panel (`keys`) and every `key:N` are the same M4315 module — one HID handle —
//! so a single writer serializes their chunked image transfers (two writers interleaving
//! chunks on one module would tear the image, exactly what §3 forbids); the dial (M131E)
//! gets its own writer. Each writer:
//!
//! - pulls the oldest **waiting** frame of its module's targets whose slot has no in-flight
//!   frame, marks it in flight, and sends it **whole** — every chunk in order, paced by a
//!   token bucket at the device's ~2.1 MB/s ceiling (`IMAGE_WRITE_RATE_PER_S`) — never
//!   abandoning a transfer mid-sequence;
//! - holds **no lock across device I/O**: the frame is moved out of the slot under the
//!   slots lock, the lock is released, then encode + chunk + send happen lock-free; the
//!   writer shares no lock with the state task, so a slow or stuck device can never starve
//!   the control plane (structural independence, streaming.md §3);
//! - on shutdown drains what is queued (graceful stop) and exits;
//! - on a device error drops the frame (counted in `telemetry.dropped`) and publishes
//!   `deck/device/<serial>/disconnected` on the bus.
//!
//! The queue itself is bounded (1 in flight + 1 waiting per target, owned by
//! [`super::service`]), so the writer never blocks the IPC/stream task either: producers
//! only take the slots lock and push a wake signal.

use super::service::{QueuedFrame, ServiceInner};
use super::{chunk, encode, FrameTarget};
use crate::device::IMAGE_WRITE_RATE_PER_S;
use crate::lifecycle;
use crate::router;
use ds_device::module::module_info;
use ds_device::rate::RateLimiter;
use ds_device::vtransport::virtual_module_id;
use ds_protocol::ModuleKind;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long a writer waits on its wake channel before re-checking shutdown and the queues.
pub const WRITER_POLL: Duration = Duration::from_millis(50);

/// A whole-frame send slower than this is logged (the in-flight frame is never dropped or
/// timed out — streaming.md §3 — but a stuck device must be visible; the control plane is
/// never blocked either way because this runs on the writer's own thread).
pub const SLOW_SEND_WARN: Duration = Duration::from_secs(5);

/// Spawn the writer thread for `module`.
pub fn spawn(inner: Arc<ServiceInner>, module: ModuleKind, rx: Receiver<()>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("ds-frames-writer-{module:?}"))
        .spawn(move || run(inner, module, rx))
        .expect("spawn frame writer thread")
}

/// The writer loop: drain every queued frame of the module, then sleep on the wake channel
/// until signaled; on shutdown, drain once more and exit (graceful stop/drain).
pub fn run(inner: Arc<ServiceInner>, module: ModuleKind, rx: Receiver<()>) {
    let limiter = RateLimiter::new(IMAGE_WRITE_RATE_PER_S);
    loop {
        while let Some((target, frame)) = take_next(&inner, module) {
            send_frame(&inner, module, target, frame, &limiter);
        }
        if inner.shutdown.get() {
            break;
        }
        match rx.recv_timeout(WRITER_POLL) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    lifecycle::log(
        "info",
        "ds-daemon::frames::writer",
        &format!("writer for {module:?} stopped (queue drained)"),
    );
}

/// Take the oldest waiting frame of this module's targets that has no in-flight frame, and
/// mark it in flight (the writer now owns it until fully sent). Returns `None` when there
/// is nothing to send right now.
pub fn take_next(inner: &ServiceInner, module: ModuleKind) -> Option<(FrameTarget, QueuedFrame)> {
    let mut slots = inner.slots.lock().unwrap();
    let mut best: Option<(FrameTarget, u64)> = None;
    for (target, slot) in slots.iter() {
        if target.module() != module {
            continue;
        }
        if slot.in_flight.is_some() || slot.waiting.is_none() {
            continue;
        }
        let arrival = slot.waiting.as_ref().expect("waiting checked").arrival;
        if best.is_none_or(|(_, a)| arrival < a) {
            best = Some((*target, arrival));
        }
    }
    let (target, _) = best?;
    let slot = slots.get_mut(&target).expect("slot exists");
    let frame = slot.waiting.take().expect("waiting checked");
    slot.in_flight = Some(super::service::InFlight {
        seq: frame.seq,
        arrival: frame.arrival,
        len: frame.payload.len(),
    });
    Some((target, frame))
}

/// Encode + chunk + send one frame whole; returns the device JPEG on success.
fn send_whole(
    inner: &ServiceInner,
    module: ModuleKind,
    target: FrameTarget,
    frame: &QueuedFrame,
    limiter: &RateLimiter,
) -> Result<Vec<u8>, SendFailure> {
    let jpeg = if frame.pre_encoded {
        frame.payload.clone()
    } else {
        encode::encode_frame(target, &frame.payload).map_err(|e| SendFailure {
            message: format!("{}: {e}", e.code()),
            device: false,
        })?
    };
    let transport = inner.transport.lock().unwrap().clone();
    let Some(transport) = transport.as_ref() else {
        return Err(SendFailure {
            message: "no device transport (--device none)".to_string(),
            device: false,
        });
    };
    let out_len = module_info(module).out_len;
    let reports = chunk::reports(target, &jpeg, out_len);
    if reports.is_empty() {
        return Err(SendFailure {
            message: "chunker produced no reports".to_string(),
            device: false,
        });
    }
    // The transport owns module identity: send to the id of the module of this kind that
    // the transport actually has connected (a missing module is a typed error, not a panic).
    let Some(module_id) = transport.module_id(module) else {
        return Err(SendFailure {
            message: format!("module {} not connected on this transport", module.name()),
            device: false,
        });
    };
    for report in &reports {
        limiter.acquire(); // pace to the ~2.1 MB/s device ceiling, never block producers
        transport
            .send(&module_id, report)
            .map_err(|e| SendFailure {
                message: format!("image chunk send failed: {e}"),
                device: true,
            })?;
    }
    Ok(jpeg)
}

/// Why a whole-frame send failed.
struct SendFailure {
    message: String,
    /// True when the failure came from the transport (module gone) — the service then
    /// publishes `deck/device/<serial>/disconnected`.
    device: bool,
}

/// Encode (unless the payload is already the device JPEG), chunk and send one frame whole,
/// paced at the device ceiling. No lock is held across any of the device I/O.
fn send_frame(
    inner: &ServiceInner,
    module: ModuleKind,
    target: FrameTarget,
    frame: QueuedFrame,
    limiter: &RateLimiter,
) {
    let started = Instant::now();
    let result = send_whole(inner, module, target, &frame, limiter);
    let elapsed = started.elapsed();
    if elapsed > SLOW_SEND_WARN {
        lifecycle::log(
            "warn",
            "ds-daemon::frames::writer",
            &format!(
                "slow frame send for {} seq {}: {:?} (> {:?}); the control plane is unaffected",
                target.wire_name(),
                frame.seq,
                elapsed,
                SLOW_SEND_WARN
            ),
        );
    }
    finish(inner, module, target, frame, result, elapsed);
}

/// Record the outcome of one frame: sent (whole) or dropped (counted), release the budget
/// bytes and the in-flight marker, re-grant one stream credit, and publish the disconnect
/// event when the module went away.
fn finish(
    inner: &ServiceInner,
    module: ModuleKind,
    target: FrameTarget,
    frame: QueuedFrame,
    result: Result<Vec<u8>, SendFailure>,
    elapsed: Duration,
) {
    let mut slots = inner.slots.lock().unwrap();
    let slot = slots.get_mut(&target).expect("slot exists");
    debug_assert!(
        slot.in_flight
            .as_ref()
            .is_some_and(|f| f.seq == frame.seq && f.len == frame.payload.len()),
        "in-flight marker must match the frame being finished"
    );
    slot.in_flight = None;
    inner
        .queued_bytes
        .fetch_sub(frame.payload.len(), Ordering::SeqCst);
    match result {
        Ok(jpeg) => {
            slot.stats.sent += 1;
            slot.stats.bytes += jpeg.len() as u64;
            slot.stats.last_seq = Some(frame.seq);
            slot.stats.last_sha256 = Some(sha256_hex(&jpeg));
            slot.stats.last_frame_at = Some(Instant::now());
            slot.stats.window_sent += 1;
            slot.stats.note_writer_ms(elapsed);
        }
        Err(f) => {
            slot.stats.dropped += 1;
            slot.stats.last_seq = Some(frame.seq);
            lifecycle::log(
                "error",
                "ds-daemon::frames::writer",
                &format!(
                    "frame seq {} for target {} dropped: {}",
                    frame.seq,
                    target.wire_name(),
                    f.message
                ),
            );
            if f.device {
                let serial = inner
                    .transport
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|t| t.module_id(module))
                    .map(|m| m.serial)
                    .unwrap_or_else(|| virtual_module_id(module).serial);
                inner.bus.publish(router::device_event(
                    &serial,
                    "disconnected",
                    serde_json::json!({"target": target.wire_name(), "seq": frame.seq}),
                ));
            }
        }
    }
    drop(slots);
    // the queue drained one frame: re-grant one credit to the producers of this target
    if let Some(bus) = inner.stream_bus.lock().unwrap().as_ref() {
        bus.grant_credits(&target.wire_name(), 1);
    }
}

/// sha256 of the sent device JPEG, lowercase hex (the `last_sha256` telemetry field).
fn sha256_hex(data: &[u8]) -> String {
    ds_device::chunks::sha256_of(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_send_constant_matches_the_contract_failure_mode() {
        assert_eq!(SLOW_SEND_WARN, Duration::from_secs(5));
        assert_eq!(WRITER_POLL, Duration::from_millis(50));
    }
}

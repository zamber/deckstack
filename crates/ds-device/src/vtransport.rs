//! The virtual transport: a Unix socket speaking the report codec, so the same driver code
//! runs against ds-vdev (DS-017) with no hardware (ADR-015).
//!
//! Byte-compatible with what ds-vdev serves: the socket protocol is pinned in
//! `docs/components/virtual-device.md` §"Socket protocol". Reports are padded to the module's
//! report lengths exactly like the HID layer does, and every outgoing report is checked against
//! the forbidden-command list before it is written.
//!
//! A reader thread owns the read half and turns socket frames into [`DeviceEvent`]s; `send` and
//! `query` write a frame and wait for the matching ack/answer, so a throttled device
//! (ds-vdev `--throttle-kbps`) delays the writer instead of being dropped, and a dead device
//! surfaces as [`Error::Timeout`] / [`Error::Closed`] rather than a hang.

use std::collections::VecDeque;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ds_protocol::{ModuleKind, Report, ReportKind};
use mh_device::ModuleId;

use crate::error::{Error, Result};
use crate::frame::{self, kind, Frame};
use crate::module::{module_info, module_info_for_pid};
use crate::report::{ensure_allowed, pad_report};
use crate::transport::{DeviceEvent, Transport};

/// Default time `send` waits for the device's write ack before giving up (5 s — the HID layer
/// hangs 5 s on a wedged control transfer, `upstream/PROTOCOL.md:116-118`).
pub const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Module handle for a module behind a virtual transport: the serial is a placeholder until
/// DS-022 identifies modules by their real (feature-read) serial.
pub fn virtual_module_id(kind: ModuleKind) -> ModuleId {
    let pid = module_info(kind).pid;
    ModuleId {
        kind,
        serial: format!("virtual-{pid:04x}"),
    }
}

enum Incoming {
    Ack { pid: u16, written: u32 },
    Answer { pid: u16, data: Vec<u8> },
    Event(DeviceEvent),
    DeviceError(String),
    Closed,
}

#[derive(Default)]
struct Inbox {
    queue: VecDeque<Incoming>,
    closed: bool,
    disconnect_reported: bool,
}

struct Shared {
    inbox: Mutex<Inbox>,
    cond: Condvar,
}

/// A connection to a ds-vdev Unix socket.
pub struct VirtualTransport {
    shared: Arc<Shared>,
    writer: Arc<Mutex<UnixStream>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    path: PathBuf,
    write_timeout: Duration,
}

impl VirtualTransport {
    /// Connect to the ds-vdev socket at `path` and start the reader thread.
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)?;
        let reader_stream = stream.try_clone()?;
        let shared = Arc::new(Shared {
            inbox: Mutex::new(Inbox::default()),
            cond: Condvar::new(),
        });
        let reader_shared = Arc::clone(&shared);
        let reader = std::thread::spawn(move || reader_loop(reader_stream, reader_shared));
        Ok(VirtualTransport {
            shared,
            writer: Arc::new(Mutex::new(stream)),
            reader: Mutex::new(Some(reader)),
            path: path.to_path_buf(),
            write_timeout: DEFAULT_WRITE_TIMEOUT,
        })
    }

    /// The socket path this transport is connected to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Set how long `send` waits for the write ack (builder style).
    pub fn with_write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        self
    }

    fn write_frame(&self, frame: &Frame) -> Result<()> {
        let mut w = self.writer.lock().expect("writer poisoned");
        frame::write_frame(&mut *w, frame)
    }

    /// Wait until `deadline` for a predicate over the inbox; returns the removed item.
    fn wait_for<T>(
        &self,
        pid: u16,
        timeout: Duration,
        mut take: impl FnMut(&mut Inbox, u16) -> Option<Result<T>>,
    ) -> Result<T> {
        let mut inbox = self.shared.inbox.lock().expect("inbox poisoned");
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(v) = take(&mut inbox, pid) {
                return v;
            }
            if inbox.closed {
                return Err(Error::Closed);
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(Error::Timeout(timeout))?;
            let (guard, _) = self
                .shared
                .cond
                .wait_timeout(inbox, remaining)
                .expect("inbox poisoned");
            inbox = guard;
        }
    }

    fn is_closed(&self) -> bool {
        self.shared.inbox.lock().expect("inbox poisoned").closed
    }

    fn take_ack(inbox: &mut Inbox, pid: u16) -> Option<usize> {
        let pos = inbox
            .queue
            .iter()
            .position(|i| matches!(i, Incoming::Ack { pid: p, .. } if *p == pid))?;
        match inbox.queue.remove(pos) {
            Some(Incoming::Ack { written, .. }) => Some(written as usize),
            _ => None,
        }
    }

    fn take_answer(inbox: &mut Inbox, pid: u16) -> Option<Vec<u8>> {
        let pos = inbox
            .queue
            .iter()
            .position(|i| matches!(i, Incoming::Answer { pid: p, .. } if *p == pid))?;
        match inbox.queue.remove(pos) {
            Some(Incoming::Answer { data, .. }) => Some(data),
            _ => None,
        }
    }

    fn take_device_error(inbox: &mut Inbox) -> Option<Error> {
        let pos = inbox
            .queue
            .iter()
            .position(|i| matches!(i, Incoming::DeviceError(_)))?;
        match inbox.queue.remove(pos) {
            Some(Incoming::DeviceError(m)) => Some(Error::Device(m)),
            _ => None,
        }
    }
}

fn reader_loop(mut stream: UnixStream, shared: Arc<Shared>) {
    loop {
        let incoming = match frame::read_frame(&mut stream) {
            Ok(f) => match f.kind {
                kind::INPUT => match module_info_for_pid(f.pid) {
                    Some(info) => ds_protocol::parse_input(info.kind, &f.payload)
                        .into_iter()
                        .map(|input| {
                            Incoming::Event(DeviceEvent::Input {
                                module: virtual_module_id(info.kind),
                                input,
                            })
                        })
                        .collect::<Vec<_>>(),
                    None => vec![Incoming::DeviceError(format!(
                        "input from unknown pid {:#06x}",
                        f.pid
                    ))],
                },
                kind::ACK if f.payload.len() >= 4 => {
                    let written = u32::from_le_bytes([
                        f.payload[0],
                        f.payload[1],
                        f.payload[2],
                        f.payload[3],
                    ]);
                    vec![Incoming::Ack {
                        pid: f.pid,
                        written,
                    }]
                }
                kind::ANSWER => vec![Incoming::Answer {
                    pid: f.pid,
                    data: f.payload,
                }],
                kind::ERROR => {
                    vec![Incoming::DeviceError(
                        String::from_utf8_lossy(&f.payload).into_owned(),
                    )]
                }
                _ => Vec::new(),
            },
            Err(_) => {
                let mut inbox = shared.inbox.lock().expect("inbox poisoned");
                inbox.closed = true;
                inbox.queue.push_back(Incoming::Closed);
                drop(inbox);
                shared.cond.notify_all();
                return;
            }
        };
        if !incoming.is_empty() {
            let mut inbox = shared.inbox.lock().expect("inbox poisoned");
            for i in incoming {
                inbox.queue.push_back(i);
            }
            drop(inbox);
            shared.cond.notify_all();
        }
    }
}

impl Transport for VirtualTransport {
    fn send(&self, module: &ModuleId, report: &Report) -> Result<usize> {
        ensure_allowed(report.bytes())?;
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let info = module_info(module.kind);
        let frame_kind = match report.kind() {
            ReportKind::Output => kind::WRITE,
            ReportKind::Feature => kind::FEATURE_SET,
        };
        let frame = Frame::new(frame_kind, info.pid, pad_report(&info, report))?;
        self.write_frame(&frame)?;
        self.wait_for(info.pid, self.write_timeout, |inbox, pid| {
            if let Some(e) = VirtualTransport::take_device_error(inbox) {
                return Some(Err(e));
            }
            VirtualTransport::take_ack(inbox, pid).map(Ok)
        })
    }

    fn query(&self, module: &ModuleId, feature_id: u8, timeout: Duration) -> Result<Vec<u8>> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let info = module_info(module.kind);
        let frame = Frame::new(kind::FEATURE_GET, info.pid, vec![feature_id])?;
        self.write_frame(&frame)?;
        self.wait_for(info.pid, timeout, |inbox, pid| {
            if let Some(e) = VirtualTransport::take_device_error(inbox) {
                return Some(Err(e));
            }
            VirtualTransport::take_answer(inbox, pid).map(Ok)
        })
    }

    fn recv(&self, timeout: Duration) -> Result<DeviceEvent> {
        let mut inbox = self.shared.inbox.lock().expect("inbox poisoned");
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pos) = inbox
                .queue
                .iter()
                .position(|i| matches!(i, Incoming::Event(_)))
            {
                match inbox.queue.remove(pos) {
                    Some(Incoming::Event(e)) => return Ok(e),
                    _ => unreachable!(),
                }
            }
            if let Some(e) = VirtualTransport::take_device_error(&mut inbox) {
                return Err(e);
            }
            if inbox.closed {
                if !inbox.disconnect_reported {
                    inbox.disconnect_reported = true;
                    return Ok(DeviceEvent::Disconnected);
                }
                return Err(Error::Closed);
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(Error::Timeout(timeout))?;
            let (guard, _) = self
                .shared
                .cond
                .wait_timeout(inbox, remaining)
                .expect("inbox poisoned");
            inbox = guard;
        }
    }

    fn close(&self) -> Result<()> {
        if let Ok(frame) = Frame::new(kind::CLOSE, 0, Vec::new()) {
            let _ = self.write_frame(&frame);
        }
        if let Ok(w) = self.writer.lock() {
            let _ = w.shutdown(Shutdown::Both);
        }
        if let Some(handle) = self.reader.lock().expect("reader poisoned").take() {
            let _ = handle.join();
        }
        let mut inbox = self.shared.inbox.lock().expect("inbox poisoned");
        inbox.closed = true;
        drop(inbox);
        self.shared.cond.notify_all();
        Ok(())
    }

    fn module_id(&self, kind: ModuleKind) -> Option<ModuleId> {
        // The virtual deck is the whole hub behind one socket: every kind is present and
        // addressed by its virtual id.
        Some(virtual_module_id(kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{read_frame, write_frame};
    use ds_protocol::{self, Input, ModuleKind};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;

    struct MockOpts {
        ack: bool,
        ack_delay: Duration,
        answer: Vec<u8>,
    }

    struct MockVdev {
        path: PathBuf,
        received: Arc<Mutex<Vec<Frame>>>,
        inject: mpsc::Sender<Frame>,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl MockVdev {
        fn received(&self) -> Vec<Frame> {
            self.received.lock().unwrap().clone()
        }
    }

    impl Drop for MockVdev {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// An in-process stand-in for ds-vdev: accepts one connection, records every frame, acks
    /// writes, answers feature queries, and forwards frames the test pushes through `inject`.
    fn start_mock(dir: &Path, name: &str, opts: MockOpts) -> MockVdev {
        let path = dir.join(name);
        let listener = UnixListener::bind(&path).unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let (inject, rx) = mpsc::channel::<Frame>();
        let stop = Arc::new(AtomicBool::new(false));
        let received2 = Arc::clone(&received);
        let stop2 = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(10)))
                .unwrap();
            loop {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                match read_frame(&mut stream) {
                    Ok(f) => {
                        received2.lock().unwrap().push(f.clone());
                        match f.kind {
                            kind::WRITE | kind::FEATURE_SET => {
                                if opts.ack {
                                    if opts.ack_delay > Duration::ZERO {
                                        thread::sleep(opts.ack_delay);
                                    }
                                    let n = (f.payload.len() as u32).to_le_bytes().to_vec();
                                    write_frame(
                                        &mut stream,
                                        &Frame::new(kind::ACK, f.pid, n).unwrap(),
                                    )
                                    .unwrap();
                                }
                            }
                            kind::FEATURE_GET => {
                                let mut data = vec![0x03, f.payload[0]];
                                data.extend_from_slice(&opts.answer);
                                write_frame(
                                    &mut stream,
                                    &Frame::new(kind::ANSWER, f.pid, data).unwrap(),
                                )
                                .unwrap();
                            }
                            kind::CLOSE => break,
                            _ => {}
                        }
                    }
                    Err(Error::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        while let Ok(f) = rx.try_recv() {
                            write_frame(&mut stream, &f).unwrap();
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        MockVdev {
            path,
            received,
            inject,
            stop,
            handle: Some(handle),
        }
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ds-device-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn open_send_query_inject_close_against_mock_socket() {
        let dir = test_dir("roundtrip");
        let mock = start_mock(
            &dir,
            "vdev.sock",
            MockOpts {
                ack: true,
                ack_delay: Duration::ZERO,
                answer: vec![0x02, 0x00, 0x01, 0x00],
            },
        );
        let t = VirtualTransport::connect(&mock.path).unwrap();
        assert_eq!(t.path(), mock.path.as_path());
        let keys = virtual_module_id(ModuleKind::Keys);
        assert_eq!(keys.kind, ModuleKind::Keys);

        // a chunked panel image: every report is padded to 320 B and acked with 320
        let jpeg: Vec<u8> = (0..700u32).map(|i| (i % 256) as u8).collect();
        let reports = ds_protocol::panel_image(&jpeg, 320);
        assert!(reports.len() > 1);
        for r in &reports {
            assert_eq!(t.send(&keys, r).unwrap(), 320);
        }
        let got = mock.received();
        assert_eq!(got.len(), reports.len());
        for f in &got {
            assert_eq!(f.kind, kind::WRITE);
            assert_eq!(f.pid, 0x01FE);
            assert_eq!(f.payload.len(), 320);
        }
        // first chunk header: 02 35 | chunk 0 | len 312 | index 0 | last 0
        assert_eq!(
            &got[0].payload[..8],
            &[0x02, 0x35, 0x00, 0x00, 0x38, 0x01, 0x00, 0x00]
        );
        // last chunk has last=1
        assert_eq!(got.last().unwrap().payload[7], 1);

        // a feature report goes out as FEATURE_SET, padded to 32 B
        let (handshake, _mode) = ds_protocol::enter_software_mode(ModuleKind::Keys).unwrap();
        assert_eq!(t.send(&keys, &handshake).unwrap(), 32);
        let got = mock.received();
        let hs = got.last().unwrap();
        assert_eq!(hs.kind, kind::FEATURE_SET);
        assert_eq!(&hs.payload[..3], &[0x03, 0x3A, 0x01]);
        assert_eq!(hs.payload.len(), 32);

        // a feature query is answered
        let ans = t.query(&keys, 0x38, Duration::from_secs(2)).unwrap();
        assert_eq!(ans, vec![0x03, 0x38, 0x02, 0x00, 0x01, 0x00]);
        assert!(mock
            .received()
            .iter()
            .any(|f| f.kind == kind::FEATURE_GET && f.payload == vec![0x38]));

        // input injection: the mock pushes a key-state report, the reader thread decodes it
        let mut raw = vec![0x01, 0x34, 0x11, 0x00, 0x02, 0x0F];
        raw.extend([0u8; 15]);
        raw[6 + 3] = 1; // key 3 pressed
        mock.inject
            .send(Frame::new(kind::INPUT, 0x01FE, raw).unwrap())
            .unwrap();
        let ev = t.recv(Duration::from_secs(2)).unwrap();
        let DeviceEvent::Input { module, input } = ev else {
            panic!("expected input event")
        };
        assert_eq!(module.kind, ModuleKind::Keys);
        assert!(matches!(input, Input::Keys(k) if k[3] && !k[0] && !k[14]));

        // close reaches the mock, and later sends fail
        t.close().unwrap();
        assert!(mock.received().iter().any(|f| f.kind == kind::CLOSE));
        assert!(matches!(t.send(&keys, &reports[0]), Err(Error::Closed)));
        assert!(matches!(
            t.recv(Duration::from_millis(100)),
            Ok(DeviceEvent::Disconnected)
        ));
        assert!(matches!(
            t.recv(Duration::from_millis(100)),
            Err(Error::Closed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_timeout_when_the_device_never_acks() {
        let dir = test_dir("timeout");
        let mock = start_mock(
            &dir,
            "vdev.sock",
            MockOpts {
                ack: false,
                ack_delay: Duration::ZERO,
                answer: vec![],
            },
        );
        let t = VirtualTransport::connect(&mock.path)
            .unwrap()
            .with_write_timeout(Duration::from_millis(100));
        let keys = virtual_module_id(ModuleKind::Keys);
        let (handshake, _) = ds_protocol::enter_software_mode(ModuleKind::Keys).unwrap();
        assert!(
            matches!(t.send(&keys, &handshake), Err(Error::Timeout(d)) if d == Duration::from_millis(100))
        );
        t.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_throttled_device_delays_but_does_not_break_the_writer() {
        let dir = test_dir("throttle");
        let mock = start_mock(
            &dir,
            "vdev.sock",
            MockOpts {
                ack: true,
                ack_delay: Duration::from_millis(60),
                answer: vec![],
            },
        );
        let t = VirtualTransport::connect(&mock.path)
            .unwrap()
            .with_write_timeout(Duration::from_secs(2));
        let dial = virtual_module_id(ModuleKind::Dial);
        let (handshake, _) = ds_protocol::enter_software_mode(ModuleKind::Dial).unwrap();
        let start = Instant::now();
        assert_eq!(t.send(&dial, &handshake).unwrap(), 32);
        assert!(
            start.elapsed() >= Duration::from_millis(50),
            "ack was not delayed: {:?}",
            start.elapsed()
        );
        t.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_pid_input_surfaces_as_a_device_error() {
        let dir = test_dir("unknown-pid");
        let mock = start_mock(
            &dir,
            "vdev.sock",
            MockOpts {
                ack: true,
                ack_delay: Duration::ZERO,
                answer: vec![],
            },
        );
        let t = VirtualTransport::connect(&mock.path).unwrap();
        mock.inject
            .send(Frame::new(kind::INPUT, 0x1234, vec![0x01, 0x2A, 0x00, 0x00]).unwrap())
            .unwrap();
        assert!(matches!(
            t.recv(Duration::from_secs(2)),
            Err(Error::Device(_))
        ));
        t.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disconnect_is_reported_once_then_closed() {
        let dir = test_dir("disconnect");
        let mock = start_mock(
            &dir,
            "vdev.sock",
            MockOpts {
                ack: true,
                ack_delay: Duration::ZERO,
                answer: vec![],
            },
        );
        let t = VirtualTransport::connect(&mock.path).unwrap();
        drop(mock); // server side goes away
        assert!(matches!(
            t.recv(Duration::from_secs(2)),
            Ok(DeviceEvent::Disconnected)
        ));
        assert!(matches!(
            t.recv(Duration::from_millis(100)),
            Err(Error::Closed)
        ));
        t.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn virtual_transport_resolves_every_kind_to_its_own_virtual_id() {
        let dir = test_dir("module-id");
        let mock = start_mock(
            &dir,
            "vdev.sock",
            MockOpts {
                ack: true,
                ack_delay: Duration::ZERO,
                answer: vec![],
            },
        );
        let t = VirtualTransport::connect(&mock.path).unwrap();
        for kind in ModuleKind::ALL {
            let id = crate::transport::module_id_for_send(&t, kind).unwrap();
            assert_eq!(id, virtual_module_id(kind), "{kind:?}");
            assert_eq!(id.kind, kind);
            assert_eq!(
                id.serial,
                format!("virtual-{:04x}", module_info(kind).pid),
                "{kind:?}"
            );
        }
        // the keys panel lives on the keys module, so the panel resolves to the keys id
        assert_eq!(
            t.module_id(ModuleKind::Keys).unwrap().serial,
            "virtual-01fe"
        );
        t.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

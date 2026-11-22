//! The socket server: binds the device socket (`--socket PATH`, speaking exactly the frame
//! protocol of `docs/components/virtual-device.md` §"Socket protocol") and a sibling control
//! socket (`<socket>.ctl`, one line in → `OK`/`ERR` + body lines + `.` out) used by
//! `ds-vdevctl`. One thread per device connection; injected INPUT frames are routed to each
//! connection through a per-client queue so a connection only ever writes its own stream.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ds_device::frame::{read_frame, write_frame};

use crate::hub::{Hub, HubConfig};

/// Read timeout of a device connection: long enough to stay idle, short enough to notice
/// injected input frames on the client queue.
const POLL_TIMEOUT: Duration = Duration::from_millis(20);

/// The control socket of `socket` is `<socket>.ctl`.
pub fn control_path(socket: &Path) -> PathBuf {
    let mut p = socket.as_os_str().to_owned();
    p.push(".ctl");
    PathBuf::from(p)
}

/// A running virtual hub: device socket + control socket + accept threads.
pub struct VdevServer {
    hub: Arc<Hub>,
    socket_path: PathBuf,
    ctl_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl VdevServer {
    /// Bind both sockets and start serving. Stale socket files from a previous run are
    /// removed first.
    pub fn start(socket_path: impl AsRef<Path>, config: HubConfig) -> std::io::Result<Self> {
        let socket_path = socket_path.as_ref().to_path_buf();
        let ctl_path = control_path(&socket_path);
        for p in [&socket_path, &ctl_path] {
            if p.exists() {
                std::fs::remove_file(p)?;
            }
        }
        let device_listener = UnixListener::bind(&socket_path)?;
        let ctl_listener = UnixListener::bind(&ctl_path)?;
        let hub = Arc::new(Hub::new(config));
        let shutdown = Arc::new(AtomicBool::new(false));

        let mut threads: Vec<JoinHandle<()>> = Vec::new();
        {
            let hub = Arc::clone(&hub);
            let shutdown = Arc::clone(&shutdown);
            threads.push(std::thread::spawn(move || {
                accept_loop(device_listener, shutdown, |stream| {
                    let hub = Arc::clone(&hub);
                    std::thread::spawn(move || device_connection(stream, hub));
                });
            }));
        }
        {
            let hub = Arc::clone(&hub);
            let shutdown = Arc::clone(&shutdown);
            threads.push(std::thread::spawn(move || {
                accept_loop(ctl_listener, shutdown, |stream| {
                    let hub = Arc::clone(&hub);
                    std::thread::spawn(move || control_connection(stream, hub));
                });
            }));
        }

        Ok(VdevServer {
            hub,
            socket_path,
            ctl_path,
            shutdown,
            threads: Mutex::new(threads),
        })
    }

    /// The device socket path (`virtual:<path>` URIs point here).
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The control socket path (ds-vdevctl connects here).
    pub fn control_path(&self) -> &Path {
        &self.ctl_path
    }

    /// The shared hub (tests and the `--inject` replayer drive it directly).
    pub fn hub(&self) -> Arc<Hub> {
        Arc::clone(&self.hub)
    }

    /// Stop the accept loops and remove the socket files.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // nudge the accept loops out of their blocking accept()
        let _ = UnixStream::connect(&self.socket_path);
        let _ = UnixStream::connect(&self.ctl_path);
        for t in self.threads.lock().expect("threads poisoned").drain(..) {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.ctl_path);
    }
}

impl Drop for VdevServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket_path);
        let _ = UnixStream::connect(&self.ctl_path);
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.ctl_path);
    }
}

fn accept_loop(
    listener: UnixListener,
    shutdown: Arc<AtomicBool>,
    mut handle: impl FnMut(UnixStream) + Send,
) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => handle(stream),
            Err(_) => {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// One host↔device connection: read request frames, dispatch to the hub, send replies
/// (acks delayed by the throttle knob), forward injected INPUT frames from the client queue.
fn device_connection(mut stream: UnixStream, hub: Arc<Hub>) {
    let _ = stream.set_read_timeout(Some(POLL_TIMEOUT));
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let (client_id, rx) = hub.register_client();
    loop {
        // injected input first, so a host sees it even while idle
        while let Ok(f) = rx.try_recv() {
            if write_frame(&mut writer, &f).is_err() {
                hub.unregister_client(client_id);
                return;
            }
        }
        let frame = match read_frame(&mut stream) {
            Ok(f) => f,
            Err(ds_device::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(_) => break, // host went away
        };
        let outcome = hub.handle_frame(&frame);
        if outcome.close {
            break;
        }
        if !outcome.delay.is_zero() {
            std::thread::sleep(outcome.delay);
        }
        let mut failed = false;
        for reply in &outcome.replies {
            if write_frame(&mut writer, reply).is_err() {
                failed = true;
                break;
            }
        }
        if failed {
            break;
        }
    }
    hub.unregister_client(client_id);
}

/// One control connection: a single request line, then `OK` + body lines + `.` (or `ERR msg`).
fn control_connection(stream: UnixStream, hub: Arc<Hub>) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let response = hub.control(&line);
    let mut out = String::new();
    match response {
        Ok(lines) => {
            out.push_str("OK\n");
            for l in lines {
                out.push_str(&l);
                out.push('\n');
            }
            out.push_str(".\n");
        }
        Err(msg) => {
            out.push_str("ERR ");
            out.push_str(&msg);
            out.push('\n');
        }
    }
    let _ = writer.write_all(out.as_bytes());
    let _ = writer.flush();
}

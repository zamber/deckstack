//! `ds-daemon` — the single long-lived daemon process: state task, IPC control server,
//! device task, lifecycle (docs/components/daemon.md, ticket DS-015).
//!
//! Phase-1 skeleton, all on std threads (no async runtime — the ds-ipc server, the state
//! task and the device task each own one thread, wired by bounded channels):
//!
//! - [`state`] — the state task: the **single writer** of the `ds-state` document. The IPC
//!   handler never touches the store directly; it sends commands over a bounded channel
//!   and waits for the reply, so no state lock is ever held across device I/O or socket
//!   I/O.
//! - [`ipc`] — the control-plane handler embedded in the `ds-ipc` [`Server`]: scoped token
//!   auth is done by the server itself; this handler serves the phase-1 state ops
//!   (`ping`, `get`, `set`, `subscribe`/`unsubscribe`, `daemon status`, `schema`).
//! - [`device`] — the device task: owns the `ds-device` [`Transport`] opened from the
//!   device URI and applies state image/color pushes to the hardware.
//! - [`frames`] — the frame plane: DS-030 re-encode wire images to the device wire
//!   format (baseline JPEG, standard Huffman, 4:4:4) and chunk them into 320 B image
//!   reports; the DS-031 **frame service** ([`frames::service`]) queues accepted frames
//!   per target (latest-frame-wins: 1 in flight + 1 waiting, 256 MiB global budget),
//!   [`frames::writer`] sends them whole from one writer thread per module (no lock held
//!   across device I/O), and [`frames::telemetry`] samples non-durable
//!   `/telemetry/streams/<target>` stats ≤ 1 Hz — frames never touch durable state or
//!   `rev` (S1). The device push path (DS-095) and the DSF1 stream socket both feed the
//!   same queues.
//! - [`router`] — the event bus + router (DS-040): device input reports become
//!   `deck/control/*` events, committed state changes become state events, the `emit`
//!   op publishes `app/*`/`deck/*` topics; IPC `subscribe`/`unsubscribe` register on it.
//! - [`plugins`] — the DS-050 plugin host: one sandboxed OS process per plugin
//!   (rlimits + Landlock in a re-exec'd child, argv-only), hello/register handshake,
//!   capability enforcement per request/effect, host-mediated network, watchdog
//!   (respawn ≤ 3/10 min), per-plugin log ring buffer, lifecycle state under
//!   `/plugins`, and the `plugin` router step seam.
//! - [`actions_meta`] — the DS-113 action-metadata aggregate (core step vocabulary +
//!   registered plugin manifest actions), served over the `/preview` surface.
//! - [`preview`] — the DS-113 opt-in, read-only, localhost-only `/preview` debug
//!   surface on the loopback WS/HTTP adapter (contract `docs/contracts/preview.md`).
//! - [`daemon`] — the glue: arg parsing + assembling the tasks + the run loop. `run` is
//!   wrapped here (see below) so the DS-024 guard supervises it.
//! - [`lifecycle`] — data dir resolution + permissions, token, pid file, structured logs,
//!   SIGTERM handling, socket/pid cleanup.
//! - [`guard`] — the DS-024 guard/handback: on SIGTERM/normal stop it commits state first,
//!   drains the writers, hands the deck back over a safe-builder-only sequence and closes
//!   the transport (exit 0); on a panic in a critical task the in-process supervisor does
//!   the same handback and exits nonzero; a stale `daemon.pid` from a crashed run is
//!   detected on the next start.
//!
//! [`Server`]: ds_ipc::Server
//! [`Transport`]: ds_device::Transport

// Lets the tests under `tests/hw/` (compiled into this crate's test binary via `#[path]`
// below) use `ds_daemon::…` paths like any integration test.
extern crate self as ds_daemon;

// DS-024 wiring: this ticket's touches cover only `src/guard.rs` and this file, so the
// daemon module is included under a private name and re-exported through a thin facade
// whose `run` routes through the guard's supervisor (`guard::supervise`). Everything else
// (`Cli`, `RunOptions`, `parse_args`, `usage`, `device_uri`) is re-exported unchanged.
#[path = "daemon.rs"]
mod daemon_impl;

pub mod daemon {
    //! The daemon glue (arg parsing + run loop), with `run` supervised by the DS-024
    //! [`crate::guard`]. See `src/lib.rs` for why the wrapper lives here.

    pub use super::daemon_impl::*;

    /// Parse argv (without argv[0]). DS-043: the private exec-child marker is
    /// intercepted here — the exec policy spawns `/proc/self/exe <marker> <argv…>` so
    /// the child can apply rlimits and `execve` the allowlisted `argv[0]` without any
    /// unsafe code in this crate (the workspace lints deny `unsafe_code`). The child
    /// never returns from [`crate::policy::spawn::exec_child_main`].
    pub fn parse_args(args: &[String]) -> Result<Cli, String> {
        if args
            .first()
            .is_some_and(|a| a == crate::policy::spawn::EXEC_CHILD_ARG)
        {
            std::process::exit(crate::policy::spawn::exec_child_main(&args[1..]));
        }
        // DS-050: the plugin-child marker, same re-exec pattern as the exec child —
        // the spawned plugin process applies rlimits + Landlock and execve's the
        // manifest's argv (never a shell).
        if args
            .first()
            .is_some_and(|a| a == crate::plugins::spawn::PLUGIN_CHILD_ARG)
        {
            std::process::exit(crate::plugins::spawn::plugin_child_main(&args[1..]));
        }
        super::daemon_impl::parse_args(args)
    }

    /// Run the daemon under the DS-024 guard: SIGTERM/normal stop → state committed,
    /// writers drained, device handed back, exit 0; a critical-task panic → the same
    /// handback, exit [`crate::guard::CRASH_EXIT_CODE`]. On the way out, every tracked
    /// exec-child process group is SIGKILLed (DS-043; the children also die on their
    /// own via RLIMIT_CPU) **and every plugin process group is SIGKILLed** (DS-050:
    /// daemon exit never leaves a plugin process behind).
    pub fn run(opts: RunOptions) -> std::io::Result<i32> {
        let device = device_uri(&opts.device)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let data_dir = crate::lifecycle::resolve_data_dir(opts.data_dir.as_deref());
        let result = crate::guard::supervise(
            move || super::daemon_impl::run(opts),
            crate::guard::GuardContext { device, data_dir },
        );
        crate::policy::spawn::kill_all_tracked();
        crate::plugins::shutdown_all();
        Ok(result)
    }
}

pub mod actions_meta;
pub mod device;
pub mod device_task;
pub mod frames;
pub mod guard;
pub mod ipc;
pub mod led;
pub mod lifecycle;
pub mod plugins;
pub mod policy;
pub mod preview;
pub mod router;
pub mod state;

// The DS-024 hardware/vdev tests live in the repo-level `tests/hw/` directory (like
// `tests/hw/hid.rs` for DS-021). Registering a `[[test]]` target would need a Cargo.toml
// edit, which is outside this ticket's touches, so they are compiled into this crate's
// test binary via `#[path]` instead; `cargo test -p ds-daemon` runs them.
#[cfg(test)]
#[path = "../../../tests/hw/guard_smoke.rs"]
mod hw_guard_smoke;
#[cfg(test)]
#[path = "../../../tests/hw/vdev_sigterm.rs"]
mod hw_vdev_sigterm;

/// `schemas/stream-frame.json`, embedded so the `schema` op can serve it like the others.
pub const STREAM_FRAME_SCHEMA_JSON: &str = include_str!("../../../schemas/stream-frame.json");

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-daemon";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The `server` field of the `welcome` result (`ds-daemon/0.1.0`).
pub fn server_name() -> String {
    format!("{CRATE_NAME}/{}", version())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_metadata() {
        assert_eq!(CRATE_NAME, "ds-daemon");
        assert!(!version().is_empty());
        assert!(server_name().starts_with("ds-daemon/"));
        assert!(serde_json::from_str::<serde_json::Value>(STREAM_FRAME_SCHEMA_JSON).is_ok());
    }
}

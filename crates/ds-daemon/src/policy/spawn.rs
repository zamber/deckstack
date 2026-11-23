//! Fire-and-forget spawn for exec steps (ticket DS-043, S11; actions.md §4).
//!
//! The workspace lints deny `unsafe_code`, and both `nix::unistd::fork` and
//! `std::os::unix::process::CommandExt::pre_exec` are `unsafe`, so rlimits cannot be
//! installed in a forked child directly from this crate. Instead the parent re-executes
//! **itself** through `std::process::Command` — argv-only, never a shell — with the
//! private marker [`super::EXEC_CHILD_ARG`] followed by the step's exact `argv`:
//!
//! ```text
//! parent (router thread, never blocks)
//!   └─ Command::new(current_exe) ["__ds-exec-child", argv0, argv1, …]  cwd=<confined>
//!        env = {PATH, HOME, LANG}          (env_clear first — the token never leaks)
//!        └─ child: apply_rlimits() → setpgid(0,0) → execve(argv[0], argv, env)
//!             └─ the allowlisted binary, same pid, argv-only execve
//! ```
//!
//! The final `execve` is argv-only with exactly the step's argv (metacharacters arrive
//! literally — no shell ever sees them), in its own process group, with rlimits CPU 10 s
//! (SIGKILL at the hard limit), NPROC 64, NOFILE 64, AS 1 GiB. The pid is preserved
//! across `execve`, so the pid the parent records is the pid of the spawned binary. A
//! reaper thread per child `wait()`s it, so exec'd children never become zombies, and
//! [`kill_all_tracked`] SIGKILLs the tracked process groups on daemon exit.

use super::{RLIMIT_AS_BYTES, RLIMIT_CPU_SECS, RLIMIT_NOFILE, RLIMIT_NPROC};
use nix::sys::resource::{setrlimit, Resource};
use nix::sys::signal::{kill, killpg, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{execve, setpgid, Pid};
use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

/// The private argv marker that selects the exec-child entry point (see `lib.rs`).
pub const EXEC_CHILD_ARG: &str = "__ds-exec-child";

/// Minimal environment for exec children: PATH (no lookup is ever done for argv[0],
/// but children may use it), HOME and LANG. `env_clear` first — no token, no daemon
/// internals leak into the child.
fn child_env(cmd: &mut Command) {
    cmd.env_clear();
    cmd.env("PATH", "/usr/local/bin:/usr/bin:/bin");
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => {
            cmd.env("HOME", h);
        }
        _ => {
            cmd.env("HOME", "/tmp");
        }
    }
    match std::env::var("LANG") {
        Ok(l) if !l.is_empty() => {
            cmd.env("LANG", l);
        }
        _ => {
            cmd.env("LANG", "C.UTF-8");
        }
    }
}

/// Spawn one allowlisted binary fire-and-forget. Returns the child pid (the pid is
/// preserved across the child's `execve`, so this is the pid of the spawned binary).
/// The router never blocks: a reaper thread waits on the child.
pub fn spawn(argv: &[String], cwd: &std::path::Path) -> io::Result<u32> {
    if argv.is_empty() || !argv[0].starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "exec spawn needs an absolute argv[0]",
        ));
    }
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg(EXEC_CHILD_ARG).args(argv);
    cmd.current_dir(cwd);
    child_env(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn()?;
    let pid = child.id();
    track(pid);
    let reaper: io::Result<JoinHandle<()>> = std::thread::Builder::new()
        .name("ds-exec-reaper".to_string())
        .spawn(move || {
            // Reap with waitpid (the pid is ours; dropping std's Child never waits,
            // so this is the single reaper and the child cannot become a zombie).
            let status = waitpid(Pid::from_raw(pid as i32), None).ok();
            record_exit(pid, status);
            drop(child);
        });
    if reaper.is_err() {
        // No reaper thread (resource exhaustion): reap inline best-effort so the
        // child is not left as a zombie.
        let _ = waitpid(
            Pid::from_raw(pid as i32),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
        );
        record_exit(pid, None);
    }
    Ok(pid)
}

/// Entry point of the re-exec'd exec child (intercepted in `lib.rs`'s `daemon`
/// facade): apply the rlimits, become a process-group leader, then `execve` the
/// allowlisted `argv[0]` with the exact `argv` and the scrubbed environment.
/// Never returns on success; exits nonzero on failure (the parent only records the
/// pid, the reaper records the exit status).
pub fn exec_child_main(args: &[String]) -> i32 {
    if args.is_empty() {
        return 125;
    }
    if let Err(e) = apply_rlimits() {
        eprintln!("ds-exec-child: cannot apply rlimits: {e}");
        return 126;
    }
    // Own process group so the daemon can kill the whole tree (kill_all_tracked).
    let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));
    let path = match CString::new(args[0].as_str()) {
        Ok(p) => p,
        Err(_) => return 125,
    };
    let mut argv = Vec::with_capacity(args.len());
    for a in args {
        match CString::new(a.as_str()) {
            Ok(c) => argv.push(c),
            Err(_) => return 125, // interior NUL: unrepresentable in execve
        }
    }
    // The environment the parent set (PATH/HOME/LANG) — re-encoded for execve.
    let env: Vec<CString> = std::env::vars_os()
        .map(|(k, v)| {
            let mut bytes = k.as_bytes().to_vec();
            bytes.push(b'=');
            bytes.extend_from_slice(v.as_bytes());
            CString::new(bytes).unwrap_or_default()
        })
        .collect();
    match execve(&path, &argv, &env) {
        Ok(_) => 0, // unreachable: execve only returns on error
        Err(e) => {
            eprintln!("ds-exec-child: execve({:?}) failed: {e}", args[0]);
            127
        }
    }
}

/// The rlimits every exec child runs under (actions.md §4): CPU 10 s (soft == hard, so
/// the kernel SIGKILLs at the limit), NPROC 64, NOFILE 64, address space 1 GiB.
fn apply_rlimits() -> nix::Result<()> {
    setrlimit(Resource::RLIMIT_CPU, RLIMIT_CPU_SECS, RLIMIT_CPU_SECS)?;
    setrlimit(Resource::RLIMIT_NPROC, RLIMIT_NPROC, RLIMIT_NPROC)?;
    setrlimit(Resource::RLIMIT_NOFILE, RLIMIT_NOFILE, RLIMIT_NOFILE)?;
    setrlimit(Resource::RLIMIT_AS, RLIMIT_AS_BYTES, RLIMIT_AS_BYTES)?;
    Ok(())
}

struct Registry {
    /// Live children: pid -> process group (== pid, the child called setpgid(0,0)).
    live: HashMap<u32, ()>,
    /// Reaped exit statuses: pid -> WaitStatus.
    exited: HashMap<u32, WaitStatus>,
}

fn registry() -> &'static Mutex<Registry> {
    static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
    REG.get_or_init(|| {
        Mutex::new(Registry {
            live: HashMap::new(),
            exited: HashMap::new(),
        })
    })
}

fn track(pid: u32) {
    registry().lock().unwrap().live.insert(pid, ());
}

fn record_exit(pid: u32, status: Option<WaitStatus>) {
    let mut reg = registry().lock().unwrap();
    reg.live.remove(&pid);
    if let Some(s) = status {
        reg.exited.insert(pid, s);
        if reg.exited.len() > 1024 {
            // bound the map: drop the oldest entries (insertion order is not tracked,
            // but the cap keeps memory flat; audit history lives in policy::audit_records)
            let keys: Vec<u32> = reg.exited.keys().copied().take(512).collect();
            for k in keys {
                reg.exited.remove(&k);
            }
        }
    }
}

/// Exit status of a reaped child, if it has exited (for tests/audit follow-up).
pub fn exit_status(pid: u32) -> Option<WaitStatus> {
    registry().lock().unwrap().exited.get(&pid).cloned()
}

/// Number of currently live exec children.
pub fn live_children() -> usize {
    registry().lock().unwrap().live.len()
}

/// SIGKILL every tracked exec-child process group (called on daemon exit from
/// `lib.rs`). Children also die on their own via RLIMIT_CPU, so this is a belt-and-
/// braces cleanup, never a blocking wait.
pub fn kill_all_tracked() {
    let pids: Vec<u32> = registry().lock().unwrap().live.keys().copied().collect();
    for pid in pids {
        // The child called setpgid(0,0), so its pgid == pid: kill the whole group.
        let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
    // Reap what we just killed without blocking: WNOHANG.
    for pid in registry().lock().unwrap().live.keys().copied() {
        let _ = waitpid(
            Pid::from_raw(pid as i32),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    #[test]
    fn spawn_runs_argv0_and_reaps_the_child() {
        // /bin/true exits 0 immediately; the reaper must record Exited(_, 0).
        let pid = spawn(&["/bin/true".to_string()], &PathBuf::from("/tmp")).unwrap();
        assert!(pid > 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while exit_status(pid).is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            exit_status(pid),
            Some(WaitStatus::Exited(Pid::from_raw(pid as i32), 0))
        );
        assert_eq!(live_children(), 0);
    }

    #[test]
    fn spawn_rejects_relative_argv0() {
        assert!(spawn(&["true".to_string()], &PathBuf::from("/tmp")).is_err());
        assert!(spawn(&[], &PathBuf::from("/tmp")).is_err());
    }

    #[test]
    fn child_env_is_minimal() {
        // The child env builder only ever sets PATH/HOME/LANG on a cleared env.
        let mut cmd = Command::new("/bin/true");
        child_env(&mut cmd);
        let env = cmd.get_envs().collect::<Vec<_>>();
        let keys: Vec<_> = env
            .iter()
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        assert!(keys.contains(&"PATH".to_string()));
        assert!(keys.contains(&"HOME".to_string()));
        assert!(keys.contains(&"LANG".to_string()));
        assert_eq!(keys.len(), 3, "exactly PATH/HOME/LANG: {keys:?}");
        assert!(!keys.iter().any(|k| k.contains("TOKEN")));
    }

    #[test]
    fn exec_child_main_rejects_empty_argv() {
        assert_eq!(exec_child_main(&[]), 125);
    }
}

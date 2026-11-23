//! Spawning one plugin process (ticket DS-050; contract `docs/contracts/plugins.md` §1,
//! §4; `docs/20-security.md` §4).
//!
//! Like the DS-043 exec child, the workspace lints deny `unsafe_code` in this crate, so
//! rlimits/Landlock cannot be installed in a forked child directly. The parent
//! re-executes **itself** (argv-only, never a shell) with the private marker
//! [`PLUGIN_CHILD_ARG`] followed by the manifest's exact `command`; the sandbox
//! description travels in the `DS_PLUGIN_SANDBOX` env var:
//!
//! ```text
//! parent (plugin supervisor thread)
//!   └─ Command::new(launcher) ["__ds-plugin-child", cmd0, cmd1, …]
//!        cwd = <plugin dir>   env = {PATH, HOME, LANG, DS_PLUGIN_TOKEN, DS_PLUGIN_SANDBOX}
//!        stdin/stdout/stderr = pipes (the wire protocol runs over stdin/stdout)
//!        └─ child: apply_rlimits() → setpgid(0,0) → landlock_restrict() → execve(cmd0, command, {PATH,HOME,LANG,DS_PLUGIN_TOKEN})
//!             └─ the plugin binary, same pid, argv-only execve, pipes inherited
//! ```
//!
//! The pid is preserved across `execve`, so the pid the parent records is the plugin's
//! pid; its process group (pgid == pid) lets the daemon kill the whole plugin tree on
//! watchdog respawn and on daemon exit ([`kill_all_plugin_groups`]). A reaper thread
//! per child `waitpid`s it, so plugins never become zombies.
//!
//! Landlock (Linux ≥ 5.13, best-effort ABI detection via the `landlock` crate): the
//! child restricts itself to read+execute on the system trees the loader/interpreter
//! needs (`/usr`, `/lib*`, `/bin`, `/sbin`, `/etc`, `/proc`), read+execute on its own
//! plugin dir, read-write on the manifest's `fs_roots`, read-write on `/dev/null` &
//! friends — everything else (notably the daemon's data dir with the token) is denied.
//! Where the kernel ABI ≥ 4 (Linux ≥ 6.7) all TCP bind/connect is denied as well
//! (defense in depth for the host-mediated network, S4/T11); where Landlock is
//! unavailable the host-enforced prefix checks are the control (documented gap).

use super::manifest::Resources;
use landlock::{
    make_bitflags, path_beneath_rules, Access, AccessFs, AccessNet, Ruleset, RulesetAttr,
    RulesetCreatedAttr, ABI,
};
use nix::sys::resource::{setrlimit, Resource};
use nix::sys::signal::{kill, killpg, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{execve, setpgid, Pid};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::path::Path;
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// The private argv marker that selects the plugin-child entry point (see `lib.rs`).
pub const PLUGIN_CHILD_ARG: &str = "__ds-plugin-child";

/// The fixed PATH the plugin child runs with (also used to resolve a bare `command[0]`).
pub const CHILD_PATH_DIRS: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin"];

/// Env var carrying the per-plugin sandbox description to the re-exec'd child.
pub const SANDBOX_ENV: &str = "DS_PLUGIN_SANDBOX";

/// Env var carrying the per-plugin token (the only secret the child gets).
pub const TOKEN_ENV: &str = "DS_PLUGIN_TOKEN";

/// The sandbox description passed to the child (JSON in [`SANDBOX_ENV`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sandbox {
    pub limits: Resources,
    pub fs_roots: Vec<String>,
    pub plugin_dir: String,
}

/// A spawned plugin process: pid + the three pipes.
pub struct Spawned {
    pub pid: u32,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// The environment the plugin itself runs with (scrubbed: no daemon internals, the
/// sandbox JSON is dropped — only PATH/HOME/LANG/token survive the execve).
fn plugin_env(token: &str) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let path = CHILD_PATH_DIRS.join(":");
    env.push(("PATH".to_string(), path));
    env.push((
        "HOME".to_string(),
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
    ));
    env.push((
        "LANG".to_string(),
        std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".to_string()),
    ));
    env.push((TOKEN_ENV.to_string(), token.to_string()));
    env
}

/// Spawn one plugin process through the re-exec'd child. `launcher` is the daemon
/// binary (in production `current_exe()`; tests pass the built `ds-daemon` bin).
/// `command` must already be absolute (see [`super::manifest::resolve_command0`]).
pub fn spawn_plugin(
    launcher: &Path,
    command: &[String],
    plugin_dir: &Path,
    token: &str,
    sandbox: &Sandbox,
) -> io::Result<Spawned> {
    if command.is_empty() || !command[0].starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "plugin spawn needs an absolute command[0]",
        ));
    }
    let sandbox_json = serde_json::to_string(sandbox)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    let mut cmd = Command::new(launcher);
    cmd.arg(PLUGIN_CHILD_ARG).args(command);
    cmd.current_dir(plugin_dir);
    cmd.env_clear();
    for (k, v) in plugin_env(token) {
        cmd.env(k, v);
    }
    cmd.env(SANDBOX_ENV, sandbox_json);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let pid = child.id();
    track(pid);
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let reaper = std::thread::Builder::new()
        .name(format!("ds-plugin-reaper-{pid}"))
        .spawn(move || {
            let status = waitpid(Pid::from_raw(pid as i32), None).ok();
            record_exit(pid, status);
            drop(child);
        });
    if reaper.is_err() {
        let _ = waitpid(
            Pid::from_raw(pid as i32),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
        );
        record_exit(pid, None);
    }
    Ok(Spawned {
        pid,
        stdin,
        stdout,
        stderr,
    })
}

/// Entry point of the re-exec'd plugin child (intercepted in `lib.rs`'s `daemon`
/// facade): apply the rlimits, become a process-group leader, restrict with Landlock,
/// then `execve` the manifest's `command[0]` with the exact argv and the scrubbed
/// environment. Never returns on success; exits nonzero on failure.
pub fn plugin_child_main(args: &[String]) -> i32 {
    if args.is_empty() {
        return 125;
    }
    let sandbox: Sandbox = match std::env::var(SANDBOX_ENV)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
    {
        Some(s) => s,
        None => {
            eprintln!("ds-plugin-child: missing/invalid {SANDBOX_ENV}");
            return 124;
        }
    };
    if let Err(e) = apply_rlimits(&sandbox.limits) {
        eprintln!("ds-plugin-child: cannot apply rlimits: {e}");
        return 126;
    }
    // Own process group so the daemon can kill the whole plugin tree.
    let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));
    match landlock_restrict(&sandbox) {
        Ok(desc) => eprintln!("ds-plugin-child: landlock {desc}"),
        Err(desc) => {
            eprintln!("ds-plugin-child: landlock unavailable ({desc}); host-enforced checks only")
        }
    }
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
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
    let env: Vec<CString> = plugin_env(&token)
        .into_iter()
        .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok())
        .collect();
    match execve(&path, &argv, &env) {
        Ok(_) => 0, // unreachable
        Err(e) => {
            eprintln!("ds-plugin-child: execve({:?}) failed: {e}", args[0]);
            127
        }
    }
}

/// The rlimits every plugin runs under (docs/20-security.md §4): CPU (soft == hard, so
/// the kernel kills at the limit), NPROC, NOFILE, AS — from the manifest's
/// `resources` or the defaults.
fn apply_rlimits(limits: &Resources) -> nix::Result<()> {
    setrlimit(Resource::RLIMIT_CPU, limits.cpu_s, limits.cpu_s)?;
    setrlimit(Resource::RLIMIT_NPROC, limits.nproc, limits.nproc)?;
    setrlimit(Resource::RLIMIT_NOFILE, limits.nofile, limits.nofile)?;
    setrlimit(Resource::RLIMIT_AS, limits.as_mb << 20, limits.as_mb << 20)?;
    Ok(())
}

/// Landlock restriction for one plugin child. Returns a human-readable status
/// ("abi=V… fully-enforced" / the error) for the log ring. Best-effort: on kernels
/// without Landlock (or with ABI < 4 for the net part) the unsupported rights are
/// simply not handled — the net-deny is then the documented gap and host mediation
/// (T11) remains the control.
fn landlock_restrict(sandbox: &Sandbox) -> Result<String, String> {
    let ro = make_bitflags!(AccessFs::{Execute | ReadFile | ReadDir});
    let rw = AccessFs::from_all(ABI::V5) & !AccessFs::Refer;
    let dev = make_bitflags!(AccessFs::{ReadFile | WriteFile});
    let ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(ABI::V10))
        .map_err(|e| e.to_string())?;
    // ABI >= 4 (Linux >= 6.7): handle TCP bind/connect and grant nothing -> denied.
    let ruleset = match ruleset.handle_access(AccessNet::from_all(ABI::V4)) {
        Ok(with_net) => with_net,
        Err(_) => Ruleset::default()
            .handle_access(AccessFs::from_all(ABI::V10))
            .map_err(|e| e.to_string())?,
    };
    let created = ruleset.create().map_err(|e| e.to_string())?;
    let system_roots = ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc", "/proc"];
    let created = created
        .add_rules(path_beneath_rules(system_roots, ro))
        .map_err(|e| e.to_string())?
        .add_rules(path_beneath_rules([&sandbox.plugin_dir], ro))
        .map_err(|e| e.to_string())?
        .add_rules(path_beneath_rules(&sandbox.fs_roots, rw))
        .map_err(|e| e.to_string())?
        .add_rules(path_beneath_rules(
            [
                "/dev/null",
                "/dev/zero",
                "/dev/full",
                "/dev/urandom",
                "/dev/random",
            ],
            dev,
        ))
        .map_err(|e| e.to_string())?;
    let status = created.restrict_self().map_err(|e| e.to_string())?;
    Ok(format!(
        "{:?} no_new_privs={}",
        status.ruleset, status.no_new_privs
    ))
}

// ---- pid registry (kill-on-exit + watchdog) -------------------------------

struct Registry {
    live: HashMap<u32, ()>,
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
            let keys: Vec<u32> = reg.exited.keys().copied().take(512).collect();
            for k in keys {
                reg.exited.remove(&k);
            }
        }
    }
}

/// Exit status of a reaped plugin child, if it has exited.
pub fn exit_status(pid: u32) -> Option<WaitStatus> {
    registry().lock().unwrap().exited.get(&pid).cloned()
}

/// Number of currently live plugin processes.
pub fn live_plugin_processes() -> usize {
    registry().lock().unwrap().live.len()
}

/// SIGKILL one plugin's process group (pgid == pid) plus the pid itself.
pub fn kill_plugin_group(pid: u32) {
    let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
    let _ = waitpid(
        Pid::from_raw(pid as i32),
        Some(nix::sys::wait::WaitPidFlag::WNOHANG),
    );
}

/// SIGKILL every live plugin process group (called on daemon exit from `lib.rs`).
pub fn kill_all_plugin_groups() {
    let pids: Vec<u32> = registry().lock().unwrap().live.keys().copied().collect();
    for pid in pids {
        kill_plugin_group(pid);
    }
}

/// Sandbox JSON for tests/inspection.
pub fn sandbox_json(sandbox: &Sandbox) -> Value {
    json!(sandbox)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_env_is_exactly_path_home_lang_token() {
        let env = plugin_env("tok123");
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["PATH", "HOME", "LANG", TOKEN_ENV]);
        assert_eq!(env[3].1, "tok123");
        assert!(env[0].1.contains("/usr/bin"));
    }

    #[test]
    fn spawn_requires_absolute_command0() {
        let dir = tempfile::tempdir().unwrap();
        let sb = Sandbox {
            limits: Resources::default(),
            fs_roots: vec![],
            plugin_dir: dir.path().display().to_string(),
        };
        assert!(spawn_plugin(
            Path::new("/nonexistent-launcher"),
            &["relative".to_string()],
            dir.path(),
            "t",
            &sb
        )
        .is_err());
        assert!(spawn_plugin(
            Path::new("/nonexistent-launcher"),
            &[],
            dir.path(),
            "t",
            &sb
        )
        .is_err());
    }

    #[test]
    fn landlock_status_is_reported_on_this_kernel() {
        // Feature-detect only: the sandbox must either restrict or report why not.
        let dir = tempfile::tempdir().unwrap();
        let sb = Sandbox {
            limits: Resources::default(),
            fs_roots: vec![dir.path().display().to_string()],
            plugin_dir: dir.path().display().to_string(),
        };
        match landlock_restrict(&sb) {
            Ok(desc) => assert!(desc.contains("no_new_privs=true"), "{desc}"),
            Err(e) => {
                // Only acceptable on kernels without Landlock.
                assert!(
                    e.contains("NotSupported")
                        || e.contains("unsupported")
                        || e.contains("OPNOTSUPP")
                        || e.contains("ENOSYS")
                        || e.contains("Invalid"),
                    "unexpected landlock error: {e}"
                );
            }
        }
    }

    #[test]
    fn registry_tracks_and_kills() {
        // kill on an unknown pid must not panic or block.
        kill_plugin_group(999_999_999);
        assert_eq!(live_plugin_processes(), 0);
        kill_all_plugin_groups();
    }
}

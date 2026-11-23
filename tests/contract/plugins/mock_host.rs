//! Mock host speaking the plugin wire protocol (`docs/contracts/plugins.md` §1) to a spawned
//! plugin process. Shared by the contract tests in `plugins.rs`.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The example plugin binary built from `crates/ds-plugin-sdk/src/bin/demo_plugin.rs`.
pub const DEMO_PLUGIN: &str = env!("CARGO_BIN_EXE_demo_plugin");

pub struct MockHost {
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl MockHost {
    /// Spawn the demo plugin with piped stdio (stderr discarded — it goes to the daemon log
    /// in production). A watchdog kills the child after 30 s so a hung plugin fails the test
    /// instead of blocking it forever.
    pub fn spawn() -> Self {
        let mut child = Command::new(DEMO_PLUGIN)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn demo plugin");
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let child = Arc::new(Mutex::new(child));
        let watchdog = Arc::clone(&child);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            if let Ok(mut c) = watchdog.lock() {
                let _ = c.kill();
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
        }
    }

    /// Send one wire line (newline-JSON).
    pub fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(line.as_bytes()).expect("write line");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush");
    }

    /// Read one wire line from the plugin (asserts the plugin did not close stdout).
    pub fn recv(&mut self) -> String {
        let mut buf = String::new();
        let n = self.stdout.read_line(&mut buf).expect("read line");
        assert!(
            n > 0,
            "plugin closed stdout unexpectedly (last output: {buf:?})"
        );
        assert!(
            buf.ends_with('\n'),
            "wire messages are newline-terminated: {buf:?}"
        );
        buf
    }

    /// Close stdin (host shutdown by EOF) and wait for a clean exit (status 0).
    pub fn expect_clean_exit_on_eof(&mut self) {
        let status = self.wait_exit_status();
        assert!(status.success(), "plugin must exit 0 on EOF, got {status}");
    }

    /// Wait for the child to exit and return its exit status.
    pub fn wait_exit(&mut self) -> std::process::ExitStatus {
        self.wait_exit_status()
    }

    fn wait_exit_status(&mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        self.child.lock().unwrap().wait().expect("wait")
    }
}

/// The `hello` line a DS-050-style host sends (caps/roots/limits per plugins.md §1).
pub fn hello_line() -> String {
    r#"{"op":"hello","protocol":1,"plugin_token":"mock-token-0123456789","caps":["state.read","state.write","net:lights.lan"],"roots":{"fs":["/home/u/media"]},"limits":{"cpu_s":10,"nproc":64,"nofile":64,"as_mb":1024}}"#
        .to_string()
}

/// The exact `register` line the demo plugin must answer with (byte-for-byte contract).
/// `command` is the spawned binary path — the manifest defaults to the current executable.
pub fn expected_register_line() -> String {
    format!(
        concat!(
            r#"{{"op":"register","id":"demo","manifest":{{"id":"demo","name":"Demo counter","version":"0.1.0","#,
            r#""description":"Example plugin: counts key presses and reads a lamp over host-mediated HTTP","#,
            r#""command":["{bin}"],"#,
            r#""actions":[{{"id":"bump","params":{{"additionalProperties":false,"properties":{{"by":{{"default":1,"type":"integer"}}}},"type":"object"}}}},"#,
            r#"{{"id":"get_lamp","params":{{"type":"object"}}}}],"#,
            r#""capabilities":["state.write","net:lights.lan"],"events":["keyDown"]}}}}"#,
            "\n"
        ),
        bin = DEMO_PLUGIN
    )
}

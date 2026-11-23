# tests/limits — DS-080 limits suite

`python3 tests/limits/run.py` boots ds-vdev + ds-daemon on a fresh temp dir (helpers
imported from `tests/conformance/run.py`, DS-018) and drives the **real sockets** with
hostile input, asserting the documented limits of `docs/contracts/ipc.md` §2/§5 and
`docs/contracts/streaming.md` §2/§5. Stdlib only. Exit 0 + `LIMITS RESULT: OK` when
every check passes; every wait carries a timeout; the daemon and vdev are always
killed (SIGTERM then SIGKILL) and the temp dir removed.

## What is covered

| Area | Cases (see `run.py` for the exact assertions) |
|---|---|
| Malformed NDJSON (daemon.sock) | invalid JSON, two objects on a line, non-object, unknown op, `ok` in a request, missing op → `E_SCHEMA` with the connection staying open; missing `id` → id-less `ok` reply; op before hello / bad protocol → `E_VERSION` (fatal, connection closed); bad token → `E_AUTH`; partial line + disconnect → `E_SCHEMA` then clean EOF; non-UTF-8 line → typed envelope + close. Daemon health (fresh ping) asserted after the battery. |
| Request-line cap (1 MiB) | a line of exactly 1 MiB (incl. `\n`) is answered; 1 MiB + 1 and a 2 MiB line get the id-less `E_LIMIT` envelope and the connection is closed (ipc.md §2). |
| Response cap (4 MiB) | a `/settings` subtree grown to ~4.5 MB via five ≤ 1 MiB set requests: `get /settings` is answered with the `E_LIMIT` replacement envelope and the connection closed; every observed response line is asserted ≤ 4 MiB. (State values are schema-capped, so the over-cap response is the only way to reach the writer.) |
| Connection cap (64) | 64 connections are served (all answer hello); the 65th/66th are refused with a clean close (EOF/RST, never a hang) while connection 1 keeps answering. |
| In-flight (≤ 64/connection) | 70 pipelined requests on one connection each get a well-formed response with the echoed id. **Structural note:** the reference server dispatches strictly sequentially (one read → dispatch → one write), so the in-flight depth never exceeds 1 and the 64 cap can never be exceeded; there is no refusal path to trigger. Asserted instead: no crash, no hang, no lost reply. |
| Image / frame bombs | `ds set <key>/image --file` with crafted PNGs: w=4097 and h=4097 → state set succeeds but the device push is refused with a typed `E_FORMAT: decoded image … exceeds the 4096x4096 decode cap` log line and **no bytes reach the vdev**; w=4096 is accepted and the frame lands on the vdev (positive control); a 17 MB file → `over the 16777216-byte cap`; a truncated 65535×65535 PNG → decode failure; corrupt PNG → decode failure. RSS delta through the bomb run < 50 MB (T4), no panic in daemon.log. |
| `ds stream push` caps | 17 MB file → exit 5 `E_LIMIT` (16 MB wire cap); a directory of 301 frames → exit 1 (300-frame cap); `--fps 31` → exit 1 (30 fps cap). |
| Queue exhaustion (throttled vdev) | `ds-vdevctl throttle 32` paces the device; 200 image sets are flooded over raw IPC in < 1 s: every set is answered ok, concurrent `get /meta` p99 ≈ 4 ms (budget 100 ms), the device-push channel (cap 64, `DEVICE_CHANNEL_CAP`) overflows — proven by vdev write-record counting (accepted ≈ 68 of 200, drops ≈ 132) — RSS delta < 50 MB, `rev` advanced by exactly 200 (drops lose device pushes, never state), daemon alive, no panic. |
| Idle timeout (300 s) | ipc.md §5 documents 300 s (subscriptions exempt). The daemon binary has **no test hook** for it in this tree (it uses `ServerConfig::default()`; the 300 s default is asserted by the ds-ipc unit test `limits.rs::defaults_match_the_contract`). Default run: a connection still answers after a 10 s idle window (not closed early). With `DS_LIMITS_IDLE_WAIT=1` the suite waits out the real 300 s and asserts the clean close. |

## Reachability notes (this tree, verified by probing)

- The running daemon binds **only `daemon.sock`**: `FrameService::serve_stream_socket`
  (`crates/ds-daemon/src/frames/service.rs`) is not called from `daemon::run` yet, so
  `stream.sock` does not exist at runtime and `ds stream push` on a valid frame exits 6
  `E_UNREACHABLE` ("stream.sock missing" — asserted). The DSF1 wire plane is therefore
  exercised by this suite through (a) the `ds stream push` client-side caps above,
  (b) the daemon-side image decode caps via `set --file` (16 MB wire, 4096×4096),
  and (c) `tests/fuzz/` sending mutated DSF1 bytes to the real sockets.
- The 4096×4096 decode cap and the 16 MB wire cap are enforced on the daemon's device
  task; refusals are logged as typed `E_FORMAT`/`E_LIMIT` lines and never reach the
  device — the suite asserts exactly that (log line + zero vdev writes for the bomb).
- The `> 300 frames` / `> 30 fps` caps are DSF1-meta/schema-level; at runtime they are
  enforced client-side by `ds stream push` (exit 1) and by the schema fixture gate
  (`sh tests/fixtures/validate.sh`) plus the ds-ipc cargo tests.
- The device-push drop counter (`DeviceHandle::dropped`) is not exposed over IPC in
  this tree, so drops are proven observably via vdev write records.
- Observed behavior worth knowing: a connection that holds a subscription keeps its
  fd open on the event bus even after a fatal framing error closed its handler
  thread, so the client sees no EOF (it must close). Non-subscribed connections get
  the documented clean close (asserted above). The fuzz suite simply never subscribes
  on its persistent connection.

## Usage

```sh
python3 tests/limits/run.py            # ~40 s, exit 0, LIMITS RESULT: OK
DS_LIMITS_IDLE_WAIT=1 python3 tests/limits/run.py   # additionally waits the real 300 s
```

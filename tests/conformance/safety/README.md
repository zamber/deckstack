# tests/conformance/safety — DS-029 safety conformance suite

Machine-checked proof of the mh-protocol safety invariants (AGENTS.md invariants;
`docs/20-security.md` threat row **T8**) on the **real binaries**, black-box only
(`ds`, `ds-daemon`, `ds-vdev`, `ds-vdevctl`, the raw NDJSON control socket, the raw
ds-vdev device socket, the `<socket>.ctl` control socket). No driver/state library code
is imported — everything is observed on the wire.

Run it from the repository root:

```sh
python3 tests/conformance/run.py            # main suite + this safety suite
python3 tests/conformance/run.py -k safety  # only the safety cases
python3 tests/conformance/safety/run.py     # standalone, like bindings/ and events/
DS_HARDWARE=1 python3 tests/conformance/safety/run.py   # incl. the hardware pass
```

## Cases

| Case | Proves | Oracle |
|---|---|---|
| `test_safety_010_*` | Forbidden HID commands are **unrepresentable** at the ds-protocol level (public API: no builder emits a forbidden report; backlight-0 refused; reads need the software-mode token) | `cargo test -p ds-protocol --test protocol_safety` (tests/protocol/safety.rs) must pass |
| `test_safety_020_*` | An attempted forbidden write on the device socket is refused with an ERROR frame — never an ACK, so **zero bytes accepted**; IPC model maps it to `E_DEVICE`/exit 7 | raw frame client on the vdev socket + vdev's write log (`writes` shows every attempt `refused`) |
| `test_safety_025_*` | Host-side raw-path guard refuses forbidden bytes before they leave; vdev refuses device-side; `EDevice` → `E_DEVICE` → exit 7 | `cargo test -p ds-device -- forbidden`, `-p ds-vdev -- forbidden`, `-p ds-ipc` |
| `test_safety_030_*` | The daemon's own traffic never carries a forbidden (report id, command) pair | zero `refused` in vdev's write log **and** payload scan of every frame the daemon sent (recorded by `FakeVdev`) |
| `test_safety_040_*` | No read/query and no other write on keys/dial **before the software-mode handshake** (`03 3A 01` / `03 49 01` are the first frames per screen module) | `FakeVdev` frame recording + vdev write log (handshake is the first write per screen module) + `ds-vdevctl reports` (`software_mode=true`) + `cargo test -p ds-device -- handshake` |
| `test_safety_060_*` | **Backlight 0 is never sent** to the dial/keys (a `set … brightness 0` never becomes a `03 34 00`/`03 44 00` report; every backlight report on the wire is 1..=100) | `FakeVdev` payload scan + `ds-vdevctl reports` (`backlight=` in 1..=100) + builder refusal (010) |
| `test_safety_070_*` | **No raw escape hatch**: the CLI verb set, the IPC op set (schema + live probes), the stream-frame meta fields and the state visual kinds contain nothing that takes raw HID bytes | `ds --help`, `bins/ds/src/cli.rs` verb arms, `ds schema ipc|stream-frame|state`, live IPC probes of candidate raw ops, `ds <candidate> --help` all exit 1 |
| `test_safety_080_*` | `DS_HARDWARE=1` pass: read-only checks on a connected deck (enumerate/open/one M0 feature read/handshake+init/close — no writes beyond the handshake); skips cleanly without hardware | `DS_HARDWARE=1 cargo test -p ds-device --test hid` |

## Files

- `run.py` — the suite (also registered by `tests/conformance/run.py`; standalone-runnable).
- `harness.py` — stack boot/teardown, the raw device-socket frame codec, the `<socket>.ctl`
  client, `FakeVdev` (recording stand-in for ds-vdev), the pinned forbidden-command list.
- `README.md` — this file.

Threat-row mapping lives in `docs/20-security.md` §2a. A red case here is a merge blocker
by design (DS-029: "None acceptable — a red safety test blocks merge").

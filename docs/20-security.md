# 20 — Security model

R8. Design: ADR-009, D14. Threat model first, mitigations, then the test that proves each.
Prefixes: `hubdeck/` = `the Python predecessor`, `upstream/` = `the Tauri predecessor`.
Revised for review M1–M5 and supervisor S3/S4/S5/S12/S13.

## 1. Assets and trust boundaries

Assets: the deck hardware (bricking is the worst outcome), the host (local code execution =
game over), the token file, user media. Boundaries: (a) anything connecting to the
daemon socket/pipe, (b) plugin processes, (c) state/config content, (d) images/frames,
(e) the optional HTTP/WS adapter vs the web.

## 2. Threat model

| # | Threat | Evidence it's real | Mitigation | Test |
|---|---|---|---|---|
| T1 | Local attacker, no token, connects to socket/pipe | Both apps gate only on a token in a 0600 file over random TCP (`hubdeck/.../control.py:51,81-90,67`) | Socket dir 0700/socket 0600; per-install token; scopes read/mutate/admin; constant-time compare; `E_AUTH`/`E_SCOPE` split | **green** — `python tests/conformance/security/run.py -k T1` → `tests/conformance/run.py -k test_110 -k test_120` + `tests/conformance/cli/run.py -k cli_100`: no/bad token → `E_AUTH` (exit 4), op before auth → `E_AUTH`, read-scope token `set` → `E_SCOPE` (exit 4) |
| T2 | Malicious config = RCE via code-in-config | `{python: …}` exec in-process (`hubdeck/.../plugins/python.py:58`); analysis D5 kept it | Config/state is data; schemas have no code field (`binding.json` negative fixture: `{"do":"eval"}` rejected); no `eval` in core; behaviour = external programs (R4) | **green** — `python tests/conformance/security/run.py -k T2` → `tests/conformance/bindings/run.py` (test_070/test_130: `{"do":"eval"}` → `E_SCHEMA` exit 1, nothing stored) + `sh tests/fixtures/validate.sh` (negative fixtures "eval-like step rejected (R8)", "relative argv[0] rejected (M1)") |
| T3 | Malicious plugin corrupts daemon / fork-bombs | In-process plugins share the engine (`hubdeck/.../plugins/base.py`); upstream permissions displayed only (`upstream/.../plugins.rs:56-58`) | Process-per-plugin + plugin_token + per-cap grants + rlimits + Landlock fs (Linux ≥6.7, OPEN O6) + watchdog ≤3/10 min + kill-on-exit | **green** — `python tests/conformance/security/run.py -k T3` → `tests/conformance/plugins/run.py` via `python tests/conformance/run.py -k plugins_010 -k plugins_020 -k plugins_030 -k plugins_060 -k plugins_080`: fork bomb killed by the nproc grant, AS memory bomb, `E_CAP` with no state change, invoke timeout → degraded → restart, crash → respawn ≤3/600 s → watchdog disable |
| T4 | Image/frame bombs | per-app caps 10/16/20 MB (`hubdeck/.../render/images.py:21`, `control.py:36`, `upstream/.../plugins.rs:266`) | wire 16 MB; decode ≤4096², ≤300 frames, ≤30 fps; bounded queues (2/target, 256 MiB, latest-wins) | **green** — `python tests/conformance/security/run.py -k T4` → `tests/conformance/streaming/run.py -k streaming_040 -k streaming_050 -k streaming_060 -k streaming_090` (301st frame refused, `--fps 31` refused, 17 MB file → `E_LIMIT` exit 5, wire-cap NACKs) + `tests/limits/run.py` (image bombs: >4096 px decode refused, 16 MB wire cap, RSS delta < 50 MB, queue exhaustion) + `tests/fuzz/run.py --cases 2000` (seeded run: typed verdicts + daemon/vdev health, corpus restored; `--replay` currently drifts on case 499 — `error:E_SCHEMA+eof` vs `error:E_SCHEMA+error:E_INTERNAL+eof`, a pre-existing DS-080 flake, reported not fixed here) |
| T5 | Shell injection via exec | `shell=True` (`hubdeck/.../platforms/posix.py:70`); `launch.run` via control (`control.py:101-123`) | **Enforced, not asserted** (S3): no shell anywhere; exec argv-only; `argv[0]` absolute (schema `prefixItems ^/`) AND in `/policy/exec/allow` AND NOT in `/policy/exec/deny` (default deny = `/bin/sh /bin/bash /bin/dash /bin/zsh /usr/bin/env /usr/bin/python3 …`); cwd confined to `/policy/fs/roots`+`/tmp` at runtime (M2); rlimits; admin scope + `allow_exec`; audit event `deck/system/exec` | **green** — `python tests/conformance/security/run.py -k T5` → `tests/conformance/exec/run.py` (test_020–test_100: `["/bin/sh","-c",…]` → `E_CAP` with zero child processes and no file created, `/usr/bin/env` → `E_CAP`, relative argv[0] → `E_SCHEMA` at bind time, no `allow_exec` → `E_CAP`, cwd outside roots → `E_CAP`, metachar argument arrives literally) + `sh tests/fixtures/validate.sh` (M1 argv[0] pattern) |
| T6 | Path traversal via file paths | GUI confined edits to config dir (`hubdeck/.../gui/server.py:428-436`); actions took arbitrary paths | Schema: `kind:file` path pattern rejects any `..` segment (M3, negative fixture); daemon canonicalizes + confines to `/policy/fs/roots`; plugin fs via `fs_roots` + Landlock | **green** — `python tests/conformance/security/run.py -k T6` → `tests/conformance/plugins/run.py` via `-k plugins_040` (`../../etc/shadow` / outside `fs_roots` → `E_CAP`, nothing read) + `tests/conformance/cli/run.py -k cli_100` (`set --file` outside `/policy/fs/roots` → `E_CAP` exit 4) + `sh tests/fixtures/validate.sh` (M3 `..`-segment pattern) |
| T7 | DNS rebinding on HTTP/WS adapter | Host check `hubdeck/.../gui/server.py:601` | 127.0.0.1 bind only; `Host: 127.0.0.1\|localhost` AND bearer on every request incl. WS upgrade | **green** — opt-in `/preview` debug surface (DS-113, ADR-019): `tests/conformance/preview/run.py` (via `python3 tests/conformance/run.py -k preview`) asserts `Host: evil.example` → 421, missing bearer → 401 on HTTP and on the WS upgrade, 127.0.0.1-only bind with no listener at all when off; contract `docs/contracts/preview.md` + `docs/contracts/ipc.md` §1.1 |
| T8 | Device bricking via firmware/serial/reset | FORBIDDEN bytes accepted by firmware (`upstream/crates/mh-protocol/src/safety.rs:8-32`; `hubdeck/.../device/hid.py:52-62,85-104`) | Unrepresentable (builders only for safe cmds + raw-path `is_forbidden`, `report.rs:28-30`); backlight-0 refused (`upstream/PROTOCOL.md:61`); hardware-mode reads refused (`module.rs:158-166`) | **green** — `python tests/conformance/security/run.py -k T8` → `tests/conformance/safety/run.py` via `python tests/conformance/run.py -k safety` (case detail in §2a below): all pinned forbidden pairs → `E_DEVICE` / exit 7 with zero bytes on the wire, handshake before any read on keys/dial, backlight-0 never sent, no raw escape hatch |
| T9 | Token rotation window | review M4 / S12: a 60 s grace for new requests is a real window | **Instant revocation for new connections** (auth reads the current token set per connection); ≤ 5 s grace only for requests already in flight on an established connection, via a connection-generation counter bumped on rotate. Rotation is explicit (`ds token revoke`), not automatic | **green** — `python tests/conformance/security/run.py -k T9` → glue case in `tests/conformance/security/run.py` (`test_security_glue_t09_*`): rotate → NEW connection with the old token → `E_AUTH` immediately, an established connection keeps its session (≤5 s in-flight grace), admin token unaffected; crate twin `cargo test -p ds-client -- revoked_token_is_rejected_on_new_connection` |
| T10 | Vendor app contention | `hubdeck/CLAUDE.md:66`; `upstream/PROTOCOL.md:217-221` | startup busy-check names the holder; guard child handback (`upstream/.../src/guard.rs`) | **skip** — vendor-app contention needs a real deck held open by another process (none exists on vdev); covered by the hardware job `DS_HARDWARE=1 python tests/hw/smoke.py` (guard handback, no lingering deck-node holders; DS-024/DS-026) — `tests/hw/smoke.py`, docs/30-testing.md §7–§8 |
| T11 | Plugin network to non-granted hosts | Landlock can restrict ports, not hostnames (S4); a plugin can open any socket (M5 glob ambiguity) | Plugins never open sockets: `sdk.http` → daemon-mediated `request kind:"net"`; daemon matches host against plugin's `net:<glob>` (glob = `*` matches exactly one DNS label, anchored: `lights.lan` ≠ `lights.lan.evil.com`; `*.lan` matches `lights.lan` but not `a.b.lan`) AND `/policy/net/allow_hosts`; daemon resolves DNS and connects itself. Landlock net-deny (ABI≥4) as second layer where available; where unavailable the mediation is the only control — documented gap | **green** — `python tests/conformance/security/run.py -k T11` → `tests/conformance/plugins/run.py` via `-k plugins_050 -k plugins_070`: exact glob matrix vs a fake lights server (`lights.lan.evil.com` → `E_CAP`, `a.b.lan` with `*.lan` → `E_CAP`, allowed host succeeds), URL > 2048 B → `E_LIMIT`, non-http URL → `E_SCHEMA`, invalid `net:<glob>` refused at manifest validation |
| T12 | Another local user connects to the Windows pipe / reads token files | named pipes default to broad ACLs; review S5 | Pipe SDDL `D:P(A;;GA;;;SY)(A;;GA;;;<user-sid>)`; token dir + token files ACL = user+SYSTEM only | **skip** — Windows-only (pipe SDDL + token-file ACLs need a second local account; the alpha is Linux-only with no Windows CI gate): covered by the DS-072 Windows test plan — `docs/contracts/ipc.md` §1.1, `docs/40-platforms.md` §2 (O5/T12), docs/30-testing.md §8 Windows gate |

Every row above is executed or referenced by `python tests/conformance/security/run.py` (DS-081), which prints the T1–T12 `green|red|skip` status table and exits nonzero on any red row; the mapping is registered in `tests/conformance/run.py` (`-k security`), and `tests/conformance/security/run.py` asserts this table lists exactly the rows it runs. §2a below details the DS-029 device-safety cases behind T8.

## 2a. Executable safety checks (DS-029) — every safety claim has a runnable case

`tests/conformance/safety/` (registered in `tests/conformance/run.py`, so
`python tests/conformance/run.py` runs it; `-k safety` runs only these cases; the suite
is also standalone-runnable) proves the device-safety claims of the threat model on the
**real binaries** over vdev, black-box. A red case blocks merge by design.

| Case (`tests/conformance/safety/run.py`) | Claim proven | Threat row | How it is checked |
|---|---|---|---|
| `test_safety_010_*` | Forbidden HID commands (firmware/serial/factory/reset/recovery) are **unrepresentable** through the ds-protocol public API; backlight-0 refused by every builder; reads need the software-mode token | T8 | `cargo test -p ds-protocol --test protocol_safety` (tests/protocol/safety.rs) must pass, incl. `no_builder_can_emit_a_forbidden_report`, `raw_path_rejects_exactly_the_pinned_forbidden_set`, `backlight_zero_is_refused_on_every_module`, `reads_require_the_software_mode_token` |
| `test_safety_020_*` | An attempted forbidden write on the device socket (the socket the daemon writes through) is refused with an ERROR frame and **zero bytes accepted**; the IPC model maps it to `E_DEVICE` / CLI exit 7 (`docs/contracts/ipc.md` §4) | T8 | raw frame client sends all 21 pinned forbidden (report id, command) pairs to a bare ds-vdev → every reply is `0x84` ERROR "refusing a forbidden report", never an `0x82` ACK; vdev's write log (`writes`) records every attempt `refused`; `ds schema ipc` enum contains `E_DEVICE`, `ds --help` maps `7 E_DEVICE` |
| `test_safety_025_*` | Defense in depth: host raw-path guard (`ensure_allowed` → `Error::Forbidden` before any byte leaves), device-side refusal in the hub, `EDevice`→`E_DEVICE`→exit 7 | T8 | `cargo test -p ds-device -- forbidden`, `-p ds-vdev -- forbidden`, `-p ds-ipc` |
| `test_safety_030_*` | The daemon's own traffic never carries a forbidden pair | T8 | zero `refused` entries in vdev's write log after driving image/LED pushes **and** a payload-level scan of every frame the daemon sent (recorded by a fake device socket speaking the pinned frame protocol) |
| `test_safety_040_*` | **No read/query and no other write on keys/dial before the software-mode handshake** (`03 3A 01` keys / `03 49 01` dial are the first frames per screen module; reading in hardware mode wedges the module) | T8 | frame recording of the daemon's traffic (first frame per screen pid is the handshake; no `FEATURE_GET`/`WRITE` before it), vdev write log (handshake is the first write per screen module) + `ds-vdevctl reports` (`software_mode=true`), and `cargo test -p ds-device -- handshake` (`reads_are_refused_before_the_handshake…`, `a_failed_send_is_never_followed_by_a_read`) |
| `test_safety_060_*` | **Backlight 0 is never sent** to the dial/keys (it wedges the screens; "off" is a black image) — a `set /controls/…/brightness 0` never becomes a `03 34 00` / `03 44 00` report | T8 | payload scan of all daemon frames: no backlight feature-set with pct 0, every backlight report is 1..=100; `ds-vdevctl reports` shows `backlight=` in 1..=100 on every module; builder refusal covered by 010 |
| `test_safety_070_*` | **No raw escape hatch**: no CLI verb, IPC op, stream-frame meta field or state visual kind takes raw HID bytes | T8 (a raw hatch would make the forbidden set representable; same "enforced, not asserted" rule as T5) | enumerated from the binary (`ds --help` verb set == the 9 phase-1 verbs; every candidate raw verb exits 1 "unknown verb"), from the CLI source verb arms, from the daemon's own `ds schema ipc` op consts (== the 18 declared ops) and `stream-frame`/`state` schema fields, plus live IPC probes of ~28 candidate raw ops (all answered `E_SCHEMA`/`E_INTERNAL`, never ok) and raw-bytes visual values (all `E_SCHEMA`, exit 1); `ds-vdevctl` stays within its inspection-only command set |
| `test_safety_080_*` | The same checks repeat on a **connected deck**, read-only (no writes beyond the software-mode handshake); skips cleanly without `DS_HARDWARE=1` or without a deck | T8 (on-hardware), T10-adjacent | `DS_HARDWARE=1 cargo test -p ds-device --test hid` (tests/hw/hid.rs: enumerate, open, one feature read on M0 modules only, handshake+init, close) |

Rows T1–T7 and T9–T12 are not device-wire claims; `tests/conformance/security/run.py` (DS-081, run directly or via `python tests/conformance/run.py -k security`) maps each of them to its executable case and prints its status — the ownership below stays the rationale
(T1/T9 auth+rotation: conformance + token tickets; T2/T5/T6 config/exec/path: bindings
suite DS-041 and the exec-policy ticket DS-043; T3 plugin sandbox, T6 plugin fs layer and
T11 plugin net: **executable in `tests/conformance/plugins/` (DS-053)** — see the
T3/T6/T11 Test cells above; T4 frame bombs: stream limits; T7 adapter: tests/conformance/preview/ (DS-113);
T10 vendor contention: hw smoke DS-026; T12 Windows ACLs: Windows CI/hw test).

## 3. Scoped tokens and capability grants

Install token = admin. `ds token issue --scope …` → scoped tokens in `tokens.json` (0600).
Scope = min(requested, granted); constant-time compare (`control.py:67` precedent).
Plugin caps: `state.read|state.write|device.write|exec|net:<glob>|fs:<root>|emit`, enforced
per request/effect. Binding exec: admin + `allow_exec` + `/policy/exec/allow` − `deny`.

## 4. Sandbox / rlimits

Plugin spawn: setrlimit NPROC/NOFILE/AS/CPU (+SIGXCPU), process group, pipes, stderr→log,
env = `{PATH, HOME, LANG, DS_PLUGIN_TOKEN}`. Landlock (Linux ≥6.7, OPEN O6): fs restricted
to `fs_roots` + read-only `/usr`; net connect denied where ABI ≥ 4 (OPEN O11). Windows:
job object `KILL_ON_JOB_CLOSE` + pipe/file ACLs (T12). No container runtime — boring.

## 5. `/policy` — the single policy surface (S13)

Every enforcement point has a schema field in `schemas/state.json` → `/policy`: `net.allow_hosts[]`,
`fs.roots[]`, `exec.allow[]`, `exec.deny[]` (pre-seeded with shells/interpreters), `led_rate_per_s`.
Admin scope only. No policy exists only in prose.

## 6. Explicit non-goals

- No protection against root or the user themselves (`ds token issue --scope admin` is the
  point of a local tool).
- No remote exposure: socket local; HTTP adapter loopback + opt-in.
- No multi-user isolation beyond file/pipe ACLs (T12).
- A plugin using its *granted* caps maliciously (e.g. `net:lights.lan` attacking that host)
  is a policy question — auditable in `/policy`, not preventable in core.
- Firmware updates out of scope (official app only, `upstream/PROTOCOL.md:168-170`).

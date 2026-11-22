# 20 — Security model

R8. Design: ADR-009, D14. Threat model first, then mitigations, then the test that proves
each. Prefixes: `hubdeck/` = `~/repos/hubdeck`, `upstream/` = `~/repos/hubdeck-upstream`.

## 1. Assets and trust boundaries

Assets: the deck hardware (bricking it is the worst outcome), the host (local code
execution = game over), the token file, user media. Trust boundaries: (a) anything that
connects to the daemon socket, (b) plugin processes, (c) state/config content, (d)
images/frames, (e) the optional HTTP/WS adapter vs the web.

## 2. Threat model

| # | Threat | Evidence it's real | Mitigation | Test |
|---|---|---|---|---|
| T1 | Local attacker with no token reads/mutates via the socket | Both apps gate only on a token in a 0600 file; Python's random TCP port is reachable by any local process that reads `control.json` (`hubdeck/.../control.py:51,81-90,67`) | Socket dir 0700, socket 0600, per-install token, scopes read/mutate/admin, constant-time compare; `read` scope cannot `set` | C: connect without token → `E_AUTH`; read-token issues `set` → `E_AUTH` exit 4 |
| T2 | Malicious config = RCE via code-in-config | `{python: …}` is `exec` in the engine process (`hubdeck/.../plugins/python.py:58`); analysis D5 kept it (`hubdeck/analysis/decision-log.md:45`) | Config/state is data only; `schemas/*.json` have no code field; no `eval` anywhere in core; behaviour = external programs (R4) | C: `set` a binding whose step is `{"do":"exec","argv":"sh -c …"}` without admin → `E_CAP`; config containing a string field that looks like code is inert data |
| T3 | Malicious plugin corrupts daemon / fork-bombs / holds the deck | In-process plugins share the engine (`hubdeck/.../plugins/base.py`); upstream declares permissions but "does not sandbox" (`upstream/.../engine/plugins.rs:56-58`) | Process-per-plugin + plugin_token + per-cap grants + rlimits (nproc/nofile/AS/CPU) + Landlock fs on Linux ≥6.7 (OPEN O6) + watchdog kill/respawn ≤3/10 min + kill-on-exit | C: plugin spawning 1000 children → killed by nproc, daemon healthy; plugin writing outside `fs_roots` → `E_CAP`; plugin crash → respawn, `/plugins/<id>/status` shows it |
| T4 | Image/frame bombs (decompression DoS, memory) | Caps exist but per-app: 10 MB download (`hubdeck/.../render/images.py:21`), 20 MB send (`control.py:36`), 16 MB body (`upstream/.../plugins.rs:266`) | Wire cap 16 MB; decode caps ≤4096×4096 px, ≤300 frames, ≤30 fps; frame queues bounded (2/target, 256 MiB total, latest-wins drop) | Fuzz: max-size/max-rate frames at the stream socket on ds-vdev; assert memory flat (RSS delta < 50 MB) and control p99 within budget |
| T5 | Shell injection through shellouts | `launch.run` uses `shell=True` (`hubdeck/.../platforms/posix.py:70`); `hubdeck send action` can run any config action incl. launch (`control.py:101-123`) | Core never spawns a shell; `exec` step = argv array only, cwd confined to media root or /tmp, env scrubbed, requires admin scope + `allow_exec` + `/policy` exec allowlist | C: argv `["sh","-c","touch /tmp/pwn"]` with metachars in args → file NOT created (no shell interpretation); exec without admin → `E_CAP` |
| T6 | Path traversal via plugin/fs paths | GUI confines edits to config dir (`hubdeck/.../gui/server.py:428-436`) but actions take arbitrary paths | fs access confined to declared roots: plugin `fs_roots`, media root for `file` images, `/tmp`; daemon resolves + canonicalizes and rejects `..` escaping the root | C: `set /controls/keys/0/image {"kind":"file","path":"../../../../etc/shadow"}` → `E_SCHEMA`/`E_PATH` |
| T7 | DNS rebinding on the HTTP/WS adapter | Python GUI checks Host (`hubdeck/.../gui/server.py:601`); Rust relies on bearer only | Adapter binds 127.0.0.1 only; requires `Host: 127.0.0.1`/`localhost` AND bearer token on every request incl. WS upgrade | C: request with `Host: evil.example` → 421; WS upgrade without token → 401 |
| T8 | Device bricking via firmware/serial/reset commands | FORBIDDEN list exists because firmware accepts them (`upstream/crates/mh-protocol/src/safety.rs:8-32`; `hubdeck/.../device/hid.py:52-62,85-104`) | Unrepresentable: builders only for safe commands + raw-path `is_forbidden` refusal (`report.rs:28-30`); backlight-0 refused on dial (`upstream/PROTOCOL.md:61`); hardware-mode reads refused (`module.rs:158-166`) | Unit: assert no builder output is forbidden (precedent `safety.rs:40-79`); vdev test: daemon path attempting raw forbidden report → `E_DEVICE`, nothing written |
| T9 | Token file leakage / rotation races | per-run token written 0600 (`control.py:86-90`) | 0600 token file, dir 0700; `ds token rotate` keeps old token valid 60 s (OPEN O10) | C: old token rejected after grace; file perms asserted in CI |
| T10 | Vendor app contention (two holders fight over HID) | `hubdeck/CLAUDE.md:66`; `upstream/PROTOCOL.md:217-221` | Startup check: busy → `ds daemon status` names the holder; guard child does handback on crash (`upstream/.../src/guard.rs`) | Smoke (hardware): start daemon with vendor app open → clear error, no writes sent |

## 3. Scoped tokens and capability grants

- Install token = `admin`. `ds token issue --scope mutate` → scoped token in
  `tokens.json` (0600). Scope check: min(requested, granted); constant-time compare
  (`hubdeck/.../control.py:67` precedent).
- Plugin caps (`schemas/plugin-manifest.json`): `state.read|state.write|device.write|exec|net:<glob>|fs:<root>|emit`; enforced per request/effect; violations counted + logged.
- Binding exec: admin scope + `allow_exec:true` + `/policy/exec/allow` argv-prefix list.

## 4. Sandbox / rlimits

Plugin spawn: `setrlimit` NPROC/NOFILE/AS/CPU (+ SIGXCPU), process group, stdin/stdout
pipes, stderr→log, env = `{PATH, HOME, LANG, DS_PLUGIN_TOKEN}` only (no secrets).
Landlock (Linux ≥ 6.7, OPEN O6): restrict fs to `fs_roots` + read-only `/usr`, no
network unless `net:*`. Windows: job object `KILL_ON_JOB_CLOSE` (`upstream/.../plugins.rs:180-205`).
No container runtime — deliberately boring.

## 5. Explicit non-goals

- No protection against root or against the user themselves (the user can `ds token
  issue --scope admin` — that is the point of a local tool).
- No remote/network exposure: the socket is local; the HTTP adapter is loopback-only and
  opt-in. No TLS needed on loopback.
- No multi-user isolation beyond file permissions.
- No guarantee against a plugin that legitimately uses its granted caps maliciously
  (e.g. `net:lights.lan` used to attack that host) — policy lives in `/policy` (D18),
  auditable in state.
- Firmware updates remain out of scope entirely (official app only, `upstream/PROTOCOL.md:168-170`).

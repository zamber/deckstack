# ADR-003 — IPC transport and framing

Status: accepted (planning).

## Context

R7: local IPC for CLI/daemon (Unix socket; named pipe on Windows) plus optional
WebSocket/HTTP for UIs and agents; one event model, several transports. Both current apps
use ephemeral loopback TCP + port/token files (`hubdeck/.../control.py:81-90`;
`upstream/.../engine/plugins.rs:336-358`) — racy across users/installs (discovery/20 §2).

## Decision

- Primary: Unix domain socket at `$XDG_RUNTIME_DIR/deckstack/daemon.sock` (fallback
  `~/.local/state/deckstack/`), directory 0700, socket 0600. Windows: named pipe
  `\\.\pipe\deckstack-<user>` — same framing via the `ds-ipc` shim. Contract:
  `docs/contracts/ipc.md`.
- Framing: NDJSON for control (1 MiB line cap); binary `DSF1` frames on a second stream
  endpoint for the frame plane (16 MB cap); optional WS/HTTP adapter on 127.0.0.1 with
  bearer token + Host check (T7).
- Versioning: `hello {"protocol":1}` handshake; additive-only changes, breaking changes
  bump `protocol`; schemas carry `/v1` `$id`s.
- Auth: per-install token file 0600 with scopes read/mutate/admin (D14); constant-time
  compare (`hubdeck/.../control.py:67` precedent). `E_SCOPE` distinguishes valid-token-
  insufficient-scope from `E_AUTH`. Rotation (S12/M4): instant for new connections; ≤ 5 s
  grace only for in-flight requests on an existing connection (connection-generation
  counter). Windows pipe ACL (S5/T12): SDDL `D:P(A;;GA;;;SY)(A;;GA;;;<user-sid>)`, token
  files user+SYSTEM only.

## Alternatives

- **Loopback TCP + port file (status quo)** — rejected: ephemeral ports, racy file
  discovery, weaker perms story.
- **D-Bus** — rejected: Linux-only, session policy complexity, R9 needs Windows parity.
- **Plain files / maildir** — rejected: no backpressure, no auth, no subscriptions.

## Consequences

Stable path = agents can start/inspect deterministically (tension 7). Windows pipe
throughput is unmeasured (OPEN O5) — phase 2 VM test.

## References

BRIEF.md R1/R7/R8/R9; docs/contracts/ipc.md; decision log D04.

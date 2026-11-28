# ADR-006 — Plugin isolation: process-per-plugin, capability grants, rlimits

Status: accepted (planning).

## Context

Tension 5 + R6/R8: plugins must be process/API-based with declared actions + JSON-Schema
params, misbehaviour must not corrupt the daemon, and the Python ecosystem should be
reusable. Current state: hubdeck plugins are in-process by default
(`hubdeck/.../plugins/base.py:112-272`) with opt-in socketpair isolation
(`plugins/host/remote.py`); upstream runs one process per plugin over WebSocket but
permissions are declared and displayed only — "does not sandbox plugins"
(`upstream/.../engine/plugins.rs:56-58`).

## Decision

One OS process per plugin; newline-JSON over stdio; manifest with JSON-Schema params
(`schemas/plugin-manifest.json`); per-plugin capability token (`state.read|write,
device.write, exec, net:<glob>, fs:<root>, emit`); rlimits (nproc/nofile/AS/CPU) +
Landlock fs policy on Linux ≥ 6.7 (RESOLVED DS-116: ABI 11 measured on the target kernel family), else host-enforced `fs_roots`
prefix checks; watchdog respawn ≤ 3/10 min; kill-on-exit (job object on Windows). Effects
are schema-validated before touching state; invoke timeout 2 s → degraded → restart.
Python compatibility via the `ds-plugin-py` wrapper (D09): it guarantees process
isolation, lifecycle, wire contract and resource limits — NOT deep engine-internal API
compat (OPEN C1); plugins needing internals are ported.

## Alternatives

- **In-process plugins (hubdeck default)** — rejected: one plugin corrupts daemon state; R6
  forbids it.
- **Upstream model (WS + declared-only permissions)** — rejected: no enforcement (R8).
- **Containers/WASM** — rejected: heavy, not boring, Windows pain; rlimits+Landlock cover
  the stated threats (docs/20-security.md T3).

## Consequences

Plugin quirks never leak into core (invariant, docs/components/plugin-host.md). Cost:
spawn overhead per plugin (~ms) and a wrapper to maintain.

## References

BRIEF.md R6/R8; docs/contracts/plugins.md; decision log D08/D09.

# ADR-009 — Security model: local-only, scoped tokens, capability grants, no eval

Status: accepted (planning).

## Context

R8: treat malicious state/config/plugins as real. Evidence of real vectors in the current
apps: per-run token on random TCP (`hubdeck/.../control.py:51,81-90`), `exec` in config
(`plugins/python.py:58`), `shell=True` shellouts (`platforms/posix.py:70`), unsandboxed
plugins (`upstream/.../plugins.rs:56-58`), image caps per-app (10/16/20 MB), GUI Host check
(`gui/server.py:601`). Threat model: docs/20-security.md.

## Decision

Local-only by default. Per-install token (0600 file, 0700 dir) with scopes
read/mutate/admin, constant-time compare; scoped tokens via `ds token issue`. No `eval` of
config anywhere; `exec` only via admin scope + `allow_exec` binding + argv-only + policy
allowlist + rlimits + audit event. Plugins: capability grants enforced per request
(ADR-006). Images: 16 MB wire, ≤ 4096² decode, ≤ 300 frames, bounded queues. HTTP/WS
adapter: 127.0.0.1 only, Host allowlist + bearer (DNS rebinding, T7). Device safety:
forbidden commands unrepresentable (ADR-010), backlight-0 refused, hardware-mode reads
refused. Path policy: canonicalize + root confinement (media root, plugin fs_roots, /tmp).
Non-goals: root/user self-protection, remote exposure, multi-user (docs/20-security.md §5).

## Alternatives

- **Per-run tokens (status quo)** — rejected: break stateless CLI across daemon restarts.
- **Trust all local processes** — rejected: R8 explicitly names malicious local config as
  a threat.
- **Full sandboxing (seccomp/containers)** — rejected as default: rlimits+Landlock cover
  T3; seccomp profile is a phase-5 hardening option if Landlock is unavailable (OPEN O6).

## Consequences

Every threat T1–T10 has a test in docs/20-security.md §2; conformance suite runs them.

## References

BRIEF.md R8; docs/20-security.md; decision log D14.

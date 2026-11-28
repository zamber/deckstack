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
read/mutate/admin, constant-time compare; scoped tokens via `ds token issue`; **rotation
is instant for new connections** with ≤ 5 s in-flight grace via connection generation
(S12/M4). No `eval` of config anywhere; `exec` only via admin scope + `allow_exec` +
argv[0] absolute (schema) + `/policy/exec/allow` − `/policy/exec/deny` (shells
pre-denied — S3) + rlimits + audit; fire-and-forget (S11). Plugins: capability grants
enforced per request (ADR-006); **network is host-mediated** — plugins never open sockets,
`sdk.http` → daemon checks the anchored host glob (`*` = one DNS label, M5) against the
plugin grant and `/policy/net/allow_hosts`; Landlock net-deny where ABI ≥ 4, documented gap
otherwise (S4/T11). Images: 16 MB wire, ≤ 4096² decode, ≤ 300 frames, bounded queues;
file paths reject `..` at schema level + canonicalized root confinement (M3/T6). HTTP/WS
adapter: 127.0.0.1 only, Host allowlist + bearer (T7). Windows: pipe SDDL + file ACLs
user/SYSTEM only (S5/T12). `/policy` is fully schema'd (S13). Device safety: forbidden
commands unrepresentable (ADR-010), backlight-0 refused, hardware-mode reads refused.
Non-goals: root/user self-protection, remote exposure, multi-user beyond ACLs
(docs/20-security.md §6).

## Alternatives

- **Per-run tokens (status quo)** — rejected: break stateless CLI across daemon restarts.
- **Trust all local processes** — rejected: R8 explicitly names malicious local config as
  a threat.
- **Full sandboxing (seccomp/containers)** — rejected as default: rlimits+Landlock cover
  T3; seccomp profile is a phase-5 hardening option if Landlock is unavailable (RESOLVED DS-116: available, ABI 11, on the target kernel family).

## Consequences

Every threat T1–T10 has a test in docs/20-security.md §2; conformance suite runs them.

## References

BRIEF.md R8; docs/20-security.md; decision log D14.

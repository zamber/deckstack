# ADR-007 — Action model: bindings-as-data, tiny core vocabulary, no catalog

Status: accepted (planning).

## Context

Tensions 3+4 + R3/R4/R8: every observable attribute settable; arbitrary behaviour without
RCE; the daemon must not become a dumb device service nor another closed action catalog.
Both current engines hardcode vocabularies (`upstream/.../engine/actions.rs:105`;
`hubdeck/.../plugins/__init__.py:22-28` + `ALIASES` `config/schema.py:55-67`) and Python
extends it with `exec` in config (`plugins/python.py:58`).

## Decision

Events on topics (`deck/control/…`, `deck/device/…`, `deck/system/…`, `app/<sender>/…`)
are routed to **bindings stored in state** (`/bindings/…`, `schemas/binding.json`).
Core vocabulary is closed and small: `set | emit | plugin | exec | sleep` + `when`
conditions. `exec` is argv-only (no shell anywhere), needs admin scope + per-binding
`allow_exec` + `/policy` allowlist, rlimits, audit event. Everything else — window
watching, cron, webhooks, media — is an external program subscribing and calling the CLI
(R4). The daemon is a router + state + device, never an app.

## Alternatives

- **Hardcoded catalog (status quo)** — rejected: the walled garden being replaced; new
  behaviour would require forking core (BRIEF problem statement).
- **python-in-yaml / eval config** — rejected: R8 (S2).
- **Shell-string actions** — rejected: injection (S5, `platforms/posix.py:70`).
- **Full rule DSL (conditions, loops)** — rejected: complexity; composition happens
  between programs via topics, `when` stays a single predicate.

## Consequences

Latency budget (event p99 ≤ 5 ms, plugin ≤ 2 s) in docs/contracts/actions.md §5. Chains A/B
in §6 are the conformance tests.

## References

BRIEF.md R3/R4/R8; docs/contracts/actions.md; decision log D07.

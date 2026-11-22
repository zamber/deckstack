# ADR-002 — CLI language and the stateless-call vs query-speed tension

Status: accepted (planning).

## Context

Tension 1: every read/write goes through a stateless CLI (R1), but a fresh process per call
pays startup cost; under 32 fps frame load the query path must stay p50 ≤ 2 ms / p99 ≤ 10 ms
(R2, D13).

## Decision

The CLI `ds` is a Rust binary sharing the daemon's `ds-client` crate. One invocation =
connect + auth + one request + print + exit (~1–3 ms total, measured in phase 1 against the
budget; interpreter CLIs cost 30–50 ms and would dominate p50). Long-lived needs are
served by `ds watch <path>` — a persistent subscription on one connection — not by a
client-side cache. A Python client *library* is published later for composition, but the
CLI binary itself stays Rust. The daemon holds the only state; the CLI keeps none between
invocations (invariant, docs/components/cli.md).

## Alternatives

- **Python CLI** — rejected on startup cost and on drift (two protocol impls).
- **Read state.json directly per call** — rejected: no single writer, races with the daemon,
  no auth, no transactions.
- **Persistent client daemon** — rejected: that's a second daemon (R1 violation).

## Consequences

p50/p99 budget is achievable and asserted by `cargo bench -p ds-bench -- query-under-load`.
Negative: shell users must quote JSON (examples in docs/contracts/cli.md §6, incl.
PowerShell).

## References

BRIEF.md R1/R2/R10/R11; docs/contracts/cli.md, docs/contracts/state.md §5; decision log D02,
D13.

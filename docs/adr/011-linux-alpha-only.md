# ADR-011 — Alpha is Linux-only; Windows/macOS are design TODOs

Status: accepted (owner steering 2026-11-22, delta 1).

## Context

R9 names Linux primary, Windows supported shape, macOS reasonable. Owner steering: alpha
ships Linux only; the other platforms are unproven (Windows pipe perf OPEN O5, macOS
untested O8) and would split the small team.

## Decision

Alpha implements Linux (Bazzite/Fedora, KDE Wayland first) only. `docs/40-platforms.md` §2–3
carry Windows/macOS as TODO sections: exactly what to implement, where the shim goes
(`ds-platform`/`ds-ipc`), what must be tested, and "do not start until Linux alpha is
stable". The shims stay in the core so ports are additive. No alpha phase exit criterion
or CI gate mentions Windows/macOS (`docs/60-roadmap.md`, `docs/30-testing.md` §8).

## Alternatives

- Ship Windows in alpha too — rejected: unmeasured pipe transport (O5), no hardware gate possible now, halves the Linux effort where the users are.
- Drop the shims — rejected: R9 still holds post-alpha; keeping `ds-ipc`/`ds-platform` makes the ports additive instead of a refactor.

## References

BRIEF.md amendment 2026-11-22; `docs/00-scope.md`; `docs/40-platforms.md`; decision log D31.

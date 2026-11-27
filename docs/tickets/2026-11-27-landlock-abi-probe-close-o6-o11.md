# Ticket DS-116 — close open questions O6/O11 with a local Landlock ABI probe

Date: 2026-11-27. Type: docs+tooling ticket (spec filed from
`docs/reviews/2026-11-27-opportunities.md` item P1-1 / section 1, rows O6/O11).
Scope guard: documentation + one stdlib-only probe script + doc-marker assertions.
**No hardware needed — vdev/local tests only** (a raw syscall probe plus
`cargo test` and the existing vdev conformance suite; no deck, no camera, no
network).

## Motivation

O6 (Landlock fs confinement) and O11 (Landlock net rules, ABI ≥ 4) are the only
two open questions closable without hardware. The 2026-11-27 review already ran
the deciding probe on this machine (kernel `7.3.5-ogc3.1.fc45`, same Fedora
family as Bazzite): `landlock_create_ruleset(NULL, 0,
LANDLOCK_CREATE_RULESET_VERSION)` via syscall 444 returned **ABI 11**. The
daemon already feature-detects Landlock at runtime, so the only remaining work
is to record the measured ABI in the docs and ship the probe as a repeatable
tool.

## Requirements

1. **Add `tools/landlock_probe.py`** — stdlib-only Python wrapper around
   `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` via
   syscall 444 (x86_64; use `libc.syscall` through `ctypes`). Prints
   `Landlock ABI version on this kernel: N` and exits 0; on kernels without
   Landlock prints the errno and exits 2. No network, no hardware, no daemon.
2. **Close O6** in `docs/open-questions.md:24`: mark the row RESOLVED, citing
   the measured ABI 11 on kernel `7.3.5-ogc3.1.fc45` (Fedora family of Bazzite)
   and noting ABI ≥ 1 covers fs, so the full fs ruleset used by the daemon is
   available (the daemon requests `AccessFs::from_all(ABI::V10)` for the
   ruleset and `ABI::V5` rights for `fs_roots` — both ≤ 11).
3. **Close O11** in `docs/open-questions.md:29`: mark RESOLVED — ABI 11 ≥ 4, so
   `LANDLOCK_ACCESS_NET_CONNECT_TCP`/`BIND_TCP` are available and the net-deny
   second layer is active on the target kernel family.
4. **Update `docs/20-security.md`**: line 20 (T3 row — drop the `OPEN O6`
   marker, keep the Landlock mention), lines 68-69 (§4 — replace
   "Landlock (Linux ≥6.7, OPEN O6)" / "where ABI ≥ 4 (OPEN O11)" with the
   confirmed wording: ABI 11 measured, fs + net-deny both active), line 28
   (T11 row — the "where unavailable the mediation is the only control"
   caveat becomes "not applicable on the target kernel family; kept for
   older kernels").
5. **Update `docs/contracts/plugins.md` §2**: line 25 (ABI ≥ 4 restricts ports,
   not hostnames — unchanged fact) and line 37 (drop the `OPEN O6/O11` marker;
   state that on the target kernel family ABI 11 ≥ 4 so the outbound-connect
   deny is active; host-mediated `net` requests remain the primary control
   regardless, S4). Line 53 (lifecycle: "Landlock fs + net-deny where
   available") may keep its "where available" phrasing for old kernels.
6. **Doc-marker test.** Extend the conformance/docs checks (e.g. a small case
   in `tests/conformance/security/run.py` or a grep assertion alongside
   `tests/fixtures/validate.sh`) asserting that `docs/open-questions.md`,
   `docs/20-security.md`, and `docs/contracts/plugins.md` no longer contain
   the strings `OPEN O6` / `OPEN O11`. This locks the closure against silent
   re-opening.

## Acceptance

- `python3 tools/landlock_probe.py` prints `Landlock ABI version on this kernel: 11`
  on this machine (and a clean exit 2 with errno on a Landlock-less kernel).
- `cargo test -p ds-daemon landlock` passes, including the existing
  feature-detection unit test at `crates/ds-daemon/src/plugins/spawn.rs:374`
  (`landlock_status_is_reported_on_this_kernel`).
- `python tests/conformance/run.py -k plugins_040 -k plugins_070` passes; both
  already assert the raw-socket deny when the plugin's landlock status line
  says restricted (`tests/conformance/plugins/run.py:86-89,313,461`).
- `grep -rn "OPEN O6\|OPEN O11" docs/` returns nothing; `sh tests/fixtures/validate.sh`
  prints ALL CHECKS PASSED.

## References

- `docs/reviews/2026-11-27-opportunities.md` §1 rows O6/O11 and §5 P1-1
  (measured ABI 11 during the review).
- `crates/ds-daemon/src/plugins/spawn.rs:178-181` (runtime `landlock_restrict`
  call + status messages), `:225-262` (`landlock_restrict`; net ABI ≥ 4
  fallback at `:233-239`), `:374-396` (unit test).
- `docs/open-questions.md:24` (O6), `:29` (O11); `docs/20-security.md:20,28,68-69`;
  `docs/contracts/plugins.md:25,37,53`.
- `tests/conformance/plugins/run.py:86-89` (`landlock_enforced`), `:313`
  (plugins_040), `:461` (plugins_070); `tests/conformance/security/run.py:295`
  (T11 entry).

# AGENTS.md — contributing to deckstack

deckstack is a CLI-first Linux daemon + CLI that drives a Cooler Master MasterHub deck.
This file is the contributor and coding-agent guide. Scope/invariants: `docs/00-scope.md`
(**normative**). Index: `README.md`. Plan: `PLAN.md`.

## Read order (first session)

1. `BRIEF.md` (R1–R12 + Scope amendment 2026-11-22) → 2. `docs/00-scope.md` →
3. `PLAN.md` → 4. `docs/contracts/` + `schemas/` (contracts are normative) →
5. `docs/components/` for the component you touch → 6. this file.

**Agents driving a deck (not working on the codebase)?** Read `docs/agents.md` — the one
route from install → udev permissions → daemon → `ds-mcp`/`ds-py` → the 10 MCP tools →
**micro-apps** (`docs/microapps.md`, examples in `examples/microapps/`) →
troubleshooting, with the scripted proof in `tests/conformance/agent-onboarding/` and
`tests/conformance/microapps/`.

**Upkeep rule for `docs/agents.md`:** it is a living guide, not a snapshot. When a contract,
tool name, default, or install path changes (a `docs/contracts/*` or `schemas/*` change, a
`ds-mcp` tool, a `ds` verb/flag, a packaging layout or version), update `docs/agents.md` in
the same commit — and keep `tests/conformance/agent-onboarding/run.py` green so the page
stays executable. Same for the authoring route: when a contract, tool name, default,
install path or example changes, update `docs/microapps.md` and the examples in
`examples/microapps/` in the same commit — and keep `tests/conformance/microapps/run.py`
green so the guide and its examples stay executable.

## Invariants (never violate; conformance tests enforce them)

- Linux-only alpha; empty state at start; no presets/sample decks; no `ds migrate`; no web
  UI; CLI-first (every capability reachable via `ds` with no UI).
- Contracts are normative: change `schemas/*.json` and `docs/contracts/*` in the same
  commit; keep `sh tests/fixtures/validate.sh` green (ALL CHECKS PASSED).
- No `eval` of config, no shell spawns; exec is argv-only + `/policy/exec` allow−deny.
- Frames never enter JSON state or bump `rev` (S1); stats in `/telemetry` only.
- Forbidden HID commands stay unrepresentable (mh-protocol safety); never backlight-0 the
  dial; software-mode handshake before reads.

## Commands

```sh
cargo build --workspace                  # build everything
sh tests/fixtures/validate.sh            # schema/fixture gate (must print ALL CHECKS PASSED)
sh tools/check.sh                        # the single local gate: fixture gate + cargo fmt/clippy/test + conformance
python3 tests/conformance/run.py         # the conformance suite (also run by tools/check.sh)
python3 tests/e2e/run.py                 # end-to-end smoke on the virtual device
DS_HARDWARE=1 python3 tests/hw/smoke.py  # hardware smoke (needs a real deck + camera, phase 2+)
```

`sh tools/check.sh [tree-root]` validates this tree, or the tree passed as an argument.

## Layout

```
crates/    ds-protocol, ds-device, ds-state, ds-ipc, ds-client, ds-daemon, ds-vdev,
           ds-plugin-sdk, ds-bench
bins/      ds (CLI), ds-daemon, ds-vdev, ds-vdevctl, ds-focus, ds-mcp
schemas/   the JSON Schemas (normative wire formats)
docs/      00-scope (normative), contracts/, components/, adr/, discovery/, reviews/, …
tests/     fixtures/ (schema gate), conformance/, e2e/, golden/, hw/, bench/, …
tools/     check.sh (local gate), verify/ (camera harness)
packaging/ release.sh, systemd unit, udev rules, AppImage/AppDir, linux install scripts
examples/  micro-apps, agent configs, plugin demo
plugins-py/  Python plugin SDK (ds-plugin-py, PyPI)
clients/ds-py/  stdlib-only Python client
```

## Contracts and wire formats

Contracts live in `docs/contracts/*.md` with machine-readable schemas in `schemas/*.json`;
`tests/fixtures/validate.sh` checks both against the example corpora. **To change a wire
format:** update the schema and the contract doc in the same commit, bump the schema `$id`
version on breaking changes only, add/extend examples in `tests/fixtures/`, and keep
`sh tools/check.sh` green (fixture gate + conformance). Never change crate code, schemas,
or test expectations in a way that alters daemon behaviour without an ADR in `docs/adr/`
and a row in `docs/decision-log.tsv`.

## Working rules

- Work on a branch off `main`; keep commits small and ordered; verify each unit before the
  next (`sh tools/check.sh`).
- `docs/00-scope.md` is normative: if a change conflicts with it, the change is wrong
  unless the scope doc is amended first (with an ADR).
- Don't edit `docs/discovery/` — it is a read-only evidence record of the two predecessor
  codebases.
- Code comments may reference implementation history ids (e.g. `// DS-070: …`); keep them.

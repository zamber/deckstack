# Contributing to deckstack

deckstack is a Linux daemon and CLI that drives a Cooler Master MasterHub deck.
This file is the contributor and coding-agent guide. The scope and invariants are
normative in `docs/00-scope.md`. The index is `README.md`. The plan is `PLAN.md`.

## Read order for a first session

1. `BRIEF.md`, requirements R1 to R12 and the scope amendment of 2026-11-22.
2. `docs/00-scope.md`.
3. `PLAN.md`.
4. `docs/contracts/` and `schemas/`. These are normative.
5. `docs/components/` for the component you touch.
6. This file.

If you drive a deck rather than change the codebase, read `docs/agents.md`. It
covers install, udev permissions, the daemon, `ds-mcp` and `ds-py`, the ten MCP
tools, micro-apps, and troubleshooting. The scripted proof is in
`tests/conformance/agent-onboarding/` and `tests/conformance/microapps/`.

`docs/agents.md` is a living guide. When a contract, tool name, default, or
install path changes, update it in the same commit. Keep
`tests/conformance/agent-onboarding/run.py` green so the guide stays
executable. Apply the same rule to `docs/microapps.md` and the examples in
`examples/microapps/`, with `tests/conformance/microapps/run.py` as the check.

## Invariants

The conformance tests enforce these.

- The alpha is Linux only. The daemon starts from an empty state. It ships no presets or sample decks. There is no `ds migrate`. There is no web UI. Every capability is reachable through `ds`.
- Contracts are normative. Change `schemas/*.json` and `docs/contracts/*` in the same commit. Keep `sh tests/fixtures/validate.sh` printing `ALL CHECKS PASSED`.
- The daemon does not evaluate config. It never spawns a shell. An exec step takes an argv list and must pass the `/policy/exec` allow and deny lists.
- Frames never enter JSON state and never bump `rev` (S1). Frame statistics live in `/telemetry`.
- Forbidden HID commands stay unrepresentable (mh-protocol safety). Never set the dial backlight to 0. Send the software-mode handshake before reads.

## Commands

```sh
cargo build --workspace
sh tests/fixtures/validate.sh
sh tools/check.sh
python3 tests/conformance/run.py
python3 tests/e2e/run.py
DS_HARDWARE=1 python3 tests/hw/smoke.py
```

`sh tools/check.sh [tree-root]` validates this tree, or the tree passed as an
argument. The fixtures gate must print `ALL CHECKS PASSED`. The check script
also runs rustfmt, clippy, the workspace tests and the conformance suite.

## Layout

```
crates/      ds-protocol, ds-device, ds-state, ds-ipc, ds-client, ds-daemon,
             ds-vdev, ds-plugin-sdk, ds-bench
bins/        ds (CLI), ds-daemon, ds-vdev, ds-vdevctl, ds-focus, ds-mcp
schemas/     JSON Schemas for the wire formats
docs/        00-scope (normative), contracts/, components/, adr/, discovery/,
             reviews/
tests/       fixtures/ (schema gate), conformance/, e2e/, golden/, hw/, bench/
tools/       check.sh (local gate), verify/ (camera check)
packaging/   release.sh, systemd unit, udev rules, AppImage tree, install scripts
examples/    micro-apps, agent configs, plugin demo
plugins-py/  Python plugin SDK (ds-plugin-py, PyPI)
clients/     ds-py, a stdlib-only Python client
```

## Contracts and wire formats

Contracts live in `docs/contracts/*.md`. Machine-readable schemas live in
`schemas/*.json`. `tests/fixtures/validate.sh` checks both against the example
corpora.

To change a wire format, update the schema and the contract doc in the same
commit. Bump the schema `$id` version only for a breaking change. Add or extend
examples in `tests/fixtures/`. Keep `sh tools/check.sh` green. Do not change
crate code, schemas, or test expectations in a way that alters daemon behaviour
without an ADR in `docs/adr/` and a row in `docs/decision-log.tsv`.

## Working rules

- Work on a branch off `main`. Keep commits small and ordered. Run `sh tools/check.sh` before each unit lands.
- `docs/00-scope.md` is normative. If a change conflicts with it, amend the scope doc first with an ADR.
- Do not edit `docs/discovery/`. It records evidence about the two predecessor codebases.
- Code comments may cite implementation history ids, for example `// DS-070: ...`. Keep them.

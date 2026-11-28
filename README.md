# deckstack

A ground-up, CLI-first replacement base for the MasterHub/HubDeck controller stack
(Cooler Master MasterHub + key, dial, fader, knob and roller modules). Governing intent:
[`BRIEF.md`](BRIEF.md), requirements R1 to R12 and the scope amendment of 2026-11-22.
**Scope is normative in [`docs/00-scope.md`](docs/00-scope.md)**. The alpha is Linux only,
starts from an empty state, ships no presets or sample decks, has no migration or import,
has no web UI, and treats agent steering as first-class. Evidence about the two earlier
implementations is in [`docs/discovery/`](docs/discovery/). That directory is read-only.
Citation prefixes: `hubdeck/` is the Python predecessor, `upstream/` is the Tauri
predecessor.

**Status: alpha scope, working software.** The daemon, CLI, virtual device, and
conformance suites exist and run (see "Run it"). Nothing here edits the two predecessor
codebases.

## Run it

Operator guide: **[docs/runbook.md](docs/runbook.md)**. It covers install and build, first start on the
virtual device, real hardware (udev and a `hid:` URI), the systemd user unit, logs, status and health,
token handling, restart and upgrade, crash recovery, backup and restore, and troubleshooting.

```sh
cargo build --release --workspace                       # target/release/{ds,ds-daemon,ds-vdev,ds-vdevctl}
python3 tests/e2e/run.py --no-bench                     # end-to-end demo: boots ds-vdev + ds-daemon
                                                        # on a temp dir and drives a key image through it
# or by hand:
target/release/ds-vdev --socket /tmp/vdev.sock &
target/release/ds daemon start --device virtual:/tmp/vdev.sock
target/release/ds daemon status                         # ok running pid=… rev=… device=…
target/release/ds set /controls/keys/0/image --json '{"kind":"color","value":"#3cc86e"}'
target/release/ds daemon stop
```

Current capabilities: `ds get`, `set`, `colors`, `watch`, `schema`, `describe` and `status`.
Also `ds bind` and `unbind`, `ds daemon start`, `stop` and `status`, and `ds stream push`,
`stats` and `stop`. The frame-plane push is wired in the CLI, but the daemon does not bind
`stream.sock` in this tree yet. The `token`, `emit`, `events` and `plugin` verbs are
later-phase and refused with exit 1. Linux only. No `ds migrate`. Empty state at first start.
## Working on this repo

```sh
cargo build --workspace          # build everything
sh tools/check.sh                # the single local gate: fixture gate + cargo fmt/clippy/test + conformance
python3 tests/e2e/run.py         # end-to-end smoke on the virtual device
```

Docs start at [`docs/00-scope.md`](docs/00-scope.md) (normative alpha scope) and
[`PLAN.md`](PLAN.md), the one-page plan. Contributor and coding-agent conventions live in
[`AGENTS.md`](AGENTS.md). Wire formats are contracts. Change `schemas/*.json` and
`docs/contracts/*` together and keep `sh tests/fixtures/validate.sh` printing

## How to read this repo

| You are… | Start at |
|---|---|
| An operator running the daemon | [docs/runbook.md](docs/runbook.md) (start/stop, logs, tokens, recovery, troubleshooting) |
| A contributor or coding agent | [AGENTS.md](AGENTS.md) then [docs/00-scope.md](docs/00-scope.md) then [PLAN.md](PLAN.md) |
| A coder starting on the core | [PLAN.md](PLAN.md) then §First walking skeleton then [docs/60-roadmap.md](docs/60-roadmap.md) |
| Checking what is in/out of alpha | [docs/00-scope.md](docs/00-scope.md) (one hop from here and from PLAN) |
| Reviewing a load-bearing choice | PLAN.md decision table then the ADR in [docs/adr/](docs/adr/) then [docs/decision-log.tsv](docs/decision-log.tsv) |
| Implementing a component | [docs/components/](docs/components/) |
| Implementing a wire format | [docs/contracts/](docs/contracts/) + [schemas/](schemas/) + [tests/fixtures/](tests/fixtures/) |
| Verifying on hardware | [docs/90-verification.md](docs/90-verification.md) (camera check) |
| Auditing safety | [docs/20-security.md](docs/20-security.md) |
| Wondering what is unverified | [docs/open-questions.md](docs/open-questions.md) |

Reading order for full context: `BRIEF.md` then `docs/00-scope.md` then `docs/discovery/10..50` then
`PLAN.md` then `docs/10-architecture.md` then contracts then components then ADRs.

## Repo topology

```
deckstack/                    this repo (one workspace: contracts + crates + bins + tests + docs)
├── BRIEF.md                  governing intent (R1-R12) + scope amendment
├── AGENTS.md                 contributor / coding-agent guide
├── README.md  PLAN.md        this index / one-page plan
├── skills/deckstack/SKILL.md agent orientation skill
├── docs/
│   ├── 00-scope.md           normative alpha scope and invariants. Read first.
│   ├── discovery/            cited facts about the two predecessors. Read-only.
│   ├── reviews/              adversarial review and response
│   ├── decision-log.tsv      one row per material decision
│   ├── open-questions.md     OPEN items and contradictions, each with a resolving experiment
│   ├── 10-architecture.md    process model, planes, ownership, recovery
│   ├── components/*.md       one doc per component (no web UI in alpha)
│   ├── contracts/*.md        state, ipc, cli, streaming, actions, plugins
│   ├── 20-security.md  30-testing.md  40-platforms.md (Linux alpha. Windows and macOS are TODO)
│   ├── 50-repo-topology.md  60-roadmap.md (alpha phases 0-6 + backlog)
│   ├── 70-porting-notes.md   post-alpha manual porting (no importer)
│   ├── 80-integrations.md    lights.lan as a plugin (post-alpha)
│   ├── 90-verification.md    camera verification tools (deckstack-verify)
│   └── adr/NNN-*.md          decision records (referenced from PLAN.md)
├── schemas/                  JSON Schemas: state, ipc-message, stream-frame, plugin-manifest, binding
├── crates/  bins/            the workspace: ds-protocol, ds-device, ds-state, ds-ipc, ds-client,
│                             ds-daemon, ds-vdev, ds-plugin-sdk, ds-bench. Bins: ds, ds-focus, ds-mcp
├── plugins-py/               Python plugin SDK (PyPI: ds-plugin-py)
├── clients/ds-py/            stdlib-only Python client helper
├── clients/preview-web/      static preview webapp (DS-121): deck map + plugin action mappings over /preview
├── examples/                 micro-apps, agent configs, plugin demo
├── tests/                    fixtures/ (schema gate), conformance/, e2e/, golden/, hw/, …
├── tools/                    check.sh (local gate), verify/ (camera check)
├── packaging/                release.sh, systemd unit, udev rules, AppImage spec, linux install
└── .github/workflows/        CI + tag-triggered release
```

Separate publish targets: `deckstack-verify` (camera check, `tools/verify/`) and
`deckstack-plugin-py` (`plugins-py/`, PyPI) are developed in-tree during the alpha and
published from their own repositories and wheels at release time. `deckstack-lights`, the
lights.lan plugin, is a separate repository post-alpha.
## Glossary

- **daemon (`ds-daemon`)** is the single long-lived process that owns all runtime state
  and hardware input and output (R1).
- **control plane and frame plane** are the NDJSON state channel and the binary DSF1 frame
  stream. Frames never enter state or bump `rev`.
- **state** is the single JSON document addressed by paths like `/controls/keys/0` (R2).
- **binding** is an element of the `/bindings` array that maps a topic to steps (R3/R4).
- **virtual device (`ds-vdev`)** is a HID-report-level fake of the MasterHub for CI (R10).
- **sender** is an external program that emits events through the CLI, such as a window
  watcher or cron (R4).
- **plugin** is a separate OS process with a manifest that talks newline JSON to the daemon.
- **capability token** is a per-install token with the scopes `read`, `mutate` and `admin`.
  Plugins get smaller capability grants (R8).
- **camera check** is webcam verification of pixels on the live deck
  (`docs/90-verification.md`).
- Citations like `upstream/PROTOCOL.md:244` refer to `PROTOCOL.md:244` in the Tauri
  predecessor.

# deckstack — source brief

Working name for a ground-up rebuild of the MasterHub/HubDeck controller stack.
This file is the governing intent. Everything in this repo is derived from it.
Written 2026-11-22 with the owner. Do not silently relax a requirement; mark the
conflict as OPEN and propose a decision.

## The problem

Two existing implementations drive the same hardware:

- `~/repos/hubdeck` — Python, pywebview-ish web editor, YAML presets, in-process
  plugins, `python-in-yaml` actions, ~380 tests and reverse-engineered protocol
  notes under `docs/protocol/`. Runs on this machine as an AppImage.
- `~/repos/hubdeck-upstream` — Rust + Tauri, `mh-protocol`/`mh-device` crates,
  PROTOCOL.md verified on hardware, plugins as separate processes over WebSocket,
  local HTTP/WS API, CLI and MCP planned.

Both are walled gardens: the UI and the action vocabulary are opinionated and
in-process, so behaviour that was not anticipated cannot be built without
patching the app. The hard-won assets are the protocol knowledge, the device
layer and the operational experience; the application shape is what we replace.

## Goal

A **CLI-first base** for the deck where:

- a persistent daemon owns all runtime state and hardware I/O;
- every read and every write goes through a stateless CLI (and the same API is
  reachable over sockets/HTTP for UIs and agents);
- button visuals, bindings and behaviour are data that any program can set;
- arbitrary behaviour is composed from small programs that call the CLI, not
  from a fixed action menu baked into the host.

The unix-philosophy framing is the design test: small, composable, observable
parts with a clean contract, instead of one opinionated application.

## Hard requirements

R1. **Daemon + CLI split.** One long-lived process owns device state. The CLI is
    stateless: one invocation reads or mutates state and exits. No UI is required
    for any capability. A GUI (web or native) is only another client.

R2. **State is the product.** State is addressable by stable paths, queryable
    cheaply (single key, subtree, filtered), and machine-readable by default
    (JSON; NDJSON for streams). Queries must stay fast while frames stream to
    the deck. Design the latency budget and prove it with a benchmark harness.

R3. **Arbitrary button behaviour.** Every observable attribute is settable:
    per-key image/frame/colour/text, per-module state, action bindings, event
    subscriptions. The base ships a generic renderer and action resolver, not a
    closed catalogue of actions. Anything the hardware can display must be
    reachable without forking the core.

R4. **Composability.** Event sources (window watcher, cron, webhooks, git hooks,
    other services) are ordinary programs that call the CLI. The window watcher
    from the existing implementations must become an external/composable sender:
    on focus change it calls the CLI to set colours, labels, bindings, anything.
    The same mechanism must allow chaining: one command's output feeds another
    command's arguments.

R5. **Streaming is first class.** Pushing video/animation to buttons is a normal
    operation. The control path must not starve under frame load. Backpressure,
    frame dropping policy (latest-frame-wins), bounded queues and separate I/O
    threads are part of the design. Tests must be able to capture what the
    device reports; if the hardware has no readback, define the best proxy
    (frames sent + timing + device acknowledgements) and document the gap.

R6. **Plugins.** The plugin host is a thin, API-driven shell. Plugins may be
    Python (the existing Python plugin ecosystem should be reusable through a
    wrapper), but the contract is process/API based, not in-process Python.
    Each plugin declares actions and parameters (JSON Schema) and receives events;
    it returns effects the daemon applies. A misbehaving plugin must not corrupt
    state or take the daemon down. Plugin-specific quirks stay inside the plugin,
    never leak into the core.

R7. **Transports.** Local IPC for CLI/daemon (Unix socket; named pipe on Windows),
    plus optional WebSocket/HTTP for browser UIs and remote agents. One event
    model, several transports. UI state is not privileged or special.

R8. **Security.** Local-only by default. Per-install token for anything that can
    mutate. No `eval` of user configuration. Plugins run with an explicit,
    minimal capability grant. Treat malicious state/config/plugins as a real
    threat: the design must not become an RCE vector. Include a threat model and
    concrete mitigations (path handling, image/frame limits, resource budgets,
    command injection through shellouts, plugin sandboxing).

R9. **Cross-platform.** Linux is the primary target (Bazzite/Fedora now). Windows
    must remain a supported shape: named pipes and PowerShell as the glue; the
    hub exposes general building blocks and does not depend on POSIX specifics in
    its core. macOS should be reasonable. Document exactly which platform shims
    are needed and what is untested.

R10. **Agent-optimized.** Coding agents are the main developers and users.
     - Every component is self-contained and independently testable.
     - Docs per component: purpose, invariants, API surface, failure modes, test
       command, and a minimal working example.
     - The CLI is discoverable: `--help`, `schema`/`describe` commands that emit
       JSON Schema or equivalents, stable exit codes, actionable errors.
     - Tests drive the public contract (CLI/API) as a black box, in whatever
       language is convenient, with deterministic fixtures and a virtual device
       so CI needs no hardware.

R11. **Performance.** Define and measure budgets: control-path latency while
     streaming, query latency at p50/p99, frame rate per key/module, memory and
     CPU. The streaming path must degrade by dropping frames, not by blocking
     control.

R12. **Repo topology.** The project lives in its own top-level directory,
     `~/repos/deckstack`, distinct from the existing hub apps. Each component
     must be releasable as its own repo/app so parts can be swapped. The plan
     must choose a concrete topology (umbrella + per-component repos, manifest,
     submodules, or siblings) and justify it. Clean versioned contracts between
     components; no shared mutable internals.

## Facts and pointers

- Hardware: Cooler Master MasterHub base plus modules (keys, dial, faders,
  rollers, full-panel screen). Protocol details: `~/repos/hubdeck-upstream/PROTOCOL.md`
  and `~/repos/hubdeck/docs/protocol/`.
- Device safety: firmware commands that rewrite serial, save factory data, reset
  or flash must be unrepresentable in the protocol layer, not merely unused.
- Existing Python architecture and prior rewrite analysis:
  `~/repos/hubdeck/analysis/` (read it, then note where this brief supersedes it:
  that analysis chose "refactor, keep the Python app"; this project chooses a new
  CLI-first base).
- Existing Rust architecture: `~/repos/hubdeck-upstream/ARCHITECTURE.md`,
  `PROTOCOL.md`, `crates/`, `plugins/`.
- Existing window watching exists in both implementations; find it and describe
  its current limits in the discovery notes.
- Live config on this machine: `~/.config/hubdeck/config.yaml` plus included
  section files; the engine watches config files. huemux integration is HTTP
  (`plugins.huemux.url` -> `lights.lan`).
- Home rule: only Salon lights may ever be driven from the deck. This is a
  configuration/policy concern, not a core assumption, but the plan must show
  where such policy lives and how it is enforced.

## Decisions the plan must make

1. Component decomposition, names and ownership boundaries.
2. Language per component, with alternatives considered and rejected.
3. Daemon API: transport, framing, versioning, error model, auth.
4. State model: addressing, schema, transactions, subscriptions, persistence,
   what is durable vs ephemeral, crash recovery.
5. Streaming path: encode/push pipeline, frame identity, backpressure, test hooks.
6. Plugin host: process model, manifest, ABI/messages, lifecycle, capabilities,
   Python compatibility wrapper.
7. Action model: how a button press resolves to behaviour without hardcoding
   actions in the core.
8. CLI grammar and output contract (human vs `--json`, streams, errors).
9. Test architecture: virtual device, golden frames, conformance suite, load and
   latency benchmarks, hardware smoke tests.
10. Repo/release topology and how components version against each other.
11. Interop/migration: can the existing Python engine be driven or replaced
    incrementally? What is the first end-to-end slice?
12. Phased roadmap with an observable exit criterion per phase.

## Quality bar for the plan

- Every claim about existing behaviour cites a file path (and line where useful).
- Each major decision lists the alternatives considered and why they lost.
- Unknowns are marked **OPEN** with a concrete experiment that resolves them.
- A competent coder can implement phase 1 from the plan without asking questions.
- The plan is written for agents: short sections, explicit contracts, examples
  with real commands and real payloads.

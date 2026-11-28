# Opportunities review — additional work beyond DS-112..DS-115 and the post-alpha backlog

**Date:** 2026-11-27
**Reviewer:** independent review run on branch `vibe/project-review`
**Scope:** READ-ONLY investigation of this worktree. Every item below is grounded in files
read in this tree; citations are `file:line`. No daemon code, schemas, contracts, or tests
were modified. Constraints honoured: no hardware, no camera, no network — the salon deck is
reserved by another session and the webcam is disconnected, so "cheap" below means
*closable with vdev/local experiments only*.

## 1. Open questions O1–O8 / O11: which are cheap to close without hardware

Source: `docs/open-questions.md:19-29`.

| Q | Hardware needed? | Verdict | Evidence / experiment |
|---|---|---|---|
| O1 (Wayland focus backends, `open-questions.md:19`) | No deck | **Partially cheap now.** The event path is exercisable on vdev: `ds-focus --inject` → `app/focus/changed` → `/focus` mirror is already proven in `docs/porting/salon-key.md` §6 step 8 (`bins/ds-focus/src/args.rs:52,178`). Backend *selection/latency* (KWin D-Bus vs `wlr-foreign-toplevel` vs kdotool) needs a live Wayland session but **not** the deck — can run while the deck is reserved. |
| O2 (fader LED rate limit ~25/s, `:20`) | Yes (faders module soak) | **Not cheap.** Hardware-only soak; schedule as a salon-deck session (see `docs/tickets/salon-deck-ticketing.md`). Interim cap already shipped: `/policy/led_rate_per_s` default 30 (`schemas/state.json` policy `led_rate_per_s`, D39). |
| O3 (two same-kind screen modules, `:21`) | Yes (two keys modules) | **Not cheap.** Hardware-only. Affects `/streams` limits in `docs/contracts/streaming.md`. |
| O4 (hidraw+udev vs libusb hot-plug, `:22`) | Yes (real device) | **Not cheap.** Needs the physical deck. |
| O5 (Windows pipe throughput, `:23`) | Windows VM + deck | **Not cheap.** Post-alpha platform TODO (`docs/40-platforms.md` §2). |
| O6 (Landlock fs confinement, `:24`) | **No** | **Cheap — close now.** A one-call probe decides it. Run on this machine (kernel `7.3.5-ogc3.1.fc45`, same Fedora family as Bazzite): `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` via syscall 444 returns **ABI 11** (executed during this review, exit observed `Landlock ABI version on this kernel: 11`). ABI ≥ 1 covers fs; the daemon already feature-detects at runtime (`crates/ds-daemon/src/plugins/spawn.rs:178-181`, test at `:374`). Remaining work: a tiny probe script + a line in `docs/20-security.md` closing O6. |
| O7 (reader-thread vs polled reads under frame load, `:25`) | **No (vdev half)** | **Cheap on vdev.** `ds-vdev` has throttle/backpressure knobs (`docs/components/virtual-device.md:30-32`, `crates/ds-vdev/src/throttle.rs`, `bins/ds-vdevctl/src/main.rs:9` `--throttle-kbps`/`--drop-after`), so reader-thread vs polled behaviour under frame load can be measured without hardware. The real-hidraw half (2.7 ms vs 34.8 ms per write) needs the deck. |
| O8 (macOS, `:26`) | macOS runner + deck | **Not cheap.** Post-alpha (`docs/40-platforms.md` §3). |
| O11 (Landlock net rules ABI ≥ 4, `:29`) | **No** | **Cheap — close now.** Same probe as O6: this kernel reports **ABI 11 ≥ 4**, so `LANDLOCK_ACCESS_NET_*` rules are available; only the doc line in `docs/contracts/plugins.md` §2 / `docs/20-security.md` remains. |

Net: **O6 and O11 can be closed today with a local syscall probe (no hardware, no camera);
O1's event path and O7's vdev half can be advanced on ds-vdev; O2/O3/O4/O5/O8 genuinely need
hardware or another platform and belong in reserved salon-deck sessions.**

## 2. Gaps between docs/contracts and the implementation (found by reading code)

1. **CLI contract lists later-phase verbs as plain verbs.** `docs/contracts/cli.md:22-28`
   documents `ds emit`, `ds events`, `ds plugin …`, `ds token …`, and `ds daemon … restart`
   with no "refused in alpha" marker, but the binary refuses them:
   `bins/ds/src/cli.rs:117` (`LATER_PHASE = ["emit","events","plugin","token"]`), enforced at
   `cli.rs:602` and `cli.rs:715`, and `cli.rs:1012` asserts `daemon restart` → "unknown
   daemon subcommand". Only `docs/porting/salon-key.md` §3/§7 mentions this in passing.
   *Work:* add a one-line "recognized but exit 1 until phase 5/6" note to the cli.md table.
   Effort: ~0.5 h, docs-only. Relates to DS-113/DS-114 (which assume `plugin`/`emit`
   surfaces).
2. **Target vocabulary diverges between the CLI contract and the vdev tooling.** Contract
   S6: targets are `keys | key:<0-14> | dial`, "`panel`/`screen` are not targets"
   (`docs/contracts/streaming.md` §1, `docs/contracts/state.md` §1; enforced in
   `bins/ds/src/stream/mod.rs:45-58`, which refuses `panel`). But ds-vdev speaks `panel`:
   `crates/ds-vdev/src/target.rs:33` (`"panel" => Some(Target::Panel)`) with a test at
   `:86` asserting `Target::parse("keys") == None`, and `bins/ds-vdevctl/src/main.rs:13,27`
   plus `docs/components/virtual-device.md:29,81` document `frames … panel|key:N|dial`.
   *Work:* either document the vdev-internal spelling explicitly in
   `docs/components/virtual-device.md`, or align ds-vdevctl to the S6 vocabulary.
   Effort: 0.5–1 h. Matters for salon-deck session scripts that mix `ds stream` and
   `ds-vdevctl frames`.
3. **`/settings` is free-form in the schema.** `schemas/state.json:113` is
   `"settings": {"type": "object"}` with no properties, while `docs/contracts/state.md:40`
   documents concrete keys (sender backends, log level). *Work:* either close the schema
   object or mark `/settings` explicitly free-form in the contract. Effort: ~1 h.
4. **`/devices/<serial>` has `slot` but no position/orientation fields** —
   `schemas/state.json:211` defines only `slot`; the daemon decodes orientation into an
   event (`crates/ds-daemon/src/router/mod.rs:283`) but the state subtree carries no
   orientation/position. This is exactly DS-112's requirement 2
   (`docs/tickets/2026-11-25-module-position-orientation.md`); confirmed, not new.

## 3. Post-alpha backlog items worth pulling forward (`docs/60-roadmap.md:72-75`)

- **`deckstack-verify` split to its own repo** — low risk to *plan* now (docs-only RFC +
  subtree-split command per `docs/50-repo-topology.md`); salon-deck hardware sessions need
  camera verification (`docs/90-verification.md`) independent of daemon releases.
  Effort: 2–3 h planning, split itself mechanical.
- **DS-113 `/preview` read-only debug deck** (`docs/tickets/2026-11-25-action-metadata-virtual-deck.md`)
  — its stated motivation is the *salon-deck map*; a localhost-only, read-only, opt-in
  surface fits the no-UI scope (ADR-014) and directly serves salon-deck operators.
  Effort: feature-sized (one phase slice); pull forward as the next feature ticket.
- **Porting lanes from `docs/porting/inventory.md`**: row 13 (knob-delta "adjust" action +
  LED gamma settings path), row 15 (URL-fetch micro-app), row 16 (uinput hotkeys exec
  lane), row 20 (per-integration plugin tickets). Row 13 is the smallest and unblocks the
  salon knob behaviours. Effort: row 13 ~1 day; others ticket-sized.
- **Windows (§2) / macOS (§3) of `docs/40-platforms.md`** — *not* pull-forward-able cheaply
  (O5/O8 above); keep gated on "Linux alpha stable".

## 4. Other concrete work found during the review

5. **Salon-deck ticketing convention** (deliverable of this review:
   `docs/tickets/salon-deck-ticketing.md` + `tools/salon_ticket.py`) — makes hardware/session
   work trackable like DS-xxx tickets. Effort: ~1 h, docs/tooling only.
6. **Landlock probe as a tiny offline tool** (`tools/landlock_probe.py`, stdlib-only
   syscall 444 wrapper) + close O6/O11 in `docs/open-questions.md` with the measured ABI 11.
   Effort: ~0.5 h.
7. **vdev experiment scripts for O1 (inject path) and O7 (throttled-load read model)**,
   committed under `tests/` or `docs/breakdowns/` so results are repeatable while the deck
   is reserved. Effort: 2–4 h each.
8. **Unattended LED-soak harness for O2** (extends `tests/hw/smoke.py`,
   `docs/90-verification.md` §hardware smoke) to run inside a reserved salon-deck session
   and be tracked as a SALON ticket. Effort: 2–3 h.
9. **Session start/end capture procedure in `docs/runbook.md`**: `ds get /devices --json`,
   `ds-vdevctl frames/writes`, camera shot when available — so every salon-deck session
   leaves evidence a SALON ticket can cite. Effort: 1–2 h, docs-only.

## 5. Prioritized list

| # | Item | Why | Effort | Relates to |
|---|---|---|---|---|
| P1-1 | Close O6/O11 with a local Landlock probe (ABI 11 measured on this kernel) | Zero hardware; removes two phase-5 unknowns | 0.5 h | `open-questions.md:24,29`; `plugins/spawn.rs:374` |
| P1-2 | Salon-deck ticketing convention + helper (this review's other deliverable) | Hardware/session work is currently untracked | 1 h | DS-112..DS-115; `docs/porting/salon-key.md` |
| P1-3 | cli.md later-phase-verb note; vdev `panel` vs `keys` spelling; `/settings` schema | Contract/implementation honesty, cheap docs fixes | 1–2 h total | `cli.md:22-28` vs `cli.rs:117`; `target.rs:33` |
| P2-1 | vdev experiments: O1 inject/focus path, O7 throttled-load reads | Advance two open questions with no hardware | 2–4 h each | `open-questions.md:19,25` |
| P2-2 | DS-113 `/preview` salon-deck map | Named motivation; fits no-UI scope | 1 phase slice | `docs/tickets/2026-11-25-action-metadata-virtual-deck.md` |
| P3-1 | Hardware-session items as SALON tickets: O2 soak, O3 dual-screen, O4 backend, DS-112 orientation capture | Only closable with the deck; now trackable | per session | `open-questions.md:20-22`; DS-112 |
| P3-2 | `deckstack-verify` split RFC; runbook session-capture procedure | Unblocks independent verification + evidence | 2–3 h | `60-roadmap.md:72-75`; `runbook.md` |
| P4-1 | Porting lanes: inventory row 13 (adjust/gamma) first, then 15/16/20 | Small, ticket-sized old-app behaviours | ~1 day each | `docs/porting/inventory.md` |

## 6. Spec tickets filed from this review (2026-11-27, branch `vibe/spec-cheap-wins`)

- **DS-116** — close O6/O11 with a local Landlock ABI probe (ABI 11 measured) + doc updates: `docs/tickets/2026-11-27-landlock-abi-probe-close-o6-o11.md`
- **DS-117** — cli.md later-phase verb markers (`emit`/`events`/`plugin`/`token`/`daemon restart` refused, exit 1): `docs/tickets/2026-11-27-cli-contract-later-phase-verbs.md`
- **DS-118** — document ds-vdevctl `panel` as the vdev-only alias of contract-S6 `keys`: `docs/tickets/2026-11-27-vdev-target-vocabulary-panel-alias.md`
- **DS-119** — mark `/settings` explicitly free-form in `schemas/state.json` + `docs/contracts/state.md`: `docs/tickets/2026-11-27-settings-schema-free-form.md`

Out of scope for this cheap-wins batch (bigger than a spec): the `deckstack-verify` repo split (P3-2) and the DS-113 `/preview` salon-deck map (P2-2).

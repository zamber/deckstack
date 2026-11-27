# Salon-deck ticketing — a lightweight convention for tracking hardware/session work

**Date:** 2026-11-27. **Type:** process proposal (docs + tooling only; no daemon code,
schemas, contracts, or tests change with this file).
**Status:** proposed.

## Motivation

deckstack tracks code work as DS-xxx tickets (`docs/tickets/2026-11-25-*.md`,
`docs/tickets/2026-11-26-*.md`). But real work also happens **on the physical salon deck**:
module reseats and orientation captures (DS-112), LED soaks (O2), backend comparisons
(O4), camera verification sessions (`docs/90-verification.md`), salon-deck systemd unit
sessions (DS-115 was found during a `salon-deck` stop). Today that work is invisible to the
DS-xxx ledger — it lives in session notes at best. The salon deck is driven through
deckstack (the Salon key port, `docs/porting/salon-key.md`; the lights.lan policy,
`docs/80-integrations.md`), so its hardware/session work should be tracked the same way.

## Convention

### Where tickets live

```
docs/tickets/salon/SALON-###-<slug>.md
```

One file per ticket, numbered sequentially from `SALON-001`. The `salon/` subdirectory keeps
them visually separate from code tickets while staying inside the existing `docs/tickets/`
tree (same commit discipline as DS-xxx tickets: small, ordered, on a branch off `main`).

### Fields (front matter, plain markdown — no schema change)

Every SALON ticket has, in order:

| Field | Meaning |
|---|---|
| `# SALON-### — <title>` | One-line title. |
| `Status:` | `open` · `in-progress` · `blocked` (name the blocker, e.g. "deck reserved") · `done` |
| `Date:` / `Session:` | Date opened; the salon-deck session (systemd unit run, date, operator) the ticket belongs to. |
| `Owner:` | Who runs the session. |
| `Deck state:` | Physical setup at session start: modules present, `/devices/<serial>` slot/orientation (`ds get /devices --json`), base orientation. Captured per `docs/runbook.md` session procedure. |
| `Hardware involved:` | Modules (keys M4315 / dial M131E / M0 family), webcam (`/dev/video2` AUKEY, `docs/90-verification.md`), udev state. |
| `Links:` | Related DS-xxx tickets (e.g. `DS-112`) and open questions (e.g. `O2`, `docs/open-questions.md`). |
| `Goal:` / `Steps:` | What the session does, as runnable commands where possible. |
| `Verification evidence:` | Exact commands run + outcome + where the transcript/log lives (`ds-vdevctl frames|writes` sha256, `deckstack-verify check` exit code, daemon log excerpt). vdev-only, hardware, and camera evidence are labelled as such — camera evidence only when the webcam is actually connected. |
| `Outcome:` | What was learned; which DS-xxx ticket or open question it closes or advances. |

### How SALON tickets link to DS-xxx

- A SALON ticket never changes a DS-xxx ticket's text; it **references** it in `Links:` and
  records in `Outcome:` what the hardware session proved about it (e.g. "DS-112 req 3:
  re-handshake measured at 1.8 s ≤ 5 s budget").
- A DS-xxx ticket may reference SALON tickets in its Acceptance section when acceptance
  needs hardware (DS-112 already implies this; its hardware step extends
  `tests/hw/smoke.py`).
- Closure rule: a SALON ticket moves to `done` only with committed evidence; a DS-xxx
  ticket closes only when all its linked SALON tickets are `done`.

### Helper script

`tools/salon_ticket.py` (stdlib-only, offline, `--help`) creates, lists, and shows SALON
tickets from the template:

```sh
python3 tools/salon_ticket.py new --title "DS-112 orientation capture" --links DS-112 --dry-run
python3 tools/salon_ticket.py new --title "DS-112 orientation capture" --links DS-112   # writes docs/tickets/salon/SALON-001-....md
python3 tools/salon_ticket.py list
python3 tools/salon_ticket.py show SALON-001
python3 tools/salon_ticket.py --root /tmp/some-tree list   # operate on another checkout (testing)
```

`new` numbers the next free `SALON-###` by scanning `docs/tickets/salon/`; `--dry-run`
prints the file it would write without touching the tree. No network, no hardware.

## Worked example

`docs/tickets/salon/SALON-001.md` — the DS-112 hardware orientation capture session
(reserved-deck session; blocked until the deck is free).

## Rules

- SALON tickets are docs-only: they never edit daemon code, schemas, contracts, or tests.
  If a session reveals a code bug, file/extend a DS-xxx ticket and link it.
- No hardware claims without evidence: vdev evidence proves protocol behaviour; only
  hardware + camera evidence (`docs/90-verification.md`) proves pixels.
- The deck has one owner (`docs/70-porting-notes.md` §4); a SALON ticket in `in-progress`
  means the deck is reserved for that session.

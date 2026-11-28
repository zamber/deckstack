#!/usr/bin/env python3
"""Create, list, and show salon-deck SALON-### tickets.

Salon-deck tickets track hardware/session work on the physical deck driven
through deckstack. Convention: docs/tickets/salon-deck-ticketing.md.

Usage:
    python3 tools/salon_ticket.py new --title "DS-112 orientation capture" \
        --links DS-112 [--session S] [--owner O] [--dry-run]
    python3 tools/salon_ticket.py list
    python3 tools/salon_ticket.py show SALON-001
    python3 tools/salon_ticket.py --root /path/to/tree list   # another checkout

Tickets live in <root>/docs/tickets/salon/SALON-###-<slug>.md. The script is
stdlib-only and fully offline: it never touches hardware, camera, or network.
`new --dry-run` prints the file it would write and exits 0 without writing.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

TICKET_DIR = Path("docs/tickets/salon")
ID_RE = re.compile(r"^SALON-(\d{3})")

TEMPLATE = """# {id} — {title}

Status: open
Date: {date}
Session: {session}
Owner: {owner}
Deck state: captured at session start (`ds get /devices --json`: serial, slot, connection, orientation)
Hardware involved: {hardware}
Links: {links}

## Goal

{goal}

## Steps

1. (fill in)

## Verification evidence

- (fill in: commands run + outcome, labelled vdev / hardware / camera)

## Outcome

- (fill in: DS-xxx tickets / open questions closed or advanced)
"""


def find_root(explicit: str | None) -> Path:
    if explicit:
        return Path(explicit).resolve()
    # Walk up from this script to the tree root (tools/ is one level down).
    return Path(__file__).resolve().parent.parent


def ticket_dir(root: Path) -> Path:
    return root / TICKET_DIR


def existing_ids(root: Path) -> list[int]:
    d = ticket_dir(root)
    if not d.is_dir():
        return []
    ids = []
    for p in d.iterdir():
        m = ID_RE.match(p.name)
        if m and p.is_file():
            ids.append(int(m.group(1)))
    return sorted(ids)


def slugify(title: str) -> str:
    s = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")
    return s[:60] or "ticket"


def cmd_new(args: argparse.Namespace) -> int:
    root = find_root(args.root)
    ids = existing_ids(root)
    next_id = (ids[-1] + 1) if ids else 1
    tid = f"SALON-{next_id:03d}"
    fname = f"{tid}-{slugify(args.title)}.md"
    path = ticket_dir(root) / fname
    from datetime import date

    text = TEMPLATE.format(
        id=tid,
        title=args.title,
        date=date.today().isoformat(),
        session=args.session or "(fill in: salon-deck session)",
        owner=args.owner or "(fill in)",
        hardware=args.hardware or "(fill in: modules, webcam, udev state)",
        links=args.links or "(none)",
        goal=args.goal or "(fill in)",
    )
    if args.dry_run:
        print(f"dry-run: would create {path}")
        print("---")
        print(text, end="")
        return 0
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        print(f"error: {path} already exists", file=sys.stderr)
        return 1
    path.write_text(text, encoding="utf-8")
    print(f"created {path}")
    return 0


def cmd_list(args: argparse.Namespace) -> int:
    root = find_root(args.root)
    d = ticket_dir(root)
    if not d.is_dir():
        print(f"no salon tickets under {d}")
        return 0
    rows = []
    for p in sorted(d.iterdir()):
        m = ID_RE.match(p.name)
        if not m or not p.is_file():
            continue
        title = ""
        status = ""
        for line in p.read_text(encoding="utf-8").splitlines():
            if line.startswith("# ") and not title:
                title = line[2:].strip()
            if line.startswith("Status:") and not status:
                status = line.split(":", 1)[1].strip()
        rows.append((m.group(1), status, title, p.name))
    if not rows:
        print(f"no salon tickets under {d}")
        return 0
    for num, status, title, name in rows:
        print(f"SALON-{num}  [{status or '?'}]  {title}  ({name})")
    return 0


def cmd_show(args: argparse.Namespace) -> int:
    root = find_root(args.root)
    d = ticket_dir(root)
    m = ID_RE.match(args.id.upper())
    num = m.group(1) if m else None
    for p in sorted(d.glob("SALON-*.md")):
        pm = ID_RE.match(p.name)
        if pm and pm.group(1) == num:
            print(p.read_text(encoding="utf-8"), end="")
            return 0
    print(f"error: no salon ticket {args.id} under {d}", file=sys.stderr)
    return 1


def main() -> int:
    ap = argparse.ArgumentParser(
        prog="salon_ticket.py",
        description="Create, list, and show salon-deck SALON-### tickets "
        "(convention: docs/tickets/salon-deck-ticketing.md). Offline, stdlib-only.",
    )
    ap.add_argument("--root", help="tree root to operate on (default: this repo)")
    sub = ap.add_subparsers(dest="cmd", required=True)

    p_new = sub.add_parser("new", help="create a new SALON ticket from the template")
    p_new.add_argument("--title", required=True, help="one-line ticket title")
    p_new.add_argument("--links", default="", help="linked DS-xxx tickets / open questions, e.g. 'DS-112, O2'")
    p_new.add_argument("--session", default="", help="salon-deck session description")
    p_new.add_argument("--owner", default="", help="session owner")
    p_new.add_argument("--hardware", default="", help="hardware involved")
    p_new.add_argument("--goal", default="", help="goal paragraph")
    p_new.add_argument("--dry-run", action="store_true", help="print the file without writing it")
    p_new.set_defaults(func=cmd_new)

    p_list = sub.add_parser("list", help="list salon tickets")
    p_list.set_defaults(func=cmd_list)

    p_show = sub.add_parser("show", help="print one salon ticket")
    p_show.add_argument("id", help="ticket id, e.g. SALON-001")
    p_show.set_defaults(func=cmd_show)

    args = ap.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())

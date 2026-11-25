#!/usr/bin/env python3
"""Count source lines in tracked files, grouped by area and language.

Usage:
    python3 tools/sloc.py                 # this repository
    python3 tools/sloc.py --root ../hubdeck
    python3 tools/sloc.py --json

Only files tracked by git are counted, so build output, virtualenvs and
generated files never inflate a number. Lines are physical lines; blank
lines and comments are included.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

LANGUAGES = {
    ".rs": "rust",
    ".py": "python",
    ".md": "docs",
    ".html": "web",
    ".css": "web",
    ".js": "web",
    ".sh": "shell",
    ".toml": "config",
    ".json": "config",
    ".yml": "config",
    ".yaml": "config",
}

ORDER = ["rust", "python", "docs", "web", "shell", "config"]


def tracked_files(root: Path) -> list[str]:
    out = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"],
        check=True,
        capture_output=True,
    ).stdout
    return [name for name in out.decode().split("\0") if name]


def area_of(path: str) -> str:
    parts = path.split("/")
    if len(parts) > 1 and parts[0] in {"crates", "bins", "clients", "plugins-py", "tests"}:
        return parts[0] + "/" + parts[1] if parts[0] in {"crates", "bins"} else parts[0]
    if len(parts) > 1 and parts[0] in {"docs", "site", "schemas", "examples", "skills"}:
        return parts[0]
    return "root"


def count(root: Path) -> dict[str, dict[str, int]]:
    totals: dict[str, dict[str, int]] = defaultdict(lambda: defaultdict(int))
    for name in tracked_files(root):
        lang = LANGUAGES.get(Path(name).suffix.lower())
        if lang is None:
            continue
        path = root / name
        try:
            if b"\0" in path.read_bytes()[:4096]:
                continue
            with path.open("r", encoding="utf-8", errors="replace") as handle:
                lines = sum(1 for _ in handle)
        except OSError:
            continue
        totals[area_of(name)][lang] += lines
        totals[area_of(name)]["total"] += lines
    return totals


def render(totals: dict[str, dict[str, int]]) -> str:
    areas = sorted(totals, key=lambda a: (a != "root", a))
    width = max([len("area")] + [len(a) for a in areas])
    header = "area".ljust(width) + "".join(lang.rjust(10) for lang in ORDER) + "total".rjust(10)
    lines = [header, "-" * len(header)]
    for area in areas:
        row = area.ljust(width)
        for lang in ORDER:
            value = totals[area].get(lang, 0)
            row += (str(value) if value else ".").rjust(10)
        row += str(totals[area].get("total", 0)).rjust(10)
        lines.append(row)
    grand = {lang: sum(t.get(lang, 0) for t in totals.values()) for lang in ORDER}
    lines.append("-" * len(header))
    row = "total".ljust(width)
    for lang in ORDER:
        row += (str(grand[lang]) if grand[lang] else ".").rjust(10)
    row += str(sum(grand.values())).rjust(10)
    lines.append(row)
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="repository root (default: .)")
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    args = parser.parse_args()
    root = Path(args.root).resolve()
    totals = count(root)
    if args.json:
        print(json.dumps(totals, indent=2, sort_keys=True))
    else:
        print(render(totals))
    return 0


if __name__ == "__main__":
    sys.exit(main())

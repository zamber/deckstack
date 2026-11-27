#!/usr/bin/env python3
"""deckstack doc-marker conformance suite — tests/conformance/docs_markers/.

Shared home for the doc-marker assertions of the DS-116..119 doc tickets (the specs of
DS-117/DS-118/DS-119 each say "alongside the cli.md marker check"; this directory is the
one shared location so all DS-116..119 marker rules live together instead of being
scattered across the cli/plugins suites). Each rule pins wording in docs/ that must be
absent (so a closed marker — e.g. an open question — cannot silently re-open) or
present (so a contract cannot silently drift). docs/tickets/ is excluded from the
absence scans: the ticket specs themselves quote the markers they ask to remove.

Rules:

  DS-116  no `OPEN O6` / `OPEN O11` marker anywhere under docs/ (outside docs/tickets/);
          O6/O11 closed by the measured Landlock ABI 11 (tools/landlock_probe.py), and
          docs/open-questions.md must carry the `RESOLVED (DS-116` closure.
          (DS-117/118/119 add their rules here when they land.)

This module is registered into tests/conformance/run.py (docs_markers_suite()), so the
normal `python tests/conformance/run.py` runs these cases; use `-k docs_markers` to run
only them. Standalone-runnable: `python3 tests/conformance/docs_markers/run.py`.

Exit code 0 = all marker rules hold, nonzero = at least one violated.
"""

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
DOCS = ROOT / "docs"

# (ticket, forbidden substrings, required substrings) — forbidden scan covers every
# docs/**/*.md except docs/tickets/; required substrings are checked per named file.
FORBIDDEN_EVERYWHERE = {
    "DS-116": ["OPEN O6", "OPEN O11"],
}
REQUIRED_IN = {
    "DS-116": {
        "docs/open-questions.md": ["RESOLVED (DS-116", "ABI 11"],
    },
}


def _doc_files():
    return sorted(p for p in DOCS.rglob("*.md") if "tickets" not in p.relative_to(DOCS).parts)


class DocsMarkers(unittest.TestCase):
    def test_docs_markers_ds_116_o6_o11_closed(self):
        """DS-116: no `OPEN O6`/`OPEN O11` marker survives under docs/ (tickets dir
        excluded — the specs quote the markers), and docs/open-questions.md carries the
        RESOLVED (DS-116) closure of both rows."""
        problems = []
        for f in _doc_files():
            text = f.read_text(encoding="utf-8")
            for ticket, needles in FORBIDDEN_EVERYWHERE.items():
                for needle in needles:
                    if needle in text:
                        problems.append(
                            "%s: %s still contains %r" % (ticket, f.relative_to(ROOT), needle)
                        )
        for ticket, per_file in REQUIRED_IN.items():
            for rel, needles in per_file.items():
                text = (ROOT / rel).read_text(encoding="utf-8")
                for needle in needles:
                    if needle not in text:
                        problems.append("%s: %s missing required %r" % (ticket, rel, needle))
        if problems:
            self.fail("doc-marker violations:\n  " + "\n  ".join(problems))


def docs_markers_suite():
    """The suite object registered by tests/conformance/run.py."""
    loader = unittest.defaultTestLoader
    suite = unittest.TestSuite()
    suite.addTests(loader.loadTestsFromTestCase(DocsMarkers))
    return suite


def main():
    runner = unittest.TextTestRunner(verbosity=1)
    result = runner.run(docs_markers_suite())
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())

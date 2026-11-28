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

  DS-117  docs/contracts/cli.md verb-table rows for the later-phase verbs (emit,
          events, plugin, token — LATER_PHASE_VERBS imported from the cli suite) and
          for `daemon … restart` carry the "recognized but refused in alpha, exit 1"
          marker with the binary's refusal message, and the summary note under the
          table cites exit 1 + "not implemented in phase 1" (tripwire: when a verb's
          ticket lands, this rule fails until the marker moves to the real grammar).
          (DS-116/118/119 add their rules in this same file when their branches land.)

This module is registered into tests/conformance/run.py (docs_markers_suite()), so the
normal `python tests/conformance/run.py` runs these cases; use `-k docs_markers` to run
only them. Standalone-runnable: `python3 tests/conformance/docs_markers/run.py`.

Exit code 0 = all marker rules hold, nonzero = at least one violated.
"""

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
DOCS = ROOT / "docs"

# The later-phase verb list has one source of truth: the CLI suite's LATER_PHASE_VERBS
# (tests/conformance/cli/run.py, mirroring bins/ds/src/cli.rs LATER_PHASE).
_CONF_DIR = ROOT / "tests" / "conformance"
if str(_CONF_DIR) not in sys.path:
    sys.path.insert(0, str(_CONF_DIR))
from cli.run import LATER_PHASE_VERBS  # noqa: E402

# DS-117: the canonical refusal marker that must appear on every refused cli.md row.
CLI_REFUSAL_MARKER = "recognized but refused in alpha, exit 1"


class DocsMarkers(unittest.TestCase):
    def test_docs_markers_ds_117_cli_later_phase_verbs_refused(self):
        """DS-117: docs/contracts/cli.md marks the recognized-but-refused verbs — every
        LATER_PHASE verb row (emit/events/plugin/token) and the daemon row (for the
        refused `restart` subcommand) carry the "recognized but refused in alpha,
        exit 1" marker with the binary's refusal message, and the summary note under
        the verb table cites exit 1 + "not implemented in phase 1". Tripwire: when a
        verb's ticket lands, this fails until the marker moves to the real grammar."""
        text = (DOCS / "contracts" / "cli.md").read_text(encoding="utf-8")
        problems = []

        def row_of(verb):
            prefix = "| `%s` |" % verb
            for line in text.splitlines():
                if line.startswith(prefix):
                    return line
            return None

        for v in LATER_PHASE_VERBS:
            row = row_of(v)
            if row is None:
                problems.append("DS-117: cli.md verb table has no row for %r" % v)
                continue
            if CLI_REFUSAL_MARKER not in row:
                problems.append("DS-117: cli.md row for %r lost the %r marker"
                                % (v, CLI_REFUSAL_MARKER))
            if "not implemented in phase 1" not in row:
                problems.append("DS-117: cli.md row for %r must cite the refusal message"
                                % v)
        row = row_of("daemon")
        if row is None:
            problems.append("DS-117: cli.md verb table has no daemon row")
        else:
            if CLI_REFUSAL_MARKER not in row or "restart" not in row:
                problems.append("DS-117: cli.md daemon row must mark `restart` with the "
                                "%r marker" % CLI_REFUSAL_MARKER)
            if "unknown daemon subcommand" not in row:
                problems.append("DS-117: cli.md daemon row must cite the refusal message")
        # summary note under the table (may wrap across lines — compare per paragraph)
        paragraphs = [" ".join(p.split()) for p in text.split("\n\n")]
        summary = [p for p in paragraphs if "later-phase" in p.lower()]
        if not summary:
            problems.append("DS-117: cli.md must carry a summary note under the verb "
                            "table for the later-phase verbs")
        else:
            joined = " ".join(summary)
            for needle in ("exit 1", "not implemented in phase 1", "docs_markers"):
                if needle not in joined:
                    problems.append("DS-117: cli.md summary note missing %r" % needle)
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

#!/usr/bin/env python3
"""deckstack doc-marker conformance suite — tests/conformance/docs_markers/.

Shared home for the doc-marker assertions of the DS-116..119 doc tickets (the specs of
DS-116/117/118/119 each say "alongside the cli.md marker check"; this directory is the
one shared location so all DS-116..119 marker rules live together instead of being
scattered across the cli/plugins suites). Each rule pins wording in docs/ that must be
present (so a contract cannot silently drift from the binary or the tooling).

Rules:

  DS-118  docs/components/virtual-device.md, docs/contracts/streaming.md, and
          docs/contracts/state.md each carry the note that `panel` in
          `ds-vdevctl frames panel` is the ds-vdevctl-local, vdev-only alias of the
          contract-S6 target `keys` (`ds-vdevctl frames panel` ≡ `ds stream push
          keys`), while the S6 sentence "`panel`/`screen` are not targets" stays in
          streaming.md and "no `panel`/`screen` synonyms" stays in state.md (the alias
          must not weaken the contract vocabulary). (DS-116/117/119 add their rules in
          this same file when their branches land.)

This module is registered into tests/conformance/run.py (docs_markers_suite()), so the
normal `python tests/conformance/run.py` runs these cases; use `-k docs_markers` to run
only them. Standalone-runnable: `python3 tests/conformance/docs_markers/run.py`.

Exit code 0 = all marker rules hold, nonzero = at least one violated.
"""

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
DOCS = ROOT / "docs"

# DS-118: the canonical alias marker that must appear in all three docs.
PANEL_ALIAS_MARKER = "vdev-only alias"

# DS-118: the S6 sentences the alias note must NOT replace — the contract vocabulary
# for the `ds` CLI and all state paths stays `keys | key:<0-14> | dial` only.
S6_VOCAB_GUARDS = {
    "docs/contracts/streaming.md": "`panel`/`screen` are not targets",
    "docs/contracts/state.md": "no\n`panel`/`screen` synonyms",
}


class DocsMarkers(unittest.TestCase):
    def test_docs_markers_ds_118_vdev_panel_alias_of_s6_keys(self):
        """DS-118: the vdev-only `panel` alias of the S6 `keys` target is documented in
        all three places — docs/components/virtual-device.md (the ds-vdevctl surface),
        docs/contracts/streaming.md §1 (the S6 target vocabulary), and
        docs/contracts/state.md (the state-path vocabulary) — each naming the
        `panel` ≡ `keys` mapping, while the original S6 "not a target"/"no synonyms"
        sentences remain so the alias cannot silently widen the contract."""
        problems = []
        alias_files = [
            "docs/components/virtual-device.md",
            "docs/contracts/streaming.md",
            "docs/contracts/state.md",
        ]
        for rel in alias_files:
            text = (ROOT / rel).read_text(encoding="utf-8")
            if PANEL_ALIAS_MARKER not in text:
                problems.append("DS-118: %s lost the %r marker" % (rel, PANEL_ALIAS_MARKER))
            if "`panel`" not in text or "`keys`" not in text:
                problems.append("DS-118: %s must name the `panel` ≡ `keys` mapping"
                                % rel)
        for rel, guard in S6_VOCAB_GUARDS.items():
            text = (ROOT / rel).read_text(encoding="utf-8")
            if guard not in text:
                problems.append("DS-118: %s lost the S6 guard %r — the alias note must "
                                "not weaken the contract vocabulary" % (rel, guard))
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

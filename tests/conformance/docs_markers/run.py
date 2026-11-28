"""Shared doc-marker conformance suite (DS-116..119).

One home for the doc-wording assertions of the documentation tickets, so
`tests/conformance/run.py -k docs_markers` machine-checks that the docs say what the
tickets require. Cases from the DS-116/117/118/119 implementation branches merged here.
"""
import json
import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
DOCS = ROOT / "docs"

conf_dir = str(ROOT / "tests" / "conformance")
if conf_dir not in sys.path:
    sys.path.insert(0, conf_dir)
from cli.run import LATER_PHASE_VERBS  # noqa: E402


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


class DocsMarkersDS116(unittest.TestCase):
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


class DocsMarkersDS117(unittest.TestCase):
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


class DocsMarkersDS118(unittest.TestCase):
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


ROOT = Path(__file__).resolve().parents[3]
SCHEMA = ROOT / "schemas" / "state.json"
CONTRACT = ROOT / "docs" / "contracts" / "state.md"

VERBOSE = "--verbose" in sys.argv


def _settings_node():
    schema = json.loads(SCHEMA.read_text(encoding="utf-8"))
    return schema["properties"]["settings"]


def _settings_contract_line():
    for line in CONTRACT.read_text(encoding="utf-8").splitlines():
        if line.startswith("/settings/"):
            return line
    raise AssertionError("docs/contracts/state.md has no /settings/... line")


class DocsMarkersDS119(unittest.TestCase):
    def test_docs_markers_ds_119_schema_description(self):
        node = _settings_node()
        self.assertEqual(node.get("type"), "object")
        self.assertNotIn("properties", node)
        self.assertNotIn("additionalProperties", node)
        desc = node.get("description", "")
        self.assertIn("free-form", desc)
        self.assertIn("docs/contracts/state.md", desc)

    def test_docs_markers_ds_119_contract_wording(self):
        line = _settings_contract_line()
        self.assertIn("free-form", line)
        self.assertIn("documented subset", line)
        self.assertRegex(line, re.compile(r"arbitrary additional keys.*(permitted|ignored)", re.I))
        self.assertIn("/policy/exec", line)

    def test_docs_markers_ds_119_schema_contract_agree(self):
        desc = _settings_node().get("description", "")
        line = _settings_contract_line()
        self.assertIn("free-form", desc)
        self.assertIn("free-form", line)
        self.assertIn("documented subset", line)
        self.assertIn("not an exhaustive schema", desc)


def docs_markers_suite():
    suite = unittest.TestSuite()
    loader = unittest.defaultTestLoader
    for cls in (DocsMarkersDS116, DocsMarkersDS117, DocsMarkersDS118, DocsMarkersDS119):
        suite.addTests(loader.loadTestsFromTestCase(cls))
    return suite


if __name__ == "__main__":
    unittest.main()

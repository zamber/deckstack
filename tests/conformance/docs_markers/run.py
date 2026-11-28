#!/usr/bin/env python3
"""DS-119 docs-marker conformance suite: machine-checks that the /settings
free-form intent is declared in both normative places and that they agree
(ticket docs/tickets/2026-11-27-settings-schema-free-form.md).

Pure local doc/schema assertions — no daemon, no vdev, no hardware, no network.
Registered into tests/conformance/run.py, so `python3 tests/conformance/run.py
-k docs_markers` runs only these cases.

Cases (test ids contain "docs_markers_ds_119"):

  test_docs_markers_ds_119_schema_description   schemas/state.json keeps
               "settings": {"type": "object"} exactly (no properties, no
               additionalProperties) and carries a description declaring
               /settings intentionally free-form
  test_docs_markers_ds_119_contract_wording     docs/contracts/state.md states the
               free-form rule on the /settings/... line: documented keys are
               the documented subset, arbitrary additional keys permitted and
               ignored by the daemon
  test_docs_markers_ds_119_schema_contract_agree  schema description and contract
               wording agree (both say free-form / documented subset)

Usage: python3 tests/conformance/docs_markers/run.py [--verbose]
"""

import json
import re
import sys
import unittest
from pathlib import Path

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
    return unittest.defaultTestLoader.loadTestsFromTestCase(DocsMarkersDS119)


if __name__ == "__main__":
    runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=2 if VERBOSE else 1)
    result = runner.run(docs_markers_suite())
    sys.exit(0 if result.wasSuccessful() else 1)

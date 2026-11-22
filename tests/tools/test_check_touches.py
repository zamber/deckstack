import sys
import unittest
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[2] / "tools"
sys.path.insert(0, str(TOOLS))
import tickets


class CheckTouchesTest(unittest.TestCase):
    def test_own_coordination_files_are_ignored(self):
        files = [
            "prs/DS-004.md",
            "prs/DS-004.review.md",
            "tickets/DS-004.md",
            "tickets/BOARD.md",
            "tickets/GRAPH.md",
            "tickets/index.json",
        ]
        self.assertEqual(tickets.outside_touches(files, ["new:Cargo.toml"], "DS-004"), [])

    def test_feature_files_must_be_in_touches(self):
        files = ["Cargo.toml", "Cargo.lock", "crates/ds-state/src/lib.rs", "README.md"]
        touches = ["new:Cargo.toml", "new:crates/"]
        self.assertEqual(tickets.outside_touches(files, touches, "DS-004"), ["README.md"])

    def test_generated_cargo_lock_is_ignored(self):
        self.assertEqual(tickets.outside_touches(["Cargo.lock"], [], "DS-004"), [])

    def test_another_tickets_files_are_not_coordination(self):
        files = ["tickets/DS-005.md", "prs/DS-005.md", "prs/README.md"]
        self.assertEqual(
            tickets.outside_touches(files, ["new:Cargo.toml"], "DS-004"),
            files,
        )


if __name__ == "__main__":
    unittest.main()

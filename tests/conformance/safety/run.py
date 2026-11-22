#!/usr/bin/env python3
"""deckstack SAFETY conformance suite (ticket DS-029, tests/conformance/safety/).

Machine-checked proof of the mh-protocol safety invariants (AGENTS.md; docs/20-security.md
T8) on the REAL binaries, black-box:

    010 forbidden HID commands are unrepresentable at the ds-protocol level — the public
        API's own safety tests (tests/protocol/safety.rs, run through cargo) must pass:
        no builder emits a forbidden report, backlight-0 is refused on every module,
        reads need the software-mode token.
    020 an attempted forbidden write on the device socket (the exact socket the daemon
        writes through) is refused by vdev with an ERROR frame — never an ACK, so ZERO
        bytes are accepted — and vdev's write log (the oracle, `ds-vdevctl writes`)
        records every attempt as `refused`. The IPC error model maps hardware errors
        incl. Forbidden to E_DEVICE / exit 7 (ipc.md §4).
    025 host-side defense in depth: the raw-path guard refuses forbidden bytes before any
        byte leaves the host (ds-device unit tests), vdev refuses them device-side
        (ds-vdev unit tests), and ds-ipc maps EDevice -> "E_DEVICE" -> exit 7.
    030 the daemon's own traffic never contains a forbidden (report id, command) pair:
        zero `refused` entries in vdev's write log AND a payload-level scan of every frame
        the daemon sent (recorded by a fake device socket).
    040 no read/query and no other write on keys/dial before the software-mode handshake:
        on the recorded daemon traffic the FIRST frame per screen module is the handshake
        feature-set (`03 3A 01` keys / `03 49 01` dial) and no FEATURE_GET/WRITE precedes
        it; vdev's write log shows the handshake as the first write per screen module and
        `ds-vdevctl reports` shows software_mode=true; the handshake state machine's own
        unit tests (reads refused before the handshake) pass.
    060 backlight 0 is never sent to the dial (or keys): no backlight feature-set with
        percent 0 ever appears on the daemon's wire traffic (even after `set ... brightness
        0` requests), vdev reports backlight in 1..=100 for every module, and the builder
        refuses 0 (010).
    070 no raw escape hatch: the CLI verb set and the IPC op set are enumerated (help
        text, source constants, the daemon's own `ds schema ipc`, and live probes of
        candidate raw ops) and must contain nothing that takes raw HID bytes; state
        visuals have no raw/bytes kind. Any appearance fails loudly with a clear message.
    080 DS_HARDWARE=1 pass: repeats the read-only safety checks that can run on a
        connected deck (enumerate, open, one feature read on M0 modules only, handshake
        + init, close — no writes beyond the handshake) via the ds-device hardware smoke
        target; skips cleanly without DS_HARDWARE=1 or without a deck.

Usage:
    python3 tests/conformance/safety/run.py [--verbose] [-k SUBSTR]
    python3 tests/conformance/run.py            # runs this suite too (registered there)
    python3 tests/conformance/run.py -k safety  # only the safety cases

Exit code 0 = all green. Every wait carries a timeout; the daemon and vdev are always
killed (SIGTERM then SIGKILL) and the temp dir removed, so the run can never hang or
leak processes. A red safety case is a merge blocker by design (ticket DS-029: "None
acceptable — a red safety test blocks merge").
"""

import json
import os
import re
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from harness import (  # noqa: E402
    BACKLIGHT_CMDS,
    EXPECTED_DS_VERBS,
    EXPECTED_IPC_OPS,
    EXPECTED_SET_OPS,
    FORBIDDEN,
    FORBIDDEN_SET,
    HANDSHAKE_DIAL,
    HANDSHAKE_KEYS,
    KIND_ACK,
    KIND_ERROR,
    KIND_FEATURE_GET,
    KIND_FEATURE_SET,
    KIND_WRITE,
    PID_DIAL,
    PID_KEYS,
    RAWISH,
    ROOT,
    VDEVCTL_SAFE,
    Stack,
    VdevClient,
    cargo_available,
    ctl_request,
    ensure_binaries,
    parse_reports,
    parse_writes,
    run_cargo_test,
)

VERBOSE = "--verbose" in sys.argv

# Candidate "raw" CLI verbs / IPC ops probed live in test_safety_070. None of these may
# exist; if one ever answers with rc 0 / ok:true the suite fails loudly.
RAW_CANDIDATES = [
    "raw", "hid", "hidraw", "hid-write", "write", "devwrite", "send", "report",
    "reports", "bytes", "hex", "inject", "feature", "feature-set", "firmware",
    "serial", "factory", "reset", "flash", "dev", "device", "debug", "exec", "cmd",
    "command", "push", "raw-write", "control",
]

# The safety test names that must appear (and pass) in the ds-protocol public-API run.
PROTOCOL_SAFETY_TESTS = [
    "raw_path_rejects_exactly_the_pinned_forbidden_set",
    "no_builder_can_emit_a_forbidden_report",
    "backlight_zero_is_refused_on_every_module",
    "led_writes_never_persist",
    "reads_require_the_software_mode_token",
    "wire_version_is_pinned_to_one",
]


def _cargo_skip(testcase, what):
    if not cargo_available():
        testcase.skipTest("cargo not on PATH — %s not run (binaries were prebuilt); "
                          "the wire-level cases below still prove the invariant" % what)


def _assert_cargo_ok(testcase, proc, what, required_names=()):
    out = proc.stdout or ""
    testcase.assertEqual(
        proc.returncode, 0,
        "%s failed (rc=%d):\n%s" % (what, proc.returncode, out[-4000:]),
    )
    testcase.assertIn("test result: ok", out, "%s: no ok result line:\n%s" % (what, out[-2000:]))
    for name in required_names:
        testcase.assertRegex(
            out, re.escape(name) + r" \.\.\. ok",
            "%s: safety test %r did not run/pass:\n%s" % (what, name, out[-2000:]),
        )
    testcase.assertIn("0 failed", out, "%s: failures present:\n%s" % (what, out[-2000:]))


def _drive_device_traffic(ds):
    """Commit a spread of /controls changes so the device task writes image chunks and
    LED feature reports (bounded, best-effort — the wire assertions are what matter)."""
    sets = [
        ("/controls/keys/0/image", '{"kind":"color","value":"#ff0000"}'),
        ("/controls/keys/3/image", '{"kind":"color","value":"#00ff00"}'),
        ("/controls/dial/image", '{"kind":"color","value":"#123456"}'),
        ("/controls/faders/0/led", '{"mode":"static","color":"#00ff00","brightness":40}'),
        ("/controls/knobs/1/led", '{"mode":"static","color":"#010203","brightness":40}'),
        ("/controls/rollers/0/led", '{"mode":"static","color":"#ff00ff","brightness":40}'),
    ]
    for path, value in sets:
        p = ds("set", path, "--json", value)
        if p.returncode != 0:
            raise AssertionError("set %s failed rc=%d stderr=%s" % (path, p.returncode, p.stderr))


def _backlight_frames(frames):
    """All FEATURE_SET frames that carry a backlight command, as (index, pid, pct)."""
    out = []
    for idx, kind, pid, payload in frames:
        if kind == KIND_FEATURE_SET and len(payload) >= 3 and payload[1] in BACKLIGHT_CMDS:
            out.append((idx, pid, payload[2]))
    return out


def _forbidden_frames(frames):
    return [f for f in frames
            if len(f[3]) >= 2 and (f[3][0], f[3][1]) in FORBIDDEN_SET]


class SafetyVdev(unittest.TestCase):
    """Safety cases over the real binaries: one stack on a real ds-vdev + daemon
    (device-side oracle), one daemon on a recording fake device socket (payload-level
    oracle), and one bare ds-vdev for raw socket probes that must not pollute a
    daemon's write log."""

    vdev_stack = None   # ds-vdev + ds-daemon
    fake_stack = None   # FakeVdev + ds-daemon
    bare_vdev = None    # ds-vdev only (raw probes)
    boot_error = None

    @classmethod
    def setUpClass(cls):
        try:
            bins = ensure_binaries()
            cls.bins = bins
            cls.vdev_stack = _make_stack("vdev", bins)
            cls.fake_stack = _make_stack("fake", bins)
            cls.bare_vdev = _make_stack("bare", bins)
        except Exception as e:
            cls.boot_error = "%s: %s" % (type(e).__name__, e)

    @classmethod
    def tearDownClass(cls):
        problems = []
        for st in (cls.vdev_stack, cls.fake_stack, cls.bare_vdev):
            if st is not None:
                problems += st.stop()
        if problems:
            print("CLEANUP PROBLEMS: %s" % "; ".join(problems), file=sys.stderr)

    def setUp(self):
        if self.boot_error:
            self.fail("safety stacks did not boot: %s" % self.boot_error)
        self.ds = self.vdev_stack.ds

    # ---- 010: ds-protocol public API — forbidden commands unrepresentable --------

    def test_safety_010_protocol_public_api_safety_tests(self):
        _cargo_skip(self, "ds-protocol public-API safety tests")
        proc = run_cargo_test(["-p", "ds-protocol", "--test", "protocol_safety"])
        _assert_cargo_ok(self, proc, "cargo test -p ds-protocol --test protocol_safety",
                         PROTOCOL_SAFETY_TESTS)

    # ---- 020: attempted forbidden write -> ERROR, zero bytes accepted -----------

    def test_safety_020_forbidden_write_refused_zero_bytes_on_vdev(self):
        # bare vdev (no daemon): the raw probe below must not pollute a daemon's log
        client = VdevClient(self.bare_vdev.vdev_sock)
        try:
            acks = 0
            for rid, cmd in FORBIDDEN:
                kind = KIND_WRITE if rid == 0x02 else KIND_FEATURE_SET
                payload = bytes([rid, cmd]) + b"\x00" * 30
                reply = client.rpc(kind, PID_KEYS, payload, timeout=10)
                self.assertIsNotNone(reply, "no reply for forbidden %02x %02x" % (rid, cmd))
                rkind, rpid, rpayload = reply
                self.assertEqual(
                    rkind, KIND_ERROR,
                    "forbidden report %02x %02x was NOT refused with an error frame "
                    "(reply kind %#04x) — a forbidden write reached the device" % (rid, cmd, rkind),
                )
                self.assertIn(b"forbidden", rpayload.lower(),
                              "error message for %02x %02x: %r" % (rid, cmd, rpayload))
                if rkind == KIND_ACK:
                    acks += 1
            self.assertEqual(acks, 0, "an ACK means bytes were accepted for a forbidden write")
        finally:
            client.close()
        # vdev's write log is the oracle: every attempt recorded as refused, none acked
        lines = ctl_request(self.bare_vdev.vdev_sock, "writes 100000")
        writes = parse_writes(lines)
        self.assertTrue(writes, "vdev write log is empty")
        refused = [w for w in writes if w["refused"]]
        self.assertGreaterEqual(
            len(refused), len(FORBIDDEN),
            "vdev write log must record every forbidden attempt as refused; got %d of %d"
            % (len(refused), len(FORBIDDEN)),
        )
        self.assertTrue(all(w["pid"] == PID_KEYS for w in refused))
        # the IPC error model: hardware errors incl. Forbidden are E_DEVICE, CLI exit 7
        p = self.ds("schema", "ipc")
        self.assertEqual(p.returncode, 0, p.stderr)
        schema = json.loads(p.stdout)
        codes = schema["$defs"]["error"]["properties"]["code"]["enum"]
        self.assertIn("E_DEVICE", codes)
        p = self.ds("--help")
        self.assertEqual(p.returncode, 0)
        self.assertIn("7 E_DEVICE", p.stdout)

    # ---- 025: host-side guard + device-side refusal + E_DEVICE mapping ----------

    def test_safety_025_forbidden_guard_layers_and_E_DEVICE_mapping(self):
        _cargo_skip(self, "ds-device/ds-vdev/ds-ipc guard tests")
        # raw-path guard on the host: refused before any byte leaves (Error::Forbidden)
        proc = run_cargo_test(["-p", "ds-device", "--", "forbidden"])
        _assert_cargo_ok(self, proc, "cargo test -p ds-device -- forbidden",
                         ["forbidden_bytes_are_refused_on_the_raw_path"])
        # device-side refusal in the virtual hub
        proc = run_cargo_test(["-p", "ds-vdev", "--", "forbidden"])
        _assert_cargo_ok(self, proc, "cargo test -p ds-vdev -- forbidden",
                         ["forbidden_reports_are_refused_device_side"])
        # EDevice -> "E_DEVICE" -> exit 7 in the IPC error model (docs/contracts/ipc.md §4)
        proc = run_cargo_test(["-p", "ds-ipc"])
        _assert_cargo_ok(self, proc, "cargo test -p ds-ipc")

    # ---- 030: the daemon's own traffic never carries a forbidden pair -----------

    def test_safety_030_daemon_traffic_never_forbidden(self):
        _drive_device_traffic(self.vdev_stack.ds)
        _drive_device_traffic(self.fake_stack.ds)
        time.sleep(1.0)  # let the device tasks drain the push channels (bounded settle)
        # vdev-side: a forbidden report is always refused, so zero refused == zero forbidden
        lines = ctl_request(self.vdev_stack.vdev_sock, "writes 100000")
        writes = parse_writes(lines)
        self.assertTrue(writes, "no device writes recorded at all")
        refused = [w for w in writes if w["refused"]]
        self.assertEqual(
            refused, [],
            "the daemon attempted a forbidden/unknown report; vdev refused: %s" % refused,
        )
        # payload-level: every frame the daemon sent to the fake device
        frames = self.fake_stack.fake.recorded()
        self.assertTrue(frames, "the daemon sent no frames to the fake device")
        bad = _forbidden_frames(frames)
        self.assertEqual(
            bad, [],
            "the daemon put a forbidden (report id, command) pair on the wire: %s"
            % [(hex(f[2]), f[3][:2].hex()) for f in bad],
        )

    # ---- 040: software-mode handshake before any read/write on keys/dial ---------

    def test_safety_040_handshake_before_any_read_or_write_on_keys_dial(self):
        fake = self.fake_stack.fake
        # wait for the daemon's handshake reports (it sends them right after connect)
        hs_keys = fake.wait_for(
            lambda f: f[2] == PID_KEYS and f[1] == KIND_FEATURE_SET
            and f[3][:3] == HANDSHAKE_KEYS,
            what="keys software-mode handshake",
        )
        hs_dial = fake.wait_for(
            lambda f: f[2] == PID_DIAL and f[1] == KIND_FEATURE_SET
            and f[3][:3] == HANDSHAKE_DIAL,
            what="dial software-mode handshake",
        )
        frames = fake.recorded()
        for pid, hs, name in ((PID_KEYS, hs_keys, "keys"), (PID_DIAL, hs_dial, "dial")):
            first = min((f for f in frames if f[2] == pid), key=lambda f: f[0])
            self.assertEqual(
                first[0], hs[0],
                "the FIRST frame the daemon sent to %s was not the software-mode "
                "handshake: %s" % (name, first),
            )
            before = [f for f in frames if f[2] == pid and f[0] < hs[0]]
            self.assertEqual(before, [], "%s had frames before the handshake" % name)
            pre_reads = [f for f in frames if f[2] == pid and f[0] < hs[0]
                         and f[1] == KIND_FEATURE_GET]
            pre_writes = [f for f in frames if f[2] == pid and f[0] < hs[0]
                          and f[1] == KIND_WRITE]
            self.assertEqual(pre_reads, [],
                             "a feature read hit %s before the handshake (would wedge it)"
                             % name)
            self.assertEqual(pre_writes, [],
                             "an output write hit %s before the handshake" % name)
        # after driving traffic, image writes on keys come after the handshake
        _drive_device_traffic(self.fake_stack.ds)
        frames = fake.recorded()
        key_writes = [f for f in frames if f[2] == PID_KEYS and f[1] == KIND_WRITE]
        self.assertTrue(key_writes, "no image writes reached the keys module")
        self.assertTrue(all(f[0] > hs_keys[0] for f in key_writes),
                        "a keys write preceded the handshake")
        self.assertTrue(any(f[3][:2] == bytes([0x02, 0x36]) for f in key_writes),
                        "expected chunked key image reports (02 36 …) after the handshake")
        # vdev-side oracle: the handshake is the first write per screen module, and the
        # modules report software_mode=true afterwards
        lines = ctl_request(self.vdev_stack.vdev_sock, "writes 100000")
        writes = parse_writes(lines)
        for pid, name in ((PID_KEYS, "keys"), (PID_DIAL, "dial")):
            mine = sorted((w for w in writes if w["pid"] == pid), key=lambda w: w["seq"])
            self.assertTrue(mine, "no writes for %s" % name)
            first = mine[0]
            self.assertEqual(first["kind"], KIND_FEATURE_SET,
                             "%s: first write is not the handshake feature-set: %s" % (name, first))
            self.assertEqual(first["len"], 32, "handshake feature report is padded to 32 B")
            earlier_writes = [w for w in mine if w["kind"] == KIND_WRITE and w["seq"] < first["seq"]]
            self.assertEqual(earlier_writes, [],
                             "%s: an output write preceded the handshake: %s" % (name, earlier_writes))
        reports = parse_reports(ctl_request(self.vdev_stack.vdev_sock, "reports"))
        by_kind = {r["kind"]: r for r in reports}
        self.assertTrue(by_kind.get("keys", {}).get("mode"),
                        "vdev reports keys not in software mode: %s" % reports)
        self.assertTrue(by_kind.get("dial", {}).get("mode"),
                        "vdev reports dial not in software mode: %s" % reports)
        # the handshake state machine itself: reads refused before the handshake
        _cargo_skip(self, "ds-device handshake state-machine tests")
        proc = run_cargo_test(["-p", "ds-device", "--", "handshake"])
        _assert_cargo_ok(self, proc, "cargo test -p ds-device -- handshake", [
            "reads_are_refused_before_the_handshake_and_on_unknown_modules",
            "a_failed_send_is_never_followed_by_a_read",
            "happy_path_steps_through_the_phases_in_order",
        ])

    # ---- 060: backlight 0 is never sent -----------------------------------------

    def test_safety_060_backlight_zero_never_sent(self):
        fake = self.fake_stack.fake
        before = _backlight_frames(fake.recorded())
        # a request for 0 brightness at state level must never become a backlight-0 report
        for path in ("/controls/keys/0/brightness", "/controls/dial/brightness"):
            p = self.fake_stack.ds("set", path, "0")
            self.assertIn(p.returncode, (0, 1),
                          "set %s 0 rc=%d stderr=%s" % (path, p.returncode, p.stderr))
        _drive_device_traffic(self.fake_stack.ds)
        time.sleep(0.5)
        frames = fake.recorded()
        bl = _backlight_frames(frames)
        zero = [b for b in bl if b[2] == 0]
        self.assertEqual(
            zero, [],
            "a backlight-0 report reached the device (wedges the screens): %s" % zero,
        )
        for idx, pid, pct in bl:
            self.assertGreaterEqual(pct, 1, "backlight pct %d at frame %d" % (pct, idx))
            self.assertLessEqual(pct, 100)
        self.assertEqual(len(bl), len(before),
                         "the brightness-0 requests produced backlight reports: %s"
                         % [b for b in bl if b not in before])
        # vdev-side: every module still reports a sane backlight (default 50, never 0)
        reports = parse_reports(ctl_request(self.vdev_stack.vdev_sock, "reports"))
        self.assertTrue(reports)
        for r in reports:
            self.assertGreaterEqual(r["backlight"], 1,
                                     "vdev reports backlight=0 on %s" % r["kind"])
            self.assertLessEqual(r["backlight"], 100)
        # builder level: backlight(kind, 0) is None on every module (asserted in 010)
        _cargo_skip(self, "backlight-0 builder refusal")
        proc = run_cargo_test(["-p", "ds-protocol", "--test", "protocol_safety",
                               "--", "backlight_zero"])
        _assert_cargo_ok(self, proc, "protocol_safety backlight_zero",
                         ["backlight_zero_is_refused_on_every_module"])

    # ---- 070: no raw escape hatch in the CLI or the IPC surface ------------------

    def test_safety_070_no_raw_escape_hatch(self):
        # (a) the CLI verb surface, enumerated from the binary's own help text
        p = self.ds("--help")
        self.assertEqual(p.returncode, 0)
        lines = p.stdout.splitlines()
        start = next((i for i, l in enumerate(lines) if l.startswith("Verbs (phase 1):")), None)
        self.assertIsNotNone(start, "no 'Verbs (phase 1):' block in ds --help:\n%s" % p.stdout)
        verbs = set()
        for l in lines[start + 1:]:
            if not l.startswith("  "):
                break  # end of the verb block ("Global flags:" etc.)
            if l.startswith("   ") or l[2:3] == " ":
                continue  # wrapped description line of the previous verb
            verbs.add(l.split()[0])
        self.assertEqual(verbs, EXPECTED_DS_VERBS,
                         "ds verb set changed: %s" % sorted(verbs))
        for v in verbs:
            self.assertIsNone(RAWISH.search(v),
                              "RAW ESCAPE HATCH: `ds %s` is a verb matching the raw-HID "
                              "pattern — no CLI verb may take raw device bytes" % v)
        # (b) source-level: the verb constants and match arms in the CLI parser
        cli_src = (ROOT / "bins" / "ds" / "src" / "cli.rs").read_text()
        for lit in re.findall(r'"([a-z][a-z0-9-]*)"\s*=>', cli_src):
            self.assertIsNone(
                RAWISH.search(lit),
                "RAW ESCAPE HATCH: verb arm %r in bins/ds/src/cli.rs matches the raw-HID "
                "pattern" % lit,
            )
        # (c) live probe: candidate raw verbs must all be unknown (exit 1)
        for v in RAW_CANDIDATES:
            p = self.ds(v, "--help")
            self.assertEqual(
                p.returncode, 1,
                "RAW ESCAPE HATCH: `ds %s --help` exited 0 — %r is a live verb" % (v, v),
            )
            self.assertIn("unknown verb", p.stderr, "ds %s stderr=%s" % (v, p.stderr))
        # (d) ds-vdevctl is inspection-only: its command set must stay within the
        #     known read-only/input-injection commands
        p = self.vdev_stack.vdevctl("--help")
        self.assertEqual(p.returncode, 0, p.stderr)
        hlines = p.stdout.splitlines()
        cstart = next((i for i, l in enumerate(hlines) if l.strip() == "COMMANDS:"), 0)
        cmds = {l.split()[0] for l in hlines[cstart + 1:]
                if re.match(r"^    [a-z][a-z-]* ", l)}
        extra = cmds - VDEVCTL_SAFE
        self.assertEqual(extra, set(),
                         "new ds-vdevctl command(s) %s — verify they cannot take raw host->"
                         "device HID bytes before accepting" % sorted(extra))
        # (e) the IPC op surface, enumerated from the daemon's own schema output
        p = self.ds("schema", "ipc")
        self.assertEqual(p.returncode, 0)
        ipc_schema = json.loads(p.stdout)
        ops = _walk_op_consts(ipc_schema)
        self.assertEqual(ops, EXPECTED_IPC_OPS,
                         "IPC request op set changed: %s" % sorted(ops))
        for op in ops:
            self.assertIsNone(RAWISH.search(op),
                              "RAW ESCAPE HATCH: IPC op %r matches the raw-HID pattern" % op)
        set_ops = _walk_set_subops(ipc_schema)
        self.assertEqual(set_ops, EXPECTED_SET_OPS)
        # stream-frame meta must not carry raw bytes either: the pinned meta field set
        # (docs/contracts/ipc.md §2) is exactly these — anything new fails loudly
        p = self.ds("schema", "stream-frame")
        self.assertEqual(p.returncode, 0)
        frame_schema = json.loads(p.stdout)
        meta = _walk_property_names(frame_schema)
        expected_meta = {"crc32", "format", "fps", "frames", "h", "loop",
                         "max_payload_bytes", "seq", "target", "ts_ms", "w"}
        self.assertEqual(
            meta, expected_meta,
            "stream-frame meta fields changed (%s) — a new field must be proven unable "
            "to carry raw HID bytes before it is accepted" % sorted(meta ^ expected_meta),
        )
        # (f) live IPC probe: candidate raw ops must never be served
        c = self.vdev_stack.ipc()
        try:
            c.hello_auth(self.vdev_stack.token)
            for i, op in enumerate(RAW_CANDIDATES):
                r = c.rpc({"id": 100 + i, "op": op})
                self.assertEqual(
                    r.get("ok"), False,
                    "RAW ESCAPE HATCH: IPC op %r was ACCEPTED by the daemon: %s" % (op, r),
                )
                self.assertIn(r["error"]["code"], ("E_SCHEMA", "E_INTERNAL"),
                              "op %r -> %s" % (op, r))
        finally:
            c.close()
        # (g) value level: no state visual kind can carry raw HID bytes
        p = self.ds("schema", "state")
        self.assertEqual(p.returncode, 0)
        state_schema = json.loads(p.stdout)
        kinds = {branch["properties"]["kind"]["const"]
                 for branch in state_schema["$defs"]["visual"]["oneOf"]
                 if "const" in branch.get("properties", {}).get("kind", {})}
        for k in kinds:
            self.assertIsNone(RAWISH.search(k),
                              "RAW ESCAPE HATCH: visual kind %r in state.json" % k)
        for rawish in ('{"kind":"raw","bytes":"02 34 00"}', '{"kind":"bytes","value":"0234"}',
                       '{"kind":"hid","report":"02 2A 00 00"}'):
            p = self.ds("set", "/controls/keys/0/image", "--json", rawish)
            self.assertEqual(p.returncode, 1,
                             "a raw-bytes visual was accepted: %r rc=%d" % (rawish, p.returncode))
            env = json.loads(p.stderr)
            self.assertEqual(env["error"]["code"], "E_SCHEMA")


class HardwareSafety(unittest.TestCase):
    """DS_HARDWARE=1 pass: the safety checks that can run on a connected deck, read-only
    (enumerate, open, one feature read on M0 modules only, software-mode handshake + init,
    close — no writes beyond the handshake). Skips cleanly without hardware."""

    def test_safety_080_hardware_pass(self):
        if os.environ.get("DS_HARDWARE") != "1":
            self.skipTest(
                "DS_HARDWARE is not 1 — hardware safety pass skipped cleanly "
                "(the vdev cases above prove the invariants in CI)"
            )
        _cargo_skip(self, "hardware smoke")
        proc = run_cargo_test(
            ["-p", "ds-device", "--test", "hid", "--", "--test-threads=1"],
            timeout_s=300, extra_env={"DS_HARDWARE": "1"},
        )
        out = proc.stdout or ""
        # the hw target self-skips (and still passes) when no deck is attached
        self.assertEqual(proc.returncode, 0,
                         "hardware safety pass failed:\n%s" % out[-4000:])
        self.assertIn("test result: ok", out)


def _walk_op_consts(schema):
    """All request `op` consts declared anywhere in the IPC schema (server-message ops
    like welcome/event are enums, not consts, so they are excluded automatically)."""
    ops = set()

    def walk(o):
        if isinstance(o, dict):
            props = o.get("properties")
            if isinstance(props, dict) and isinstance(props.get("op"), dict):
                if "const" in props["op"]:
                    ops.add(props["op"]["const"])
            for v in o.values():
                walk(v)
        elif isinstance(o, list):
            for v in o:
                walk(v)

    walk(schema)
    return ops


def _walk_set_subops(schema):
    txt = json.dumps(schema)
    m = re.search(r'"opSet".*?"enum"\s*:\s*(\[[^\]]*\])', txt, re.DOTALL)
    return set(json.loads(m.group(1))) if m else set()


def _walk_property_names(schema):
    names = set()

    def walk(o):
        if isinstance(o, dict):
            props = o.get("properties")
            if isinstance(props, dict):
                names.update(props.keys())
            for v in o.values():
                walk(v)
        elif isinstance(o, list):
            for v in o:
                walk(v)

    walk(schema)
    return names


def _make_stack(device, bins):
    st = Stack(device=device)
    st.start(bins)
    return st


def safety_suite():
    """The suite object registered by tests/conformance/run.py."""
    loader = unittest.defaultTestLoader
    suite = unittest.TestSuite()
    suite.addTests(loader.loadTestsFromTestCase(SafetyVdev))
    suite.addTests(loader.loadTestsFromTestCase(HardwareSafety))
    return suite


class LineResult(unittest.TextTestResult):
    def addSuccess(self, test):
        super().addSuccess(test)
        print("PASS  %s" % test.id())

    def addFailure(self, test, err):
        super().addFailure(test, err)
        print("FAIL  %s" % test.id())

    def addError(self, test, err):
        super().addError(test, err)
        print("ERROR %s" % test.id())

    def addSkip(self, test, reason):
        super().addSkip(test, reason)
        print("SKIP  %s (%s)" % (test.id(), reason))


def _filter_suite(suite, patterns):
    out = unittest.TestSuite()
    for t in suite:
        if isinstance(t, unittest.TestSuite):
            sub = _filter_suite(t, patterns)
            if sub.countTestCases():
                out.addTests(sub)
        else:
            if any(p.lower() in t.id().lower() for p in patterns):
                out.addTest(t)
    return out


def _parse_args(argv):
    patterns = []
    i = 1
    while i < len(argv):
        if argv[i] == "-k" and i + 1 < len(argv):
            patterns.append(argv[i + 1])
            i += 2
        else:
            i += 1
    return patterns


def main(argv):
    patterns = _parse_args(argv)
    suite = safety_suite()
    if patterns:
        suite = _filter_suite(suite, patterns)
    runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=0,
                                     resultclass=LineResult)
    result = runner.run(suite)
    n = result.testsRun
    bad = len(result.failures) + len(result.errors)
    print("-" * 60)
    print("safety: %d tests, %d passed, %d failed/errored, %d skipped"
          % (n, n - bad - len(result.skipped), bad, len(result.skipped)))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

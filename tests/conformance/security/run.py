#!/usr/bin/env python3
"""deckstack security conformance mapping (ticket DS-081) — tests/conformance/security/.

One table-driven runner over docs/20-security.md §2: for every threat row T1-T12 it
executes (or references) the concrete conformance case that proves the mitigation and
prints a status table

    T<n> green|red|skip  <case>  <path>

Exit code 0 = no red row (every skip carries a precise reason plus the hardware/CI job
that covers it); nonzero = at least one red row.

The rows REUSE the existing suites — each check is a subprocess of the owning suite with
its own `-k` selector (or the whole suite when that suite has no selector), so no case
logic is duplicated here:

  T1  tests/conformance/run.py -k test_110 -k test_120  +  tests/conformance/cli/run.py -k cli_100
  T2  tests/conformance/bindings/run.py                 +  sh tests/fixtures/validate.sh
  T3  tests/conformance/run.py -k plugins_010 … plugins_080        (tests/conformance/plugins/)
  T4  tests/conformance/streaming/run.py -k streaming_040 … 090  +  tests/limits/run.py
      +  tests/fuzz/run.py --cases 2000 (seeded; the corpus file is restored after the run)
  T5  tests/conformance/exec/run.py                      +  sh tests/fixtures/validate.sh
  T6  tests/conformance/run.py -k plugins_040  +  cli -k cli_100  +  sh tests/fixtures/validate.sh
  T7  tests/conformance/run.py -k preview        (DS-113 /preview debug surface: T7 Host/bearer/421/401 + no listener when off)
  T8  tests/conformance/run.py -k safety                          (tests/conformance/safety/)
  T9  glue case in THIS file (live rotation on vdev) + ds-client crate test
  T10 skip — vendor contention needs real hardware (DS_HARDWARE=1 tests/hw/smoke.py)
  T11 tests/conformance/run.py -k plugins_050 -k plugins_070
  T12 skip — Windows-only pipe/file ACLs (DS-072 Windows test plan)

Note on the fuzz invocation: `tests/fuzz/run.py --replay` asserts the recorded corpus
verdicts reproduce exactly, but case 499's verdict is nondeterministic in the daemon
(observed both `error:E_SCHEMA+eof` and `error:E_SCHEMA+error:E_INTERNAL+eof` — a
pre-existing DS-080 flake, reported here precisely, not fixed here because tests/fuzz
is outside this ticket). T4 therefore runs a seeded `tests/fuzz/run.py --cases 2000`,
which asserts only typed verdicts + daemon/vdev health, and restores
tests/fuzz/corpus/cases.jsonl afterwards so the run never dirties the tree.

Only two checks are owned by this directory (the only new case logic): the T9 live
token-rotation case on a booted vdev+daemon (SecurityGlueStack) and the doc↔runner 1:1
assertion (SecurityDoc: docs/20-security.md §2 must list exactly the rows this runner
runs, and every Test cell must name an existing path and its status).

This module is also registered into tests/conformance/run.py (security_suite()), so the
normal `python tests/conformance/run.py` runs the mapping; use `-k security` there to run
only these rows. Row checks that re-invoke tests/conformance/run.py set DS_SECURITY_CHILD=1,
which makes the child skip registering this suite — no recursion, no duplicated rows.

Usage:
    python tests/conformance/security/run.py [--verbose] [-k SUBSTR ...]

    -k SUBSTR   only run rows whose id/threat/case contains SUBSTR (e.g. -k T9, -k T1)

Exit code 0 = all rows green or documented-skip, nonzero = any red row (or boot failure).
Every subprocess carries a timeout; referenced suites clean up their own stacks.
"""

import importlib.util
import io
import json
import os
import re
import signal
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
VERBOSE = "--verbose" in sys.argv
PY = sys.executable

# Set on every row-check child that re-invokes tests/conformance/run.py: the child then
# does NOT register this security suite (guard against recursion / duplicated rows).
CHILD_ENV = "DS_SECURITY_CHILD"

CMD_TIMEOUT_S = 600    # per referenced suite command (limits ~40 s, safety ~45 s observed)
RUN_TIMEOUT_S = 1800   # whole-runner watchdog (standalone mode only)

DOC = ROOT / "docs" / "20-security.md"


def log(msg):
    if VERBOSE:
        print(msg, file=sys.stderr)


# ---------------------------------------------------------------- row checks ---

def _suite(script, *selectors):
    """argv that runs one existing suite, filtered to the given `-k` selectors."""
    argv = [PY, str(ROOT / script)]
    for s in selectors:
        argv += ["-k", s]
    return argv


_RESULTS = {}


def _run_cmd(argv, timeout_s=CMD_TIMEOUT_S, preserve=()):
    """Run one referenced command (memoized across rows in this process). Returns (rc, tail).
    `preserve` lists repo-relative files the command rewrites as a side effect (the fuzz
    corpus); they are snapshotted and restored so the run never dirties the tree."""
    key = (tuple(argv), tuple(preserve))
    if key in _RESULTS:
        return _RESULTS[key]
    snapshots = {}
    for p in preserve:
        f = ROOT / p
        if f.is_file():
            snapshots[f] = f.read_bytes()
    env = dict(os.environ)
    env[CHILD_ENV] = "1"
    try:
        try:
            proc = subprocess.run(argv, cwd=str(ROOT), capture_output=True, text=True,
                                  timeout=timeout_s, env=env)
            rc, out = proc.returncode, proc.stdout + "\n" + proc.stderr
        except subprocess.TimeoutExpired:
            rc, out = 124, "timed out after %ss: %s" % (timeout_s, " ".join(argv))
    finally:
        for f, data in snapshots.items():
            f.write_bytes(data)
    tail = "\n".join(out.strip().splitlines()[-8:]) if out.strip() else "<no output>"
    log("cmd rc=%d: %s\n%s" % (rc, " ".join(argv), tail))
    res = (rc, tail)
    _RESULTS[key] = res
    return res


def _conf():
    """The DS-018 harness module (Stack/Ipc/ensure_binaries), imported not copied —
    the same pattern tests/fuzz/run.py and tests/limits/run.py use."""
    global _CONF
    if _CONF is None:
        spec = importlib.util.spec_from_file_location(
            "ds_conformance_run", ROOT / "tests" / "conformance" / "run.py")
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        _CONF = mod
    return _CONF


_CONF = None


def _run_glue(pattern):
    """Run the in-process glue cases (SecurityGlueStack / SecurityDoc) whose id contains
    `pattern`. Returns (rc, detail)."""
    suite = unittest.TestSuite()
    loader = unittest.defaultTestLoader
    for cls in (SecurityGlueStack, SecurityDoc):
        for t in loader.loadTestsFromTestCase(cls):
            if pattern.lower() in t.id().lower():
                suite.addTest(t)
    if not suite.countTestCases():
        return 1, "no glue case matches %r" % pattern
    buf = io.StringIO()
    result = unittest.TextTestRunner(stream=buf, verbosity=0).run(suite)
    if VERBOSE:
        sys.stderr.write(buf.getvalue())
    bad = len(result.failures) + len(result.errors)
    detail = "%d glue case(s) for %r, %d failed/errored" % (result.testsRun, pattern, bad)
    for _, tb in result.failures + result.errors:
        detail += "\n" + tb.strip().splitlines()[-1]
    return (0 if not bad else 1), detail


def run_row(row):
    """Execute one threat row. Returns (status, detail) with status in green|red|skip."""
    if row.get("skip"):
        return "skip", row["skip"]["reason"]
    for check in row["checks"]:
        kind = check[0]
        if kind == "run":
            rc, tail = _run_cmd(check[1], preserve=check[2] if len(check) > 2 else ())
            if rc != 0:
                return "red", "command failed rc=%d: %s\n%s" % (rc, " ".join(check[1]), tail)
        elif kind == "glue":
            rc, detail = _run_glue(check[1])
            if rc != 0:
                return "red", "glue case failed: %s" % detail
        else:
            return "red", "unknown check kind %r" % kind
    return "green", "all %d check(s) green" % len(row["checks"])


# ------------------------------------------------------------------- rows ------

ROWS = [
    {
        "id": "T1",
        "threat": "local attacker, no token, connects to the socket/pipe",
        "case": "no token / bad token -> E_AUTH (exit 4); op before auth -> E_AUTH; read-scope token `set` -> E_SCOPE (exit 4)",
        "path": "tests/conformance/run.py `-k test_110 -k test_120`; tests/conformance/cli/run.py `-k cli_100`",
        "checks": [
            ("run", _suite("tests/conformance/run.py", "test_110", "test_120")),
            ("run", _suite("tests/conformance/cli/run.py", "cli_100")),
        ],
    },
    {
        "id": "T2",
        "threat": "malicious config = RCE via code-in-config",
        "case": "code-ish binding step `{\"do\":\"eval\"}` -> E_SCHEMA exit 1, nothing stored; fixture gate rejects the eval-like step (R8) and relative argv[0] (M1)",
        "path": "tests/conformance/bindings/run.py (test_070/test_130); sh tests/fixtures/validate.sh",
        "checks": [
            ("run", [PY, str(ROOT / "tests/conformance/bindings/run.py")]),
            ("run", ["sh", "tests/fixtures/validate.sh"]),
        ],
    },
    {
        "id": "T3",
        "threat": "malicious plugin corrupts the daemon / fork-bombs",
        "case": "fork bomb killed by the nproc grant; AS memory bomb; E_CAP violation with no state change; invoke timeout -> degraded -> restart; crash -> respawn -> watchdog disable",
        "path": "tests/conformance/plugins/run.py via `python tests/conformance/run.py -k plugins_010 -k plugins_020 -k plugins_030 -k plugins_060 -k plugins_080`",
        "checks": [
            ("run", _suite("tests/conformance/run.py", "plugins_010", "plugins_020",
                           "plugins_030", "plugins_060", "plugins_080")),
        ],
    },
    {
        "id": "T4",
        "threat": "image/frame bombs",
        "case": "301st frame refused (300 cap), --fps 31 refused, 17 MB file -> E_LIMIT exit 5, wire-cap NACKs; limits suite image bombs (>4096 px decode refused, 16 MB cap, RSS delta < 50 MB, queue exhaustion); seeded fuzz run (2000 cases, typed verdicts + health; corpus restored)",
        "path": "tests/conformance/streaming/run.py `-k streaming_040 -k streaming_050 -k streaming_060 -k streaming_090`; tests/limits/run.py; tests/fuzz/run.py --cases 2000",
        "checks": [
            ("run", _suite("tests/conformance/streaming/run.py", "streaming_040",
                           "streaming_050", "streaming_060", "streaming_090")),
            ("run", [PY, str(ROOT / "tests/limits/run.py")]),
            ("run", [PY, str(ROOT / "tests/fuzz/run.py"), "--cases", "2000"],
             ["tests/fuzz/corpus/cases.jsonl"]),
        ],
    },
    {
        "id": "T5",
        "threat": "shell injection via exec",
        "case": "[\"/bin/sh\",\"-c\",…] -> E_CAP with zero child processes and no file created; /usr/bin/env -> E_CAP; relative argv[0] -> E_SCHEMA at bind time; no allow_exec -> E_CAP; cwd outside fs roots -> E_CAP; metachar argument arrives literally",
        "path": "tests/conformance/exec/run.py (test_020-test_100); sh tests/fixtures/validate.sh (M1 argv[0] pattern)",
        "checks": [
            ("run", [PY, str(ROOT / "tests/conformance/exec/run.py")]),
            ("run", ["sh", "tests/fixtures/validate.sh"]),
        ],
    },
    {
        "id": "T6",
        "threat": "path traversal via file paths",
        "case": "plugin fs request `../../etc/shadow` / outside fs_roots -> E_CAP, nothing read; `set --file` outside /policy/fs/roots -> E_CAP (exit 4); schema pattern rejects any `..` segment (M3 fixture)",
        "path": "tests/conformance/plugins/run.py via `-k plugins_040`; tests/conformance/cli/run.py `-k cli_100`; sh tests/fixtures/validate.sh",
        "checks": [
            ("run", _suite("tests/conformance/run.py", "plugins_040")),
            ("run", _suite("tests/conformance/cli/run.py", "cli_100")),
            ("run", ["sh", "tests/fixtures/validate.sh"]),
        ],
    },
    {
        "id": "T7",
        "threat": "DNS rebinding on HTTP/WS adapter",
        "case": "opt-in /preview debug surface (DS-113, ADR-019): `Host: evil.example` -> 421; missing bearer -> 401 on HTTP and on the WS upgrade; 127.0.0.1-only bind, no listener at all when the surface is off (default)",
        "path": "tests/conformance/preview/run.py via `python3 tests/conformance/run.py -k preview`; contract docs/contracts/preview.md + docs/contracts/ipc.md §1.1",
        "checks": [
            ("run", _suite("tests/conformance/run.py", "preview")),
        ],
    },
    {
        "id": "T8",
        "threat": "device bricking via firmware/serial/reset",
        "case": "all pinned forbidden (report,cmd) pairs -> 0x84 ERROR with zero bytes accepted on vdev (E_DEVICE / exit 7); software-mode handshake before any read on keys/dial; backlight-0 never sent; no raw escape hatch; cargo protocol-safety tests",
        "path": "tests/conformance/safety/run.py via `python tests/conformance/run.py -k safety` (details in §2a below)",
        "checks": [
            ("run", _suite("tests/conformance/run.py", "safety")),
        ],
    },
    {
        "id": "T9",
        "threat": "token rotation window",
        "case": "rotate (rewrite tokens.json, what `ds token revoke` does) -> NEW connection with the old token -> E_AUTH immediately; an already-established connection keeps its session (<=5 s in-flight grace); the admin install token is unaffected",
        "path": "tests/conformance/security/run.py `-k T9` (glue case test_security_glue_t09_*); crate twin: cargo test -p ds-client -- revoked_token_is_rejected_on_new_connection",
        "checks": [
            ("glue", "t09"),
        ],
    },
    {
        "id": "T10",
        "threat": "vendor app contention",
        "case": "daemon start with the vendor app holding the deck -> clear error naming the holder, no writes",
        "path": "tests/hw/smoke.py (hardware job: DS_HARDWARE=1 python tests/hw/smoke.py)",
        "checks": [],
        "skip": {
            "reason": "vendor-app contention requires a real deck held open by another process; the vdev socket has no vendor app to contend with",
            "covered_by": "hardware CI job `DS_HARDWARE=1 python tests/hw/smoke.py` (guard handback, no lingering deck-node holders after daemon exit; DS-024/DS-026, docs/30-testing.md §7-§8)",
        },
    },
    {
        "id": "T11",
        "threat": "plugin network to non-granted hosts",
        "case": "exact glob matrix vs a fake 127.0.0.1 lights server (`lights.lan.evil.com` -> E_CAP, `a.b.lan` with `*.lan` -> E_CAP, allowed host succeeds); URL > 2048 B -> E_LIMIT; non-http URL -> E_SCHEMA; invalid net:<glob> refused at manifest validation",
        "path": "tests/conformance/plugins/run.py via `-k plugins_050 -k plugins_070`",
        "checks": [
            ("run", _suite("tests/conformance/run.py", "plugins_050", "plugins_070")),
        ],
    },
    {
        "id": "T12",
        "threat": "another local user connects to the Windows pipe / reads token files",
        "case": "second local account connect to \\\\.\\pipe\\deckstack-<user> -> access denied; token dir + token files ACL = user+SYSTEM only",
        "path": "docs/contracts/ipc.md §1.1 (pipe SDDL); DS-072 Windows test plan (docs/40-platforms.md §2, O5/T12)",
        "checks": [],
        "skip": {
            "reason": "Windows-only: the named-pipe SDDL and token-file ACLs can only be probed with a second local account on Windows; the alpha is Linux-only with no Windows CI gate",
            "covered_by": "Windows port phase — DS-072 test plan: a Windows CI/hw job with two local accounts (docs/30-testing.md §8 Windows gate: 'second-account ACL test (O5/T12)')",
        },
    },
]


# ------------------------------------------------------------- glue cases ------

class SecurityGlueStack(unittest.TestCase):
    """The one live case this directory owns: token rotation on a booted vdev+daemon
    (T9). Everything else in the mapping is referenced, not duplicated."""

    stack = None
    boot_error = None

    @classmethod
    def setUpClass(cls):
        try:
            conf = _conf()
            bins = conf.ensure_binaries()
            cls.stack = conf.Stack()
            cls.stack.start(bins)
        except Exception as e:  # noqa: BLE001
            cls.boot_error = "%s: %s" % (type(e).__name__, e)

    @classmethod
    def tearDownClass(cls):
        if cls.stack is not None:
            problems = cls.stack.stop()
            if problems and VERBOSE:
                print("glue cleanup problems: %s" % "; ".join(problems), file=sys.stderr)

    def test_security_glue_t09_token_rotation_is_instant_for_new_connections(self):
        if self.boot_error:
            self.fail("glue stack did not boot: %s" % self.boot_error)
        stack = self.stack
        tokens_json = stack.data / "tokens.json"
        scoped = "s" * 64
        tokens_json.write_text(json.dumps(
            {"tokens": [{"id": "t-glue", "token": scoped, "scope": "read"}]}))
        tokens_json.chmod(0o600)
        # 1. a new connection authenticates with the scoped token (read scope)
        c = stack.ipc()
        try:
            r = c.rpc({"id": 1, "op": "hello", "protocol": 1, "client": "sec/1"})
            self.assertEqual(r["ok"], True, r)
            r = c.rpc({"id": 2, "op": "auth", "token": scoped, "scope": "read"})
            self.assertEqual(r["ok"], True, r)
            self.assertEqual(r["result"]["scope"], "read", r)
            r = c.rpc({"id": 3, "op": "ping"})
            self.assertEqual(r["ok"], True, r)
            # 2. rotate: rewrite tokens.json without the token (what `ds token revoke` does)
            tokens_json.write_text(json.dumps({"tokens": []}))
            tokens_json.chmod(0o600)
            # 3. a NEW connection with the revoked token -> E_AUTH immediately (T9/S12)
            c2 = stack.ipc()
            try:
                c2.rpc({"id": 1, "op": "hello", "protocol": 1, "client": "sec/2"})
                r = c2.rpc({"id": 2, "op": "auth", "token": scoped, "scope": "read"})
                self.assertEqual(r["ok"], False, r)
                self.assertEqual(r["error"]["code"], "E_AUTH", r)
            finally:
                c2.close()
            # 4. the already-established connection keeps its session (in-flight grace)
            r = c.rpc({"id": 4, "op": "ping"})
            self.assertEqual(r["ok"], True,
                             "established connection must keep working after rotation: %s" % r)
        finally:
            c.close()
        # 5. the install (admin) token is unaffected by the scoped-token rotation
        c3 = stack.ipc()
        try:
            c3.rpc({"id": 1, "op": "hello", "protocol": 1, "client": "sec/3"})
            r = c3.rpc({"id": 2, "op": "auth", "token": stack.token, "scope": "admin"})
            self.assertEqual(r["ok"], True, r)
            self.assertEqual(r["result"]["scope"], "admin", r)
        finally:
            c3.close()


class SecurityDoc(unittest.TestCase):
    """1:1 assertion (DS-081 acceptance): docs/20-security.md §2 lists exactly the rows
    this runner runs, and every Test cell names an existing path + its status."""

    def test_security_doc_table_rows_match_runner(self):
        text = DOC.read_text()
        sec = text.split("## 2. Threat model", 1)[1].split("## 2a.", 1)[0]
        ids = re.findall(r"^\|\s*(T\d+)\s*\|", sec, re.M)
        self.assertEqual(ids, [r["id"] for r in ROWS],
                         "docs/20-security.md §2 table must list exactly the rows this "
                         "runner runs (got %s, want %s)" % (ids, [r["id"] for r in ROWS]))
        cells = {}
        for line in sec.splitlines():
            parts = [p.strip() for p in line.split("|")]
            if len(parts) >= 3 and re.fullmatch(r"T\d+", parts[1]):
                cells[parts[1]] = parts[-2]  # last column = Test cell
        self.assertEqual(len(cells), len(ROWS))
        for row in ROWS:
            cell = cells[row["id"]]
            hits = []
            for bt in re.findall(r"`([^`]+)`", cell):
                for tok in bt.split():
                    tok = tok.strip("()[]{},;:.")
                    if "/" in tok and (ROOT / tok).exists():
                        hits.append(tok)
                        break
            self.assertTrue(hits, "%s Test cell must name at least one existing path "
                           "(backticked), got cell=%r" % (row["id"], cell))
            want = "skip" if row.get("skip") else "green"
            self.assertIn(want, cell.lower(),
                          "%s Test cell must state its status %r: %r" % (row["id"], want, cell))


# ------------------------------------------------------- unittest binding ------

class SecurityMapping(unittest.TestCase):
    """One test per threat row; the methods are attached by security_suite()."""


def _row_test_name(row):
    slug = re.sub(r"[^a-z0-9]+", "_", row["threat"].lower()).strip("_")[:48]
    return "test_security_%s_%s" % (row["id"].lower(), slug)


def _make_row_test(row):
    def test(self):
        status, detail = run_row(row)
        if status == "red":
            self.fail("%s is red: %s" % (row["id"], detail))
        if status == "skip":
            raise unittest.SkipTest("%s: %s (covered by: %s)"
                                    % (row["id"], row["skip"]["reason"],
                                       row["skip"]["covered_by"]))
    test.__doc__ = "%s — %s\ncase: %s\npath: %s" % (
        row["id"], row["threat"], row["case"], row["path"])
    return test


def security_suite():
    """unittest view of the T1-T12 mapping, registered into tests/conformance/run.py.
    One test per threat row (ids contain `security_t<n>`) plus the doc 1:1 test
    (`security_doc`); `-k security` in the main runner selects exactly these."""
    for row in ROWS:
        setattr(SecurityMapping, _row_test_name(row), _make_row_test(row))
    suite = unittest.TestSuite()
    suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(SecurityMapping))
    suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(SecurityDoc))
    return suite


# ------------------------------------------------------------------ main --------

def _alarm(signum, frame):
    raise TimeoutError("security mapping exceeded the %ss watchdog" % RUN_TIMEOUT_S)


def _parse_k(argv):
    patterns = []
    i = 1
    while i < len(argv):
        if argv[i] == "-k" and i + 1 < len(argv):
            patterns.append(argv[i + 1])
            i += 2
        else:
            i += 1
    return patterns


def main():
    patterns = _parse_k(sys.argv)
    rows = [r for r in ROWS
            if not patterns
            or any(p.lower() in (r["id"] + " " + r["threat"] + " " + r["case"]).lower()
                   for p in patterns)]
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(RUN_TIMEOUT_S)
    try:
        statuses = [run_row(r) for r in rows]
    finally:
        signal.alarm(0)
    print("security conformance mapping — docs/20-security.md §2 (ticket DS-081)")
    for row, (status, detail) in zip(rows, statuses):
        print("%s %s  %s  %s" % (row["id"], status, row["case"], row["path"]))
        if status == "skip":
            print("    skip: %s" % row["skip"]["reason"])
            print("    covered by: %s" % row["skip"]["covered_by"])
        elif status == "red":
            print("    red: %s" % detail.replace("\n", "\n    "))
    rc, detail = _run_glue("doc")
    print("DOC %s  §2 threat-table rows == this runner's rows; every Test cell names an "
          "existing path + status  docs/20-security.md"
          % ("green" if rc == 0 else "red"))
    if rc != 0:
        print("    red: %s" % detail.replace("\n", "\n    "))
    row_red = sum(1 for s, _ in statuses if s == "red")
    n_red = row_red + (1 if rc else 0)
    n_skip = sum(1 for s, _ in statuses if s == "skip")
    n_green = len(rows) - row_red - n_skip
    print("-" * 60)
    print("security mapping: %d rows, %d green, %d red, %d skip"
          % (len(rows), n_green, n_red, n_skip))
    if n_red:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())

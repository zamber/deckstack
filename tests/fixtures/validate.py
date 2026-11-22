#!/usr/bin/env python3
"""Validate tests/fixtures/ipc-examples.json against schemas/ipc-message.json, and
schemas themselves (draft 2020-12 metaschema check). CI gate; also the REVIEW command:

    python3 tests/fixtures/validate.py            # exits non-zero on any failure

Requires: jsonschema (pip install jsonschema). Usage:
    pip install jsonschema && python3 tests/fixtures/validate.py
"""
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCHEMAS = ROOT / "schemas"
FIXTURES = ROOT / "tests" / "fixtures"


def main() -> int:
    from jsonschema import Draft202012Validator

    schemas = {}
    for f in sorted(SCHEMAS.glob("*.json")):
        s = json.loads(f.read_text())
        Draft202012Validator.check_schema(s)
        schemas[f.name] = s
        print(f"ok  schema parses + metaschema-valid: {f.name}")

    # external $ref resolution: every non-local $ref must point at a loaded $id
    ids = {s["$id"] for s in schemas.values()}
    refs = []

    def collect(o):
        if isinstance(o, dict):
            if isinstance(o.get("$ref"), str):
                refs.append(o["$ref"])
            for v in o.values():
                collect(v)
        elif isinstance(o, list):
            for v in o:
                collect(v)

    for s in schemas.values():
        collect(s)
    missing = [r for r in refs if not r.startswith("#") and r not in ids]
    if missing:
        print("FAIL  unresolved $refs:", missing)
        return 1
    print(f"ok  all {len(refs)} $refs resolve ({sum(1 for r in refs if r.startswith('#'))} local, {len(refs) - sum(1 for r in refs if r.startswith('#'))} external)")

    # example corpus against the IPC schema
    corpus = json.loads((FIXTURES / "ipc-examples.json").read_text())
    ipc = Draft202012Validator(schemas["ipc-message.json"])
    failures = 0
    for i, msg in enumerate(corpus["messages"]):
        errs = list(ipc.iter_errors(msg))
        if errs:
            failures += 1
            print(f"FAIL  message[{i}] {msg.get('op')}: {errs[0].message}")
        else:
            print(f"ok  message[{i}] {msg.get('op') or 'response id=' + str(msg.get('id'))}")
    if failures:
        return 1

    # regex regression tests (S8/m2): key index patterns must accept 0,9,10,14 and reject 15,99
    import re
    keys_pat = schemas["state.json"]["properties"]["controls"]["properties"]["keys"]["patternProperties"]
    pat = next(iter(keys_pat))
    rx = re.compile(pat)
    expect = {"0": True, "9": True, "10": True, "14": True, "15": False, "99": False, "1": True, "": False}
    for k, want in expect.items():
        got = bool(rx.fullmatch(k) or rx.match(k)) and bool(re.search(pat, k))
        # patternProperties uses search semantics; emulate exactly:
        got = re.search(pat, k) is not None
        status = "ok" if got == want else "FAIL"
        if got != want:
            failures += 1
        print(f"{status}  keys pattern {pat!r} vs {k!r}: match={got} want={want}")

    # path traversal pattern (M3)
    file_pat = None
    for variant in schemas["state.json"]["$defs"]["visual"]["oneOf"]:
        p = variant.get("properties", {}).get("path", {}).get("pattern")
        if p:
            file_pat = p
    trav = {"/home/u/pic.png": True, "/etc/shadow": True, "../../etc/shadow": False,
            "/a/../b": False, "/..": False, "/a..b/x.png": True, "relative.png": False}
    for p, want in trav.items():
        got = re.search(file_pat, p) is not None and re.match(file_pat, p) is not None
        got = re.match(file_pat, p) is not None
        status = "ok" if got == want else "FAIL"
        if got != want:
            failures += 1
        print(f"{status}  file path pattern vs {p!r}: match={got} want={want}")

    # exec argv[0] must be absolute (M1/S3)
    bind = schemas["binding.json"]
    exec_step = next(s for s in bind["$defs"]["step"]["oneOf"] if s["properties"]["do"]["const"] == "exec")
    argv0_pat = exec_step["properties"]["argv"]["prefixItems"][0]["pattern"]
    for a, want in {"/usr/bin/ffmpeg": True, "sh": False, "../../malicious": False, "/bin/sh": True}.items():
        got = re.match(argv0_pat, a) is not None
        status = "ok" if got == want else "FAIL"
        if got != want:
            failures += 1
        print(f"{status}  argv[0] pattern vs {a!r}: match={got} want={want} (note: /bin/sh passes schema but is killed by /policy/exec/deny at runtime — S3)")

    print("FAIL" if failures else "ALL CHECKS PASSED")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())

#!/bin/sh
# deckstack single local gate. Runs on every change and in CI.
#   always:      sh tests/fixtures/validate.sh  (must print ALL CHECKS PASSED)
#   conditional: cargo fmt/clippy/test when Cargo.toml exists
#                conformance suite when tests/conformance/run.py exists
# Clear summary at the end; exit nonzero on any failure.
# Usage: sh tools/check.sh [tree-root]
#   no argument  -> validate the tree this script lives in (the tree it is invoked from)
#   tree-root    -> validate that tree instead
set -u
ROOT=${1:-}
if [ -z "$ROOT" ]; then
  ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
else
  case "$ROOT" in /*) ;; *) ROOT="$(pwd)/$ROOT" ;; esac
fi
[ -f "$ROOT/tools/check.sh" ] || { echo "check.sh: '$ROOT' is not a deckstack tree (no tools/check.sh there)" >&2; exit 2; }
cd "$ROOT"

fail=0
pass() { echo "PASS  $1"; }
bad()  { echo "FAIL  $1"; fail=1; }

echo "== tools/check.sh (validating tree: $ROOT) =="

echo "-- always: fixture gate (schemas + examples + regex regressions)"
if out=$(sh tests/fixtures/validate.sh 2>&1); then rc=0; else rc=$?; fi
echo "$out" | tail -n 2 | sed 's/^/    /'
if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "ALL CHECKS PASSED"; then
  pass "sh tests/fixtures/validate.sh -> ALL CHECKS PASSED"
else
  bad "sh tests/fixtures/validate.sh (rc=$rc)"; printf '%s\n' "$out" | tail -n 20 | sed 's/^/    /'
fi

echo "-- conditional: cargo workspace (runs when Cargo.toml exists)"
if [ -f Cargo.toml ]; then
  if cargo fmt --check >/dev/null 2>&1; then pass "cargo fmt --check"; else bad "cargo fmt --check"; cargo fmt --check 2>&1 | head -n 20 | sed 's/^/    /'; fi
  if cargo clippy --workspace -- -D warnings >/dev/null 2>&1; then pass "cargo clippy --workspace -- -D warnings"; else bad "cargo clippy --workspace -- -D warnings"; cargo clippy --workspace -- -D warnings 2>&1 | tail -n 20 | sed 's/^/    /'; fi
  if cargo test --workspace >/dev/null 2>&1; then pass "cargo test --workspace"; else bad "cargo test --workspace"; cargo test --workspace 2>&1 | tail -n 20 | sed 's/^/    /'; fi
else
  echo "SKIP  no Cargo.toml in this tree"
fi

echo "-- conditional: conformance suite (runs when tests/conformance/run.py exists)"
if [ -f tests/conformance/run.py ]; then
  if python3 tests/conformance/run.py >/dev/null 2>&1; then pass "python3 tests/conformance/run.py"; else bad "python3 tests/conformance/run.py"; python3 tests/conformance/run.py 2>&1 | tail -n 20 | sed 's/^/    /'; fi
else
  echo "SKIP  no tests/conformance/run.py in this tree"
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "CHECK: ALL GREEN"
  exit 0
else
  echo "CHECK: FAILURES PRESENT (see FAIL lines above)"
  exit 1
fi

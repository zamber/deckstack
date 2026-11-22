#!/bin/sh
# deckstack single local gate. Runs on every PR (pr.sh review/merge) and in CI.
#   always:      sh tests/fixtures/validate.sh  (must print ALL CHECKS PASSED)
#                python3 tools/tickets.py validate (DAG + generated files in sync)
#   conditional: cargo fmt/clippy/test when Cargo.toml exists (DS-004)
#                conformance suite when tests/conformance/run.py exists (DS-018)
# Clear summary at the end; exit nonzero on any failure.
# Usage: sh tools/check.sh
set -u
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

fail=0
pass() { echo "PASS  $1"; }
bad()  { echo "FAIL  $1"; fail=1; }

echo "== tools/check.sh =="

echo "-- always: fixture gate (schemas + examples + regex regressions)"
if out=$(sh tests/fixtures/validate.sh 2>&1); then rc=0; else rc=$?; fi
echo "$out" | tail -n 2 | sed 's/^/    /'
if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "ALL CHECKS PASSED"; then
  pass "sh tests/fixtures/validate.sh -> ALL CHECKS PASSED"
else
  bad "sh tests/fixtures/validate.sh (rc=$rc)"; printf '%s\n' "$out" | tail -n 20 | sed 's/^/    /'
fi

echo "-- always: ticket board (DAG valid, generated files in sync)"
if out=$(python3 tools/tickets.py validate 2>&1); then
  pass "python3 tools/tickets.py validate"
  printf '%s\n' "$out" | sed 's/^/    /'
else
  bad "python3 tools/tickets.py validate"
  printf '%s\n' "$out" | sed 's/^/    /'
fi

echo "-- conditional: cargo workspace (runs once Cargo.toml exists, DS-004)"
if [ -f Cargo.toml ]; then
  if cargo fmt --check >/dev/null 2>&1; then pass "cargo fmt --check"; else bad "cargo fmt --check"; cargo fmt --check 2>&1 | head -n 20 | sed 's/^/    /'; fi
  if cargo clippy --workspace -- -D warnings >/dev/null 2>&1; then pass "cargo clippy --workspace -- -D warnings"; else bad "cargo clippy --workspace -- -D warnings"; cargo clippy --workspace -- -D warnings 2>&1 | tail -n 20 | sed 's/^/    /'; fi
  if cargo test --workspace >/dev/null 2>&1; then pass "cargo test --workspace"; else bad "cargo test --workspace"; cargo test --workspace 2>&1 | tail -n 20 | sed 's/^/    /'; fi
else
  echo "SKIP  no Cargo.toml yet (workspace skeleton lands with DS-004)"
fi

echo "-- conditional: conformance suite (runs once tests/conformance/run.py exists, DS-018)"
if [ -f tests/conformance/run.py ]; then
  PY=python3
  [ -x .venv/bin/python ] && PY=.venv/bin/python
  if "$PY" tests/conformance/run.py >/dev/null 2>&1; then pass "$PY tests/conformance/run.py"; else bad "$PY tests/conformance/run.py"; "$PY" tests/conformance/run.py 2>&1 | tail -n 20 | sed 's/^/    /'; fi
else
  echo "SKIP  no tests/conformance/run.py yet (lands with DS-018)"
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "CHECK: ALL GREEN"
  exit 0
else
  echo "CHECK: FAILURES PRESENT (see FAIL lines above)"
  exit 1
fi

#!/bin/sh
# deckstack one-command dev setup: python venv + test requirements + tools/check.sh.
# Offline-safe: the pip install is best-effort (warn only) — the gate itself needs
# no network. `sh packaging/bootstrap.sh` is a thin wrapper around this script.
# Usage: sh tools/bootstrap.sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

echo "==> python venv (.venv)"
if [ ! -d .venv ]; then
  python3 -m venv .venv
  echo "    created .venv"
else
  echo "    .venv already exists"
fi

echo "==> install tests/requirements.txt into .venv (best-effort; offline -> warn only)"
if .venv/bin/pip install -q --disable-pip-version-check --timeout 5 -r tests/requirements.txt 2>/dev/null; then
  echo "    installed pytest + jsonschema"
else
  echo "    warn: pip install failed (offline environment?). Not fatal: tools/check.sh only"
  echo "    needs the venv once tests/conformance/run.py exists (DS-018)."
fi

echo "==> cargo build (only when the workspace exists, DS-004)"
if [ -f Cargo.toml ]; then
  cargo build --workspace
else
  echo "    SKIP: no Cargo.toml yet (workspace skeleton lands with DS-004)"
fi

echo "==> gate: sh tools/check.sh"
sh tools/check.sh

echo "==> bootstrap OK"
echo "    next: python3 tools/tickets.py ready   # pick a ticket"
echo "          sh tools/wt.sh new <DS-XXX> --owner <your-name>"

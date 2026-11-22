#!/bin/sh
# deckstack phase-1 bootstrap. Executable form of docs/50-repo-topology.md §5.
# Usage: sh packaging/bootstrap.sh
set -eu
cd "$(dirname "$0")/.."

echo "==> cargo build --workspace"
cargo build --workspace

echo "==> schema + fixture gate"
sh tests/fixtures/validate.sh

echo "==> python test deps (optional; conformance suite)"
if command -v python3 >/dev/null 2>&1; then
  python3 -m venv .venv
  . .venv/bin/activate
  pip install -q -r tests/requirements.txt || echo "warn: pip install failed; conformance suite needs: pytest jsonschema"
else
  echo "warn: python3 not found; skipping venv (cargo tests + fixture gate still ran)"
fi

echo "==> done. Next:"
echo "    ds-vdev --socket /tmp/vdev.sock &"
echo "    ds daemon start --device virtual:/tmp/vdev.sock"
echo "    ds set /controls/keys/0/color '#3cc86e' && ds get /controls/keys/0 --json"
echo "    cargo test --workspace && python tests/conformance/run.py"

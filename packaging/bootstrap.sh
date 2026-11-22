#!/bin/sh
# Backward-compatible entry point — the real bootstrap lives in tools/bootstrap.sh
# (venv + requirements + tools/check.sh). See AGENTS.md.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec sh "$ROOT/tools/bootstrap.sh" "$@"

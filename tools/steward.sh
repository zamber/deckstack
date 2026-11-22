#!/bin/sh
# Launch the deckstack steward agent (agents/deckstack-steward.md) with vibe.
#   sh tools/steward.sh "triage the ready set and review open PRs"
#   sh tools/steward.sh --help
# If vibe is unavailable (or no task is given), prints the exact command instead.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

AGENT="$ROOT/agents/deckstack-steward.md"
CMD="vibe --workdir $ROOT --system-prompt-file $AGENT -p \"<task>\""

task=
case "${1:-}" in
  -h|--help|help|"")
    cat <<EOF
usage: sh tools/steward.sh "<task for the steward agent>"

example:
  sh tools/steward.sh "triage the ready set; review and merge any approved PRs"

runs (headless):
  $CMD
EOF
    exit 0
    ;;
  *) task=$1 ;;
esac

if [ ! -f "$AGENT" ]; then
  echo "steward.sh: missing $AGENT" >&2
  exit 1
fi

if command -v vibe >/dev/null 2>&1; then
  echo "steward.sh: launching vibe with $AGENT"
  exec vibe --workdir "$ROOT" --system-prompt-file "$AGENT" -p "$task"
else
  echo "steward.sh: 'vibe' not found on PATH — run the steward manually with:"
  echo
  echo "  $CMD"
  echo
  echo "(replace <task> with: $task)"
  exit 0
fi

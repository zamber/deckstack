#!/bin/sh
# Launch the deckstack steward agent (agents/deckstack-steward.md) with vibe.
#   sh tools/steward.sh "triage the ready set and review open PRs"
#   sh tools/steward.sh --help
# The steward agent runs with DS_STEWARD=1 exported: tools/pr.sh review/merge are
# steward-only and refuse to run without it (and refuse self-approval), so only the
# steward launched from here can approve PRs and merge to main.
# If vibe is unavailable (or no task is given), prints the exact command instead.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

AGENT="$ROOT/agents/deckstack-steward.md"
CMD="DS_STEWARD=1 vibe --workdir $ROOT --system-prompt-file $AGENT -p \"<task>\""

task=
case "${1:-}" in
  -h|--help|help|"")
    cat <<EOF
usage: sh tools/steward.sh "<task for the steward agent>"

example:
  sh tools/steward.sh "triage the ready set; review and merge any approved PRs"

runs (headless), with DS_STEWARD=1 exported so pr.sh review/merge accept the steward:
  $CMD

manual equivalent (steward shell):
  DS_STEWARD=1 sh tools/pr.sh review DS-XXX --approve   # or --request-changes "reason"
  DS_STEWARD=1 sh tools/pr.sh merge DS-XXX
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
  echo "steward.sh: launching vibe with $AGENT (DS_STEWARD=1)"
  DS_STEWARD=1 exec vibe --workdir "$ROOT" --system-prompt-file "$AGENT" -p "$task"
else
  echo "steward.sh: 'vibe' not found on PATH — run the steward manually with:"
  echo
  echo "  $CMD"
  echo
  echo "(replace <task> with: $task)"
  exit 0
fi

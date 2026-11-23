"""deckstack — stdlib-only Python helper for agents driving the deck via `ds`.

Thin wrapper over the `ds` CLI (docs/contracts/cli.md): one subprocess per call,
argv-only, never shell=True. Optional sugar — the `ds` CLI alone is sufficient
for every capability (R1). See README.md for the API surface and discovery.
"""

from .client import DeckClient, WatchStream
from .errors import DeckError, EXIT_CODE_MEANING, EXIT_CODE_TO_ERROR

__all__ = [
    "DeckClient",
    "WatchStream",
    "DeckError",
    "EXIT_CODE_MEANING",
    "EXIT_CODE_TO_ERROR",
]

__version__ = "0.1.0"

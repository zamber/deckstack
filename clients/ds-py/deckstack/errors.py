"""Structured errors for the deckstack Python helper.

Exit codes are the stable CLI contract of docs/contracts/cli.md section 3:

    0 ok | 1 usage/schema (E_SCHEMA) | 2 E_PATH | 3 E_CONFLICT |
    4 E_AUTH/E_SCOPE/E_CAP | 5 E_LIMIT | 6 E_UNREACHABLE | 7 E_DEVICE |
    8 E_INTERNAL/E_VERSION

Every raised :class:`DeckError` carries the process exit code and renders the
``[exit N]`` suffix the human CLI prints (docs/contracts/cli.md section 5).
"""

import re

# exit code -> IPC error code name (docs/contracts/ipc.md section 4 / cli.md section 3)
EXIT_CODE_TO_ERROR = {
    1: "E_SCHEMA",
    2: "E_PATH",
    3: "E_CONFLICT",
    4: "E_AUTH",  # E_SCOPE / E_CAP also map to exit 4
    5: "E_LIMIT",
    6: "E_UNREACHABLE",
    7: "E_DEVICE",
    8: "E_INTERNAL",  # E_VERSION also maps to exit 8
}

EXIT_CODE_MEANING = {
    0: "ok",
    1: "usage/schema error (E_SCHEMA)",
    2: "path not found / bad syntax (E_PATH)",
    3: "rev conflict (E_CONFLICT)",
    4: "auth/scope/capability (E_AUTH/E_SCOPE/E_CAP)",
    5: "limit (E_LIMIT)",
    6: "daemon unreachable (E_UNREACHABLE)",
    7: "device error (E_DEVICE)",
    8: "internal/version (E_INTERNAL/E_VERSION)",
}


class DeckError(Exception):
    """A failed `ds` invocation.

    Attributes:
        exit_code: the process exit code (0-8 per cli.md section 3), or None for
            client-side failures (binary missing, timeout) that never reached `ds`.
        code: the mapped IPC error code (``E_*``) or ``E_CLIENT`` when exit_code
            is None.
        stderr / stdout: the raw captured streams (stderr may be a JSON envelope
            when the call ran with --json; the token is never logged).
    """

    def __init__(self, message, exit_code=None, stderr="", stdout=""):
        self.exit_code = exit_code
        self.code = EXIT_CODE_TO_ERROR.get(exit_code, "E_CLIENT") if exit_code else "E_CLIENT"
        self.stderr = stderr
        self.stdout = stdout
        # the human CLI already prints the suffix; strip it so we never double it
        message = re.sub(r"\s*\[exit \d+\]\s*$", "", message or "")
        suffix = "  [exit %d]" % exit_code if exit_code is not None else ""
        super().__init__(message.rstrip() + suffix)

    @property
    def meaning(self):
        return EXIT_CODE_MEANING.get(self.exit_code, "client-side failure")

    def __repr__(self):
        return "DeckError(exit_code=%r, code=%r, message=%r)" % (
            self.exit_code, self.code, str(self))

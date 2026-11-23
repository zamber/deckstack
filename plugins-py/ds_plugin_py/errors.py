"""Error codes and error envelopes for the plugin side of the wire protocol.

The codes are exactly the stable control-plane enum of ``schemas/ipc-message.json``
(``$defs.error``): the plugin->host ``error`` object and the ``ok:false`` reply payload
use the same spellings, so one mapping table serves the whole deckstack surface.

Mirror of ``crates/ds-plugin-sdk/src/error.rs`` (DS-051) — same wire contract.
"""

from __future__ import annotations

import enum


class ErrorCode(enum.Enum):
    """The stable error codes (``schemas/ipc-message.json`` ``$defs.error``)."""

    E_AUTH = "E_AUTH"
    E_SCOPE = "E_SCOPE"
    E_PATH = "E_PATH"
    E_SCHEMA = "E_SCHEMA"
    E_CONFLICT = "E_CONFLICT"
    E_LIMIT = "E_LIMIT"
    E_CAP = "E_CAP"
    E_UNREACHABLE = "E_UNREACHABLE"
    E_DEVICE = "E_DEVICE"
    E_VERSION = "E_VERSION"
    E_INTERNAL = "E_INTERNAL"

    @classmethod
    def parse(cls, s: str) -> "ErrorCode | None":
        try:
            return cls(s)
        except ValueError:
            return None

    def exit_code(self) -> int:
        """The stable CLI exit code for this error (``docs/contracts/cli.md`` §3)."""
        return {
            ErrorCode.E_AUTH: 4,
            ErrorCode.E_SCOPE: 4,
            ErrorCode.E_CAP: 4,
            ErrorCode.E_PATH: 2,
            ErrorCode.E_SCHEMA: 1,
            ErrorCode.E_CONFLICT: 3,
            ErrorCode.E_LIMIT: 5,
            ErrorCode.E_UNREACHABLE: 6,
            ErrorCode.E_DEVICE: 7,
            ErrorCode.E_VERSION: 8,
            ErrorCode.E_INTERNAL: 8,
        }[self]


class PluginError(Exception):
    """One error object as it appears on the wire: ``{"code":…, "message":…}``.

    Handlers raise this to answer an invoke/event with a typed error reply; any other
    exception is mapped to ``E_INTERNAL`` by the serve loop.
    """

    def __init__(self, code: ErrorCode | str, message: str):
        if isinstance(code, str):
            code = ErrorCode.parse(code)
            if code is None:
                code = ErrorCode.E_INTERNAL
        self.code = code
        self.message = message
        super().__init__("%s: %s" % (code.value, message))

    # -- constructors mirroring the Rust SDK ---------------------------------
    def __eq__(self, other):
        return (isinstance(other, PluginError)
                and self.code == other.code and self.message == other.message)

    def __hash__(self):
        return hash((self.code, self.message))

    @classmethod
    def schema(cls, message: str) -> "PluginError":
        return cls(ErrorCode.E_SCHEMA, message)

    @classmethod
    def cap(cls, message: str) -> "PluginError":
        return cls(ErrorCode.E_CAP, message)

    @classmethod
    def limit(cls, message: str) -> "PluginError":
        return cls(ErrorCode.E_LIMIT, message)

    @classmethod
    def unreachable(cls, message: str) -> "PluginError":
        return cls(ErrorCode.E_UNREACHABLE, message)

    @classmethod
    def internal(cls, message: str) -> "PluginError":
        return cls(ErrorCode.E_INTERNAL, message)

    # -- wire ----------------------------------------------------------------
    def to_dict(self) -> dict:
        return {"code": self.code.value, "message": self.message}

    @classmethod
    def from_dict(cls, v) -> "PluginError | None":
        if not isinstance(v, dict):
            return None
        code = ErrorCode.parse(v.get("code") or "")
        message = v.get("message")
        if code is None or not isinstance(message, str):
            return None
        return cls(code, message)


class ServeError(Exception):
    """Fatal, top-level errors of :func:`ds_plugin_py.serve`: the plugin could not run
    the protocol at all (bad manifest, wrong first message, protocol mismatch, I/O
    error). Reported on stderr with the documented exit code; the host's watchdog
    treats them like a crash."""

    MANIFEST = "manifest"
    EXPECTED_HELLO = "expected_hello"
    VERSION = "version"
    UNEXPECTED_EOF = "unexpected_eof"
    IO = "io"

    def __init__(self, kind: str, message: str, got: int | None = None, want: int | None = None):
        self.kind = kind
        self.message = message
        self.got = got
        self.want = want
        super().__init__(message)

    def exit_code(self) -> int:
        if self.kind == self.MANIFEST:
            return ErrorCode.E_SCHEMA.exit_code()
        if self.kind == self.EXPECTED_HELLO:
            return ErrorCode.E_SCHEMA.exit_code()
        if self.kind == self.VERSION:
            return ErrorCode.E_VERSION.exit_code()
        if self.kind == self.UNEXPECTED_EOF:
            return ErrorCode.E_UNREACHABLE.exit_code()
        return ErrorCode.E_INTERNAL.exit_code()

"""The bounded host-mediated HTTP helper (S4/M5, ``docs/contracts/plugins.md`` §2).

Plugins **never open sockets**: :func:`ds_plugin_py.http` sends
``{"op":"request","kind":"net",…}`` to the daemon, which resolves the host, matches it
against the plugin's granted ``net:<glob>`` and ``/policy/net/allow_hosts``, performs
the request itself, and replies ``{"op":"reply","ok":true,"status":200,"body":"…"}``.
The SDK bounds what it will ask for (URL length, method, headers, body, reply size) so
a buggy plugin cannot ask the host for something outside the documented envelope.

Mirror of ``crates/ds-plugin-sdk/src/http.rs`` (DS-051).
"""

from __future__ import annotations

import json
from typing import Any, Optional

from .errors import ErrorCode, PluginError

#: Max length of the request URL (chars). The host additionally caps its own request size.
MAX_NET_URL_CHARS = 2048

#: Max length of the request body (bytes of its JSON encoding).
MAX_NET_BODY_BYTES = 1024 * 1024

#: Max number of request headers.
MAX_NET_HEADERS = 16

#: Max accepted reply body (chars). The host caps bodies at its own bound; the SDK
#: refuses anything beyond this with ``E_LIMIT``.
MAX_NET_REPLY_CHARS = 4 * 1024 * 1024

#: HTTP methods the SDK will send. The host may allowlist further.
ALLOWED_METHODS = ("GET", "HEAD", "POST", "PUT", "PATCH", "DELETE")


class HttpResponse:
    """A host-mediated HTTP response."""

    def __init__(self, status: int, body: str):
        self.status = status
        self.body = body

    def body_json(self) -> Optional[Any]:
        """The body parsed as JSON, if it is valid JSON."""
        try:
            return json.loads(self.body)
        except (ValueError, TypeError):
            return None

    # pythonic alias
    json = body_json

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return "HttpResponse(status=%r, body=%r)" % (self.status, self.body[:80])


def check_request(
    method: str,
    url: str,
    body: Any = None,
    headers: Optional[dict] = None,
) -> str:
    """Validate method/URL/headers/body and return the normalized (uppercase) method."""
    method = method.upper()
    if method not in ALLOWED_METHODS:
        raise PluginError(
            ErrorCode.E_SCHEMA,
            "http method %r is not one of %s" % (method, list(ALLOWED_METHODS)),
        )
    if not url or "://" not in url:
        raise PluginError.schema("http url %r is not absolute" % (url,))
    if len(url) > MAX_NET_URL_CHARS:
        raise PluginError.limit("http url longer than %d chars" % MAX_NET_URL_CHARS)
    if body is not None:
        encoded = json.dumps(body, separators=(",", ":"))
        if len(encoded.encode("utf-8")) > MAX_NET_BODY_BYTES:
            raise PluginError.limit(
                "http body larger than %d bytes" % MAX_NET_BODY_BYTES
            )
    if headers is not None and len(headers) > MAX_NET_HEADERS:
        raise PluginError.limit("more than %d http headers" % MAX_NET_HEADERS)
    return method


def check_reply_body(body: str) -> None:
    if len(body) > MAX_NET_REPLY_CHARS:
        raise PluginError.limit(
            "net reply body larger than %d chars" % MAX_NET_REPLY_CHARS
        )

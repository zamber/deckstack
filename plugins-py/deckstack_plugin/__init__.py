"""Alias package for :mod:`ds_plugin_py`.

The contract (``docs/contracts/plugins.md`` §7/§8) and ``repos.toml`` pin the import
name ``ds_plugin_py`` (PyPI distribution ``ds-plugin-py``); this alias exists so code
written against the ``deckstack_plugin`` spelling works unchanged. It re-exports the
same objects (the action registry is shared), so the two spellings are fully
interchangeable.
"""

from ds_plugin_py import *  # noqa: F401,F403
from ds_plugin_py import (  # noqa: F401
    MAX_LINE_BYTES,
    PROTOCOL_VERSION,
    ErrorCode,
    HttpResponse,
    Manifest,
    Plugin,
    PluginError,
    ServeError,
    Sdk,
    action,
    build_manifest,
    compat,
    emit,
    get_state,
    http,
    http_request,
    log,
    on_event,
    serve,
    set,
    set_state,
    sleep,
    validate_params,
    __version__,
)

__all__ = [n for n in dir() if not n.startswith("_")]

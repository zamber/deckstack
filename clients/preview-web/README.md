# clients/preview-web — static preview webapp (ticket DS-121)

A single-page, stdlib-only, **no-build-step** webapp that visualizes the deck as
the daemon sees it: the base connector grid with each module at its position
(flat 6×4 vs vertical 4×6, DS-112), the control surface, and — on hover/click of a
control — the plugin actions bound to it (name, params schema, capabilities) from
the action-metadata aggregate.

It is a **separate client app**, not part of the daemon: nothing in `crates/` serves
this HTML, and the no-UI scope of `docs/00-scope.md` delta 4 / ADR-014 is untouched.
It only *consumes* the read-only `/preview` debug surface merged in DS-113
(`docs/contracts/preview.md`): `GET /preview/api/state`,
`GET /preview/api/action-metadata`, and the `GET /preview/ws` live stream on the
loopback adapter (`docs/contracts/ipc.md` §1.1 — `127.0.0.1` only, bearer token,
loopback `Host` check). The app never sends a mutating op.

## Layout

```
clients/preview-web/
  index.html   the page (loads config.js, app.js, style.css)
  app.js       vanilla JS + SVG rendering, WS/HTTP client
  style.css    dark theme
  config.js    optional static config (window.PREVIEW_WEB_CONFIG)
  README.md    this file
```

## Run

Start the daemon with the debug surface enabled (default-off otherwise):

```sh
ds-vdev --socket /tmp/vdev.sock &
ds-daemon run --device virtual:/tmp/vdev.sock --preview
cat ~/.local/state/deckstack/http.port   # loopback port while --preview runs
cat ~/.local/state/deckstack/token       # install token
```

Serve the app with any static server and open it in a browser:

```sh
cd clients/preview-web
python3 -m http.server 8080
# open http://127.0.0.1:8080/?port=<http.port>&token=<token>
```

Config precedence: `?token=…&port=…` query params → `config.js` → the connection
panel in the page. Opening `index.html` directly via `file://` also works.

**Transport note:** the daemon sends no CORS headers on `/preview`, so when the
app is served from a different origin (any static server, or `file://`) the
browser blocks the HTTP `fetch` reads; the app then falls back to the
`/preview/ws?token=…` WebSocket, whose `preview.snapshot` frame carries the same
`{rev, state, actions}` document plus live `event` frames (the daemon accepts
`?token=` on the WS upgrade precisely because browsers cannot set handshake
headers — ADR-019).

## Defensive reads (DS-112 in flight)

`/devices/<serial>.position` and `.orientation` are landing on a separate branch
(DS-112). The app reads them **only if present**; otherwise modules are placed in
serial order on the connector grid and the base orientation defaults to flat.

## Test

Black-box smoke test, no browser automation (boots ds-vdev + `ds-daemon
--preview`, serves this directory, asserts the app files and the API shapes it
needs):

```sh
python3 tests/conformance/run.py -k preview_web
```

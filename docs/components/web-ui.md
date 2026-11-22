# Component: ds-web (web UI shell — phase 7, deliberately thin)

R1: no UI is required for any capability; a GUI is only another client. ds-web is a static
HTML/JS page served by the daemon's optional HTTP adapter (127.0.0.1, bearer token, Host
check — `docs/20-security.md` S6) that speaks WS/JSON to the daemon. All logic lives in
the daemon/CLI; the page only renders state and sends `set`/`bind` calls.

- **Language:** vanilla JS + WebSocket, no framework (boring, no build step in phase 7).
- **Public surface:** `GET /` (page), `GET /api/...` and `WS /ws` proxying the daemon
  protocol 1:1; no server-side state.
- **Invariants:** the UI can do nothing the CLI cannot (same token scopes); no privileged
  routes; CSP `default-src 'self'`; Host header must be 127.0.0.1/localhost
  (precedent `hubdeck/src/hubdeck/gui/server.py:601`).
- **Failure modes:** daemon down → page shows status + the exact `ds daemon start`
  command; WS drop → auto-reconnect with backoff, subscription re-armed with last rev.
- **Dependencies:** none beyond the daemon's HTTP/WS adapter.
- **Test command:** Playwright smoke in CI against ds-vdev: load page, set a key colour
  from the UI, assert `ds get` sees it.
- **Minimum working example:** phase 7 slice = a single page listing
  `/controls/keys/*` with a colour picker per key.

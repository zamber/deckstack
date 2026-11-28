// DS-121 preview webapp — optional static configuration.
// URL query params (?token=...&port=...) win over this file.
// The port is published by the daemon in <data-dir>/http.port while the
// /preview surface runs; the token lives in <data-dir>/token.
window.PREVIEW_WEB_CONFIG = {
  port: null,
  token: ""
};

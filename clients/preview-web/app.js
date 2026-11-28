// DS-121 — static preview webapp: deck map (module position/orientation) +
// plugin action mappings, consuming the daemon's read-only /preview surface
// (docs/contracts/preview.md). Vanilla JS + SVG, no dependencies, no build step.
// Read-only: this app only ever uses GET endpoints and the WS read ops
// (ping/get/subscribe/unsubscribe).
(function () {
  "use strict";

  var SVGNS = "http://www.w3.org/2000/svg";
  var CONNECTORS = 26; // any module fits any of the 26 base connectors

  var q = new URLSearchParams(location.search);
  var cfg = window.PREVIEW_WEB_CONFIG || {};
  var port = parseInt(q.get("port") || cfg.port, 10);
  var token = q.get("token") || cfg.token || "";

  var state = { rev: null, controls: {}, bindings: [], plugins: {}, devices: {} };
  var actions = null;
  var ws = null;
  var httpOk = false;
  var selection = null; // {type:"control",path} | {type:"device",serial}
  var renderTimer = null;

  function apiBase() { return "http://127.0.0.1:" + port; }

  function getJSON(path) {
    return fetch(apiBase() + path, {
      headers: { "Authorization": "Bearer " + token }
    }).then(function (r) {
      if (!r.ok) { throw new Error(path + " -> HTTP " + r.status); }
      return r.json();
    });
  }

  function setConn(up, note) {
    var c = document.getElementById("conn");
    c.textContent = up ? "connected" : (note || "not connected");
    c.className = "conn " + (up ? "conn-up" : "conn-down");
  }

  function setRev() {
    document.getElementById("rev").textContent =
      state.rev === null ? "" : "rev " + state.rev + (httpOk ? " · http+ws" : " · ws only");
  }

  function connect() {
    if (!port || !token) {
      document.getElementById("connect-panel").classList.remove("hidden");
      return;
    }
    document.getElementById("connect-panel").classList.add("hidden");
    Promise.all([getJSON("/preview/api/state"), getJSON("/preview/api/action-metadata")])
      .then(function (res) {
        httpOk = true;
        state = res[0];
        actions = res[1];
        setConn(true);
        renderAll();
        openWs();
      })
      .catch(function () {
        // Cross-origin static server: no CORS headers on /preview, so the HTTP
        // reads are blocked — the WS snapshot carries the same {state, actions}.
        httpOk = false;
        openWs();
      });
  }

  function openWs() {
    var url = "ws://127.0.0.1:" + port + "/preview/ws?token=" + encodeURIComponent(token);
    ws = new WebSocket(url);
    ws.onopen = function () {
      ["/controls/*", "/devices/*", "/bindings/*", "/plugins/*"].forEach(function (p, i) {
        ws.send(JSON.stringify({ id: i + 1, op: "subscribe", path: p }));
      });
    };
    ws.onmessage = function (m) {
      var msg;
      try { msg = JSON.parse(m.data); } catch (e) { return; }
      if (msg.op === "preview.hello") { setConn(true); return; }
      if (msg.op === "preview.snapshot") {
        state = msg.state || state;
        actions = msg.actions || actions;
        setConn(true);
        renderAll();
        return;
      }
      if (msg.op === "event") {
        if (msg.path && Object.prototype.hasOwnProperty.call(msg, "value")) {
          setPath(state, msg.path, msg.value);
        }
        if (typeof msg.rev === "number") { state.rev = msg.rev; }
        renderSoon();
        return;
      }
      if (msg.op === "error") { setConn(false, "error: " + (msg.error && msg.error.code)); }
    };
    ws.onclose = function () {
      setConn(false, "disconnected — retrying");
      setTimeout(function () { if (port && token) { openWs(); } }, 3000);
    };
    ws.onerror = function () { setConn(false, "connection failed"); };
  }

  function setPath(obj, path, value) {
    var segs = path.split("/").filter(Boolean);
    var o = obj;
    for (var i = 0; i < segs.length - 1; i++) {
      if (typeof o[segs[i]] !== "object" || o[segs[i]] === null) { o[segs[i]] = {}; }
      o = o[segs[i]];
    }
    o[segs[segs.length - 1]] = value;
  }

  // ---- topic grammar (docs/contracts/actions.md §2): '/' and '.' both split
  // segments; a pattern is a segment-wise prefix of the topic, '*' = one segment.
  function segs(t) { return t.split(/[./]/).filter(Boolean); }
  function matchPattern(pSegs, tSegs) {
    if (pSegs.length > tSegs.length) { return false; }
    for (var i = 0; i < pSegs.length; i++) {
      if (pSegs[i] !== "*" && pSegs[i] !== tSegs[i]) { return false; }
    }
    return true;
  }
  function controlHasBinding(controlTopics, pattern) {
    var p = segs(pattern);
    return controlTopics.some(function (t) {
      var s = segs(t);
      return matchPattern(p, s) || matchPattern(s, p);
    });
  }

  // ---- orientation (DS-112, defensive: field may not exist yet) ------------
  function baseOrientation() {
    var devs = state.devices || {};
    var found = null;
    Object.keys(devs).forEach(function (serial) {
      var d = devs[serial] || {};
      var o = d.orientation;
      if (o === undefined || o === null) { return; }
      if (typeof o === "object") { o = o.value !== undefined ? o.value : JSON.stringify(o); }
      o = String(o).toLowerCase();
      if (o === "00" || o === "upright" || o === "vertical") { found = "vertical"; }
      else if (o === "01" || o === "flat") { found = found || "flat"; }
    });
    return found || "flat";
  }

  function svgEl(tag, attrs, parent) {
    var e = document.createElementNS(SVGNS, tag);
    if (attrs) { Object.keys(attrs).forEach(function (k) { e.setAttribute(k, attrs[k]); }); }
    if (parent) { parent.appendChild(e); }
    return e;
  }
  function svgText(parent, x, y, str, cls) {
    var t = svgEl("text", { x: x, y: y, "class": cls || "" }, parent);
    t.textContent = str;
    return t;
  }

  // ---- deck map --------------------------------------------------------------
  function renderDeck() {
    var svg = document.getElementById("deck");
    while (svg.firstChild) { svg.removeChild(svg.firstChild); }
    var vertical = baseOrientation() === "vertical";
    document.getElementById("orientation").textContent =
      "orientation: " + (vertical ? "vertical (4×6)" : "flat (6×4)");
    var cols = vertical ? 4 : 6;
    var rows = Math.ceil(CONNECTORS / cols);
    var cw = 92, ch = 46, gap = 6;
    svg.setAttribute("viewBox", "0 0 " + (cols * (cw + gap) + gap) + " " + (rows * (ch + gap) + gap));

    var devs = state.devices || {};
    var serials = Object.keys(devs).sort();
    var used = {};
    var placements = [];
    serials.forEach(function (serial, i) {
      var d = devs[serial] || {};
      var pos = (typeof d.position === "number" && isFinite(d.position)) ? d.position : null;
      if (pos === null || used[pos] || pos < 0 || pos >= CONNECTORS) {
        pos = 0;
        while (used[pos]) { pos++; }
      }
      used[pos] = true;
      placements.push({ serial: serial, pos: pos, dev: d, fallback: typeof d.position !== "number" });
    });

    var taken = {};
    placements.forEach(function (p) { taken[p.pos] = p; });
    for (var c = 0; c < CONNECTORS; c++) {
      var col = c % cols, row = Math.floor(c / cols);
      var x = gap + col * (cw + gap), y = gap + row * (ch + gap);
      var g = svgEl("g", { "class": "cell" }, svg);
      svgEl("rect", { x: x, y: y, width: cw, height: ch, rx: 6 }, g);
      svgText(g, x + 6, y + 14, "#" + c, "pos");
      var p = taken[c];
      if (p) {
        g.setAttribute("class", "cell occupied" + (p.dev.connection === "disconnected" ? " disconnected" : ""));
        svgText(g, x + 6, y + 28, p.dev.kind || "?", "kind");
        svgText(g, x + 6, y + 40, p.serial + (p.fallback ? " (serial order)" : ""), "serial");
        if (p.dev.slot) { svgText(g, x + cw - 6, y + 14, "slot " + p.dev.slot, "pos"); }
        g.addEventListener("click", function () { select({ type: "device", serial: p.serial }); });
      }
    }
  }

  // ---- controls --------------------------------------------------------------
  function controlList() {
    var c = state.controls || {};
    var list = [];
    function add(path, label, topics, shape) {
      list.push({ path: path, label: label, topics: topics, shape: shape });
    }
    if (c.panel) { add("/controls/panel", "panel", ["deck/control/panel", "deck/control/keys"], "panel"); }
    if (c.keys) {
      for (var i = 0; i <= 14; i++) {
        if (c.keys[String(i)]) { add("/controls/keys/" + i, "key " + i, ["deck/control/keys/" + i], "key"); }
      }
    }
    if (c.dial) { add("/controls/dial", "dial", ["deck/control/dial"], "dial"); }
    if (c.knobs) { for (var k = 0; k <= 2; k++) { if (c.knobs[String(k)]) { add("/controls/knobs/" + k, "knob " + k, ["deck/control/knobs/" + k], "knob"); } } }
    if (c.faders) { for (var f = 0; f <= 4; f++) { if (c.faders[String(f)]) { add("/controls/faders/" + f, "fader " + f, ["deck/control/faders/" + f], "fader"); } } }
    if (c.rollers) { for (var r = 0; r <= 1; r++) { if (c.rollers[String(r)]) { add("/controls/rollers/" + r, "roller " + r, ["deck/control/rollers/" + r], "roller"); } } }
    if (c.base) { add("/controls/base", "base led", ["deck/control/base"], "base"); }
    return list;
  }

  function renderControls() {
    var svg = document.getElementById("controls");
    while (svg.firstChild) { svg.removeChild(svg.firstChild); }
    var W = 660, H = 300;
    svg.setAttribute("viewBox", "0 0 " + W + " " + H);
    var list = controlList();
    var x = 10, y = 34;
    var kw = 56, kh = 40, gap = 6;
    list.forEach(function (ctl) {
      var w = ctl.shape === "panel" ? 150 : ctl.shape === "fader" ? 30 : ctl.shape === "dial" ? 64 : kw;
      var h = ctl.shape === "panel" ? 120 : ctl.shape === "fader" ? 110 : ctl.shape === "dial" ? 64 : kh;
      if (x + w > W - 10) { x = 10; y += 130; }
      var g = svgEl("g", { "class": "ctl" + (selection && selection.type === "control" && selection.path === ctl.path ? " selected" : ""), "data-path": ctl.path }, svg);
      if (ctl.shape === "dial" || ctl.shape === "knob") {
        svgEl("circle", { cx: x + w / 2, cy: y + h / 2, r: Math.min(w, h) / 2 - 2 }, g);
      } else {
        svgEl("rect", { x: x, y: y, width: w, height: h, rx: ctl.shape === "fader" ? 4 : 8 }, g);
      }
      svgText(g, x + w / 2, y + h / 2 + 3, ctl.label, "label").setAttribute("text-anchor", "middle");
      g.addEventListener("click", function () { select({ type: "control", path: ctl.path }); });
      g.addEventListener("mouseenter", function () { select({ type: "control", path: ctl.path }); });
      x += w + gap;
    });
  }

  // ---- inspector -------------------------------------------------------------
  function getPath(path) {
    return path.split("/").filter(Boolean).reduce(function (o, s) {
      return (o && typeof o === "object") ? o[s] : undefined;
    }, state);
  }

  function coreDoc(do_) {
    if (!actions || !actions.core) { return null; }
    for (var i = 0; i < actions.core.length; i++) { if (actions.core[i].do === do_) { return actions.core[i]; } }
    return null;
  }

  function stepHtml(step) {
    var div = document.createElement("div");
    div.className = "step";
    var head = document.createElement("span");
    head.innerHTML = "";
    var doSpan = document.createElement("span");
    doSpan.className = "do";
    doSpan.textContent = step.do || "?";
    head.appendChild(doSpan);
    head.appendChild(document.createTextNode(" " + JSON.stringify(step)));
    div.appendChild(head);
    var meta = document.createElement("div");
    meta.className = "meta";
    if (step.do === "plugin" && actions && actions.plugins) {
      var plugin = null, action = null;
      actions.plugins.forEach(function (p) {
        if (p.id === step.id) {
          plugin = p;
          (p.actions || []).forEach(function (a) { if (a.id === step.action) { action = a; } });
        }
      });
      if (plugin) {
        meta.textContent = "plugin " + plugin.name + " v" + plugin.version +
          " · capabilities: " + (plugin.capabilities || []).join(", ");
        div.appendChild(meta);
        if (action) {
          var am = document.createElement("div");
          am.className = "meta";
          am.textContent = "action " + (action.name || action.id) + " — " + (action.description || "");
          div.appendChild(am);
          var pre = document.createElement("pre");
          pre.textContent = "params schema: " + JSON.stringify(action.params || {});
          div.appendChild(pre);
        }
        return div;
      }
      meta.textContent = "plugin " + step.id + " not registered";
    } else {
      var c = coreDoc(step.do);
      if (c) {
        meta.textContent = "capability: " + c.capability + " · args: " + Object.keys(c.args || {}).join(", ") +
          " — " + c.description;
      }
    }
    div.appendChild(meta);
    return div;
  }

  function renderInspector() {
    var body = document.getElementById("inspector-body");
    body.innerHTML = "";
    if (!selection) {
      var p0 = document.createElement("p");
      p0.className = "hint";
      p0.textContent = "nothing selected — hover or click a control or module";
      body.appendChild(p0);
      return;
    }
    var title, val, ctl = null;
    if (selection.type === "device") {
      title = "/devices/" + selection.serial;
      val = (state.devices || {})[selection.serial];
    } else {
      title = selection.path;
      val = getPath(selection.path);
      ctl = controlList().filter(function (c) { return c.path === selection.path; })[0];
    }
    var h = document.createElement("h3");
    h.textContent = title;
    h.style.font = "12px ui-monospace, monospace";
    body.appendChild(h);
    var pre = document.createElement("pre");
    pre.textContent = JSON.stringify(val === undefined ? null : val, null, 1);
    body.appendChild(pre);
    if (selection.type === "device") { return; }
    var bound = (state.bindings || []).filter(function (b) {
      return ctl && controlHasBinding(ctl.topics, b.topic);
    });
    var label = document.createElement("div");
    label.className = "hint";
    label.textContent = bound.length ? bound.length + " bound action(s):" : "no bindings on this control's topics";
    body.appendChild(label);
    bound.forEach(function (b) {
      var div = document.createElement("div");
      div.className = "binding";
      var bid = document.createElement("div");
      bid.className = "bid";
      bid.textContent = b.id;
      var topic = document.createElement("div");
      topic.className = "topic";
      topic.textContent = b.topic + (b.when ? " when " + JSON.stringify(b.when) : "");
      div.appendChild(bid);
      div.appendChild(topic);
      (b.steps || []).forEach(function (s) { div.appendChild(stepHtml(s)); });
      body.appendChild(div);
    });
  }

  function select(sel) {
    selection = sel;
    renderControls();
    renderInspector();
  }

  function renderSoon() {
    if (renderTimer) { return; }
    renderTimer = setTimeout(function () { renderTimer = null; renderAll(); }, 60);
  }

  function renderAll() {
    setRev();
    renderDeck();
    renderControls();
    renderInspector();
  }

  document.getElementById("cfg-connect").addEventListener("click", function () {
    var p = parseInt(document.getElementById("cfg-port").value, 10);
    var t = document.getElementById("cfg-token").value.trim();
    if (p) { port = p; }
    if (t) { token = t; }
    connect();
  });

  if (port && token) {
    if (!q.get("port")) { document.getElementById("cfg-port").value = port; }
    if (!q.get("token")) { document.getElementById("cfg-token").value = token; }
    connect();
  } else {
    if (port) { document.getElementById("cfg-port").value = port; }
    if (token) { document.getElementById("cfg-token").value = token; }
    document.getElementById("connect-panel").classList.remove("hidden");
  }
})();

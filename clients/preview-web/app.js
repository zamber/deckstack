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
  var selection = null; // {type:"control",path} | {type:"device",serial}
  var renderTimer = null;

  function apiBase() { return "http://127.0.0.1:" + port; }

  function setConn(up, note) {
    var c = document.getElementById("conn");
    c.textContent = up ? "connected" : (note || "not connected");
    c.className = "conn " + (up ? "conn-up" : "conn-down");
  }

  function setRev() {
    document.getElementById("rev").textContent =
      state.rev === null ? "" : "rev " + state.rev + " · ws";
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

  function connect() {
    if (!port || !token) {
      document.getElementById("connect-panel").classList.remove("hidden");
      return;
    }
    document.getElementById("connect-panel").classList.add("hidden");
    // WS-only transport: the daemon's /preview sends hello + snapshot (state and
    // action-metadata) over the websocket, and websockets are not subject to CORS,
    // so this works from any static origin (no fetch, no preflight, no 401/CORS).
    openWs();
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
  // Physical connector anchors on the base slab (flat orientation, 6x4 cells;
  // vertical transposes to 4x6). Coordinates in cell units, from the SALON-001
  // hardware sweeps (docs/discovery/salon-001/README.md).
  var ANCHORS = {
    "top-left": [0.9, 0.0], "middle-top": [3.0, 0.0], "top-right": [5.1, 0.0],
    "bottom-left": [0.9, 4.0], "bottom-middle": [3.0, 4.0], "bottom-right": [5.1, 4.0],
    "left": [0.0, 2.0], "right": [6.0, 2.0]
  };
  // (module kind, slot) -> [anchor, rotation deg CCW], from the SALON-001 sweeps.
  var SLOT_MAP = {
    "knobs|1-2-3": ["top-left",0], "knobs|1-2-4": ["top-left",90],
    "knobs|3-1": ["middle-top",0], "knobs|1-1-1": ["middle-top",90], "knobs|3-3": ["middle-top",180],
    "knobs|3-4": ["top-right",0], "knobs|1-4-3": ["top-right",90], "knobs|1-4-1": ["top-right",180],
    "knobs|1-2-2": ["bottom-left",0], "knobs|4-4": ["bottom-left",90],
    "knobs|4-2": ["bottom-middle",0], "knobs|2-3": ["bottom-middle",90], "knobs|2-1": ["bottom-middle",180],
    "knobs|1-1-3": ["bottom-right",0], "knobs|1-3-4": ["bottom-right",90], "knobs|1-3-1": ["bottom-right",180],
    "keys|2-2": ["top-left",0], "keys|1-3-2": ["top-right",0], "keys|1-3-4": ["bottom-right",0],
    "keys|1-1-2": ["bottom-middle",0], "keys|2-3": ["bottom-left",0],
    "keys|1-2-2": ["left",90], "keys|4-1": ["left",90], "keys|4-2": ["right",90],
    "keys|2-4": ["right",90], "keys|1-4-1": ["right",270], "keys|3-3": ["left",270],
    "faders|1-2-2": ["right",0], "faders|4-1": ["right",0], "faders|4-2": ["middle-top",0],
    "faders|2-4": ["right",0], "faders|1-1-3": ["right",180], "faders|1-4-1": ["middle-top",180],
    "faders|3-3": ["top-left",270], "faders|1-1-1": ["top-right",270], "faders|1-4-3": ["top-right",270],
    "faders|1-3-4": ["bottom-right",270], "faders|1-1-2": ["bottom-middle",270], "faders|2-3": ["bottom-left",270],
    "rollers|1-2-1": ["left",0], "rollers|4-4": ["left",0], "rollers|4-3": ["left",0],
    "rollers|1-3-4": ["middle-top",0], "rollers|1-2-3": ["right",0], "rollers|3-1": ["right",0],
    "rollers|1-2-2": ["left",90], "rollers|4-1": ["right",90], "rollers|4-2": ["bottom-left",90],
    "rollers|3-3": ["bottom-middle",90], "rollers|1-4-1": ["bottom-right",90],
    "rollers|1-4-2": ["top-left",270], "rollers|1-3-1": ["top-right",270], "rollers|1-3-3": ["top-right",270],
    "rollers|1-1-4": ["bottom-right",270], "rollers|2-1": ["bottom-right",270], "rollers|3-2": ["bottom-left",270]
  };
  // module footprint in cell units: [w, h]
  var FOOTPRINT = { keys: [3.0, 1.1], dial: [1.5, 1.5], knobs: [2.0, 1.2], faders: [3.0, 1.0], rollers: [2.2, 0.9], base: [6, 4] };

  function baseOrientation() {
    var devs = state.devices || {};
    for (var k in devs) { if (/base$/.test(devs[k].kind) && devs[k].orientation) { return devs[k].orientation; } }
    return "horizontal";
  }

  function renderDeck() {
    var svg = document.getElementById("deck");
    while (svg.firstChild) { svg.removeChild(svg.firstChild); }
    var vertical = baseOrientation() === "vertical";
    document.getElementById("orientation").textContent =
      "orientation: " + (vertical ? "vertical (4×6)" : "flat (6×4)");
    var CW = 110, CH = 80, GAP = 8;
    var cols = vertical ? 4 : 6, rows = vertical ? 6 : 4;
    var W = cols * (CW + GAP) + GAP, H = rows * (CH + GAP) + GAP;
    svg.setAttribute("viewBox", "0 0 " + W + " " + H);

    // base slab
    var slab = svgEl("g", { "class": "base" }, svg);
    svgEl("rect", { x: GAP, y: GAP, width: cols * (CW + GAP) - GAP, height: rows * (CH + GAP) - GAP, rx: 18, "class": "slab" }, slab);
    svgText(slab, GAP + 12, GAP + 20, "MasterHub base " + (vertical ? "4×6" : "6×4"), "slab-label");

    var devs = state.devices || {};
    var serials = Object.keys(devs).sort();
    var usedAnchors = {};
    serials.forEach(function (serial) {
      var d = devs[serial] || {};
      if (/base$/.test(d.kind || "")) { return; }
      var key = d.kind + "|" + (d.slot || "");
      var mapped = SLOT_MAP[key];
      var anchor, rot;
      if (mapped) { anchor = mapped[0]; rot = mapped[1]; }
      else {
        // fallback: first free anchor in reading order
        var order = ["top-left","middle-top","top-right","left","right","bottom-left","bottom-middle","bottom-right"];
        anchor = order.find(function (a) { return !usedAnchors[a]; }) || "middle-top";
        rot = 0;
      }
      usedAnchors[anchor] = true;
      drawModule(svg, d, serial, anchor, rot, vertical, CW, CH, GAP);
    });
  }

  function drawModule(svg, d, serial, anchor, rot, vertical, CW, CH, GAP) {
    var cols = vertical ? 4 : 6, rows = vertical ? 6 : 4;
    var ax = GAP + ANCHORS[anchor][0] * (CW + GAP) + (ANCHORS[anchor][0] >= cols ? 0 : 0);
    var ay = GAP + ANCHORS[anchor][1] * (CH + GAP);
    if (vertical) { // transpose: swap cell coords
      var t = ANCHORS[anchor];
      ax = GAP + t[1] * (CW + GAP); ay = GAP + t[0] * (CH + GAP);
    }
    var fp = FOOTPRINT[d.kind] || [1.5, 1.0];
    var w = fp[0] * CW, h = fp[1] * CH;
    var disconnected = d.connection === "disconnected";
    var g = svgEl("g", {
      "class": "module m-" + d.kind + (disconnected ? " disconnected" : ""),
      transform: "translate(" + (ax - w / 2) + "," + (ay - h / 2) + ") rotate(" + (-rot) + " " + (w / 2) + " " + (h / 2) + ")"
    }, svg);
    svgEl("rect", { x: 0, y: 0, width: w, height: h, rx: 10, "class": "body" }, g);
    var label = d.kind + " · slot " + (d.slot || "?") + " · " + (rot ? rot + "°" : "0°");
    svgText(g, 8, 16, label, "mod-label");
    svgText(g, 8, h - 8, serial + (disconnected ? " · DISCONNECTED" : " · " + (d.connection || "connected")), "mod-status");
    // per-kind content: screen preview / control state
    var c = state.controls || {};
    if (d.kind === "keys") {
      var scr = svgEl("rect", { x: 6, y: 22, width: w - 12, height: h - 36, rx: 4, "class": "screen" }, g);
      var keys = c.keys || {};
      for (var i = 0; i < 15; i++) {
        var kx = 10 + (i % 5) * ((w - 20) / 5), ky = 26 + Math.floor(i / 5) * ((h - 42) / 3);
        var kd = keys[String(i)] || {};
        svgEl("rect", { x: kx, y: ky, width: (w - 20) / 5 - 3, height: (h - 42) / 3 - 3, rx: 2,
          fill: kd.color || "#20242c", "class": "key" }, g);
        if (kd.label) { svgText(g, kx + 3, ky + 11, String(kd.label).slice(0, 6), "key-label"); }
      }
    } else if (d.kind === "dial") {
      var dv = c.dial || {};
      svgEl("circle", { cx: w / 2, cy: (h + 14) / 2, r: Math.min(w, h) / 2 - 10, "class": "screen" }, g);
      if (dv.label) { svgText(g, w / 2, (h + 14) / 2 + 4, String(dv.label).slice(0, 10), "dial-label").setAttribute("text-anchor", "middle"); }
    } else if (d.kind === "knobs") {
      var kn = c.knobs || {};
      for (var k = 0; k < 3; k++) {
        var kc = kn[String(k)] || {};
        var cx = w * (k === 2 ? 0.5 : (k === 0 ? 0.28 : 0.72)), cy = h * (k === 2 ? 0.68 : 0.4);
        svgEl("circle", { cx: cx, cy: cy, r: Math.min(w, h) * 0.2, fill: kc.led || kc.color || "#3a4150", "class": "knob" }, g);
      }
    } else if (d.kind === "faders") {
      var fa = c.faders || {};
      for (var f = 0; f < 5; f++) {
        var fx = 12 + f * ((w - 24) / 5);
        svgEl("rect", { x: fx, y: 24, width: 6, height: h - 40, rx: 3, "class": "track" }, g);
        var v = (fa[String(f)] && typeof fa[String(f)].value === "number") ? fa[String(f)].value : 50;
        svgEl("rect", { x: fx - 2, y: 24 + (h - 40) * (1 - v / 100), width: 10, height: 8, rx: 2, "class": "handle" }, g);
      }
    } else if (d.kind === "rollers") {
      var ro = c.rollers || {};
      for (var r = 0; r < 2; r++) {
        var rc = ro[String(r)] || {};
        svgEl("rect", { x: 10, y: 26 + r * ((h - 36) / 2), width: w - 20, height: (h - 36) / 2 - 4, rx: 6,
          fill: rc.led || rc.color || "#3a4150", "class": "roller" }, g);
      }
    }
    g.addEventListener("click", function () { select({ type: "device", serial: serial }); });
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

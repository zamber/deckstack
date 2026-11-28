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
      ["/controls", "/devices", "/bindings", "/plugins", "/deck", "/app"].forEach(function (p, i) {
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
        if (!frameTimer) { frameTimer = setInterval(pollFrames, 400); pollFrames(); }
        return;
      }
      if (msg.op === "preview.frame") { handleFrame(msg); return; }
      if (msg.op === "event") { if (window.WSDEBUG) { console.log("ws event", msg.path, JSON.stringify(msg.value)); }
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
  // Faithful grid layout: the base is a cols x rows slab of unit cells (flat 6x4,
  // vertical 4x6). Each physical connector anchors a module's cell range; modules
  // snap inside the slab, sized by their footprint in cells, rotated about center.
  // Connector anchors + (kind, slot) -> rotation come from the SALON-001 sweeps
  // (docs/discovery/salon-001/README.md).
  var SLOT_ROT = {
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
    "rollers|1-1-4": ["bottom-right",270], "rollers|2-1": ["bottom-right",270], "rollers|3-2": ["bottom-left",270],
    "dial|1-1-3": ["bottom-right",0], "dial|1-3-4": ["bottom-right",0]
  };
  var ANCHOR_EDGE = { "top-left":"top", "top-mid-left":"top", "middle-top":"top", "top-mid-right":"top", "top-right":"top",
    "bottom-left":"bottom", "bottom-mid-left":"bottom", "bottom-middle":"bottom", "bottom-mid-right":"bottom", "bottom-right":"bottom",
    "left-top":"left", "left":"left", "left-bottom":"left", "right-top":"right", "right":"right", "right-bottom":"right" };
  // module footprint in cells [w,h] at rot 0 (flat grid)
  var FOOTPRINT = { keys: [4,3], dial: [2,2], knobs: [2,2], faders: [2,4], rollers: [1,4], base: [6,4] };

  function baseOrientation() {
    var devs = state.devices || {};
    for (var k in devs) { if (/base$/.test(devs[k].kind || "") && devs[k].orientation) { return devs[k].orientation; } }
    return "horizontal";
  }

  // Modules mount on the base PERIMETER, resting one cell onto the slab and
  // overhanging outward; the slab is the 6x4 (flat) / 4x6 (vertical) cell field.
  // Rects may lie outside the slab.
  function cellRect(anchor, rot, fp, cols, rows) {
    var w = fp[0], h = fp[1];
    if (rot === 90 || rot === 270) { var t = w; w = h; h = t; }
    var edge = ANCHOR_EDGE[anchor] || "top";
    var col, row;
    var tops = ["top-left","top-mid-left","middle-top","top-mid-right","top-right"];
    var bottoms = ["bottom-left","bottom-mid-left","bottom-middle","bottom-mid-right","bottom-right"];
    var lefts = ["left-top","left","left-bottom"];
    var rights = ["right-top","right","right-bottom"];
    if (edge === "top") { row = 1 - h; var i = tops.indexOf(anchor); col = Math.round((cols - w) * i / (tops.length - 1)); }
    else if (edge === "bottom") { row = rows - 1; var j = bottoms.indexOf(anchor); col = Math.round((cols - w) * j / (bottoms.length - 1)); }
    else if (edge === "left") { col = 1 - w; var k = lefts.indexOf(anchor); row = Math.round((rows - h) * k / (lefts.length - 1)); }
    else { col = cols - 1; var m = rights.indexOf(anchor); row = Math.round((rows - h) * m / (rights.length - 1)); }
    return { col: col, row: row, w: w, h: h };
  }

  function renderDeck() {
    var svg = document.getElementById("deck");
    while (svg.firstChild) { svg.removeChild(svg.firstChild); }
    var vertical = baseOrientation() === "vertical";
    document.getElementById("orientation").textContent =
      "orientation: " + (vertical ? "vertical (4×6)" : "flat (6×4)");
    var CW = 96, CH = 96, GAP = 8; // square cells
    var cols = vertical ? 4 : 6, rows = vertical ? 6 : 4;
    // canvas includes a one-module margin around the slab for overhanging modules
    var MW = 5, MH = 5; // margin in cells (max module extent)
    var W = (cols + 2 * MW) * (CW + GAP) + GAP, H = (rows + 2 * MH) * (CH + GAP) + GAP;
    var OX = MW * (CW + GAP) + GAP, OY = MH * (CH + GAP) + GAP; // slab origin
    svg.setAttribute("viewBox", "0 0 " + W + " " + H);

    var slab = svgEl("g", { "class": "base" }, svg);
    svgEl("rect", { x: OX, y: OY, width: cols * (CW + GAP) - GAP, height: rows * (CH + GAP) - GAP, rx: 18, "class": "slab" }, slab);
    for (var gy = 0; gy < rows; gy++) for (var gx = 0; gx < cols; gx++)
      svgEl("rect", { x: OX + gx * (CW + GAP), y: OY + gy * (CH + GAP), width: CW, height: CH, "class": "gridcell" }, slab);
    svgText(slab, OX + cols * (CW + GAP) - GAP, OY + rows * (CH + GAP) + 22, "MasterHub base " + (vertical ? "4×6 (vertical)" : "6×4 (flat)"), "slab-label").setAttribute("text-anchor", "end");

    var devs = state.devices || {};
    var usedCells = {};
    var order = ["top-left","top-mid-left","middle-top","top-mid-right","top-right","left-top","left","left-bottom","right-top","right","right-bottom","bottom-left","bottom-mid-left","bottom-middle","bottom-mid-right","bottom-right"];
    Object.keys(devs).sort().forEach(function (serial) {
      var d = devs[serial] || {};
      if (/base$/.test(d.kind || "")) { return; }
      var mapped = SLOT_ROT[d.kind + "|" + (d.slot || "")];
      var fp = FOOTPRINT[d.kind] || [2, 1];
      var rots = mapped ? [mapped[1], (mapped[1] + 90) % 360, (mapped[1] + 180) % 360, (mapped[1] + 270) % 360] : [0, 90, 180, 270];
      var anchors = mapped ? [mapped[0]].concat(order) : order;
      var placed = null;
      outer:
      for (var ai = 0; ai < anchors.length; ai++)
        for (var ri = 0; ri < rots.length; ri++) {
          var cand = cellRect(anchors[ai], rots[ri], fp, cols, rows);
          var clash = false;
          for (var cy = cand.row; cy < cand.row + cand.h && !clash; cy++)
            for (var cx = cand.col; cx < cand.col + cand.w && !clash; cx++)
              if (usedCells[cy + "," + cx]) { clash = true; }
          if (!clash) { placed = { anchor: anchors[ai], rot: rots[ri], rect: cand }; break outer; }
        }
      if (!placed) { placed = { anchor: mapped ? mapped[0] : "middle-top", rot: mapped ? mapped[1] : 0, rect: cellRect(mapped ? mapped[0] : "middle-top", mapped ? mapped[1] : 0, fp, cols, rows) }; }
      for (var uy = placed.rect.row; uy < placed.rect.row + placed.rect.h; uy++)
        for (var ux = placed.rect.col; ux < placed.rect.col + placed.rect.w; ux++) { usedCells[uy + "," + ux] = true; }
      drawModule(svg, d, serial, placed, OX, OY, CW, CH, GAP);
    });
  }

  function drawModule(svg, d, serial, placed, OX, OY, CW, CH, GAP) {
    var r = placed.rect, rot = placed.rot;
    var x = OX + r.col * (CW + GAP), y = OY + r.row * (CH + GAP);
    var w = r.w * CW + (r.w - 1) * GAP, h = r.h * CH + (r.h - 1) * GAP;
    var disconnected = d.connection === "disconnected";
    var g = svgEl("g", { "class": "module m-" + d.kind + (disconnected ? " disconnected" : "") }, svg);
    svgEl("rect", { x: x, y: y, width: w, height: h, rx: 10, "class": "body" }, g);
    svgText(g, x + 8, y + 15, d.kind + " · slot " + (d.slot || "?") + " · " + rot + "°", "mod-label");
    svgText(g, x + w - 8, y + h - 8, disconnected ? "DISCONNECTED" : (d.connection || "connected"), "mod-status").setAttribute("text-anchor", "end");
    var inner = svgEl("g", { transform: "translate(" + (x + w / 2) + "," + (y + h / 2) + ") rotate(" + rot + ") translate(" + (-(w / 2)) + "," + (-(h / 2)) + ")" }, g);
    var c = state.controls || {};
    if (d.kind === "keys") {
      var sw = w - 12, sh = h - 34;
      svgEl("rect", { x: x + 6, y: y + 22, width: sw, height: sh, rx: 4, "class": "screen" }, g);
      var img = svgEl("image", { x: x + 6, y: y + 22, width: sw, height: sh, "class": "frameshot", preserveAspectRatio: "xMidYMid meet" }, g);
      frameTargets[d.kind] = { img: img, sha: null };
    } else if (d.kind === "dial") {
      var sw2 = w - 12, sh2 = h - 34;
      svgEl("rect", { x: x + 6, y: y + 22, width: sw2, height: sh2, rx: 4, "class": "screen" }, g);
      var img2 = svgEl("image", { x: x + 6, y: y + 22, width: sw2, height: sh2, "class": "frameshot", preserveAspectRatio: "xMidYMid meet" }, g);
      frameTargets[d.kind] = { img: img2, sha: null };
      var dv = c.dial && typeof c.dial.value === "number" ? c.dial.value : 0;
      var ang = -120 + (dv / 100) * 240;
      var dc = svgEl("g", { transform: "translate(" + (x + w / 2) + "," + (y + h / 2 + 6) + ") rotate(" + ang + ")" }, g);
      svgEl("circle", { cx: 0, cy: 0, r: Math.min(w, h) * 0.3, "class": "knob dial-knob" }, dc);
      svgEl("rect", { x: -2, y: -Math.min(w, h) * 0.3, width: 4, height: Math.min(w, h) * 0.12, rx: 2, "class": "dial-tick" }, dc);
    } else if (d.kind === "knobs") {
      var kn = c.knobs || {};
      var kpos = [[0.5, 0.32], [0.3, 0.72], [0.7, 0.72]]; // triangle: knob 1 top, 2/3 bottom
      for (var k = 0; k < 3; k++) {
        var kc = kn[String(k)] || {};
        var kv = typeof kc.value === "number" ? kc.value : 0;
        var ka = -120 + (kv / 100) * 240;
        var kg = svgEl("g", { transform: "translate(" + (w * kpos[k][0]) + "," + (h * kpos[k][1]) + ")" }, inner);
        svgEl("circle", { cx: 0, cy: 0, r: Math.min(w, h) * 0.19, "class": "knob", fill: kc.led || kc.color || "#4a5468" }, kg);
        var kt = svgEl("g", { transform: "rotate(" + ka + ")" }, kg);
        svgEl("rect", { x: -2, y: -Math.min(w, h) * 0.19, width: 4, height: Math.min(w, h) * 0.1, rx: 2, "class": "dial-tick" }, kt);
      }
    } else if (d.kind === "faders") {
      var fa = c.faders || {};
      for (var f = 0; f < 5; f++) {
        var fx = 12 + f * ((w - 24) / 5);
        svgEl("rect", { x: fx, y: 26, width: 6, height: h - 44, rx: 3, "class": "track" }, inner);
        var v = (fa[String(f)] && typeof fa[String(f)].value === "number") ? fa[String(f)].value : 50;
        svgEl("rect", { x: fx - 2, y: 26 + (h - 44) * (1 - v / 100), width: 10, height: 8, rx: 2, "class": "handle" }, inner);
      }
    } else if (d.kind === "rollers") {
      var ro = c.rollers || {};
      for (var r2 = 0; r2 < 2; r2++) {
        var rc = ro[String(r2)] || {};
        svgEl("rect", { x: 10, y: 26 + r2 * ((h - 40) / 2), width: w - 20, height: (h - 40) / 2 - 4, rx: 6,
          fill: rc.led || rc.color || "#4a5468", "class": "roller" }, inner);
      }
    }
    g.addEventListener("click", function () { select({ type: "device", serial: serial }); });
  }

  // ---- frame stream (screenshot stream over WS, DS-121) ----------------------
  var frameTargets = {};
  var frameTimer = null;
  function pollFrames() {
    if (!ws || ws.readyState !== 1) { return; }
    Object.keys(frameTargets).forEach(function (kind, i) {
      ws.send(JSON.stringify({ id: 900 + i, op: "frame", target: kind === "keys" ? "keys" : "dial" }));
    });
  }
  function handleFrame(msg) {
    var t = frameTargets[msg.target === "keys" ? "keys" : "dial"];
    if (!t || !t.img) { return; }
    if (msg.sha256 && msg.sha256 !== t.sha && msg.data) {
      t.sha = msg.sha256;
      t.img.setAttribute("href", "data:image/jpeg;base64," + msg.data);
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

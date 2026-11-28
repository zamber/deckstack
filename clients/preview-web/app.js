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
  // Placement is DEDUCED, never read from the hardware's (unreliable) rotation
  // readout: the DS-122 matrix (placement-matrix.json, derived from the recorded
  // SALON-001 sweeps) maps each (kind, slot) to its connector cell + absolute
  // rotation on the flat 6x4 grid, and the DS-123 solver fits the connected
  // modules onto the grid. Cross-position rule: the connector cell is the module
  // corner nearest the deck center (the corner indicator sits diagonally across
  // from the connector). Modules may overhang the edge but keep >= 1 cell on base.
  var MATRIX = {};
  // module footprint in flat-grid cells [w,h] at rotation 0 (owner-measured)
  var FOOTPRINT = { keys: [4,3], faders: [2,4], knobs: [2,2], rollers: [1,4], dial: [2,2] };
  var SQUARE_KINDS = { knobs: true, dial: true };
  var FLAT_COLS = 6, FLAT_ROWS = 4;

  // led state is an object {mode,color,brightness}; use its color for fills
  function ledColor(c) {
    if (!c) { return null; }
    if (typeof c === "string") { return c; }
    return c.color || null;
  }

  function rotSize(fp, rot) {
    return (rot % 180 === 90) ? [fp[1], fp[0]] : [fp[0], fp[1]];
  }

  // Top-left cell of a module whose connector cell is (cc,cr) at rotation rot.
  // The connector cell is the module's OUTWARD corner (the deck-edge corner);
  // the module extends inward onto the grid. Edge-middle connectors (top/bottom/
  // left/right middle) are ambiguous between two corners — the cross-position
  // rule picks: the corner indicator at rot 0 is TL for keys, TR for the others,
  // the connector sits diagonally opposite, turned by rot (CCW positive).
  function topLeftOf(kind, fp, cc, cr, rot, cols, rows) {
    var rs = rotSize(fp, rot);
    var cx = (cols - 1) / 2, cy = (rows - 1) / 2;
    var dx = cc < cx ? -1 : cc > cx ? 1 : 0;
    var dy = cr < cy ? -1 : cr > cy ? 1 : 0;
    var corners = [[0, 0], [rs[0] - 1, 0], [rs[0] - 1, rs[1] - 1], [0, rs[1] - 1]]; // TL,TR,BR,BL
    var idx;
    if (dx !== 0 && dy !== 0) { idx = dx < 0 ? (dy < 0 ? 0 : 3) : (dy < 0 ? 1 : 2); }
    else {
      var cands = dy < 0 ? [0, 1] : dy > 0 ? [2, 3] : dx < 0 ? [0, 3] : [1, 2];
      var kindIdx0 = kind === "keys" ? 0 : 1;
      var want = (kindIdx0 + 3 * ((rot / 90) % 4) + 2) % 4;
      idx = cands.indexOf(want) >= 0 ? want : cands[0];
    }
    return { col: cc - corners[idx][0], row: cr - corners[idx][1], w: rs[0], h: rs[1] };
  }

  function baseOrientation() {
    var devs = state.devices || {};
    for (var k in devs) { if (/base$/.test(devs[k].kind || "") && devs[k].orientation) { return devs[k].orientation; } }
    return "horizontal";
  }

  function cellsOverlap(used, col, row, w, h) {
    for (var r = row; r < row + h; r++)
      for (var c = col; c < col + w; c++)
        if (used[r + "," + c]) { return true; }
    return false;
  }
  function markCells(used, col, row, w, h) {
    for (var r = row; r < row + h; r++)
      for (var c = col; c < col + w; c++) { used[r + "," + c] = true; }
  }
  function onBaseCells(col, row, w, h, cols, rows) {
    var n = 0;
    for (var r = row; r < row + h; r++)
      for (var c = col; c < col + w; c++)
        if (c >= 0 && c < cols && r >= 0 && r < rows) { n++; }
    return n;
  }

  // DS-123 solver: deduce {col,row,w,h,rot} per connected module, or unplaced.
  // Largest footprint first (keys shrinks the solution space most); candidates
  // from the matrix entry (all rotations for square 2x2 modules, which may rotate
  // in place); vertical base transposes the grid (col,row)->(row,col); survivors
  // ranked by orientation bias, then connector-nearest-center, then (col,row,rot).
  function solvePlacement(devs, orientation) {
    var vertical = orientation === "vertical";
    var cols = vertical ? FLAT_ROWS : FLAT_COLS, rows = vertical ? FLAT_COLS : FLAT_ROWS;
    var cx = (cols - 1) / 2, cy = (rows - 1) / 2;
    var mods = Object.keys(devs).map(function (s) { return { serial: s, d: devs[s] || {} }; })
      .filter(function (m) { return !/base$/.test(m.d.kind || ""); });
    mods.sort(function (a, b) {
      var fa = FOOTPRINT[a.d.kind] || [2, 1], fb = FOOTPRINT[b.d.kind] || [2, 1];
      return (fb[0] * fb[1] - fa[0] * fa[1]) || ((a.d.kind + "@" + a.d.slot) < (b.d.kind + "@" + b.d.slot) ? -1 : 1);
    });
    var used = {}, out = [];
    mods.forEach(function (m) {
      var d = m.d;
      if (d.connection === "disconnected") { out.push({ serial: m.serial, d: d, unplaced: "disconnected" }); return; }
      var fp = FOOTPRINT[d.kind] || [2, 1];
      var entry = MATRIX[d.kind + "@" + (d.slot || "")];
      if (!entry) { out.push({ serial: m.serial, d: d, unplaced: "no matrix entry" }); return; }
      var rots = SQUARE_KINDS[d.kind] ? [0, 90, 180, 270] : (entry.rotations || [entry.rot || 0]);
      var cands = [];
      rots.forEach(function (rot) {
        var cc = vertical ? entry.row : entry.col, cr = vertical ? entry.col : entry.row;
        var tl = topLeftOf(d.kind, fp, cc, cr, rot, cols, rows);
        if (onBaseCells(tl.col, tl.row, tl.w, tl.h, cols, rows) < 1) { return; }
        if (cellsOverlap(used, tl.col, tl.row, tl.w, tl.h)) { return; }
        var bias = vertical ? (rot % 180 === 90 ? 0 : 1) : (rot % 180 === 0 ? 0 : 1);
        var dcx = Math.abs(cc - cx), dcy = Math.abs(cr - cy);
        cands.push({ col: tl.col, row: tl.row, w: tl.w, h: tl.h, rot: rot, bias: bias, cd: dcx + dcy });
      });
      if (!cands.length) { out.push({ serial: m.serial, d: d, unplaced: "connector taken / no fit" }); return; }
      cands.sort(function (a, b) {
        return (a.bias - b.bias) || (a.cd - b.cd) || (a.col - b.col) || (a.row - b.row) || (a.rot - b.rot);
      });
      var p = cands[0];
      markCells(used, p.col, p.row, p.w, p.h);
      out.push({ serial: m.serial, d: d, col: p.col, row: p.row, w: p.w, h: p.h, rot: p.rot });
    });
    return out;
  }

  function renderDeck() {
    var svg = document.getElementById("deck");
    while (svg.firstChild) { svg.removeChild(svg.firstChild); }
    var orientation = baseOrientation();
    var vertical = orientation === "vertical";
    document.getElementById("orientation").textContent =
      "orientation: " + (vertical ? "vertical (4×6)" : "flat (6×4)");
    var CELL = 96, GAP = 8, U = CELL + GAP;
    var cols = vertical ? FLAT_ROWS : FLAT_COLS, rows = vertical ? FLAT_COLS : FLAT_ROWS;
    var M = 4; // margin cells around the slab for overhanging modules
    var W = (cols + 2 * M) * U + GAP, H = (rows + 2 * M) * U + GAP;
    svg.setAttribute("viewBox", "0 0 " + W + " " + H);
    var OX = M * U + GAP, OY = M * U + GAP;

    var slab = svgEl("g", { "class": "base" }, svg);
    svgEl("rect", { x: OX, y: OY, width: cols * U - GAP, height: rows * U - GAP, rx: 18, "class": "slab" }, slab);
    for (var gy = 0; gy < rows; gy++) for (var gx = 0; gx < cols; gx++)
      svgEl("rect", { x: OX + gx * U, y: OY + gy * U, width: CELL, height: CELL, "class": "gridcell" }, slab);
    svgText(slab, OX + 12, OY + 20, "MasterHub base " + cols + "×" + rows, "slab-label");

    var placed = solvePlacement(state.devices || {}, orientation);
    var offDeck = [];
    placed.forEach(function (p) {
      if (p.unplaced) { offDeck.push(p); return; }
      drawModule(svg, p.d, p.serial, p, FOOTPRINT[p.d.kind] || [2, 1], OX, OY, CELL, GAP);
    });
    if (offDeck.length) {
      svgText(svg, 12, H - 14, "not on deck: " + offDeck.map(function (p) {
        return p.d.kind + " · slot " + (p.d.slot || "?") + " (" + p.unplaced + ")";
      }).join("   "), "slab-label");
    }
  }

  // Draws one module at its grid spot. The body is placed in board space; the
  // content is drawn in the module's own (unrotated) frame inside a rotated group,
  // so e.g. fader caps always travel along the module's long axis.
  function drawModule(svg, d, serial, p, fp, OX, OY, CELL, GAP) {
    var U = CELL + GAP;
    var x = OX + p.col * U, y = OY + p.row * U;
    var w = p.w * CELL + (p.w - 1) * GAP, h = p.h * CELL + (p.h - 1) * GAP;
    var w0 = fp[0] * CELL + (fp[0] - 1) * GAP, h0 = fp[1] * CELL + (fp[1] - 1) * GAP;
    var g = svgEl("g", { "class": "module m-" + d.kind, transform: "translate(" + x + "," + y + ")" }, svg);
    svgEl("rect", { x: 0, y: 0, width: w, height: h, rx: 14, "class": "body" }, g);
    svgText(g, 8, 15, d.kind + " · slot " + (d.slot || "?") + " · " + p.rot + "°", "mod-label");
    var inner = svgEl("g", { transform: "translate(" + (w / 2) + "," + (h / 2) + ") rotate(" + (-p.rot) + ") translate(" + (-(w0 / 2)) + "," + (-(h0 / 2)) + ")" }, g);
    var c = state.controls || {};
    if (d.kind === "keys" || d.kind === "dial") {
      svgEl("rect", { x: 8, y: 8, width: w0 - 16, height: h0 - 16, rx: 8, "class": "screen" }, inner);
      var img = svgEl("image", { x: 8, y: 8, width: w0 - 16, height: h0 - 16, "class": "frameshot", preserveAspectRatio: "xMidYMid meet" }, inner);
      frameTargets[d.kind] = { img: img, sha: null };
      if (d.kind === "dial") {
        var dv = c.dial && typeof c.dial.value === "number" ? c.dial.value : 0;
        var ang = -120 + (dv / 100) * 240;
        var dc = svgEl("g", { transform: "translate(" + (w0 / 2) + "," + (h0 / 2) + ") rotate(" + ang + ")" }, inner);
        svgEl("circle", { cx: 0, cy: 0, r: Math.min(w0, h0) * 0.32, "class": "dial-knob" }, dc);
        svgEl("rect", { x: -2, y: -Math.min(w0, h0) * 0.32, width: 4, height: Math.min(w0, h0) * 0.13, rx: 2, "class": "dial-tick" }, dc);
      }
    } else if (d.kind === "knobs") {
      var kn = c.knobs || {};
      var kpos = [[0.5, 0.3], [0.28, 0.72], [0.72, 0.72]]; // triangle: knob 1 top, 2/3 bottom
      for (var k = 0; k < 3; k++) {
        var kc = kn[String(k)] || {};
        var kv = typeof kc.value === "number" ? kc.value : 0;
        var ka = -120 + (kv / 100) * 240;
        var kg = svgEl("g", { transform: "translate(" + (w0 * kpos[k][0]) + "," + (h0 * kpos[k][1]) + ")" }, inner);
        svgEl("circle", { cx: 0, cy: 0, r: Math.min(w0, h0) * 0.2, "class": "knob", fill: ledColor(kc.led) || kc.color || "#4a5468" }, kg);
        var kt = svgEl("g", { transform: "rotate(" + ka + ")" }, kg);
        svgEl("rect", { x: -2, y: -Math.min(w0, h0) * 0.2, width: 4, height: Math.min(w0, h0) * 0.1, rx: 2, "class": "dial-tick" }, kt);
      }
    } else if (d.kind === "faders") {
      var fa = c.faders || {};
      svgEl("rect", { x: 10, y: 10, width: w0 - 20, height: h0 - 20, rx: 10, "class": "tray" }, inner);
      var top = 0.22 * h0, bottom = 0.78 * h0;
      for (var f = 0; f < 5; f++) {
        var cx = w0 * (0.5 + f) / 5;
        var v = (fa[String(f)] && typeof fa[String(f)].value === "number") ? fa[String(f)].value : 50;
        var ty = bottom - (bottom - top) * v / 100;
        svgEl("rect", { x: cx - 3.5, y: top - 8, width: 7, height: bottom - top + 16, rx: 3.5, "class": "slot" }, inner);
        svgEl("rect", { x: cx - 18, y: ty - 12, width: 36, height: 24, rx: 8, "class": "cap" }, inner);
        svgEl("rect", { x: cx - 12, y: ty - 9, width: 24, height: 3, rx: 1.5, "class": "cap-rib" }, inner);
      }
    } else if (d.kind === "rollers") {
      var ro = c.rollers || {};
      for (var r2 = 0; r2 < 2; r2++) {
        var rc = ro[String(r2)] || {};
        var rx = r2 === 0 ? w0 * 0.06 : w0 * 0.52;
        var rw = w0 * 0.42, ry = h0 * 0.24, rh = h0 * 0.52;
        svgEl("rect", { x: rx, y: ry, width: rw, height: rh, rx: rh / 2, "class": "roller", fill: ledColor(rc.led) || rc.color || "#4a5468" }, inner);
        for (var rib = 1; rib < 6; rib++)
          svgEl("line", { x1: rx + rib * rw / 6, y1: ry + 4, x2: rx + rib * rw / 6, y2: ry + rh - 4, "class": "rib" }, inner);
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

  // DS-122 placement matrix: same-origin static file served next to this app
  fetch("placement-matrix.json").then(function (r) { return r.json(); }).then(function (j) {
    MATRIX = j || {};
    renderSoon();
  }).catch(function () { /* deck renders with every module unplaced */ });

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

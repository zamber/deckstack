#!/usr/bin/env python3
"""e2e_vdev.py — DS-129 end-to-end verification of the preview-webapp loop.

Drives the real webapp (clients/preview-web, served at http://127.0.0.1:8080/)
in headless chromium against the running vdev demo daemon, executes a scripted
sequence of KNOWN attach / re-attachment points with ds-vdevctl, and asserts
after every step:

  * DOM placements == tools/placement_solver.py solve(devices, orientation,
    matrix=clients/preview-web/placement-matrix.json, connector_semantics=True)
    for the device set reported by GET /preview/api/state;
  * zero cell overlaps between rendered modules;
  * the "not on deck:" strip lists exactly the unplaced modules with the right
    reason (no matrix entry / no fit / disconnected);
  * no page reload (window.__nore marker survives every step);
  * the streamed screen <image> href never goes empty once set (no flicker,
    watched by an in-page rAF sampler plus 100 ms samples during streaming).

It also measures (DS-126 acceptance perf):
  (a) page-open -> first streamed frame applied (window.__frameFirstAt);
  (b) per-move latency: timestamp injected in the page before `ds-vdevctl move`
      (window.__moveSentAt) vs. the DOM update (cmd->DOM), and the ws /devices
      event -> DOM re-render (ws->DOM, from an in-page WebSocket wrapper);
  (c) 30 s of frame streaming with concurrent moves: blank-screen samples and
      inter-frame gaps from the in-page frame log;
  (d) total re-render count (window.__renderCount).

Finally it does one READ-ONLY sanity pass against the live deck (port 43541):
render vs the live /devices, off-deck strip handling of disconnected modules,
screenshot ds129-live.png. The live deck is never written to.

How to run (repo root, vdev demo daemon + `serve.py` on 8080 already running —
see docs/discovery/salon-001/verification-ds124.md §1):

    python3 -m pip install --user playwright        # once
    python3 -m playwright install chromium          # once; alternatively any
        # chromium under ~/.cache/ms-playwright/chromium-*/chrome-linux*/chrome
        # is auto-detected if the pinned browser is missing
    python3 clients/preview-web/tests/e2e_vdev.py                 # vdev e2e + live pass
    python3 clients/preview-web/tests/e2e_vdev.py --vdev-only     # vdev e2e only
    python3 clients/preview-web/tests/e2e_vdev.py --live-only     # live sanity only
    python3 clients/preview-web/tests/e2e_vdev.py --out /tmp/ds129.json   # + JSON dump

Env overrides: DS129_VDEV_SOCK, DS129_VDEV_PORT, DS129_VDEV_TOKEN,
DS129_LIVE_PORT, DS129_LIVE_TOKEN, DS129_WEBAPP, DS129_SHOTS, DS129_OUT.

Screenshots are written to docs/discovery/salon-001/ds129-*.png.
Exit code 0 = every assertion PASS, 1 otherwise. The vdev reference slots
(base 1-4-4, keys 2-3, faders 1-1-3, knobs 1-2-3, rollers 3-3, dial 1-3-4)
are restored in a finally block even on failure.

Stdlib + python-playwright only (no node, no MCP needed at run time).
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import subprocess
import sys
import time
import urllib.request

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
sys.path.insert(0, os.path.join(REPO, "tools"))
import placement_solver as ps  # noqa: E402

MATRIX = json.load(open(os.path.join(REPO, "clients", "preview-web", "placement-matrix.json")))

VDEV_SOCK = os.environ.get("DS129_VDEV_SOCK", "/tmp/ds121-demo/vdev.sock")
VDEVCTL = os.path.join(REPO, "target", "debug", "ds-vdevctl")
VDEV_PORT = int(os.environ.get("DS129_VDEV_PORT", "39955"))
VDEV_TOKEN = os.environ.get(
    "DS129_VDEV_TOKEN",
    "dace73774761fe86d712982550059c118bdf01ba109978ba94fde760695a7fa6",
)
LIVE_PORT = int(os.environ.get("DS129_LIVE_PORT", "43541"))
LIVE_TOKEN = os.environ.get(
    "DS129_LIVE_TOKEN",
    "a3d87b28ecc5b43ec8bba391d08d1e74ca0d0ebf7328915696331a7d738e7769",
)
WEBAPP = os.environ.get("DS129_WEBAPP", "http://127.0.0.1:8080/")
SHOTS = os.environ.get("DS129_SHOTS", os.path.join(REPO, "docs", "discovery", "salon-001"))

# vdev reference slots (crates/ds-vdev/src/hub.rs reference_slot) — restored at the end.
REF_SLOTS = {
    "base": "1-4-4",
    "keys": "2-3",
    "faders": "1-1-3",
    "knobs": "1-2-3",
    "rollers": "3-3",
    "dial": "1-3-4",
}

# Scripted re-attachment sequence. The ticket's example order (keys 2-3 -> 2-2 ->
# 1-3-4 first) is impossible as-is because the dial occupies 1-3-4 in the
# reference set, so faders is moved off-deck first and the dial is shifted to
# 1-1-3 to free 1-3-4; every target slot of the ticket's sequence is still
# visited (keys 2-2 and 1-3-4, rollers 1-1-4, knobs 1-4-3, faders 1-2-1 off-deck).
MOVES = [
    ("faders", "1-2-1", "01-faders-1-2-1"),   # off-deck: no matrix entry for faders@1-2-1
    ("dial", "1-1-3", "02-dial-1-1-3"),       # frees 1-3-4 for the keys move
    ("keys", "2-2", "03-keys-2-2"),
    ("keys", "1-3-4", "04-keys-1-3-4"),
    ("rollers", "1-1-4", "05-rollers-1-1-4"),
    ("knobs", "1-4-3", "06-knobs-1-4-3"),
]
# Concurrent moves fired during the 30 s streaming window (slots are free in
# the post-MOVES configuration): rollers 1-1-4 <-> 3-3, knobs 1-4-3 <-> 1-2-3.
STREAM_MOVES = [
    (4.0, "rollers", "3-3"),
    (8.0, "rollers", "1-1-4"),
    (12.0, "knobs", "1-2-3"),
    (16.0, "knobs", "1-4-3"),
    (20.0, "rollers", "3-3"),
    (24.0, "rollers", "1-1-4"),
]
RESTORE_MOVES = [
    ("keys", "2-3"),
    ("dial", "1-3-4"),
    ("faders", "1-1-3"),
    ("rollers", "3-3"),
    ("knobs", "1-2-3"),
]

# Injected before any page script: wraps WebSocket to timestamp /devices events
# (and the first post-event task, i.e. after app.js's synchronous renderAll),
# logs preview.frame responses, and runs an rAF sampler that records any
# streamed <image> whose href goes empty after having been set (flicker).
INIT_SCRIPT = r"""
window.__navStart = performance.now();
window.__wsDevTimes = [];
window.__frameLog = [];
window.__everSet = {};
window.__flicker = [];
(function () {
  const Orig = window.WebSocket;
  function Wrapped(url, proto) {
    const ws = proto ? new Orig(url, proto) : new Orig(url);
    ws.addEventListener('message', (m) => {
      try {
        const msg = JSON.parse(m.data);
        if (msg.op === 'event' && msg.path && msg.path.indexOf('/devices') === 0) {
          const rec = { t: performance.now(), rev: msg.rev, path: msg.path, after: null };
          window.__wsDevTimes.push(rec);
          setTimeout(() => { rec.after = performance.now(); }, 0);
        }
        if (msg.op === 'preview.frame') {
          window.__frameLog.push({ t: performance.now(), target: msg.target,
            sha: msg.sha256 || null, hasData: !!msg.data, len: msg.data ? msg.data.length : 0 });
        }
      } catch (e) {}
    });
    return ws;
  }
  Wrapped.prototype = Orig.prototype;
  window.WebSocket = Wrapped;
})();
(function loop() {
  document.querySelectorAll('image.frameshot').forEach((im) => {
    const g = im.closest('g.module');
    const kind = g ? (g.getAttribute('class').match(/m-(\w+)/) || [null, '?'])[1] : '?';
    const h = im.getAttribute('href');
    if (h && h.length > 0) { window.__everSet[kind] = true; }
    else if (window.__everSet[kind]) { window.__flicker.push({ t: performance.now(), kind: kind }); }
  });
  requestAnimationFrame(loop);
})();
"""

DOM_SNAPSHOT_JS = r"""
() => {
  const U = 104, OX = 424, OY = 424;
  const mods = Array.from(document.querySelectorAll('#deck g.module')).map((g) => {
    const tr = g.getAttribute('transform') || '';
    const m = tr.match(/translate\(([-\d.]+),\s*([-\d.]+)\)/);
    const x = m ? parseFloat(m[1]) : NaN, y = m ? parseFloat(m[2]) : NaN;
    const label = g.querySelector('.mod-label') ? g.querySelector('.mod-label').textContent : '';
    const lm = label.match(/^(.*?) · slot (.*?) · (\d+)°$/);
    const body = g.querySelector('rect.body');
    const bw = body ? parseFloat(body.getAttribute('width')) : 0;
    const bh = body ? parseFloat(body.getAttribute('height')) : 0;
    const im = g.querySelector('image.frameshot');
    return {
      kind: lm ? lm[1] : (g.getAttribute('class').match(/m-(\w+)/) || [null, '?'])[1],
      slot: lm ? lm[2] : null,
      rot: lm ? parseInt(lm[3], 10) : null,
      col: Math.round((x - OX) / U), row: Math.round((y - OY) / U),
      w: Math.round((bw + 24) / U), h: Math.round((bh + 24) / U),
      serial: g.getAttribute('data-serial'),
      disc: g.getAttribute('class').indexOf('disconnected') >= 0,
      imgLen: im ? (im.getAttribute('href') || '').length : 0
    };
  });
  const strips = Array.from(document.querySelectorAll('#deck text.slab-label')).map((t) => t.textContent);
  const off = (strips.find((s) => s.indexOf('not on deck:') === 0) || '').replace(/^not on deck:\s*/, '');
  return {
    conn: document.getElementById('conn').textContent,
    revText: document.getElementById('rev').textContent,
    rev: (document.getElementById('rev').textContent.match(/rev (\d+)/) || [null, null])[1],
    orient: document.getElementById('orientation').textContent,
    mods: mods, offText: off,
    renderCount: window.__renderCount || 0,
    nore: window.__nore === undefined ? null : window.__nore,
    helloAt: window.__helloAt, frameFirstAt: window.__frameFirstAt,
    flicker: window.__flicker || [],
    everSet: window.__everSet || {},
    wsDevTimes: window.__wsDevTimes || [],
    frameLog: window.__frameLog || [],
    moveSentAt: window.__moveSentAt
  };
}
"""

SAMPLE_JS = r"""
() => ({
  t: performance.now(),
  rev: (document.getElementById('rev').textContent.match(/rev (\d+)/) || [null, null])[1],
  renderCount: window.__renderCount || 0,
  nmods: document.querySelectorAll('#deck g.module').length,
  imgs: Array.from(document.querySelectorAll('#deck image.frameshot')).map((im) => {
    const g = im.closest('g.module');
    return { kind: g ? (g.getAttribute('class').match(/m-(\w+)/) || [null, '?'])[1] : '?',
             len: (im.getAttribute('href') || '').length };
  }),
  blankNow: Array.from(document.querySelectorAll('#deck image.frameshot')).some((im) => {
    const g = im.closest('g.module');
    const kind = g ? (g.getAttribute('class').match(/m-(\w+)/) || [null, '?'])[1] : '?';
    return window.__everSet[kind] && (im.getAttribute('href') || '').length === 0;
  }),
  flicker: (window.__flicker || []).length
})
"""


# --------------------------------------------------------------------------- helpers

def api_state(port: int, token: str) -> dict:
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/preview/api/state",
        headers={"Authorization": "Bearer " + token},
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.load(r)


def vdevctl(*args: str) -> str:
    p = subprocess.run(
        [VDEVCTL, "--socket", VDEV_SOCK, *args],
        capture_output=True, text=True, timeout=30,
    )
    return (p.stdout + p.stderr).strip()


def vdev_slots() -> dict:
    out = vdevctl("slots")
    slots = {}
    for line in out.splitlines():
        m = re.match(r"(\S+)\s+pid=\S+\s+slot=(\S+)", line)
        if m:
            slots[m.group(1)] = m.group(2)
    return slots


def expected_from_state(st: dict):
    """Run tools/placement_solver.py over the daemon-reported device set with
    the FULL served matrix and connector-cell semantics (mirrors app.js)."""
    devs, orientation = [], "horizontal"
    for serial, d in (st.get("devices") or {}).items():
        kind = d.get("kind") or ""
        if kind.endswith("base"):
            o = str(d.get("orientation", "")).lower()
            orientation = "vertical" if o in ("vertical", "00", "upright") else "horizontal"
            continue
        devs.append({
            "kind": kind,
            "slot": d.get("slot"),
            "connection": d.get("connection", "connected"),
            "_serial": serial,
        })
    clean = [{k: v for k, v in d.items() if k != "_serial"} for d in devs]
    res = ps.solve(clean, orientation, matrix=MATRIX, connector_semantics=True)
    out = {}
    for d, r in zip(devs, res):
        key = (d["kind"], d.get("slot"))
        if d["connection"] == "disconnected":
            out[key] = ("off", "disconnected")  # app.js: disconnected -> strip
        elif r.get("unplaced"):
            reason = r.get("reason", "")
            cls = "no matrix entry" if "no matrix entry" in reason else "no fit"
            out[key] = ("off", cls)
        else:
            out[key] = ("on", r["col"], r["row"], r["rotation"],
                        r["footprint"]["w"], r["footprint"]["h"])
    return out, orientation, devs


def dom_map(dom: dict) -> dict:
    out = {}
    for m in dom["mods"]:
        key = (m["kind"], m["slot"])
        if m["disc"]:
            out[key] = ("off", "disconnected")
        else:
            out[key] = ("on", m["col"], m["row"], m["rot"], m["w"], m["h"])
    return out


def strip_entries(off_text: str):
    entries = []
    for e in off_text.split("   "):
        e = e.strip()
        if not e:
            continue
        m = re.match(r"^(.*?) · slot (.*?) \((.*)\)$", e)
        if m:
            reason = m.group(3)
            if reason == "connector taken / no fit":
                reason = "no fit"
            entries.append((m.group(1), m.group(2), reason))
    return sorted(entries)


def expected_strip(exp: dict):
    return sorted(
        (k, s, v[1]) for (k, s), v in exp.items() if v[0] == "off"
    )


def dom_matches(dom: dict, exp: dict):
    """Returns (ok, problems). Compares placements for on-deck modules; off-deck
    modules are never drawn as g.module by app.js (they live only in the
    "not on deck" strip), so they are validated by the strip comparison."""
    dm = dom_map(dom)
    problems = []
    for key in sorted(set(exp) | set(dm)):
        e, d = exp.get(key), dm.get(key)
        if e is not None and e[0] == "off":
            continue  # covered by the strip check below
        if e != d:
            problems.append(f"{key}: expected {e} dom {d}")
    es, ds = expected_strip(exp), strip_entries(dom["offText"])
    if es != ds:
        problems.append(f"strip: expected {es} dom {ds}")
    return (not problems), problems


def overlaps(dom: dict):
    used = {}
    for m in dom["mods"]:
        for r in range(m["row"], m["row"] + m["h"]):
            for c in range(m["col"], m["col"] + m["w"]):
                if (c, r) in used:
                    return [(c, r), used[(c, r)], (m["kind"], m["slot"])]
                used[(c, r)] = (m["kind"], m["slot"])
    return None


def launch_browser(pw):
    try:
        return pw.chromium.launch(headless=True, args=["--no-sandbox"])
    except Exception:
        cands = sorted(glob.glob(os.path.expanduser(
            "~/.cache/ms-playwright/chromium-*/chrome-linux*/chrome")))
        cands += sorted(glob.glob(os.path.expanduser(
            "~/.cache/ms-playwright/chromium_headless_shell-*/chrome-headless-shell-linux64/chrome-headless-shell")))
        if not cands:
            raise
        return pw.chromium.launch(headless=True, args=["--no-sandbox"],
                                  executable_path=cands[-1])


# --------------------------------------------------------------------------- runner

class Runner:
    def __init__(self, page, port, token, shot_dir):
        self.page = page
        self.port = port
        self.token = token
        self.shot_dir = shot_dir
        self.results = []
        self.perf = {"moves": []}

    def shot(self, name):
        path = os.path.join(self.shot_dir, f"ds129-{name}.png")
        self.page.screenshot(path=path, full_page=True)
        return path

    def snap(self):
        return self.page.evaluate(DOM_SNAPSHOT_JS)

    def wait_connected(self, timeout=15):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                d = self.snap()
                if d["conn"] == "connected" and d["rev"] and d["mods"]:
                    return d
            except Exception:
                pass
            time.sleep(0.2)
        raise RuntimeError("page never reached connected+rendered state")

    def check_step(self, step, cmd, dom, exp, extra_checks=(), shot_name=None,
                   perf=None):
        checks = []
        ok, problems = dom_matches(dom, exp)
        checks.append(("dom==solver", ok, "; ".join(problems[:6])))
        ov = overlaps(dom)
        checks.append(("no-overlap", ov is None, str(ov)))
        es, ds = expected_strip(exp), strip_entries(dom["offText"])
        checks.append(("offdeck-strip", es == ds, f"exp={es} dom={ds}"))
        checks.append(("no-reload(__nore=1)", dom["nore"] == 1, f"nore={dom['nore']}"))
        checks.append(("ws-connected", dom["conn"] == "connected", dom["conn"]))
        checks.append(("no-flicker", len(dom["flicker"]) == 0,
                       f"flicker={dom['flicker'][:3]}"))
        for name, cok, detail in extra_checks:
            checks.append((name, cok, detail))
        if shot_name:
            path = self.shot(shot_name)
            checks.append(("screenshot", os.path.exists(path), os.path.basename(path)))
        step_ok = all(c[1] for c in checks)
        self.results.append({
            "step": step, "cmd": cmd, "ok": step_ok,
            "checks": [{"name": n, "ok": bool(o), "detail": d} for n, o, d in checks],
            "perf": perf or {},
            "dom": {"mods": [{k: m[k] for k in ("kind", "slot", "col", "row", "rot", "w", "h")}
                             for m in dom["mods"]],
                    "offText": dom["offText"], "rev": dom["rev"],
                    "renderCount": dom["renderCount"]},
        })
        return step_ok

    def wait_dom_matches(self, timeout=25.0):
        """Poll state+DOM until DOM == solver(state). The caller must first ensure
        the state reflects the commanded change (else a stale DOM trivially
        matches). Returns (st, exp, dom)."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                st = api_state(self.port, self.token)
                exp, orient, devs = expected_from_state(st)
                dom = self.snap()
                ok, problems = dom_matches(dom, exp)
                if ok and dom["conn"] == "connected":
                    return st, exp, dom
            except Exception:
                pass
            time.sleep(0.15)
        st = api_state(self.port, self.token)
        exp, orient, devs = expected_from_state(st)
        dom = self.snap()
        return st, exp, dom  # caller reports problems

    @staticmethod
    def slot_of_kind(st, kind):
        for d in (st.get("devices") or {}).values():
            if d.get("kind") == kind:
                return d.get("slot")
        return None

    def wait_state_slot(self, kind, slot, rev0, timeout=25.0):
        """Wait until the daemon publishes `kind` at `slot` (rev advanced)."""
        if self.slot_of_kind(api_state(self.port, self.token), kind) == slot:
            return True  # already there (no-op move)
        deadline = time.time() + timeout
        while time.time() < deadline:
            st = api_state(self.port, self.token)
            if self.slot_of_kind(st, kind) == slot and st.get("rev") != rev0:
                return True
            time.sleep(0.1)
        return self.slot_of_kind(api_state(self.port, self.token), kind) == slot

    def do_move(self, kind, slot, step, shot_name, expect_ok=True):
        st0 = api_state(self.port, self.token)
        rev0 = st0.get("rev")
        self.page.evaluate("window.__moveSentAt = performance.now();")
        t_sent = self.page.evaluate("window.__moveSentAt")
        cmd = f"ds-vdevctl move {kind} {slot}"
        out = vdevctl("move", kind, slot)
        refused = bool(re.search(r"occupied|refused|invalid|error", out, re.I))
        if expect_ok and refused:
            self.results.append({"step": step, "cmd": cmd, "ok": False,
                                 "checks": [{"name": "move-accepted", "ok": False, "detail": out}],
                                 "perf": {}, "dom": {}})
            return False
        published = self.wait_state_slot(kind, slot, rev0)
        st, exp, dom = self.wait_dom_matches()
        t_dom = self.page.evaluate("performance.now()")
        ws = [w for w in dom["wsDevTimes"] if w["t"] >= t_sent - 1 and w.get("after")]
        ws_raw = [w for w in dom["wsDevTimes"] if w["t"] >= t_sent - 1]
        ws_to_dom = (ws[-1]["after"] - ws[-1]["t"]) if ws else None
        perf = {
            "cmd_to_dom_ms": round(t_dom - t_sent, 1),
            "ws_event_to_dom_ms": round(ws_to_dom, 1) if ws_to_dom is not None else None,
            "ws_dev_events": len(ws),
            "ws_dev_events_raw": len(ws_raw),
            "state_published": published,
            "ctl_out": out,
            "rev_after": dom["rev"],
        }
        self.perf["moves"].append({"step": step, **perf})
        extra = [("move-accepted", not refused, out),
                 ("state-published", published, f"kind {kind} at slot {slot} in /preview/api/state")]
        return self.check_step(step, cmd, dom, exp, extra_checks=extra,
                               shot_name=shot_name, perf=perf)

    def do_replug(self, step, shot_name):
        st0 = api_state(self.port, self.token)
        rev0 = st0.get("rev")
        self.page.evaluate("window.__moveSentAt = performance.now();")
        t_sent = self.page.evaluate("window.__moveSentAt")
        cmd = "ds-vdevctl replug"
        out = vdevctl("replug")
        saw_disc_state = False
        saw_disc_dom = False
        deadline = time.time() + 40
        dom = exp = None
        while time.time() < deadline:
            st = api_state(self.port, self.token)
            devs = st.get("devices") or {}
            if any(d.get("connection") == "disconnected" for d in devs.values()):
                saw_disc_state = True
            exp, orient, _ = expected_from_state(st)
            dom = self.snap()
            if "disconnected" in dom["offText"]:
                saw_disc_dom = True
            ok, _ = dom_matches(dom, exp)
            all_conn = all(d.get("connection") != "disconnected" for d in devs.values())
            if ok and dom["conn"] == "connected" and all_conn and st.get("rev") != rev0:
                break
            time.sleep(0.05)
        st = api_state(self.port, self.token)
        exp, orient, devs = expected_from_state(st)
        dom = self.snap()
        t_dom = self.page.evaluate("performance.now()")
        ws = [w for w in dom["wsDevTimes"] if w["t"] >= t_sent - 1 and w.get("after")]
        ws_to_dom = (ws[-1]["after"] - ws[-1]["t"]) if ws else None
        perf = {
            "cmd_to_dom_ms": round(t_dom - t_sent, 1),
            "ws_event_to_dom_ms": round(ws_to_dom, 1) if ws_to_dom is not None else None,
            "ws_dev_events": len(ws),
            "saw_disconnected_in_state": saw_disc_state,
            "saw_disconnected_in_dom_strip": saw_disc_dom,
            "ctl_out": out,
        }
        self.perf["moves"].append({"step": step, **perf})
        extra = [
            ("replug-reconnected", st.get("rev") != rev0
             and all(d.get("connection") != "disconnected"
                     for d in (st.get("devices") or {}).values()),
             f"rev {rev0}->{st.get('rev')}"),
            ("replug-transient-seen", saw_disc_state or saw_disc_dom,
             f"state={saw_disc_state} dom={saw_disc_dom}"),
        ]
        return self.check_step(step, cmd, dom, exp, extra_checks=extra,
                               shot_name=shot_name, perf=perf)

    def do_stream(self, step, shot_name, duration=30.0):
        """30 s of frame streaming with concurrent moves; samples every 100 ms."""
        samples = []
        fired = set()
        t_stream_page0 = self.page.evaluate("performance.now()")
        t0 = time.time()
        while time.time() - t0 < duration:
            now = time.time() - t0
            for i, (delay, kind, slot) in enumerate(STREAM_MOVES):
                if delay <= now and i not in fired:
                    fired.add(i)
                    self.page.evaluate("window.__moveSentAt = performance.now();")
                    vdevctl("move", kind, slot)
            try:
                s = self.page.evaluate(SAMPLE_JS)
                s["wall"] = round(now, 2)
                samples.append(s)
            except Exception:
                pass
            time.sleep(0.1)
        dom = self.snap()
        frame_log = dom["frameLog"]
        per_target = {}
        for tgt in ("keys", "dial"):
            entries = [f for f in frame_log if f["target"] == tgt]
            times = [f["t"] for f in entries]
            shas = {}
            for f in entries:
                if f["sha"]:
                    shas.setdefault(f["sha"], f["t"])
            gaps = [round(b - a, 1) for a, b in zip(times, times[1:])]
            sha_times = sorted(shas.values())
            sha_gaps = [round(b - a, 1) for a, b in zip(sha_times, sha_times[1:])]
            per_target[tgt] = {
                "responses": len(entries),
                "distinct_shas": len(shas),
                "resp_gap_ms": {"min": min(gaps) if gaps else None,
                                "max": max(gaps) if gaps else None,
                                "mean": round(sum(gaps) / len(gaps), 1) if gaps else None},
                "new_frame_gap_ms": {"min": min(sha_gaps) if sha_gaps else None,
                                     "max": max(sha_gaps) if sha_gaps else None},
            }
        blank = [s for s in samples if s["blankNow"]]
        perf = {
            "duration_s": duration,
            "samples": len(samples),
            "blank_screen_samples": len(blank),
            "flicker_events": dom["flicker"],
            "ever_set_kinds": dom["everSet"],
            "frame_stream": per_target,
            "render_count_delta": (samples[-1]["renderCount"] - samples[0]["renderCount"])
                                  if samples else None,
            "stream_moves_fired": sorted(fired),
            "rev_start": samples[0]["rev"] if samples else None,
            "rev_end": samples[-1]["rev"] if samples else None,
        }
        self.perf["stream"] = perf
        # after the stream, wait until the page has processed the ws events for
        # the concurrent moves, then verify the settled configuration
        n_expected = len(STREAM_MOVES)
        deadline = time.time() + 25
        while time.time() < deadline:
            dom = self.snap()
            n_ev = len([w for w in dom["wsDevTimes"] if w["t"] >= t_stream_page0 - 1])
            if n_ev >= n_expected:
                break
            time.sleep(0.15)
        st, exp, dom = self.wait_dom_matches()
        return self.check_step(step, "30s stream + concurrent moves", dom, exp,
                               shot_name=shot_name, perf=perf)


# --------------------------------------------------------------------------- main flows

def restore_ref_slots(max_passes=4):
    """Best-effort move of every module back to its reference slot, avoiding
    occupied targets (repeat passes until stable)."""
    for _ in range(max_passes):
        cur = vdev_slots()
        if cur == REF_SLOTS:
            return cur
        occupied = set(cur.values())
        for kind, slot in REF_SLOTS.items():
            if cur.get(kind) != slot and slot not in occupied:
                vdevctl("move", kind, slot)
                occupied.discard(cur.get(kind))
                occupied.add(slot)
    return vdev_slots()


def run_vdev(pw, args):
    # pre-flight: make sure the vdev is at the reference slots so the scripted
    # sequence starts from a known attachment point (repeatable runs)
    cur = vdev_slots()
    if cur != REF_SLOTS:
        print(f"[ds129] pre-flight restore from {cur}")
        restore_ref_slots()
        deadline = time.time() + 20
        while time.time() < deadline:
            st = api_state(VDEV_PORT, VDEV_TOKEN)
            slots = {d.get("kind"): d.get("slot") for d in st.get("devices", {}).values()}
            if all(slots.get(k) == s for k, s in REF_SLOTS.items()):
                break
            time.sleep(0.3)
        print("[ds129] pre-flight slots now:", vdev_slots())

    browser = launch_browser(pw)
    page = browser.new_page(viewport={"width": 1680, "height": 1500})
    page.add_init_script(INIT_SCRIPT)
    runner = Runner(page, VDEV_PORT, VDEV_TOKEN, SHOTS)
    url = f"{WEBAPP}?token={VDEV_TOKEN}&port={VDEV_PORT}"
    print(f"[ds129] opening {url}")
    t_wall0 = time.time()
    page.goto(url, wait_until="domcontentloaded")
    dom = runner.wait_connected()
    # wait for the first streamed frame, measuring wall + in-page latency
    deadline = time.time() + 10
    first_frame_wall = None
    while time.time() < deadline:
        d = runner.snap()
        if d["frameFirstAt"] is not None:
            first_frame_wall = time.time()
            break
        time.sleep(0.05)
    dom = runner.snap()
    page.evaluate("window.__nore = 1;")
    dom = runner.snap()  # re-read so __nore is visible in the baseline snapshot
    perf_open = {
        "page_open_to_ws_hello_ms": dom["helloAt"],
        "page_open_to_first_frame_ms": dom["frameFirstAt"],
        "wall_goto_to_first_frame_ms": round((first_frame_wall - t_wall0) * 1000, 1)
                                       if first_frame_wall else None,
        "render_count_at_open": dom["renderCount"],
    }
    runner.perf["open"] = perf_open
    print(f"[ds129] open perf: {perf_open}")

    # step 0: baseline (current vdev slots) vs solver
    st = api_state(VDEV_PORT, VDEV_TOKEN)
    exp, orient, devs = expected_from_state(st)
    ok, problems = dom_matches(dom, exp)
    runner.check_step("00-baseline", "initial open", dom, exp,
                      extra_checks=[("initial-dom==solver", ok, "; ".join(problems[:6]))],
                      shot_name="00-baseline", perf=perf_open)

    try:
        for kind, slot, name in MOVES:
            print(f"[ds129] move {kind} -> {slot}")
            runner.do_move(kind, slot, name, name)

        # perf (c): 30 s streaming with concurrent moves
        print("[ds129] 30s streaming window with concurrent moves ...")
        runner.do_stream("07-stream-30s", "07-stream-30s", duration=30.0)

        # replug (all disconnect + reconnect)
        print("[ds129] replug")
        runner.do_replug("08-replug", "08-replug")

        # restore reference slots (each move verified like the others)
        for kind, slot in RESTORE_MOVES:
            print(f"[ds129] restore {kind} -> {slot}")
            runner.do_move(kind, slot, f"09-restore-{kind}-{slot}",
                           f"09-restore-{kind}-{slot}", expect_ok=True)

        st = api_state(VDEV_PORT, VDEV_TOKEN)
        exp, orient, devs = expected_from_state(st)
        dom = runner.snap()
        slots = vdev_slots()
        slots_ok = slots == REF_SLOTS
        runner.check_step("10-restored", "ds-vdevctl slots == reference", dom, exp,
                          extra_checks=[("vdev-slots==reference", slots_ok,
                                         f"slots={slots} expected={REF_SLOTS}")],
                          shot_name="10-restored")
    finally:
        # best-effort restore of the reference slots even on crash
        try:
            print("[ds129] finally-restore slots:", restore_ref_slots())
        except Exception as e:
            print("[ds129] finally-restore failed:", e)

    dom = runner.snap()
    runner.perf["final"] = {
        "total_render_count": dom["renderCount"],
        "total_flicker_events": dom["flicker"],
        "ws_dev_events_total": len(dom["wsDevTimes"]),
        "frame_responses_total": len(dom["frameLog"]),
    }
    print(f"[ds129] final perf: {runner.perf['final']}")
    browser.close()
    return runner


def run_live(pw, args):
    """Read-only sanity pass against the live real deck (port 43541)."""
    browser = launch_browser(pw)
    page = browser.new_page(viewport={"width": 1680, "height": 1500})
    page.add_init_script(INIT_SCRIPT)
    runner = Runner(page, LIVE_PORT, LIVE_TOKEN, SHOTS)
    url = f"{WEBAPP}?token={LIVE_TOKEN}&port={LIVE_PORT}"
    print(f"[ds129-live] opening {url} (READ-ONLY)")
    page.goto(url, wait_until="domcontentloaded")
    dom = runner.wait_connected()
    page.evaluate("window.__nore = 1;")
    dom = runner.snap()
    st = api_state(LIVE_PORT, LIVE_TOKEN)  # read-only GET
    exp, orient, devs = expected_from_state(st)
    disconnected = [(d["kind"], d.get("slot")) for d in devs if d["connection"] == "disconnected"]
    strip = strip_entries(dom["offText"])
    extra = [
        ("live-state-read", True, f"rev={st.get('rev')} devices={len(devs)} orientation={orient}"),
        ("disconnected-in-offdeck-strip",
         all(any(k == dk and s == ds and r == "disconnected" for (k, s, r) in strip)
             for dk, ds in disconnected),
         f"disconnected devices={disconnected} strip={dom['offText']!r}"),
    ]
    ok, problems = dom_matches(dom, exp)
    extra.append(("dom==solver(live)", ok, "; ".join(problems[:6])))
    runner.check_step("live-43541", "read-only open vs live /devices", dom, exp,
                      extra_checks=extra, shot_name="live")
    browser.close()
    return runner


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--vdev-only", action="store_true")
    ap.add_argument("--live-only", action="store_true")
    ap.add_argument("--out", default=os.environ.get("DS129_OUT"))
    args = ap.parse_args()

    from playwright.sync_api import sync_playwright
    runners = []
    with sync_playwright() as pw:
        if not args.live_only:
            runners.append(("vdev", run_vdev(pw, args)))
        if not args.vdev_only:
            runners.append(("live", run_live(pw, args)))

    all_ok = True
    print("\n================ DS-129 results ================")
    summary = {}
    for tag, runner in runners:
        print(f"\n--- {tag} (port {runner.port}) ---")
        for r in runner.results:
            status = "PASS" if r["ok"] else "FAIL"
            print(f"[{status}] {r['step']:<28} ({r['cmd']})")
            for c in r["checks"]:
                if not c["ok"]:
                    print(f"         FAIL {c['name']}: {c['detail']}")
            if r.get("perf"):
                print(f"         perf: {json.dumps(r['perf'])}")
            all_ok = all_ok and r["ok"]
        print(f"--- {tag} perf summary: {json.dumps(runner.perf)}")
        summary[tag] = {"ok": all(r["ok"] for r in runner.results),
                        "results": runner.results, "perf": runner.perf}
    print("\nDS-129 overall:", "PASS" if all_ok else "FAIL")
    payload = {"overall_ok": all_ok, "runs": summary}
    print("=== DS129 JSON ===")
    print(json.dumps(payload, indent=1))
    if args.out:
        with open(args.out, "w") as f:
            json.dump(payload, f, indent=1)
        print(f"[ds129] JSON written to {args.out}")
    return 0 if all_ok else 1


if __name__ == "__main__":
    sys.exit(main())

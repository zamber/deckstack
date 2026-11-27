#!/usr/bin/env python3
"""map_connectors.py — map MasterHub base connectors to USB port paths (DS-112 / SALON-001).

Workflow: move ONE module (e.g. the knob, USB 2516:0202) into a physical connector on
the base, then run `capture --label <connector-name>`. The tool reads `lsusb -t`, finds
the module's absolute port path (e.g. 3-2-1-4-3), converts it to the slot form relative
to the base's top hub (strip the leading <bus>-<rootport>, e.g. 1-4-3), and appends a
row to the map CSV. `render` draws the base with all mapped connectors and the currently
mounted modules, in both orientations (flat 4x6 / vertical 6x4).

Stdlib only, offline, no hardware writes — it only runs `lsusb -t`. The physical moves
are done by a human.

Subcommands:
  capture --label C07 [--pid 2516:0202] [--map MAP.csv]
  mounted [--map MAP.csv]        # list all Cooler Master modules with abs/rel path
  render  [--map MAP.csv] [--out deck-map.svg] [--ascii]
"""
import argparse, csv, datetime, json, re, subprocess, sys, os

CM_PIDS = {  # module kind by USB product id
    "01f6": "base", "01fe": "keys", "0200": "faders", "0202": "knobs",
    "0204": "rollers", "0206": "dial",
}

def lsusb_tree():
    out = subprocess.run(["lsusb", "-t"], capture_output=True, text=True).stdout
    # lines like: "    |__ Port 004: Dev 040, If 0, Class=Human Interface Device, ..."
    devs = []  # (depth, port, devnum)
    for line in out.splitlines():
        m = re.match(r"^(\s*)\|__ Port (\d+): Dev (\d+)", line)
        if m:
            devs.append((len(m.group(1)) // 2, int(m.group(2)), int(m.group(3))))
        elif re.match(r"^/:\s+Bus (\d+)", line):
            bus = int(re.search(r"Bus (\d+)", line).group(1))
            devs.append((-1, bus, 0))
    # build paths: walk stack; each entry's path = parent path + port
    paths = {}  # devnum -> (bus, path tuple)
    stack = []  # (depth, bus, path)
    bus = None
    for depth, port, dev in devs:
        if depth == -1:
            bus = port  # bus number stored in 'port' slot
            stack = [(-1, bus, ())]
            continue
        while stack and stack[-1][0] >= depth:
            stack.pop()
        parent = stack[-1]
        path = parent[2] + (port,)
        paths[dev] = (bus, path)
        stack.append((depth, bus, path))
    return paths

def lsusb_pids():
    out = subprocess.run(["lsusb"], capture_output=True, text=True).stdout
    res = {}
    for line in out.splitlines():
        m = re.match(r"Bus (\d+) Device (\d+): ID ([0-9a-f]{4}):([0-9a-f]{4})", line)
        if m and m.group(3) == "2516":
            res[int(m.group(2))] = (m.group(4), line.split("ID ", 1)[1])
    return res

def rel_path(path):
    # path excludes the bus; strip the root-hub port so the base MCU lands at its
    # documented 1-4-4 form (abs 3-2-1-4-4 -> rel 1-4-4)
    return "-".join(str(p) for p in path[1:]) if len(path) > 1 else "-".join(map(str, path))

def abs_path(bus, path):
    return f"{bus}-" + "-".join(map(str, path))

def cmd_capture(a):
    paths = lsusb_tree()
    pids = lsusb_pids()
    target = a.pid.lower()
    hit = None
    for dev, (pid, desc) in pids.items():
        if pid == target and dev in paths:
            hit = (dev, paths[dev], desc)
            break
    if not hit:
        print(f"no 2516:{target} device currently attached", file=sys.stderr)
        return 1
    dev, (bus, path), desc = hit
    kind = CM_PIDS.get(target, "?")
    row = {
        "ts": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "label": a.label, "usb_pid": f"2516:{target}", "module": kind,
        "abs_path": abs_path(bus, path), "rel_path": rel_path(path),
        "desc": desc,
    }
    exists = os.path.exists(a.map)
    with open(a.map, "a", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(row.keys()))
        if not exists:
            w.writeheader()
        w.writerow(row)
    print(json.dumps(row))
    return 0

def cmd_mounted(a):
    paths = lsusb_tree()
    pids = lsusb_pids()
    rows = []
    for dev, (pid, desc) in sorted(pids.items()):
        bus, path = paths[dev]
        rows.append({"module": CM_PIDS.get(pid, "?"), "usb_pid": f"2516:{pid}",
                     "abs_path": abs_path(bus, path), "rel_path": rel_path(path)})
    print(json.dumps(rows, indent=1))
    return 0

def load_map(a):
    rows = []
    if os.path.exists(a.map):
        with open(a.map) as f:
            rows = list(csv.DictReader(f))
    return rows

def cmd_render(a):
    rows = load_map(a)
    mounted = json.loads(subprocess.run(
        [sys.executable, __file__, "mounted"], capture_output=True, text=True).stdout)
    by_rel = {}
    for r in rows:
        by_rel.setdefault(r["rel_path"], []).append(r["label"])
    # connector positions: 26 connectors around the base perimeter, numbered clockwise
    # starting top-left (flat orientation). Perimeter of a 4x6 grid: 2*(4+6)-4 = 16 cells...
    # the 26 connectors include over-edge positions; we place 16 on the grid perimeter and
    # 10 just outside it, all labelled, and annotate each with its rel_path when mapped.
    W, H = 4, 6  # flat: 4 rows x 6 cols of cells; vertical swaps to 6x4
    def perimeter_positions(w, h, extra=10):
        pos = []
        for x in range(w): pos.append((x, -1))          # top edge, outside
        for y in range(h): pos.append((w, y))            # right edge
        for x in range(w - 1, -1, -1): pos.append((x, h))  # bottom edge
        for y in range(h - 1, -1, -1): pos.append((-1, y))  # left edge
        # extra over-edge connectors, interleaved outward
        for i in range(extra):
            side = i % 4
            if side == 0: pos.append((i // 4, -2))
            elif side == 1: pos.append((w + 1, i // 4))
            elif side == 2: pos.append((w - 1 - i // 4, h + 1))
            else: pos.append((-2, h - 1 - i // 4))
        return pos[:26]
    def svg(orient):
        w, h = (W, H) if orient == "flat" else (H, W)
        cw, ch = 90, 70
        ow, oh = 4 * cw, 3 * ch  # base slab with margin for edge connectors
        x0, y0 = 2 * cw, 2 * ch
        parts = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{ow+40}" height="{oh+60}" '
                 f'font-family="monospace" font-size="11">',
                 f'<rect x="{x0}" y="{y0}" width="{w*cw}" height="{h*ch}" fill="#1c1f26" stroke="#888"/>',
                 f'<text x="{x0}" y="{y0-8}" fill="#ccc">{orient} orientation — grid {w}x{h} '
                 f'({"4x6" if orient=="flat" else "6x4"} cells)</text>']
        # grid cells
        for gy in range(h):
            for gx in range(w):
                parts.append(f'<rect x="{x0+gx*cw}" y="{y0+gy*ch}" width="{cw-4}" height="{ch-4}" '
                             f'fill="none" stroke="#333"/>')
        # connectors
        for i, (cx, cy) in enumerate(perimeter_positions(w, h)):
            label = f"C{i+1:02d}"
            px, py = x0 + cx * cw + cw // 2, y0 + cy * ch + ch // 2
            parts.append(f'<circle cx="{px}" cy="{py}" r="9" fill="#2d333b" stroke="#adbac7"/>')
            parts.append(f'<text x="{px-12}" y="{py+4}" fill="#adbac7">{label}</text>')
        # rel_path annotations for mapped connectors (match by capture order is not
        # physical; we annotate modules at their rel path via the reference slot table
        # subset and any captured rows)
        for m in mounted:
            rel = m["rel_path"]
            parts.append(f'<text x="{x0+8}" y="{y0+h*ch+18}" fill="#7ee787">'
                         f'{m["module"]} @ {rel} (abs {m["abs_path"]})</text>')
            y0 += 0
        parts.append("</svg>")
        return "\n".join(parts)
    if a.ascii:
        for orient in ("flat", "vertical"):
            w, h = (W, H) if orient == "flat" else (H, W)
            print(f"--- {orient} ({w}x{h}) ---")
            grid = [["." for _ in range(w)] for _ in range(h + 2)]
            for i, (cx, cy) in enumerate(perimeter_positions(w, h)):
                if 0 <= cy + 1 < h + 2 and 0 <= cx < w:
                    grid[cy + 1][cx] = f"C{i+1:02d}"[-1]
            for row in grid:
                print(" ".join(row))
            for m in mounted:
                print(f'  {m["module"]:7s} rel={m["rel_path"]:8s} abs={m["abs_path"]}')
            print(f"  mapped connectors: {len(rows)}")
            for r in rows:
                print(f'  {r["label"]:6s} {r["module"]:7s} rel={r["rel_path"]:8s} abs={r["abs_path"]}')
    if a.out:
        with open(a.out, "w") as f:
            f.write(svg("flat") + "\n<!-- vertical -->\n" + svg("vertical"))
        print(f"wrote {a.out}")
    return 0


def cmd_watch(a):
    import time
    last=None
    print(f"watching 2516:{a.pid} every {a.interval}s -> {a.map} (Ctrl-C to stop)", flush=True)
    while True:
        try:
            paths=lsusb_tree(); pids=lsusb_pids()
            hit=None
            for dev,(pid,desc) in pids.items():
                if pid==a.pid.lower() and dev in paths:
                    hit=(dev,paths[dev]); break
            rel = rel_path(hit[1][1]) if hit else None
            ab = abs_path(*hit[1]) if hit else None
            if rel!=last:
                row={"ts":datetime.datetime.now(datetime.timezone.utc).isoformat(),
                     "label":a.label or "", "usb_pid":f"2516:{a.pid}",
                     "module":CM_PIDS.get(a.pid.lower(),"?"),
                     "abs_path":ab or "", "rel_path":rel or "", "desc":"auto-watch"}
                exists=os.path.exists(a.map)
                with open(a.map,"a",newline="") as f:
                    w=csv.DictWriter(f,fieldnames=list(row.keys()))
                    if not exists: w.writeheader()
                    w.writerow(row)
                print(json.dumps(row), flush=True)
                last=rel
        except Exception as e:
            print("poll error:",e,file=sys.stderr,flush=True)
        time.sleep(a.interval)

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("capture"); c.add_argument("--label", required=True)
    c.add_argument("--pid", default="0202"); c.add_argument("--map", default="/tmp/ds112-hw/connector-map.csv")
    c.set_defaults(fn=cmd_capture)
    m = sub.add_parser("mounted"); m.set_defaults(fn=cmd_mounted)
    w = sub.add_parser("watch"); w.add_argument("--pid", default="0202")
    w.add_argument("--map", default="/tmp/ds112-hw/connector-map.csv")
    w.add_argument("--interval", type=float, default=2.0); w.add_argument("--label", default="")
    w.set_defaults(fn=cmd_watch)
    r = sub.add_parser("render"); r.add_argument("--map", default="/tmp/ds112-hw/connector-map.csv")
    r.add_argument("--out"); r.add_argument("--ascii", action="store_true")
    r.set_defaults(fn=cmd_render)
    a = ap.parse_args()
    return a.fn(a)

if __name__ == "__main__":
    sys.exit(main())

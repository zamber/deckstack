#!/usr/bin/env python3
"""deckstack-verify — camera harness for pixel-level verification of the deck (DS-027).

Python 3 stdlib only; external tools: ffmpeg / ffprobe / v4l2-ctl (argv lists only,
never a shell). The device has no screen readback, so the camera is the pixel oracle
(docs/90-verification.md): this tool discovers the camera, captures frames, calibrates
the key grid / panel / LED zones from a captured frame, and compares captured pixels
against an expected spec with a tolerance.

Subcommands:
  devices                              list video devices (v4l2-ctl) + pick the default
  capture --out f.jpg [...]            one still (or a short clip / frame burst) via ffmpeg
  calibrate --frame f.jpg --out cal.json [--overlay o.png] [--grid CxR] [--panel x,y,w,h]
  compare --expected <spec> --actual f.jpg [--calibration cal.json] [--tolerance N] [--json]
  check --target key:0 --expected ...  capture + compare in one step
  selftest                             full pipeline on a synthetic frame (+ camera if present)

Exit codes: 0 ok · 2 usage · 6 camera/device unreachable · 7 verification mismatch
(docs/90-verification.md §5 reuses the E_DEVICE-class exit 7 for "verification failed").

Expected-spec grammar for compare:
  inline   "key:0=#ff0000,led:0=#0000ff"        target=#rrggbb list
  json     {"targets": {"key:0": "#ff0000"}, "tolerance": 12}   (or a bare map)
  golden   path to a .png/.jpg golden image     (per-target ROI means compared)

Targets: key:0..key:14, keys (480x272 panel), dial (240x240), led:N (calibrated LED
zones), frame (whole image). Key windows on the device panel are 72x72 at
x=5,104,203,303,402 / y=1,101,199 (docs/contracts/streaming.md §1,
upstream/PROTOCOL.md:81).
"""

import argparse
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import zlib
from pathlib import Path

EXIT_OK = 0
EXIT_USAGE = 2
EXIT_UNREACHABLE = 6
EXIT_MISMATCH = 7

TOOL = "deckstack_verify"

# Device geometry (docs/contracts/streaming.md §1).
PANEL_W, PANEL_H = 480, 272
KEY_SIZE = 72
KEY_X = [5, 104, 203, 303, 402]
KEY_Y = [1, 101, 199]
N_KEYS = 15

DEFAULT_DEVICE = os.environ.get("DSV_CAMERA", "/dev/video2")
DEFAULT_WIDTH, DEFAULT_HEIGHT = 1280, 720
DEFAULT_TOLERANCE = 12.0

SCRIPT_DIR = Path(__file__).resolve().parent
DEFAULT_CALIBRATION = SCRIPT_DIR / "calibration.json"


def eprint(*a):
    print(*a, file=sys.stderr)


def run(argv, timeout=60, capture=True):
    """Run an argv list (no shell). Returns CompletedProcess."""
    return subprocess.run(
        argv, capture_output=capture, text=True, timeout=timeout,
        stdin=subprocess.DEVNULL,
    )


def which(name):
    return shutil.which(name)


# --------------------------------------------------------------------- image ----


class Image:
    """RGB24 image held as raw bytes (ffmpeg decodes whatever we capture)."""

    def __init__(self, w, h, px):
        self.w = w
        self.h = h
        self.px = px  # bytes, len == w*h*3

    def pixel(self, x, y):
        k = (y * self.w + x) * 3
        return self.px[k], self.px[k + 1], self.px[k + 2]

    def mean(self, x0, y0, x1, y1, step=1):
        """Mean RGB over the rect [x0,x1) x [y0,y1), sampled with `step`."""
        x0, y0 = max(0, int(x0)), max(0, int(y0))
        x1, y1 = min(self.w, int(x1)), min(self.h, int(y1))
        if x1 <= x0 or y1 <= y0:
            raise ValueError("empty rect %r" % ((x0, y0, x1, y1),))
        rs = gs = bs = n = 0
        px, w = self.px, self.w
        for y in range(y0, y1, max(1, step)):
            base = y * w * 3
            for x in range(x0, x1, max(1, step)):
                k = base + x * 3
                rs += px[k]
                gs += px[k + 1]
                bs += px[k + 2]
                n += 1
        if n == 0:
            raise ValueError("empty rect sample")
        return (rs / n, gs / n, bs / n)

    def rect_stats(self, x0, y0, x1, y1, step=2):
        """(mean rgb, mean luma, luma std) over a rect."""
        x0, y0 = max(0, int(x0)), max(0, int(y0))
        x1, y1 = min(self.w, int(x1)), min(self.h, int(y1))
        rs = gs = bs = n = 0
        ls = 0.0
        l2 = 0.0
        px, w = self.px, self.w
        for y in range(y0, y1, max(1, step)):
            base = y * w * 3
            for x in range(x0, x1, max(1, step)):
                k = base + x * 3
                r, g, b = px[k], px[k + 1], px[k + 2]
                rs += r
                gs += g
                bs += b
                l = (r * 299 + g * 587 + b * 114) / 1000.0
                ls += l
                l2 += l * l
                n += 1
        if n == 0:
            raise ValueError("empty rect")
        mean = (rs / n, gs / n, bs / n)
        ml = ls / n
        var = max(0.0, l2 / n - ml * ml)
        return mean, ml, var ** 0.5

    def copy(self):
        return Image(self.w, self.h, bytearray(self.px))


def ffmpeg_dims(path, timeout=60):
    """(width, height) of the first video stream via ffprobe (ffmpeg -i fallback)."""
    ffprobe = which("ffprobe")
    if ffprobe:
        p = run([ffprobe, "-v", "error", "-select_streams", "v:0",
                 "-show_entries", "stream=width,height", "-of", "csv=p=0", str(path)],
                timeout=timeout)
        m = re.search(r"(\d+),(\d+)", p.stdout or "")
        if m:
            return int(m.group(1)), int(m.group(2))
    ffmpeg = which("ffmpeg")
    if not ffmpeg:
        raise RuntimeError("neither ffprobe nor ffmpeg found on PATH")
    p = run([ffmpeg, "-hide_banner", "-i", str(path)], timeout=timeout)
    m = re.search(r"Video:.*?(\d{2,5})x(\d{2,5})", p.stderr or "")
    if not m:
        raise RuntimeError("cannot determine dimensions of %s (ffmpeg said: %s)"
                           % (path, (p.stderr or "").strip()[-200:]))
    return int(m.group(1)), int(m.group(2))


def load_image(path, timeout=120):
    """Decode any image ffmpeg can read into an RGB24 Image (rawvideo, no shell)."""
    path = str(path)
    ffmpeg = which("ffmpeg")
    if not ffmpeg:
        raise RuntimeError("ffmpeg not found on PATH — required to decode %s" % path)
    w, h = ffmpeg_dims(path, timeout=timeout)
    p = subprocess.run(
        [ffmpeg, "-hide_banner", "-loglevel", "error", "-i", path, "-frames:v", "1",
         "-f", "rawvideo", "-pix_fmt", "rgb24", "-"],
        capture_output=True, timeout=timeout,
    )
    if p.returncode != 0 or len(p.stdout) < w * h * 3:
        raise RuntimeError(
            "ffmpeg could not decode %s (rc=%d, got %d bytes, want %d): %s"
            % (path, p.returncode, len(p.stdout), w * h * 3,
               p.stderr.decode("utf-8", "replace")[-300:])
        )
    return Image(w, h, p.stdout[: w * h * 3])


def png_write(path, img):
    """Write an RGB24 Image as an 8-bit non-interlaced PNG (stdlib zlib)."""
    w, h, px = img.w, img.h, img.px
    stride = w * 3
    raw = bytearray()
    for y in range(h):
        raw.append(0)  # filter type None
        raw += px[y * stride:(y + 1) * stride]

    def chunk(typ, data):
        return (struct.pack(">I", len(data)) + typ + data
                + struct.pack(">I", zlib.crc32(typ + data) & 0xFFFFFFFF))

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)
    out = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(bytes(raw), 6)) + chunk(b"IEND", b"")
    Path(path).write_bytes(out)


def draw_rect(img, x0, y0, x1, y1, color, thickness=2):
    """Outline a rect in-place on an Image (mutable bytearray pixels)."""
    px, w = img.px, img.w
    x0, y0 = int(x0), int(y0)
    x1, y1 = min(img.w - 1, int(x1)), min(img.h - 1, int(y1))
    r, g, b = color
    for t in range(thickness):
        for x in range(x0, x1 + 1):
            for y in (y0 + t, y1 - t):
                if 0 <= y < img.h:
                    k = (y * w + x) * 3
                    px[k], px[k + 1], px[k + 2] = r, g, b
        for y in range(y0, y1 + 1):
            for x in (x0 + t, x1 - t):
                if 0 <= x < img.w:
                    k = (y * w + x) * 3
                    px[k], px[k + 1], px[k + 2] = r, g, b


# ------------------------------------------------------------------- devices ----


def parse_list_devices(text):
    """Parse `v4l2-ctl --list-devices` output into [{name, bus, nodes:[...]}, ...]."""
    groups = []
    cur = None
    for line in text.splitlines():
        if not line.strip():
            continue
        if not line.startswith(("\t", " ", "\t")):
            m = re.match(r"^(.*?)\s*\((.*?)\):\s*$", line)
            if m:
                cur = {"name": m.group(1).strip(), "bus": m.group(2).strip(), "nodes": []}
                groups.append(cur)
            continue
        node = line.strip()
        if cur is not None and node.startswith("/dev/"):
            cur["nodes"].append(node)
    return groups


def device_info(node, timeout=30):
    """driver/card/bus for one node via `v4l2-ctl -d node --info`."""
    v4l2 = which("v4l2-ctl")
    if not v4l2:
        return {}
    p = run([v4l2, "-d", node, "--info"], timeout=timeout)
    info = {}
    for line in (p.stdout or "").splitlines():
        m = re.match(r"\s*(Driver name|Card type|Bus info)\s*:\s*(.*)$", line)
        if m:
            info[m.group(1)] = m.group(2).strip()
    return info


def list_video_devices():
    """All /dev/videoN with driver/card/bus, plus the chosen default node."""
    v4l2 = which("v4l2-ctl")
    if not v4l2:
        raise RuntimeError("v4l2-ctl not found on PATH — cannot enumerate cameras")
    p = run([v4l2, "--list-devices"], timeout=60)
    if p.returncode != 0:
        raise RuntimeError("v4l2-ctl --list-devices failed rc=%d: %s"
                           % (p.returncode, (p.stderr or "").strip()[-300:]))
    groups = parse_list_devices(p.stdout or "")
    devices = []
    for g in groups:
        for node in g["nodes"]:
            if re.match(r"^/dev/video\d+$", node):
                info = device_info(node)
                devices.append({
                    "node": node,
                    "name": g["name"],
                    "bus": g["bus"],
                    "driver": info.get("Driver name", ""),
                    "card": info.get("Card type", g["name"]),
                    "bus_info": info.get("Bus info", g["bus"]),
                    "loopback": "v4l2loopback" in (g["bus"] or "") or "OBS" in g["name"],
                })
    default = pick_default_device(devices)
    return devices, default


def pick_default_device(devices):
    """Default camera: $DSV_CAMERA, else /dev/video2, else first non-loopback node."""
    if os.environ.get("DSV_CAMERA"):
        return os.environ["DSV_CAMERA"]
    nodes = [d["node"] for d in devices]
    if "/dev/video2" in nodes:
        return "/dev/video2"
    real = [d["node"] for d in devices if not d["loopback"]]
    if real:
        return real[0]
    return nodes[0] if nodes else None


def cmd_devices(args):
    devices, default = list_video_devices()
    if args.json:
        print(json.dumps({"default": default, "devices": devices}, indent=1))
        return EXIT_OK
    if not devices:
        eprint("no /dev/videoN nodes found")
        return EXIT_UNREACHABLE
    print("video devices (v4l2-ctl --list-devices + --info):")
    for d in devices:
        print("  %-12s driver=%-8s bus=%-22s card=%s%s"
              % (d["node"], d["driver"], d["bus_info"], d["card"],
                 "  [loopback/virtual]" if d["loopback"] else ""))
    print("default camera: %s  (override: --device PATH or $DSV_CAMERA)" % default)
    return EXIT_OK


# ------------------------------------------------------------------- capture ----


def capture_frame(device, out, width, height, frames=1, settle_ms=0, q=3,
                  timeout=45, retries=3, input_format="mjpeg"):
    """Capture via ffmpeg (argv-only). Returns (path, size). Raises on failure."""
    ffmpeg = which("ffmpeg")
    if not ffmpeg:
        raise RuntimeError("ffmpeg not found on PATH — cannot capture")
    out = str(out)
    if not os.path.exists(device):
        raise RuntimeError("device node %s does not exist" % device)
    # warm-up: let auto-exposure settle, discarding one frame
    if settle_ms > 0:
        try:
            run([ffmpeg, "-hide_banner", "-loglevel", "error", "-f", "v4l2",
                 "-input_format", input_format, "-video_size", "%dx%d" % (width, height),
                 "-i", device, "-frames:v", "1", "-f", "null", "-"], timeout=timeout)
        except subprocess.TimeoutExpired:
            pass
        time.sleep(settle_ms / 1000.0)
    ext = os.path.splitext(out)[1].lower()
    if frames > 1 and "%" not in out:
        root, e = os.path.splitext(out)
        out = "%s-%%03d%s" % (root, e)
    last_err = ""
    for attempt in range(1, retries + 1):
        if ext == ".png":
            vcodec = ["-c:v", "png", "-update", "1"]
        elif ext in (".avi",):
            vcodec = ["-c:v", "mjpeg", "-q:v", str(q)]
        else:
            # mjpeg with full-range pix_fmt: some cameras emit limited-range YUV,
            # which the mjpeg encoder refuses without this
            vcodec = ["-pix_fmt", "yuvj420p", "-q:v", str(q), "-update", "1"]
        argv = [ffmpeg, "-hide_banner", "-loglevel", "error",
                "-f", "v4l2", "-input_format", input_format,
                "-video_size", "%dx%d" % (width, height),
                "-i", device, "-frames:v", str(frames)] + vcodec + ["-y", out]
        try:
            p = run(argv, timeout=timeout)
        except subprocess.TimeoutExpired as e:
            last_err = "timeout after %ss (attempt %d)" % (timeout, attempt)
            eprint("capture attempt %d/%d: %s" % (attempt, retries, last_err))
            continue
        if p.returncode == 0 and os.path.exists(out) and os.path.getsize(out) > 0:
            return out, os.path.getsize(out)
        last_err = "rc=%d stderr=%s" % (p.returncode, (p.stderr or "").strip()[-300:])
        eprint("capture attempt %d/%d failed: %s" % (attempt, retries, last_err))
    raise RuntimeError("ffmpeg capture from %s failed after %d attempt(s): %s"
                       % (device, retries, last_err))


def cmd_capture(args):
    device = args.device or DEFAULT_DEVICE
    if args.clip:
        out = args.out
        ext = os.path.splitext(out)[1].lower()
        if ext != ".avi":
            eprint("note: --clip writes MJPEG .avi (portable); got %s" % ext)
        path, size = capture_clip(device, out, args.clip, args.width, args.height,
                                  args.settle, timeout=args.timeout)
    else:
        path, size = capture_frame(device, args.out, args.width, args.height,
                                   frames=args.frames, settle_ms=args.settle,
                                   timeout=args.timeout)
    if args.json:
        print(json.dumps({"device": device, "out": path, "bytes": size,
                          "width": args.width, "height": args.height}))
    else:
        print("captured %s (%d bytes, %dx%d) from %s"
              % (path, size, args.width, args.height, device))
    return EXIT_OK


def capture_clip(device, out, seconds, width, height, settle_ms=0, timeout=60):
    ffmpeg = which("ffmpeg")
    if not ffmpeg:
        raise RuntimeError("ffmpeg not found on PATH")
    if settle_ms > 0:
        time.sleep(settle_ms / 1000.0)
    argv = [ffmpeg, "-hide_banner", "-loglevel", "error", "-f", "v4l2",
            "-input_format", "mjpeg", "-video_size", "%dx%d" % (width, height),
            "-i", device, "-t", str(seconds), "-c:v", "mjpeg", "-q:v", "4", "-y", str(out)]
    p = run(argv, timeout=timeout + int(seconds) * 5)
    if p.returncode != 0 or not os.path.exists(out) or os.path.getsize(out) == 0:
        raise RuntimeError("ffmpeg clip capture failed rc=%d: %s"
                           % (p.returncode, (p.stderr or "").strip()[-300:]))
    return out, os.path.getsize(out)


# ----------------------------------------------------------------- calibrate ----


def cell_grid(img, cell):
    """Coarse grid of per-cell (mean rgb, luma, sat). Returns (cols, rows, cells)."""
    cols = max(1, img.w // cell)
    rows = max(1, img.h // cell)
    cells = []
    px, w = img.px, img.w
    for cy in range(rows):
        row = []
        for cx in range(cols):
            x0, y0 = cx * cell, cy * cell
            x1, y1 = min(img.w, x0 + cell), min(img.h, y0 + cell)
            rs = gs = bs = n = 0
            for y in range(y0, y1, 2):
                base = y * w * 3
                for x in range(x0, x1, 2):
                    k = base + x * 3
                    rs += px[k]
                    gs += px[k + 1]
                    bs += px[k + 2]
                    n += 1
            if n == 0:
                n = 1
            r, g, b = rs / n, gs / n, bs / n
            luma = (r * 299 + g * 587 + b * 114) / 1000.0
            sat = max(r, g, b) - min(r, g, b)
            row.append({"rgb": (r, g, b), "luma": luma, "sat": sat})
        cells.append(row)
    return cols, rows, cells


def components(mask, cols, rows, min_cells):
    """8-connected components of True cells; returns list of (cells, bbox) sorted by size."""
    seen = [[False] * cols for _ in range(rows)]
    comps = []
    for y in range(rows):
        for x in range(cols):
            if not mask[y][x] or seen[y][x]:
                continue
            stack = [(x, y)]
            seen[y][x] = True
            cells = []
            while stack:
                cx, cy = stack.pop()
                cells.append((cx, cy))
                for dx in (-1, 0, 1):
                    for dy in (-1, 0, 1):
                        nx, ny = cx + dx, cy + dy
                        if 0 <= nx < cols and 0 <= ny < rows and mask[ny][nx] and not seen[ny][nx]:
                            seen[ny][nx] = True
                            stack.append((nx, ny))
            if len(cells) >= min_cells:
                xs = [c[0] for c in cells]
                ys = [c[1] for c in cells]
                comps.append({
                    "cells": cells,
                    "n": len(cells),
                    "bbox": (min(xs), min(ys), max(xs), max(ys)),
                    "center": (sum(xs) / len(xs), sum(ys) / len(ys)),
                })
    comps.sort(key=lambda c: -c["n"])
    return comps


def calibrate_core(img, grid_hint=None, panel_hint=None, orientation=None,
                   face_luma=None, detect_leds=True, verbose=False):
    """Detect panel + key grid + LED zones in an Image.

    Returns a calibration dict (JSON-serialisable). Assumptions (documented in
    README): roughly frontal camera (axis-aligned affine device->image mapping);
    key faces are brighter or more saturated than the bezel gaps and form the
    N_KEYS largest lit blobs (or one big panel blob); LED zones are saturated
    blobs outside the panel.
    """
    cell = max(4, min(img.w, img.h) // 180)
    cols, rows, cells = cell_grid(img, cell)
    # background luma = median of border cells
    border = [cells[0][x]["luma"] for x in range(cols)] + \
             [cells[rows - 1][x]["luma"] for x in range(cols)] + \
             [cells[y][0]["luma"] for y in range(rows)] + \
             [cells[y][cols - 1]["luma"] for y in range(rows)]
    border.sort()
    bg = border[len(border) // 2] if border else 0
    if face_luma:
        lo, hi = face_luma
    elif bg < 80:
        # dark room, lit panel: faces are the bright / saturated regions
        lo, hi = 110, 255
    else:
        # bright room: key faces are dimmer than the walls but not black
        lo, hi = max(50, bg - 95), max(60, bg - 45)
    lit = lambda c: (lo <= c["luma"] <= hi) or c["sat"] >= 60
    mask = [[lit(cells[y][x]) for x in range(cols)] for y in range(rows)]
    min_cells = max(3, 9)
    comps = components(mask, cols, rows, min_cells)
    if verbose:
        eprint("calibrate: bg_luma=%.0f face=[%.0f,%.0f] cells=%dx%d comps=%d"
               % (bg, lo, hi, cols, rows, len(comps)))

    def plausible(sel_cells):
        """15 blobs forming a compact, regularly-spaced panel grid."""
        if len(sel_cells) != N_KEYS:
            return False
        xs = [c["bbox"][0] for c in sel_cells] + [c["bbox"][2] for c in sel_cells]
        ys = [c["bbox"][1] for c in sel_cells] + [c["bbox"][3] for c in sel_cells]
        w = max(xs) - min(xs) + 1
        h = max(ys) - min(ys) + 1
        if w < 2 or h < 2:
            return False
        aspect = w / h
        fill = sum(c["n"] for c in sel_cells) / float(w * h)
        if not (0.3 <= aspect <= 3.0 and 0.25 <= fill <= 0.98):
            return False
        # grid-ness: every blob has a nearest neighbour at a similar pitch
        cents = [c["center"] for c in sel_cells]
        nn = []
        for i, a in enumerate(cents):
            d = min((a[0] - b[0]) ** 2 + (a[1] - b[1]) ** 2
                    for j, b in enumerate(cents) if j != i) ** 0.5
            nn.append(d)
        nn.sort()
        med = nn[len(nn) // 2]
        if med < 2:
            return False
        in_band = sum(1 for d in nn if 0.6 * med <= d <= 1.6 * med)
        return in_band >= N_KEYS - 3

    def select_faces(cands):
        """Pick the N_KEYS face blobs: sizes near the median of the big blobs."""
        if len(cands) < N_KEYS:
            return None
        big = sorted(cands, key=lambda c: -c["n"])[:40]
        sizes = sorted(c["n"] for c in big)
        med = sizes[len(sizes) // 2]
        near = [c for c in big if 0.55 * med <= c["n"] <= 1.8 * med]
        if len(near) < N_KEYS:
            near = big
        near.sort(key=lambda c: abs(c["n"] - med))
        sel = near[:N_KEYS]
        return sel if plausible(sel) else None

    hint_cols, hint_rows = grid_hint or (5, 3)
    targets = {}
    panel_rect = None
    sel = None
    if panel_hint:
        px0, py0, pw, ph = panel_hint
        panel_rect = (px0, py0, px0 + pw, py0 + ph)
    else:
        sel = select_faces(comps)
        if sel is None and bg >= 80:
            # bright room: the deck housing is a dark blob; the key faces are
            # the lit blobs inside it
            dark_thr = max(60, bg - 110)
            dmask = [[cells[y][x]["luma"] < dark_thr for x in range(cols)]
                     for y in range(rows)]
            dcomps = components(dmask, cols, rows, min_cells * 4)
            for d in dcomps[:5]:
                dx0, dy0, dx1, dy1 = d["bbox"]
                inside = [c for c in comps
                          if dx0 <= c["center"][0] <= dx1 and dy0 <= c["center"][1] <= dy1]
                sel = select_faces(inside)
                if sel is not None:
                    break
        if sel is not None:
            if verbose:
                eprint("calibrate: sel=%d sizes=%s bboxes=%s" % (
                    len(sel), [c["n"] for c in sel],
                    [c["bbox"] for c in sel][:3]))
            x0 = min(c["bbox"][0] for c in sel) * cell
            y0 = min(c["bbox"][1] for c in sel) * cell
            x1 = (max(c["bbox"][2] for c in sel) + 1) * cell
            y1 = (max(c["bbox"][3] for c in sel) + 1) * cell
            panel_rect = (x0, y0, min(img.w, x1), min(img.h, y1))
        elif comps:
            b = comps[0]["bbox"]
            panel_rect = (b[0] * cell, b[1] * cell,
                          min(img.w, (b[2] + 1) * cell), min(img.h, (b[3] + 1) * cell))
    if panel_rect is None:
        raise RuntimeError(
            "calibrate: no panel/grid detected (bg_luma=%.0f, %d components). "
            "Pass --panel x,y,w,h measured from a debug frame, or --grid CxR / "
            "--face-luma lo,hi; see tools/verify/README.md (lighting limits)."
            % (bg, len(comps)))

    px0, py0, px1, py1 = panel_rect
    pw, ph = px1 - px0, py1 - py0
    aspect = pw / ph if ph else 1
    # image-orientation grid: device panel is 480x272 landscape with a 5x3 key
    # grid; a landscape image panel -> 5 cols x 3 rows, portrait -> rotated 90deg
    if orientation is None:
        orientation = 0 if aspect >= 1.0 else 90
    if orientation in (0, 180):
        gcols, grows = hint_cols, hint_rows
    else:
        gcols, grows = hint_rows, hint_cols
    dev_cols, dev_rows = 5, 3
    cell_w = pw / gcols
    cell_h = ph / grows
    key_rects = {}
    # image-grid order of the detected face blobs (row-major in image space)
    img_grid = None
    if sel is not None and len(sel) == N_KEYS:
        img_grid = sorted(sel, key=lambda c: (c["center"][1], c["center"][0]))
    for n in range(N_KEYS):
        dc, dr = n % dev_cols, n // dev_cols
        if orientation == 0:
            gc, gr = dc, dr
        elif orientation == 90:
            gc, gr = dr, dc
        elif orientation == 180:
            gc, gr = dev_cols - 1 - dc, dev_rows - 1 - dr
        else:  # 270
            gc, gr = dev_rows - 1 - dr, dev_cols - 1 - dc
        if img_grid is not None:
            # per-face blob bbox, shrunk 20% per side (stay inside the face)
            c = img_grid[gr * gcols + gc]
            bx0, by0, bx1, by1 = c["bbox"]
            fx0 = (bx0 + 0.2 * (bx1 - bx0)) * cell
            fy0 = (by0 + 0.2 * (by1 - by0)) * cell
            fx1 = (bx1 + 1 - 0.2 * (bx1 - bx0 + 1)) * cell
            fy1 = (by1 + 1 - 0.2 * (by1 - by0 + 1)) * cell
            key_rects["key:%d" % n] = [round(fx0), round(fy0), round(fx1), round(fy1)]
        else:
            kx0 = px0 + gc * cell_w + cell_w * 0.25
            ky0 = py0 + gr * cell_h + cell_h * 0.25
            kx1 = px0 + (gc + 1) * cell_w - cell_w * 0.25
            ky1 = py0 + (gr + 1) * cell_h - cell_h * 0.25
            key_rects["key:%d" % n] = [round(kx0), round(ky0), round(kx1), round(ky1)]
    targets["keys"] = [round(px0), round(py0), round(px1), round(py1)]
    targets.update(key_rects)

    # LED zones: saturated blobs outside the panel rect
    if detect_leds:
        smask = [[cells[y][x]["sat"] >= 70 and max(cells[y][x]["rgb"]) >= 90
                  for x in range(cols)] for y in range(rows)]
        zcomps = components(smask, cols, rows, max(4, min_cells))
        zcomps = [c for c in zcomps if c["n"] >= 4]
        # drop zones whose centre is inside the panel (saturated key faces live there)
        def overlaps_panel(c):
            cx, cy = c["center"]
            return px0 / cell <= cx <= px1 / cell and py0 / cell <= cy <= py1 / cell
        zcomps = [c for c in zcomps if not overlaps_panel(c)]
        zcomps.sort(key=lambda c: (c["center"][1], c["center"][0]))
        for i, c in enumerate(zcomps[:8]):
            x0, y0, x1, y1 = c["bbox"]
            fx0 = x0 * cell
            fy0 = y0 * cell
            fx1 = (x1 + 1) * cell
            fy1 = (y1 + 1) * cell
            # keep the blob's own cells: an LED ring is an annulus, so a plain
            # bbox mean would be diluted by the dark centre/corners
            targets["led:%d" % i] = {
                "rect": [round(fx0), round(fy0), round(fx1), round(fy1)],
                "cells": [[cx, cy] for cx, cy in c["cells"]],
            }

    cal = {
        "version": 1,
        "tool": TOOL,
        "created": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "frame": {"width": img.w, "height": img.h},
        "cell": cell,
        "panel": {"rect": [round(px0), round(py0), round(px1), round(py1)],
                  "device_size": [PANEL_W, PANEL_H]},
        "grid": {"image_cols": gcols, "image_rows": grows,
                 "device_cols": dev_cols, "device_rows": dev_rows,
                 "orientation_deg": orientation},
        "assumptions": [
            "roughly frontal camera: device-panel -> image mapping is axis-aligned affine",
            "key faces are brighter or more saturated than the bezel gaps",
            "key windows on the device panel are 72x72 at x=5,104,203,303,402 / y=1,101,199",
            "detected background luma %.0f (face band [%.0f,%.0f])" % (bg, lo, hi),
        ],
        "targets": targets,
    }
    return cal


def overlay_write(img, cal, path):
    ov = img.copy()
    ov.px = bytearray(ov.px)
    t = cal["targets"]
    draw_rect(ov, *t["keys"], (255, 80, 80), 3)
    for name, rect in sorted(t.items()):
        if isinstance(rect, dict):
            rect = rect["rect"]
        if name.startswith("key:"):
            draw_rect(ov, *rect, (80, 255, 80), 2)
        elif name.startswith("led:"):
            draw_rect(ov, *rect, (80, 160, 255), 2)
    png_write(path, ov)


def cmd_calibrate(args):
    grid_hint = None
    if args.grid:
        m = re.match(r"^(\d+)x(\d+)$", args.grid.strip())
        if not m:
            eprint("--grid must be CxR, e.g. 5x3")
            return EXIT_USAGE
        grid_hint = (int(m.group(1)), int(m.group(2)))
    panel_hint = None
    if args.panel:
        m = re.match(r"^(\d+),(\d+),(\d+),(\d+)$", args.panel.strip())
        if not m:
            eprint("--panel must be x,y,w,h in pixels")
            return EXIT_USAGE
        x, y, w, h = (int(m.group(i)) for i in range(1, 5))
        panel_hint = (x, y, w, h)
    orientation = args.orientation
    face_luma = None
    if args.face_luma:
        m = re.match(r"^(\d+),(\d+)$", args.face_luma.strip())
        if not m:
            eprint("--face-luma must be lo,hi")
            return EXIT_USAGE
        face_luma = (int(m.group(1)), int(m.group(2)))
    frame = args.frame
    if not os.path.exists(frame):
        eprint("frame not found: %s" % frame)
        return EXIT_USAGE
    img = load_image(frame)
    cal = calibrate_core(img, grid_hint=grid_hint, panel_hint=panel_hint,
                         orientation=orientation, face_luma=face_luma, verbose=True)
    out = args.out
    Path(out).write_text(json.dumps(cal, indent=1) + "\n")
    if args.overlay:
        overlay_write(img, cal, args.overlay)
        print("overlay: %s" % args.overlay)
    n_keys = len([k for k in cal["targets"] if k.startswith("key:")])
    n_leds = len([k for k in cal["targets"] if k.startswith("led:")])
    print("calibration written: %s" % out)
    print("  frame %dx%d, panel rect %s, grid %dx%d (image) orientation %d deg"
          % (img.w, img.h, cal["panel"]["rect"], cal["grid"]["image_cols"],
             cal["grid"]["image_rows"], cal["grid"]["orientation_deg"]))
    print("  targets: %d key windows, keys (panel), %d led zones" % (n_keys, n_leds))
    if args.json:
        print(json.dumps(cal))
    return EXIT_OK


# -------------------------------------------------------------------- compare ----


def parse_hex_color(v):
    v = str(v).strip()
    m = re.match(r"^#?([0-9a-fA-F]{6})$", v)
    if not m:
        raise ValueError("not a #rrggbb colour: %r" % v)
    h = m.group(1)
    return tuple(int(h[i:i + 2], 16) for i in (0, 2, 4))


def parse_expected(spec):
    """-> (targets: {name: (r,g,b)}, golden_path or None, tolerance or None)."""
    spec = str(spec)
    if os.path.isfile(spec):
        if spec.lower().endswith(".json"):
            d = json.loads(Path(spec).read_text())
            tol = d.get("tolerance")
            t = d.get("targets", d)
            targets = {}
            for k, v in t.items():
                if isinstance(v, dict):
                    v = v.get("color") or v.get("value")
                targets[k] = parse_hex_color(v)
            return targets, None, tol
        return None, spec, None  # golden image
    targets = {}
    for part in spec.split(","):
        part = part.strip()
        if not part:
            continue
        if "=" not in part:
            raise ValueError("expected spec item must be target=#rrggbb, got %r" % part)
        name, col = part.split("=", 1)
        targets[name.strip()] = parse_hex_color(col)
    if not targets:
        raise ValueError("empty expected spec: %r" % spec)
    return targets, None, None


def load_calibration(path):
    if path and os.path.exists(path):
        return json.loads(Path(path).read_text())
    if DEFAULT_CALIBRATION.exists():
        return json.loads(DEFAULT_CALIBRATION.read_text())
    return None


def rect_of(cal, target):
    if cal is None:
        if target == "frame":
            return None
        raise RuntimeError("no calibration: only the 'frame' target is comparable "
                           "(pass --calibration cal.json)")
    t = cal.get("targets", {})
    if target in t:
        return t[target]
    if target == "frame":
        return None
    raise RuntimeError("target %r not in calibration (have: %s)"
                       % (target, ", ".join(sorted(t)) or "none"))


def mean_of_target(img, cal, name):
    """Mean RGB of a target: rect entry, {rect, cells} entry, or whole frame."""
    r = rect_of(cal, name)
    if r is None:
        return img.mean(0, 0, img.w, img.h, step=4)
    if isinstance(r, dict):
        cell = cal.get("cell", 4)
        rs = gs = bs = n = 0
        px, w = img.px, img.w
        for cx, cy in r.get("cells", []):
            x0, y0 = cx * cell, cy * cell
            cr = cg = cb = cn = 0
            for y in range(y0, min(img.h, y0 + cell), 2):
                base = y * w * 3
                for x in range(x0, min(img.w, x0 + cell), 2):
                    k = base + x * 3
                    cr += px[k]
                    cg += px[k + 1]
                    cb += px[k + 2]
                    cn += 1
            if cn == 0:
                continue
            cr, cg, cb = cr / cn, cg / cn, cb / cn
            # keep only strongly lit cells: an LED ring annulus has dim edges
            # that would dilute the mean towards black
            if max(cr, cg, cb) - min(cr, cg, cb) < 100 or max(cr, cg, cb) < 120:
                continue
            rs += cr
            gs += cg
            bs += cb
            n += 1
        if n == 0:
            x0, y0, x1, y1 = r["rect"]
            return img.mean(x0, y0, x1, y1, step=2)
        return (rs / n, gs / n, bs / n)
    return img.mean(*r, step=2)


def compare_core(actual_img, cal, targets, golden_path, tolerance, json_out):
    """Compare actual image against per-target colours (or a golden image).

    Returns (results, all_pass). Each result: dict(target, expected, actual, delta,
    max_delta, pass).
    """
    if golden_path:
        golden = load_image(golden_path)
        if (golden.w, golden.h) != (actual_img.w, actual_img.h):
            raise RuntimeError("golden image is %dx%d but actual is %dx%d — capture "
                               "with the same --width/--height as the golden"
                               % (golden.w, golden.h, actual_img.w, actual_img.h))
        exp_map = {}
        for name in targets:
            exp_map[name] = mean_of_target(golden, cal, name)
    else:
        exp_map = targets
    results = []
    all_pass = True
    for name in sorted(exp_map):
        am = mean_of_target(actual_img, cal, name)
        er, eg, eb = exp_map[name]
        ar, ag, ab = am
        delta = (abs(ar - er) + abs(ag - eg) + abs(ab - eb)) / 3.0
        maxd = max(abs(ar - er), abs(ag - eg), abs(ab - eb))
        ok = delta <= tolerance
        all_pass = all_pass and ok
        results.append({
            "target": name,
            "expected": [round(er, 1), round(eg, 1), round(eb, 1)],
            "actual": [round(ar, 1), round(ag, 1), round(ab, 1)],
            "delta_mean": round(delta, 2),
            "delta_max": round(maxd, 1),
            "tolerance": tolerance,
            "pass": ok,
        })
    if json_out:
        for r in results:
            print(json.dumps(r))
        print(json.dumps({"summary": {"targets": len(results), "passed": sum(1 for r in results if r["pass"]),
                                      "failed": sum(1 for r in results if not r["pass"]),
                                      "all_pass": all_pass}}))
    else:
        for r in results:
            print("%s %-8s expected rgb%s actual rgb%s delta mean %.2f max %.1f (tol %.1f)"
                  % ("PASS" if r["pass"] else "FAIL", r["target"],
                     tuple(r["expected"]), tuple(r["actual"]),
                     r["delta_mean"], r["delta_max"], r["tolerance"]))
        npass = sum(1 for r in results if r["pass"])
        print("%d/%d targets within tolerance %.1f — %s"
              % (npass, len(results), tolerance, "MATCH" if all_pass else "MISMATCH"))
    return results, all_pass


def cmd_compare(args):
    try:
        targets, golden_path, tol = parse_expected(args.expected)
    except (ValueError, OSError, json.JSONDecodeError) as e:
        eprint("bad --expected spec: %s" % e)
        return EXIT_USAGE
    tolerance = args.tolerance if args.tolerance is not None else (tol if tol is not None else DEFAULT_TOLERANCE)
    if not os.path.exists(args.actual):
        eprint("actual frame not found: %s" % args.actual)
        return EXIT_USAGE
    cal = load_calibration(args.calibration)
    img = load_image(args.actual)
    if golden_path is None and targets:
        names = set(targets)
    else:
        # golden mode: compare every calibrated target
        if cal is not None:
            names = set(cal.get("targets", {}))
        else:
            names = {"frame"}
        targets = {n: None for n in names}
    try:
        results, all_pass = compare_core(img, cal, targets, golden_path, tolerance, args.json)
    except RuntimeError as e:
        eprint(str(e))
        return EXIT_USAGE
    return EXIT_OK if all_pass else EXIT_MISMATCH


# ---------------------------------------------------------------------- check ----


def cmd_check(args):
    device = args.device or DEFAULT_DEVICE
    tmp = tempfile.mkdtemp(prefix="dsv-check-")
    try:
        out = os.path.join(tmp, "check.jpg")
        try:
            capture_frame(device, out, args.width, args.height, frames=1,
                          settle_ms=args.settle, timeout=args.timeout)
        except RuntimeError as e:
            eprint("capture failed: %s" % e)
            return EXIT_UNREACHABLE
        if args.keep:
            shutil.copyfile(out, args.keep)
            print("capture kept at %s" % args.keep)
        # restrict the expected spec to the requested targets when given inline
        spec = args.expected
        try:
            targets, golden_path, tol = parse_expected(spec)
        except (ValueError, OSError, json.JSONDecodeError) as e:
            eprint("bad --expected spec: %s" % e)
            return EXIT_USAGE
        if args.target and golden_path is None:
            want = set(t.strip() for t in args.target.split(",") if t.strip())
            targets = {k: v for k, v in targets.items() if k in want} or targets
        tolerance = args.tolerance if args.tolerance is not None else (tol if tol is not None else DEFAULT_TOLERANCE)
        cal = load_calibration(args.calibration)
        img = load_image(out)
        if golden_path is not None and cal is not None:
            targets = {n: None for n in cal.get("targets", {})}
        elif golden_path is not None:
            targets = {"frame": None}
        results, all_pass = compare_core(img, cal, targets, golden_path, tolerance, args.json)
        return EXIT_OK if all_pass else EXIT_MISMATCH
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ------------------------------------------------------------------- selftest ----


SYNTH_KEY_COLORS = [
    "#ff0000", "#00ff00", "#0000ff", "#ffff00", "#ff00ff",
    "#00ffff", "#ffffff", "#ff8000", "#8000ff", "#00ff80",
    "#ff0080", "#80ff00", "#0080ff", "#c0c0c0", "#ff4040",
]


def synth_image(w=1280, h=720):
    """Synthetic 'deck in a dark room' frame: dark bg, bezel, 15 lit key faces
    (landscape 5x3 grid, orientation 0) + one saturated LED blob."""
    px = bytearray(w * h * 3)
    for i in range(0, len(px), 3):
        px[i], px[i + 1], px[i + 2] = 18, 18, 24
    # panel bezel
    px0, py0, px1, py1 = 300, 150, 980, 534
    for y in range(py0, py1):
        base = y * w * 3
        for x in range(px0, px1):
            k = base + x * 3
            px[k], px[k + 1], px[k + 2] = 48, 46, 52
    # key faces 5 cols x 3 rows
    cols, rows = 5, 3
    cw = (px1 - px0) / cols
    ch = (py1 - py0) / rows
    for n in range(15):
        c, r = n % cols, n // cols
        col = parse_hex_color(SYNTH_KEY_COLORS[n])
        kx0 = int(px0 + c * cw + cw * 0.12)
        kx1 = int(px0 + (c + 1) * cw - cw * 0.12)
        ky0 = int(py0 + r * ch + ch * 0.12)
        ky1 = int(py0 + (r + 1) * ch - ch * 0.12)
        for y in range(ky0, ky1):
            base = y * w * 3
            for x in range(kx0, kx1):
                k = base + x * 3
                px[k], px[k + 1], px[k + 2] = col
    # LED blob (saturated blue disc) bottom-left
    cx, cy, rad = 150, 600, 45
    for y in range(cy - rad, cy + rad):
        for x in range(cx - rad, cx + rad):
            if (x - cx) ** 2 + (y - cy) ** 2 <= rad * rad and 0 <= x < w and 0 <= y < h:
                k = (y * w + x) * 3
                px[k], px[k + 1], px[k + 2] = 40, 40, 255
    return Image(w, h, bytes(px))


def cmd_selftest(args):
    ok = True

    def step(name, cond, detail=""):
        nonlocal ok
        print("%s  selftest/%s %s" % ("PASS" if cond else "FAIL", name, detail))
        if not cond:
            ok = False
        return cond

    # 1. synthetic frame + PNG roundtrip through ffmpeg
    synth = synth_image()
    with tempfile.TemporaryDirectory(prefix="dsv-selftest-") as tmp:
        png = os.path.join(tmp, "synth.png")
        png_write(png, synth)
        reloaded = load_image(png)
        step("png-roundtrip", (reloaded.w, reloaded.h) == (synth.w, synth.h)
             and reloaded.mean(0, 0, 8, 8) == synth.mean(0, 0, 8, 8),
             "%dx%d via ffmpeg decode" % (reloaded.w, reloaded.h))

        # 2. calibrate the synthetic frame (blob path: 15 key faces)
        cal = calibrate_core(reloaded, verbose=False)
        n_keys = len([k for k in cal["targets"] if k.startswith("key:")])
        step("calibrate", n_keys == 15 and "keys" in cal["targets"],
             "panel %s, %d key windows, orientation %d"
             % (cal["panel"]["rect"], n_keys, cal["grid"]["orientation_deg"]))

        # 3. compare synthetic actual vs the known spec -> must pass
        spec = ",".join("key:%d=%s" % (i, SYNTH_KEY_COLORS[i]) for i in range(15))
        spec += ",led:0=#2828ff"  # (40,40,255) blob
        targets, golden, tol = parse_expected(spec)
        results, all_pass = compare_core(reloaded, cal, targets, golden, 12.0, False)
        step("compare-pass", all_pass,
             "%d/%d targets within tolerance" % (sum(1 for r in results if r["pass"]), len(results)))

        # 4. negative: wrong colour for key:0 must be detected as mismatch
        bad = dict(targets)
        bad["key:0"] = parse_hex_color("#ffffff")
        results2, all_pass2 = compare_core(reloaded, cal, bad, None, 12.0, False)
        r0 = [r for r in results2 if r["target"] == "key:0"][0]
        step("compare-mismatch-detected", (not all_pass2) and (not r0["pass"]),
             "key:0 delta %.2f > tol -> FAIL (exit would be 7)" % r0["delta_mean"])

        # 5. overlay write
        ov = os.path.join(tmp, "overlay.png")
        overlay_write(reloaded, cal, ov)
        step("overlay", os.path.getsize(ov) > 1000, "%d bytes" % os.path.getsize(ov))

        # 6. camera, if present: capture + decode + calibrate attempt + compare plumbing
        cam = "skipped"
        try:
            devices, default = list_video_devices()
            if default:
                cap = os.path.join(tmp, "cam.jpg")
                capture_frame(default, cap, DEFAULT_WIDTH, DEFAULT_HEIGHT,
                              settle_ms=300, timeout=45)
                img = load_image(cap)
                _, ml, sd = img.rect_stats(0, 0, img.w, img.h, step=4)
                try:
                    ccal = calibrate_core(img, verbose=False)
                    n_t = len(ccal["targets"])
                    cam = "ok: captured %dx%d (luma %.0f+-%.0f), calibration found %d targets" % (
                        img.w, img.h, ml, sd, n_t)
                    # compare plumbing on real pixels: 'frame' vs its own mean -> delta 0
                    res, ap = compare_core(img, ccal, {"frame": mean_of_target(img, ccal, "frame")},
                                           None, 0.5, False)
                    step("camera-compare-plumbing", ap, cam)
                except RuntimeError as e:
                    cam = "ok: captured %dx%d (luma %.0f) but no panel detected: %s" % (img.w, img.h, ml, e)
                    step("camera-capture", True, cam)
        except RuntimeError as e:
            cam = "camera unavailable: %s" % e
            step("camera-capture", True, cam + " (selftest continues without camera)")
        if not cam.startswith("ok"):
            pass

    print("selftest: %s" % ("PASS" if ok else "FAIL"))
    return EXIT_OK if ok else 1


# ---------------------------------------------------------------------- main ----


def build_parser():
    p = argparse.ArgumentParser(
        prog=TOOL,
        description="deckstack-verify — camera harness for pixel-level deck verification "
                    "(docs/90-verification.md). stdlib + ffmpeg/v4l2-ctl, argv-only.")
    sub = p.add_subparsers(dest="cmd", required=True)

    d = sub.add_parser("devices", help="list video devices (v4l2-ctl) and pick the default camera")
    d.add_argument("--json", action="store_true")
    d.set_defaults(func=cmd_devices)

    c = sub.add_parser("capture", help="capture one still (or a clip/burst) via ffmpeg")
    c.add_argument("--out", required=True, help="output file (.jpg/.png; --frames>1 appends -NNN)")
    c.add_argument("--device", default=None, help="video device (default %s or $DSV_CAMERA)" % DEFAULT_DEVICE)
    c.add_argument("--width", type=int, default=DEFAULT_WIDTH)
    c.add_argument("--height", type=int, default=DEFAULT_HEIGHT)
    c.add_argument("--frames", type=int, default=1, help="capture N frames (image sequence if >1)")
    c.add_argument("--clip", type=float, default=None, help="capture a SECONDS-long .avi clip instead")
    c.add_argument("--settle", type=int, default=400, help="ms to let exposure settle (warm-up frame + sleep)")
    c.add_argument("--timeout", type=int, default=45, help="per-attempt ffmpeg timeout, s")
    c.add_argument("--json", action="store_true")
    c.set_defaults(func=cmd_capture)

    cal = sub.add_parser("calibrate", help="detect panel/key grid/LED zones from a captured frame")
    cal.add_argument("--frame", required=True, help="captured frame (jpg/png) to calibrate from")
    cal.add_argument("--out", default="calibration.json", help="calibration JSON output")
    cal.add_argument("--overlay", default=None, help="debug overlay PNG (rects per target)")
    cal.add_argument("--grid", default=None, help="grid hint CxR in device orientation (default 5x3)")
    cal.add_argument("--panel", default=None, help="panel hint x,y,w,h in image pixels")
    cal.add_argument("--orientation", type=int, default=None, choices=[0, 90, 180, 270],
                     help="device rotation in the image (default: auto from panel aspect)")
    cal.add_argument("--face-luma", default=None,
                     help="key-face luma band 'lo,hi' in image pixels (default: auto from background)")
    cal.add_argument("--json", action="store_true", help="also print the calibration JSON")
    cal.set_defaults(func=cmd_calibrate)

    cmp_ = sub.add_parser("compare", help="compare a captured frame against an expected spec")
    cmp_.add_argument("--expected", required=True,
                      help="inline 'key:0=#ff0000,...', a spec .json, or a golden image path")
    cmp_.add_argument("--actual", required=True, help="captured frame (jpg/png)")
    cmp_.add_argument("--calibration", default=None,
                      help="calibration JSON (default tools/verify/calibration.json if present)")
    cmp_.add_argument("--tolerance", type=float, default=None,
                      help="per-channel mean abs delta tolerance (default %.0f)" % DEFAULT_TOLERANCE)
    cmp_.add_argument("--json", action="store_true", help="NDJSON output")
    cmp_.set_defaults(func=cmd_compare)

    chk = sub.add_parser("check", help="capture + compare in one step (e.g. check key:0)")
    chk.add_argument("--target", default=None, help="comma-separated targets to restrict the spec to")
    chk.add_argument("--expected", required=True, help="same grammar as compare --expected")
    chk.add_argument("--actual", default=None, help="unused (kept for symmetry)")
    chk.add_argument("--device", default=None)
    chk.add_argument("--width", type=int, default=DEFAULT_WIDTH)
    chk.add_argument("--height", type=int, default=DEFAULT_HEIGHT)
    chk.add_argument("--settle", type=int, default=400)
    chk.add_argument("--timeout", type=int, default=45)
    chk.add_argument("--calibration", default=None)
    chk.add_argument("--tolerance", type=float, default=None)
    chk.add_argument("--keep", default=None, help="keep the captured frame at this path")
    chk.add_argument("--json", action="store_true")
    chk.set_defaults(func=cmd_check)

    st = sub.add_parser("selftest", help="run the whole pipeline on a synthetic frame (no deck needed)")
    st.set_defaults(func=cmd_selftest)
    return p


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except RuntimeError as e:
        eprint("%s: %s" % (TOOL, e))
        return EXIT_UNREACHABLE
    except subprocess.TimeoutExpired as e:
        eprint("%s: command timed out: %s" % (TOOL, e))
        return EXIT_UNREACHABLE
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())

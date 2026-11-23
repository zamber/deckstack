#!/usr/bin/env python3
"""Micro-app: gitdeck — keys 0-3 show clean/dirty, ahead, behind, branch of a repo.

Teaches the API calls: `ds set <path> <value>` on key images/labels (colour +
text), `ds get <path> --json` (read back state / prior values for cleanup), the
global `--socket` / `--data-dir` flags, and how to drive another program
(`git`) argv-only from a micro-app — never a shell.

Key map: 0 = clean (#00cc44) / dirty (#ff2222) · 1 = ahead of upstream
(#ffaa00 when > 0, else dim) · 2 = behind (#ff00aa when > 0, else dim) ·
3 = current branch (or short HEAD when detached).

Stdlib only; talks to a running ds-daemon through the `ds` binary.

    python3 examples/microapps/gitdeck.py --once --repo ~/src/project
    python3 examples/microapps/gitdeck.py --loop --repo . --interval 5
    python3 examples/microapps/gitdeck.py --once --socket /tmp/deck/daemon.sock \
        --data-dir /tmp/deck --repo /tmp/myrepo

--loop polls git at --interval and restores the four keys to their previous
values on exit (Ctrl-C); keys that did not exist before are left at the last
value (there is no unset op — see docs/microapps.md §errors).
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import time

KEYS = (0, 1, 2, 3)
CLEAN, DIRTY = "#00cc44", "#ff2222"
WARN, DIM, BRANCH = "#ffaa00", "#444444", "#3cc86e"


class DSError(RuntimeError):
    def __init__(self, argv, proc):
        self.exit_code = proc.returncode
        super().__init__(
            "ds %s failed [exit %d]: %s"
            % (" ".join(argv), proc.returncode, proc.stderr.strip() or proc.stdout.strip())
        )


def ds(ds_bin, socket, data_dir, args, timeout=15):
    cmd = [ds_bin]
    if socket:
        cmd += ["--socket", socket]
    if data_dir:
        cmd += ["--data-dir", data_dir]
    cmd += [str(a) for a in args]
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)


def ds_ok(ds_bin, socket, data_dir, args, timeout=15):
    p = ds(ds_bin, socket, data_dir, args, timeout)
    if p.returncode != 0:
        raise DSError(args, p)
    return p


def get_json(ds_bin, socket, data_dir, path):
    p = ds(ds_bin, socket, data_dir, ["get", path, "--json", "--compact"])
    if p.returncode == 2:
        return None
    if p.returncode != 0:
        raise DSError(["get", path], p)
    return json.loads(p.stdout)["value"]


def set_json(ds_bin, socket, data_dir, path, value):
    ds_ok(ds_bin, socket, data_dir, ["set", path, "--json", json.dumps(value)])


def git(repo, *argv, check=True):
    """Run git argv-only (no shell). Returns stdout, or None when check=False
    and git exits nonzero (e.g. no upstream configured)."""
    p = subprocess.run(["git", "-C", repo, *argv], capture_output=True, text=True,
                       timeout=15)
    if check and p.returncode != 0:
        raise RuntimeError("git %s failed [exit %d]: %s"
                           % (" ".join(argv), p.returncode, p.stderr.strip()))
    return p.stdout.strip() if p.returncode == 0 else None


def sample(repo):
    """One git status sample: {dirty, ahead, behind, branch}."""
    if git(repo, "rev-parse", "--is-inside-work-tree", check=False) != "true":
        raise RuntimeError("not a git repository: %s (run with --repo <path>)" % repo)
    dirty = bool(git(repo, "status", "--porcelain"))
    branch = git(repo, "branch", "--show-current") or git(repo, "rev-parse", "--short", "HEAD")
    counts = git(repo, "rev-list", "--left-right", "--count", "@{upstream}...HEAD",
                 check=False)
    if counts is None:
        ahead = behind = 0  # no upstream configured
    else:
        behind, ahead = (int(x) for x in counts.split())
    return {"dirty": dirty, "ahead": ahead, "behind": behind, "branch": branch[:32]}


def render(ds_bin, socket, data_dir, s):
    fields = {
        0: ("dirty" if s["dirty"] else "clean", DIRTY if s["dirty"] else CLEAN),
        1: ("ahead %d" % s["ahead"], WARN if s["ahead"] else DIM),
        2: ("behind %d" % s["behind"], "#ff00aa" if s["behind"] else DIM),
        3: (s["branch"], BRANCH),
    }
    for k in KEYS:
        label, color = fields[k]
        set_json(ds_bin, socket, data_dir, "/controls/keys/%d/label" % k, label)
        set_json(ds_bin, socket, data_dir, "/controls/keys/%d/image" % k,
                 {"kind": "color", "value": color})


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--once", action="store_true", default=True,
                      help="update once and exit (default)")
    mode.add_argument("--loop", action="store_true",
                      help="poll git every --interval seconds and re-render")
    ap.add_argument("--interval", type=float, default=5.0,
                    help="seconds between polls in --loop (default 5.0)")
    ap.add_argument("--repo", default=".", help="git repository path (default: cwd)")
    ap.add_argument("--socket", help="daemon control socket (default: ds's own default)")
    ap.add_argument("--data-dir", help="daemon data dir (passed through to ds)")
    ap.add_argument("--ds-bin", default=os.environ.get("DS_BIN", "ds"),
                    help="path to the ds binary (default: $DS_BIN or 'ds' on PATH)")
    args = ap.parse_args()

    prior = {k: get_json(args.ds_bin, args.socket, args.data_dir, "/controls/keys/%d" % k)
             for k in KEYS}

    def restore():
        for k in KEYS:
            old = prior[k]
            if old is None:
                continue
            for leaf, value in old.items():
                try:
                    set_json(args.ds_bin, args.socket, args.data_dir,
                             "/controls/keys/%d/%s" % (k, leaf), value)
                except DSError as e:
                    print("cleanup: %s" % e, file=sys.stderr)

    stopping = {"flag": False}

    def on_signal(signum, frame):
        stopping["flag"] = True

    signal.signal(signal.SIGINT, on_signal)
    signal.signal(signal.SIGTERM, on_signal)

    try:
        while True:
            s = sample(os.path.abspath(args.repo))
            render(args.ds_bin, args.socket, args.data_dir, s)
            print(json.dumps(s, separators=(",", ":")), flush=True)
            if not args.loop or stopping["flag"]:
                break
            time.sleep(args.interval)
        return 0
    except (DSError, RuntimeError) as e:
        print("gitdeck: %s" % e, file=sys.stderr)
        return 1
    finally:
        if args.loop:
            restore()


if __name__ == "__main__":
    sys.exit(main())

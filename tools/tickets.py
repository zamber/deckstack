#!/usr/bin/env python3
"""deckstack ticket board tool - Python 3 stdlib only. See tickets/README.md."""
import argparse, datetime, fcntl, json, os, re, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TDIR = os.path.join(ROOT, "tickets")
LOCK = os.path.join(TDIR, ".lock")

STATES = ["backlog", "ready", "doing", "review", "done", "blocked"]
SIZES = ["S", "M", "L"]
TRANSITIONS = {
    "backlog": {"ready", "blocked"},
    "ready": {"doing", "blocked", "backlog"},
    "doing": {"review", "blocked", "ready"},
    "review": {"done", "doing", "blocked"},
    "done": set(),
    "blocked": {"ready", "backlog"},
}
REQUIRED = ["id", "title", "state", "phase", "lane", "size", "owner", "depends_on",
            "touches", "branch", "worktree", "pr", "created", "updated",
            "acceptance", "verification"]
ID_RE = re.compile(r"^DS-\d{3}$")
DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}([T ].*)?$")
BLOCK_RE = re.compile(r"^```json\s*\n(.*?)\n```\s*\n?(.*)$", re.DOTALL)
BODY_SECTIONS = ["Context", "Goal", "Scope", "Out of scope", "Implementation notes",
                 "Files and boundaries", "Acceptance", "Verification", "Risks",
                 "Definition of done", "Notes"]


def today():
    return datetime.date.today().isoformat()


def atomic_write(path, data):
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(data)
    os.replace(tmp, path)


class Lock:
    def __init__(self, exclusive=True):
        self.exclusive = exclusive
        self.f = None

    def __enter__(self):
        os.makedirs(TDIR, exist_ok=True)
        self.f = open(LOCK, "a+")
        fcntl.flock(self.f, fcntl.LOCK_EX if self.exclusive else fcntl.LOCK_SH)
        return self

    def __exit__(self, *a):
        fcntl.flock(self.f, fcntl.LOCK_UN)
        self.f.close()


def parse_ticket(path):
    with open(path, encoding="utf-8") as f:
        text = f.read()
    m = BLOCK_RE.match(text.lstrip("\n"))
    if not m:
        raise ValueError(f"{path}: no leading ```json metadata block")
    try:
        meta = json.loads(m.group(1))
    except json.JSONDecodeError as e:
        raise ValueError(f"{path}: metadata block is not valid JSON: {e}")
    return meta, m.group(2)


def load_tickets():
    tickets, errors = {}, []
    for name in sorted(os.listdir(TDIR)):
        if not (name.startswith("DS-") and name.endswith(".md")):
            continue
        path = os.path.join(TDIR, name)
        try:
            meta, body = parse_ticket(path)
        except ValueError as e:
            errors.append(str(e))
            continue
        tid = meta.get("id")
        if tid in tickets:
            errors.append(f"duplicate ticket id {tid} ({name})")
            continue
        if tid != name[:-3]:
            errors.append(f"{name}: id {tid!r} does not match file name")
        tickets[tid] = {"meta": meta, "body": body, "path": path}
    return tickets, errors


def norm_touch(e):
    return e[4:] if e.startswith("new:") else e


def touch_ok(entry):
    if entry.startswith("new:"):
        return True
    if os.path.exists(os.path.join(ROOT, entry)):
        return True
    if "*" in entry:
        base = entry.split("*")[0].rstrip("/")
        return bool(base) and os.path.isdir(os.path.join(ROOT, os.path.dirname(base) or "."))
    parent = os.path.dirname(entry.rstrip("/"))
    return os.path.isdir(os.path.join(ROOT, parent if parent else "."))


def touches_overlap(a, b):
    a, b = norm_touch(a), norm_touch(b)
    if "*" in a:
        a = a.split("*")[0]
    if "*" in b:
        b = b.split("*")[0]
    a = a.rstrip("/") + "/"
    b = b.rstrip("/") + "/"
    return a.startswith(b) or b.startswith(a)


def blocks_of(tickets):
    blocks = {tid: [] for tid in tickets}
    for tid, t in tickets.items():
        for dep in t["meta"].get("depends_on", []):
            if dep in blocks:
                blocks[dep].append(tid)
    return blocks


def find_cycle(tickets):
    color = {tid: 0 for tid in tickets}
    stack = []

    def dfs(u):
        color[u] = 1
        stack.append(u)
        for v in tickets[u]["meta"].get("depends_on", []):
            if v not in color:
                continue
            if color[v] == 1:
                return stack[stack.index(v):] + [v]
            r = dfs(v)
            if r:
                return r
        stack.pop()
        color[u] = 2
        return None

    for tid in tickets:
        if color[tid] == 0:
            c = dfs(tid)
            if c:
                return c
    return None


def validate(tickets, load_errors):
    errors = list(load_errors)
    states = {}
    for tid, t in tickets.items():
        m = t["meta"]
        where = f"ticket {tid}"
        for k in REQUIRED:
            if k not in m:
                errors.append(f"{where}: missing required field {k!r}")
        if not isinstance(m.get("id"), str) or not ID_RE.match(m.get("id") or ""):
            errors.append(f"{where}: bad id {m.get('id')!r}")
        if not isinstance(m.get("title"), str) or not m.get("title"):
            errors.append(f"{where}: bad title")
        if m.get("state") not in STATES:
            errors.append(f"{where}: state {m.get('state')!r} not in {STATES}")
        ph = m.get("phase")
        if not isinstance(ph, int) or not 0 <= ph <= 7:
            errors.append(f"{where}: phase {ph!r} not an int in 0..7")
        if m.get("size") not in SIZES:
            errors.append(f"{where}: size {m.get('size')!r} not in {SIZES}")
        if not isinstance(m.get("lane"), str) or not m.get("lane"):
            errors.append(f"{where}: bad lane")
        if m.get("owner") is not None and not isinstance(m.get("owner"), str):
            errors.append(f"{where}: owner must be null or string")
        for k in ("depends_on", "touches", "acceptance", "verification"):
            v = m.get(k)
            if not isinstance(v, list) or not all(isinstance(x, str) for x in v):
                errors.append(f"{where}: {k} must be a list of strings")
        if not m.get("touches"):
            errors.append(f"{where}: touches is empty")
        for k in ("branch", "worktree", "pr"):
            v = m.get(k)
            if v is not None and not isinstance(v, str):
                errors.append(f"{where}: {k} must be null or string")
        for k in ("created", "updated"):
            if not isinstance(m.get(k), str) or not DATE_RE.match(m.get(k) or ""):
                errors.append(f"{where}: {k} must be an ISO date, got {m.get(k)!r}")
        if m.get("state") == "blocked" and not m.get("blocked_reason"):
            errors.append(f"{where}: state blocked requires blocked_reason")
        if m.get("state") in ("doing", "review") and not m.get("owner"):
            errors.append(f"{where}: state {m['state']} requires an owner")
        for sec in BODY_SECTIONS:
            if f"## {sec}" not in t["body"]:
                errors.append(f"{where}: body missing '## {sec}' section")
        states[tid] = m.get("state")
    for tid, t in tickets.items():
        for dep in t["meta"].get("depends_on", []):
            if dep not in tickets:
                errors.append(f"ticket {tid}: depends_on unknown id {dep!r}")
    cyc = find_cycle(tickets)
    if cyc:
        errors.append("dependency cycle: " + " -> ".join(cyc))
    for tid, t in tickets.items():
        st = t["meta"].get("state")
        if st in ("ready", "doing", "review", "done"):
            for dep in t["meta"].get("depends_on", []):
                if dep in states and states[dep] != "done":
                    errors.append(f"ticket {tid}: state {st} but dep {dep} is "
                                  f"{states.get(dep)!r} (only done)")
    for tid, t in tickets.items():
        for e in t["meta"].get("touches", []):
            if not touch_ok(e):
                errors.append(f"ticket {tid}: touches entry {e!r} does not exist (use a glob, "
                              f"an existing path, a new dir under an existing parent, or the "
                              f"'new:' prefix to declare a new path)")
    doing = [tid for tid, s in states.items() if s == "doing"]
    for i in range(len(doing)):
        for j in range(i + 1, len(doing)):
            a, b = doing[i], doing[j]
            ta = tickets[a]["meta"]["touches"]
            tb = tickets[b]["meta"]["touches"]
            hit = [x for x in ta for y in tb if touches_overlap(x, y)]
            if hit:
                errors.append(f"file collision: {a} and {b} are both doing and overlap on {hit}")
    return errors, states


def gen_index(tickets):
    blocks = blocks_of(tickets)
    arr = []
    for tid in sorted(tickets):
        m = dict(tickets[tid]["meta"])
        m["blocks"] = sorted(blocks[tid])
        arr.append(m)
    return json.dumps(arr, indent=2) + "\n"


def gen_board(tickets):
    lines = ["# BOARD - generated by `python3 tools/tickets.py sync`; do not edit by hand",
             "",
             f"Generated: {today()} - {len(tickets)} tickets.",
             "",
             "| id | state | phase | lane | size | owner | deps | title |",
             "|---|---|---|---|---|---|---|---|"]
    for tid in sorted(tickets):
        m = tickets[tid]["meta"]
        deps = ", ".join(m.get("depends_on", [])) or "-"
        owner = m.get("owner") or "-"
        lines.append(f"| {tid} | {m['state']} | {m['phase']} | {m['lane']} | {m['size']} "
                     f"| {owner} | {deps} | {m['title']} |")
    lines.append("")
    return "\n".join(lines)


def gen_graph(tickets):
    blocks = blocks_of(tickets)
    states = {tid: t["meta"]["state"] for tid, t in tickets.items()}
    memo = {}

    def longest(u):
        if u in memo:
            return memo[u]
        best = [u]
        for v in blocks.get(u, []):
            cand = [u] + longest(v)
            if len(cand) > len(best):
                best = cand
        memo[u] = best
        return best

    roots = [tid for tid in tickets if not tickets[tid]["meta"].get("depends_on")]
    crit = max((longest(r) for r in roots), key=len, default=[])
    ready = sorted(tid for tid, s in states.items() if s == "ready")
    lanes = {}
    for tid, t in tickets.items():
        lanes.setdefault(t["meta"]["lane"], []).append(tid)
    out = ["# GRAPH - generated by `python3 tools/tickets.py sync`; do not edit by hand", "",
           f"Generated: {today()} - {len(tickets)} tickets, DAG (no cycles; enforced by validate).", "",
           "## Mermaid DAG", "",
           "```mermaid", "graph TD"]
    for tid in sorted(tickets):
        label = tickets[tid]["meta"]["title"].replace('"', "'")
        if len(label) > 60:
            label = label[:57] + "..."
        out.append(f'  {tid.replace("-", "")}["{tid} {label}"]:::{states[tid]}')
    for tid in sorted(tickets):
        for dep in tickets[tid]["meta"].get("depends_on", []):
            out.append(f"  {dep.replace('-', '')} --> {tid.replace('-', '')}")
    out += ["  classDef backlog fill:#ddd,stroke:#999",
            "  classDef ready fill:#9cf,stroke:#36c",
            "  classDef doing fill:#ff9,stroke:#c90",
            "  classDef review fill:#f9c,stroke:#c39",
            "  classDef done fill:#9c6,stroke:#393",
            "  classDef blocked fill:#f66,stroke:#900", "```", "",
            "## Critical path (longest dependency chain)", "",
            (" -> ".join(crit) if crit else "-"), "",
            f"Critical path length: {len(crit)} tickets.", "",
            "## Parallel lanes", ""]
    for lane in sorted(lanes):
        ids = sorted(lanes[lane])
        desc = ", ".join(f"{i}({states[i]})" for i in ids)
        out.append(f"- **{lane}** ({len(ids)}): {desc}")
    out += ["", "## Ready set (state=ready, deps done - claimable now)", ""]
    if ready:
        for tid in ready:
            out.append(f"- {tid} - {tickets[tid]['meta']['title']} "
                       f"(lane {tickets[tid]['meta']['lane']})")
    else:
        out.append("- (none)")
    out.append("")
    return "\n".join(out)


def sync_outputs(tickets, promote=True):
    changed = False
    states = {tid: t["meta"]["state"] for tid, t in tickets.items()}
    if promote:
        for tid, t in tickets.items():
            m = t["meta"]
            if m["state"] == "backlog" and all(states.get(d) == "done"
                                               for d in m.get("depends_on", [])):
                m["state"] = "ready"
                m["updated"] = today()
                changed = True
    atomic_write(os.path.join(TDIR, "index.json"), gen_index(tickets))
    atomic_write(os.path.join(TDIR, "BOARD.md"), gen_board(tickets))
    atomic_write(os.path.join(TDIR, "GRAPH.md"), gen_graph(tickets))
    return changed


def write_ticket(t):
    m = t["meta"]
    text = f"```json\n{json.dumps(m, indent=2)}\n```\n\n{t['body'].rstrip()}\n"
    atomic_write(t["path"], text)


def git(*args):
    return subprocess.run(["git", "-C", ROOT, *args], capture_output=True, text=True)


def cmd_list(args):
    with Lock(exclusive=False):
        tickets, errs = load_tickets()
    rows = []
    for tid, t in sorted(tickets.items()):
        m = t["meta"]
        if args.state and m["state"] != args.state:
            continue
        if args.lane and m["lane"] != args.lane:
            continue
        if args.owner and m.get("owner") != args.owner:
            continue
        if args.phase is not None and m["phase"] != args.phase:
            continue
        rows.append((tid, m["state"], m["phase"], m["lane"], m["size"],
                     m.get("owner") or "-", ",".join(m.get("depends_on", [])) or "-",
                     m["title"]))
    if args.json:
        print(json.dumps([{"id": r[0], "state": r[1], "phase": r[2], "lane": r[3],
                           "size": r[4], "owner": r[5], "depends_on": r[6],
                           "title": r[7]} for r in rows], indent=2))
        return 0
    print(f"{'id':8} {'state':8} {'ph':2} {'lane':12} {'sz':2} {'owner':16} {'deps':24} title")
    for r in rows:
        print(f"{r[0]:8} {r[1]:8} {r[2]:<2} {r[3]:12} {r[4]:2} {r[5]:16} {r[6]:24} {r[7]}")
    print(f"\n{len(rows)} tickets")
    return 0


def cmd_show(args):
    with Lock(exclusive=False):
        tickets, errs = load_tickets()
    if args.id not in tickets:
        print(f"error: no ticket {args.id}", file=sys.stderr)
        return 1
    t = tickets[args.id]
    if getattr(args, "json", False):
        print(json.dumps(t["meta"], indent=2))
        return 0
    print(f"```json\n{json.dumps(t['meta'], indent=2)}\n```\n")
    print(t["body"])
    return 0


def cmd_field(args):
    with Lock(exclusive=False):
        tickets, errs = load_tickets()
    if args.id not in tickets:
        print(f"error: no ticket {args.id}", file=sys.stderr)
        return 1
    m = tickets[args.id]["meta"]
    if args.key not in m:
        print(f"error: ticket {args.id} has no field {args.key!r}", file=sys.stderr)
        return 1
    v = m[args.key]
    if v is None:
        print("null")
    elif isinstance(v, str):
        print(v)
    else:
        print(json.dumps(v))
    return 0


def cmd_assign(args):
    with Lock(exclusive=True):
        tickets, errs = load_tickets()
        tid = args.id
        if tid not in tickets:
            print(f"error: no ticket {tid}", file=sys.stderr)
            return 1
        m = tickets[tid]["meta"]
        if args.clear:
            m["branch"] = None
            m["worktree"] = None
            m["pr"] = None
        for key, val in (("branch", args.branch), ("worktree", args.worktree),
                         ("pr", args.pr)):
            if val is not None:
                m[key] = None if val == "-" else val
        if args.owner:
            m["owner"] = args.owner
        m["updated"] = today()
        write_ticket(tickets[tid])
        sync_outputs(tickets)
    fields = {k: m.get(k) for k in ("owner", "branch", "worktree", "pr")}
    print(f"{tid}: assigned " + json.dumps(fields))
    return 0


def cmd_ready(args):
    with Lock(exclusive=False):
        tickets, _ = load_tickets()
        _, states = validate(tickets, [])
    ready = [tid for tid in sorted(tickets) if states.get(tid) == "ready"]
    nxt = [tid for tid in sorted(tickets)
           if states.get(tid) == "backlog"
           and all(states.get(d) == "done" for d in tickets[tid]["meta"]["depends_on"])]
    print("Ready set (claimable now):")
    for tid in ready:
        m = tickets[tid]["meta"]
        print(f"  {tid} [{m['lane']}/{m['size']}] {m['title']}")
    if not ready:
        print("  (none)")
    print("\nNext up (backlog, deps done - promote with `sync`):")
    for tid in nxt:
        print(f"  {tid} {tickets[tid]['meta']['title']}")
    if not nxt:
        print("  (none)")
    doing = [tid for tid in sorted(tickets) if states.get(tid) == "doing"]
    print(f"\nIn flight (doing): {', '.join(doing) or '-'}")
    return 0


def cmd_claim(args):
    with Lock(exclusive=True):
        tickets, errs = load_tickets()
        errors, states = validate(tickets, errs)
        tid = args.id
        if tid not in tickets:
            print(f"error: no ticket {tid}", file=sys.stderr)
            return 1
        m = tickets[tid]["meta"]
        if m["state"] != "ready":
            print(f"error: {tid} is {m['state']}, must be ready to claim", file=sys.stderr)
            return 1
        for dep in m["depends_on"]:
            if states.get(dep) != "done":
                print(f"error: dep {dep} is {states.get(dep)}, not done", file=sys.stderr)
                return 1
        for other, ot in sorted(tickets.items()):
            if other == tid or ot["meta"].get("state") != "doing":
                continue
            hit = [x for x in m.get("touches", []) for y in ot["meta"].get("touches", [])
                   if touches_overlap(x, y)]
            if hit:
                print(f"error: file collision with doing ticket {other} on {hit}; "
                      f"sequence or split the tickets first", file=sys.stderr)
                return 1
        m["state"] = "doing"
        m["owner"] = args.owner
        m["branch"] = f"ticket/{tid}"
        m["worktree"] = f".worktrees/{tid}"
        m["pr"] = f"prs/{tid}.md"
        m["updated"] = today()
        write_ticket(tickets[tid])
        sync_outputs(tickets)
        tickets2, errs2 = load_tickets()
        errors2, _ = validate(tickets2, errs2)
        hard = [e for e in errors2 if "collision" in e or "cycle" in e]
        if hard:
            print("error: claim would violate invariants:", file=sys.stderr)
            for e in hard:
                print("  " + e, file=sys.stderr)
            return 1
    print(f"claimed {tid} for {args.owner}: state doing, branch ticket/{tid}, "
          f"worktree .worktrees/{tid}, pr prs/{tid}.md")
    print(f"next: sh tools/wt.sh new {tid} --owner {args.owner}")
    return 0


def cmd_state(args):
    with Lock(exclusive=True):
        tickets, errs = load_tickets()
        errors, states = validate(tickets, errs)
        tid = args.id
        if tid not in tickets:
            print(f"error: no ticket {tid}", file=sys.stderr)
            return 1
        m = tickets[tid]["meta"]
        old = m["state"]
        new = args.state
        if new not in STATES:
            print(f"error: unknown state {new!r}", file=sys.stderr)
            return 1
        if new not in TRANSITIONS.get(old, set()):
            print(f"error: illegal transition {old} -> {new} "
                  f"(allowed: {sorted(TRANSITIONS.get(old, set()))})", file=sys.stderr)
            return 1
        if new == "ready":
            bad = [d for d in m["depends_on"] if states.get(d) != "done"]
            if bad:
                print(f"error: cannot be ready, deps not done: {bad}", file=sys.stderr)
                return 1
        if new == "doing" and not (args.owner or m.get("owner")):
            print("error: doing requires an owner (--owner)", file=sys.stderr)
            return 1
        if new == "blocked" and not args.reason:
            print("error: blocked requires --reason", file=sys.stderr)
            return 1
        m["state"] = new
        if args.owner:
            m["owner"] = args.owner
        if new == "blocked":
            m["blocked_reason"] = args.reason
        if new != "blocked":
            m.pop("blocked_reason", None)
        m["updated"] = today()
        write_ticket(tickets[tid])
        sync_outputs(tickets)
    print(f"{tid}: {old} -> {new}" + (f" (reason: {args.reason})" if args.reason else ""))
    return 0


def cmd_unclaim(args):
    with Lock(exclusive=True):
        tickets, _ = load_tickets()
        tid = args.id
        if tid not in tickets:
            print(f"error: no ticket {tid}", file=sys.stderr)
            return 1
        m = tickets[tid]["meta"]
        if m["state"] != "doing":
            print(f"error: {tid} is {m['state']}, can only unclaim doing", file=sys.stderr)
            return 1
        m["state"] = "ready"
        m["owner"] = None
        m["branch"] = None
        m["worktree"] = None
        m["pr"] = None
        m["updated"] = today()
        write_ticket(tickets[tid])
        sync_outputs(tickets)
    print(f"{tid}: doing -> ready (unclaimed)")
    return 0


def cmd_recover(args):
    with Lock(exclusive=False):
        tickets, _ = load_tickets()
    wt_list = git("worktree", "list", "--porcelain").stdout
    wts = set()
    for line in wt_list.splitlines():
        if line.startswith("worktree "):
            wts.add(line[len("worktree "):])
    print(f"{'id':8} {'state':7} {'owner':14} branch  worktree  pr      last commit on ticket branch")
    for tid in sorted(tickets):
        m = tickets[tid]["meta"]
        if m["state"] not in ("doing", "review"):
            continue
        branch = m.get("branch") or f"ticket/{tid}"
        wt = m.get("worktree") or f".worktrees/{tid}"
        pr = m.get("pr") or f"prs/{tid}.md"
        br_exists = git("rev-parse", "--verify", "--quiet", f"refs/heads/{branch}").returncode == 0
        wt_path = os.path.join(ROOT, wt)
        wt_exists = wt_path in wts and os.path.isdir(wt_path)
        pr_exists = os.path.exists(os.path.join(ROOT, pr))
        last = "-"
        if br_exists:
            r = git("log", "-1", "--format=%h %ad %s", "--date=short", branch)
            last = r.stdout.strip() or "-"
        print(f"{tid:8} {m['state']:7} {str(m.get('owner')):14} "
              f"{'yes' if br_exists else 'NO':6}  {'yes' if wt_exists else 'NO':8}  "
              f"{'yes' if pr_exists else 'NO':6}  {last}")
    print("\nhints: worktree gone but ticket doing -> unclaim or re-create with tools/wt.sh; "
          "branch gone -> work is lost unless pushed to the `review` remote; prs/<id>.md "
          "missing -> re-run pr.sh submit.")
    return 0


def cmd_validate(args):
    with Lock(exclusive=False):
        tickets, errs = load_tickets()
        errors, states = validate(tickets, errs)
        want = {"index.json": gen_index(tickets), "BOARD.md": gen_board(tickets),
                "GRAPH.md": gen_graph(tickets)}
        for name, content in want.items():
            p = os.path.join(TDIR, name)
            if not os.path.exists(p):
                errors.append(f"generated file missing: tickets/{name} (run sync)")
            else:
                with open(p, encoding="utf-8") as f:
                    if f.read() != content:
                        errors.append(f"tickets/{name} is stale (run `python3 tools/tickets.py sync`)")
    if errors:
        print(f"INVALID - {len(errors)} problem(s):")
        for e in errors:
            print("  - " + e)
        return 1
    n = len(tickets)
    by_state = {}
    for s in states.values():
        by_state[s] = by_state.get(s, 0) + 1
    print(f"OK - {n} tickets, DAG valid, generated files in sync. " +
          " ".join(f"{k}:{v}" for k, v in sorted(by_state.items())))
    return 0


def cmd_sync(args):
    with Lock(exclusive=True):
        tickets, errs = load_tickets()
        errors, _ = validate(tickets, errs)
        fatal = [e for e in errors if "stale" not in e]
        if fatal:
            print("refusing to sync: board invalid:", file=sys.stderr)
            for e in fatal:
                print("  - " + e, file=sys.stderr)
            return 1
        changed = sync_outputs(tickets)
    print("synced tickets/index.json, tickets/BOARD.md, tickets/GRAPH.md" +
          (" (auto-promoted backlog tickets whose deps are done)" if changed else ""))
    return 0


def cmd_graph(args):
    with Lock(exclusive=False):
        tickets, _ = load_tickets()
    print(gen_graph(tickets))
    return 0


def cmd_index(args):
    with Lock(exclusive=False):
        tickets, _ = load_tickets()
    print(gen_index(tickets))
    return 0


def cmd_lanes(args):
    with Lock(exclusive=False):
        tickets, _ = load_tickets()
    lanes = {}
    for tid, t in tickets.items():
        lanes.setdefault(t["meta"]["lane"], []).append((tid, t["meta"]["state"]))
    for lane in sorted(lanes):
        items = sorted(lanes[lane])
        print(f"{lane} ({len(items)}): " + ", ".join(f"{i}={s}" for i, s in items))
    return 0


def main():
    ap = argparse.ArgumentParser(prog="tickets.py", description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("list", help="list tickets with filters")
    p.add_argument("--state"); p.add_argument("--lane"); p.add_argument("--owner")
    p.add_argument("--phase", type=int); p.add_argument("--json")
    p.set_defaults(func=cmd_list)
    p = sub.add_parser("show", help="show one ticket (metadata + body); --json for metadata only")
    p.add_argument("id"); p.add_argument("--json", action="store_true"); p.set_defaults(func=cmd_show)
    p = sub.add_parser("field", help="print one metadata field of a ticket (machine-readable)")
    p.add_argument("id"); p.add_argument("key"); p.set_defaults(func=cmd_field)
    p = sub.add_parser("assign", help="set linkage fields; value '-' clears, --clear clears all")
    p.add_argument("id")
    p.add_argument("--branch"); p.add_argument("--worktree"); p.add_argument("--pr")
    p.add_argument("--owner"); p.add_argument("--clear", action="store_true")
    p.set_defaults(func=cmd_assign)
    p = sub.add_parser("ready", help="show the ready set and next-up tickets")
    p.set_defaults(func=cmd_ready)
    p = sub.add_parser("claim", help="claim a ready ticket (ready -> doing)")
    p.add_argument("id"); p.add_argument("--owner", required=True)
    p.set_defaults(func=cmd_claim)
    p = sub.add_parser("state", help="change ticket state (validates the transition)")
    p.add_argument("id"); p.add_argument("state", choices=STATES)
    p.add_argument("--owner"); p.add_argument("--reason")
    p.set_defaults(func=cmd_state)
    p = sub.add_parser("unclaim", help="release a doing ticket back to ready")
    p.add_argument("id"); p.set_defaults(func=cmd_unclaim)
    p = sub.add_parser("recover", help="inspect doing/review tickets: branch/worktree/pr state")
    p.add_argument("id", nargs="?", default=None); p.set_defaults(func=cmd_recover)
    p = sub.add_parser("validate", help="validate the whole board (exit 1 on any problem)")
    p.set_defaults(func=cmd_validate)
    p = sub.add_parser("sync", help="regenerate index.json/BOARD.md/GRAPH.md, auto-promote ready")
    p.set_defaults(func=cmd_sync)
    p = sub.add_parser("graph", help="print the generated GRAPH.md")
    p.set_defaults(func=cmd_graph)
    p = sub.add_parser("index", help="print the generated index.json")
    p.set_defaults(func=cmd_index)
    p = sub.add_parser("lanes", help="tickets grouped by lane")
    p.set_defaults(func=cmd_lanes)
    args = ap.parse_args()
    sys.exit(args.func(args))


if __name__ == "__main__":
    main()

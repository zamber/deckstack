//! `--help` texts — present at every level (top, verb, group, group+verb) per
//! `docs/contracts/cli.md` §1 / `docs/AGENT-WORKFLOW.md` §CLI self-description.

/// The help text for a topic: "top", a verb, "daemon", or "daemon <sub>".
pub fn text(topic: &str) -> String {
    match topic {
        "get" => GET_HELP.to_string(),
        "set" => SET_HELP.to_string(),
        "watch" => WATCH_HELP.to_string(),
        "schema" => SCHEMA_HELP.to_string(),
        "describe" => DESCRIBE_HELP.to_string(),
        "status" => STATUS_HELP.to_string(),
        "bind" => BIND_HELP.to_string(),
        "unbind" => UNBIND_HELP.to_string(),
        "daemon" => DAEMON_HELP.to_string(),
        "daemon start" => DAEMON_START_HELP.to_string(),
        "daemon stop" => DAEMON_STOP_HELP.to_string(),
        "daemon status" => DAEMON_STATUS_HELP.to_string(),
        "stream" => STREAM_HELP.to_string(),
        "stream push" => STREAM_PUSH_HELP.to_string(),
        "stream stats" => STREAM_STATS_HELP.to_string(),
        "stream stop" => STREAM_STOP_HELP.to_string(),
        _ => TOP_HELP.to_string(),
    }
}

const TOP_HELP: &str = "\
ds — the deckstack CLI (docs/contracts/cli.md)

Usage:
  ds [--socket PATH] [--token-file PATH] [--data-dir DIR] <verb> [args] [flags]
  ds --help | ds <verb> --help | ds daemon --help | ds daemon <sub> --help
  ds --version

Verbs (phase 1):
  get <path> [--fields a,b] [--rev N]     read a state path (human value, or one
                                          JSON object with --json)
  set <path> <value> [--if-rev N] [--file f]
                                          write a state path (inline JSON/scalar,
                                          or an image file via --file)
  bind <topic> --do '<step>' [--do …] [--when '<cond>']
                                          append a binding to /bindings; prints the
                                          daemon-assigned id (bNNNN)
  unbind <id> | --topic <topic>           remove bindings (unknown id → exit 2)
  watch <path> [--ndjson] [--from-rev N] [--max-events N]
                                          stream events (persistent; --ndjson only)
  schema <state|ipc|stream-frame|plugin-manifest|binding>
                                          print the JSON Schema from schemas/
  describe <path>                         schema + current value + allowed verbs
  daemon start [--device URI] [--foreground] | stop | status
                                          control the ds-daemon process
  status                                  daemon status + client info

Verbs (phase 3):
  stream push <target> --file f | --dir d --fps N [--loop]
                                          push frames to a display target over the
                                          frame stream socket (DS-032)
  stream stats <target> [--json]          /telemetry/streams/<target> (no rev)
  stream stop <target>                    stop the producer of a target

Global flags:
  --socket PATH       control socket (default $XDG_RUNTIME_DIR/deckstack/daemon.sock)
  --token-file PATH   token file (default <socket dir>/token, 0600)
  --data-dir DIR      data dir for daemon start/stop/status
  --json              one JSON object on stdout (pretty; --compact for one line)
  --ndjson            one JSON object per line (streaming verbs only: watch)
  --compact           single-line JSON output
  -h, --help          help at this level
  -V, --version       print the version

Exit codes: 0 ok · 1 usage/schema (incl. --json on a streaming verb; there is no
`migrate` verb) · 2 E_PATH · 3 E_CONFLICT (current_rev in the payload) · 4 E_AUTH/
E_SCOPE/E_CAP · 5 E_LIMIT · 6 E_UNREACHABLE (start it with: ds daemon start) ·
7 E_DEVICE · 8 E_INTERNAL/E_VERSION.

Errors are actionable and go to stderr:  ds: path not found: /x  [exit 2]
";

const GET_HELP: &str = "\
ds get — read a state path (docs/contracts/state.md §1)

Usage:
  ds get <path> [--fields a,b] [--rev N] [--json] [--compact]

  /telemetry/* works and returns no rev (S1). --rev N fails with exit 3
  (E_CONFLICT) when the current rev is not N.

Output:
  human   the value, one line per fact (scalars plainly, objects as key: value)
  --json  exactly one object: {\"path\":…,\"rev\":…,\"value\":…}

Examples:
  ds get /controls/keys/0
  ds get /controls/keys/0 --json
  ds get /controls/keys/0 --fields image,label --json
";

const SET_HELP: &str = "\
ds set — write a state path (one transaction, needs mutate scope)

Usage:
  ds set <path> <value> [--if-rev N] [--json]
  ds set <path> --file <image-file> [--if-rev N]

  <value> is inline JSON when it parses, else a bare scalar string:
    ds set /controls/keys/0/label '\"Salon\"'
    ds set /controls/keys/3/brightness 80
    ds set /controls/keys/0/image --json '{\"kind\":\"color\",\"value\":\"#ff0000\"}'
  --file sends {\"kind\":\"file\",\"path\":…} for image paths; the file is
  canonicalized and must live under /policy/fs/roots (or /tmp), else exit 4.
  --if-rev N makes the write conditional; a mismatch exits 3 with current_rev.

Output:
  human   ok rev=N
  --json  {\"ok\":true,\"rev\":N}
";

const WATCH_HELP: &str = "\
ds watch — stream state events on one connection (persistent)

Usage:
  ds watch <path> [--ndjson] [--from-rev N] [--max-events N]

  Streaming verb: only --ndjson is accepted; --json here is a usage error (exit 1).
  Default (human) prints one line per event; --ndjson prints one JSON event object
  per line, no wrapper. Runs until Ctrl-C or --max-events N.

Example:
  ds watch /controls --ndjson | head -5
";

const SCHEMA_HELP: &str = "\
ds schema — print a JSON Schema from schemas/ (embedded at compile time)

Usage:
  ds schema <state|ipc|stream-frame|plugin-manifest|binding> [--json] [--compact]

Output is the schema itself (pretty JSON; --compact for one line). No daemon needed.
";

const DESCRIBE_HELP: &str = "\
ds describe — schema + current value + allowed verbs for a path

Usage:
  ds describe <path> [--json] [--compact]

Output:
  human   path / rev / value / verbs / schema lines
  --json  one object: {\"path\":…,\"rev\":…,\"value\":…,\"verbs\":[…],\"schema\":…}

Example:
  ds describe /controls/keys/0 --json
";

const STATUS_HELP: &str = "\
ds status — daemon health plus client info

Usage:
  ds status [--json] [--compact]

Same control op as `ds daemon status`, plus the client name. Daemon down → exit 6
with 'start it with: ds daemon start'.
";

const BIND_HELP: &str = "\
ds bind — append a binding to /bindings and print its daemon-assigned id (DS-041)

Usage:
  ds bind <topic> --do '<step>' [--do '<step>' …] [--when '<cond>'] [--json] [--compact]

  <topic> is an event topic (actions.md §1): deck/control/keys.0/down, app/focus/changed,
  … — wildcards (`*`) are allowed as whole segments. Each --do '<step>' is either a full
  JSON step object or the shorthand:
    set <path> <json-value>            e.g. --do 'set /controls/keys/1/image {\"kind\":\"color\",\"value\":\"#ff0000\"}'
    emit <topic> [json-payload]        e.g. --do 'emit app/demo/red {}'
    plugin <id.action> [json-args]     e.g. --do 'plugin demo.bump {\"by\":1}'
    exec <argv…>                       e.g. --do 'exec /usr/bin/ffmpeg -i v.mp4'  (one token = one argv entry)
    sleep <ms>                         e.g. --do 'sleep 200'
  --when '<cond>' is a JSON condition, e.g. '{\"payload\":\"app\",\"op\":\"eq\",\"value\":\"code\"}'
  (exactly one of path/payload; op exists|eq|neq|gt|lt; value required unless op=exists).

  Steps are schema-validated against schemas/binding.json on the client AND the daemon;
  code-ish steps ({\"do\":\"eval\",…}) are rejected with exit 1. Step execution lands with
  DS-042 — bind only stores and validates them.

Output:
  human   ok rev=N id=b0001
  --json  {\"ok\":true,\"rev\":N,\"result\":{\"id\":\"b0001\"}}

Examples:
  ds bind deck/control/keys.0/down --do 'plugin demo.bump {\"by\":1}' --json
  ds bind app/focus/changed --when '{\"payload\":\"app\",\"op\":\"eq\",\"value\":\"code\"}' \\
      --do 'set /controls/keys/3/image {\"kind\":\"color\",\"value\":\"#00ff00\"}'
  ds get /bindings --json          # list all
  ds watch /bindings --ndjson      # stream changes
";

const UNBIND_HELP: &str = "\
ds unbind — remove bindings from /bindings (DS-041)

Usage:
  ds unbind <id> [--json] [--compact]            remove one binding by id (bNNNN)
  ds unbind --topic <topic> [--json] [--compact] remove ALL bindings with this exact topic

One transaction per invocation (rev +1, one /bindings event). An unknown id, or a topic
with no binding, fails with E_PATH → exit 2.

Output:
  human   ok rev=N removed=b0001
  --json  {\"ok\":true,\"rev\":N,\"result\":{\"removed\":[\"b0001\"]}}

Examples:
  ds unbind b0001
  ds unbind --topic deck/control/keys.0/down --json
";

const DAEMON_HELP: &str = "\
ds daemon — control the ds-daemon process

Usage:
  ds daemon start [--device URI] [--foreground]
  ds daemon stop
  ds daemon status
  ds daemon <sub> --help
";

const DAEMON_START_HELP: &str = "\
ds daemon start — spawn ds-daemon and wait for the control socket

Usage:
  ds daemon start [--device virtual:PATH|auto] [--foreground] [--data-dir DIR]

Spawns `ds-daemon run --data-dir <dir> [--device <uri>]` (binary: $DS_DAEMON_BIN, else
the ds-daemon next to the ds binary, else ds-daemon on PATH), redirects its stderr to
<dir>/daemon.log, waits (≤ 5 s) for daemon.sock + a successful daemon status op, then
prints the status. --foreground runs it attached. --device auto maps to none in
phase 1 (only virtual:/hid: URIs exist; hardware is DS-021).
";

const DAEMON_STOP_HELP: &str = "\
ds daemon stop — stop the daemon over the control socket (admin scope)

Usage:
  ds daemon stop [--json]

Sends {\"op\":\"daemon\",\"action\":\"stop\"}. Daemon down → exit 6.
Output: human `ok stopping`, or --json {\"ok\":true,\"result\":{\"stopping\":true}}.
";

const DAEMON_STATUS_HELP: &str = "\
ds daemon status — daemon status op plus socket/pid state

Usage:
  ds daemon status [--json] [--compact]

Output: human `ok running pid=… rev=… device=… socket=…`, or --json one object
(the daemon status result plus socket/pid_file). Daemon down → exit 6:
  ds: daemon unreachable at <socket> — start it with: ds daemon start  [exit 6]
";

const STREAM_HELP: &str = "\
ds stream — the frame-plane producer verbs (docs/contracts/streaming.md, DS-032)

Usage:
  ds stream push <target> --file <frame> [--loop] [--fps N] [--credits N]
  ds stream push <target> --dir <frames-dir> --fps N [--loop] [--credits N]
  ds stream stats <target> [--json] [--compact]
  ds stream stop <target> [--json] [--compact]
  ds stream <push|stats|stop> --help

  <target> is the unified vocabulary (S6): keys | key:0-14 | dial — 'panel'/'screen'
  are not targets. Frames ride the binary DSF1 framing on <dir>/stream.sock after a
  stream-open; push is never a plain NDJSON op (ipc.md §1.2).

Exit codes: same table as the top level; a missing daemon/stream socket is exit 6
(E_UNREACHABLE), backpressure timeouts are exit 5 (E_LIMIT), a bad target is exit 1.
";

const STREAM_PUSH_HELP: &str = "\
ds stream push — push frames to a display target over the frame stream socket (DS-032)

Usage:
  ds stream push <target> --file <frame.png|jpg|gif> [--loop]
  ds stream push <target> --dir <dir> --fps N [--loop] [--credits N]
  ds stream push <target> --file <video.mp4> [--fps N]     (decoded by external ffmpeg)

  --file sends one frame. --dir sends every file of the directory in filename order
  (the %04d-style sequence) at --fps N — capped client-side at 300 frames and 30 fps
  with a clear error. --loop sets the loop flag in the frame meta. A non-image --file
  (video) is decoded by spawning ffmpeg argv-only (ffmpeg -y -i <file> <tmp>/%04d.png —
  no shell, no eval, core never links ffmpeg); a missing ffmpeg is a clear error, and
  the documented manual equivalent is: ffmpeg -i v.mp4 frames/%04d.png && ds stream
  push <target> --dir frames/ --fps 30.

  Transport: control socket stream-open (when served) then DSctl stream-open +
  DSF1 frames on <socket-dir>/stream.sock; the daemon acks each frame (stream-ack) and
  re-grants credits (stream-credit) as its queue drains. The producer honours the
  credit window, waits for replenishment on backpressure (stream-nack E_LIMIT) and
  exits 5 on a backpressure timeout — it never spins or buffers unboundedly.
  Frame payloads are capped at 16 MB on the wire.

Output:
  human   ok target=<t> sent=<n> frames=<n> [fps=N] [loop=true]
  --json  {\"ok\":true,\"target\":…,\"sent\":…,\"frames\":…,\"fps\":…,\"loop\":…}

Examples:
  ds stream push keys --file frame.png
  ds stream push keys --dir frames/ --fps 30
  ds stream push dial --file anim.gif --loop
  ds stream push keys --file v.mp4 --fps 24
";

const STREAM_STATS_HELP: &str = "\
ds stream stats — print /telemetry/streams/<target> over the control socket (DS-032)

Usage:
  ds stream stats <target> [--json] [--compact]

  <target>: keys | key:0-14 | dial (S6). Reads the stream-stats op (or the
  /telemetry/streams/<target> path) — telemetry only: the object is printed as-is and
  never carries a rev, and the read never bumps rev (S1).

Output:
  human   one line per fact: sent: 960 / dropped: 12 / fps: 32.0 / …
  --json  the stats object itself, e.g.
          {\"sent\":960,\"dropped\":12,\"fps\":32.0,\"bytes\":6700000,\"queue\":1,
           \"in_flight\":true,\"last_sha256\":\"…\",\"last_seq\":959,\"writer_ms_p99\":4.1,
           \"sampled_at\":\"…\"}

A target with no stream yet is E_PATH (exit 2).
";

const STREAM_STOP_HELP: &str = "\
ds stream stop — stop the producer of a stream target (DS-032)

Usage:
  ds stream stop <target> [--json] [--compact]

  Sends stream-stop (control op; DSctl stream-stop on stream.sock when the daemon does
  not serve the control op). The daemon drains the target's queue and refuses new
  frames. Exit 0 when the producer is already gone (no stream socket / no session).

Output:
  human   ok target=<t> stopped=true
  --json  {\"ok\":true,\"target\":…,\"stopped\":true}
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_exists_at_every_level_and_says_usage() {
        for topic in [
            "top",
            "get",
            "set",
            "watch",
            "schema",
            "describe",
            "status",
            "bind",
            "unbind",
            "daemon",
            "daemon start",
            "daemon stop",
            "daemon status",
            "stream",
            "stream push",
            "stream stats",
            "stream stop",
        ] {
            let t = text(topic);
            assert!(t.contains("Usage:"), "{topic} help has no Usage line");
            assert!(t.contains("ds"), "{topic} help does not name ds");
            assert!(!t.is_empty());
        }
        // every verb appears in the top-level help
        for v in crate::cli::VERBS {
            assert!(text("top").contains(v), "top help missing {v}");
        }
        // the top help documents the exit codes and the missing migrate verb
        let top = text("top");
        for code in [
            "0 ok",
            "1 usage",
            "2 E_PATH",
            "3 E_CONFLICT",
            "4 E_AUTH",
            "5 E_LIMIT",
            "6 E_UNREACHABLE",
            "7 E_DEVICE",
            "8 E_INTERNAL",
            "migrate",
            "[exit 2]",
        ] {
            assert!(top.contains(code), "top help missing {code:?}");
        }
        // streaming-verb rule is in the watch help
        assert!(text("watch").contains("--json here is a usage error (exit 1)"));
        // schema names are all listed
        for n in crate::cli::SCHEMA_NAMES {
            assert!(text("schema").contains(n), "schema help missing {n}");
        }
    }
}

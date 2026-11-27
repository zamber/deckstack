//! `--help` texts — present at every level (top, verb, group, group+verb) per
//! `docs/contracts/cli.md` §1 / `docs/AGENT-WORKFLOW.md` §CLI self-description.
//!
//! Shape contract (DS-099): every verb help carries Usage, its arguments/flags
//! with their defaults, exactly ONE runnable Example, the Exit codes it can
//! return, and a pointer to `ds schema`/`ds describe`. Per-verb help stays
//! short (<= 25 lines); group help keeps one line per subcommand; `ds --help`
//! is the index. Detail lives in `docs/contracts/cli.md` and the authoring
//! guide `docs/microapps.md`.

/// The help text for a topic: "top", a verb, "daemon", or "daemon <sub>".
pub fn text(topic: &str) -> String {
    match topic {
        "get" => GET_HELP.to_string(),
        "set" => SET_HELP.to_string(),
        "watch" => WATCH_HELP.to_string(),
        "colors" => COLORS_HELP.to_string(),
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
  schema <state|ipc|stream-frame|plugin-manifest|binding|action-metadata|preview>
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
  colors '<json-array>' [--if-rev N]        set all 15 key colours in one panel
                                            write (exactly 15 #rrggbb; DS-111)

Global flags:
  --socket PATH       control socket (default $XDG_RUNTIME_DIR/deckstack/daemon.sock)
  --token-file PATH   token file (default <socket dir>/token, 0600)
  --data-dir DIR      data dir for daemon start/stop/status
  --json              one JSON object on stdout (pretty; --compact for one line)
  --ndjson            one JSON object per line (streaming verbs only: watch)
  --compact           single-line JSON output
  -h, --help          help at this level
  -V, --version       print the version

Learn more: docs/contracts/cli.md (contract) · docs/microapps.md (writing programs for
            the deck) · docs/agents.md (agent sessions) · `ds schema`/`ds describe`.

Exit codes: 0 ok · 1 usage/schema (incl. --json on a streaming verb; there is no
`migrate` verb) · 2 E_PATH · 3 E_CONFLICT (current_rev in the payload) · 4 E_AUTH/
E_SCOPE/E_CAP · 5 E_LIMIT · 6 E_UNREACHABLE (start it with: ds daemon start) ·
7 E_DEVICE · 8 E_INTERNAL/E_VERSION.

Errors are actionable and go to stderr:  ds: path not found: /x  [exit 2]

Write small programs that drive the deck (status strips, focus panels, key
indicators) against this CLI: see docs/microapps.md and examples/microapps/.
";

const GET_HELP: &str = "\
ds get — read a state path (docs/contracts/state.md §1)

Usage:
  ds get <path> [--fields a,b] [--rev N] [--json] [--compact]

Arguments:
  <path>           state path, e.g. /controls/keys/0 (schema: `ds describe <path>`)
  --fields a,b     project only these fields (default: all fields)
  --rev N          succeed only if the current rev is N, else exit 3 (E_CONFLICT)
  --json           one object {\"path\":…,\"rev\":…,\"value\":…} (default: human,
                   one line per fact); --compact makes it one line
  /telemetry/*     works and returns no rev (S1)

Example:
  ds get /controls/keys/0 --json

Exit codes: 0 ok · 1 usage/schema · 2 E_PATH · 3 E_CONFLICT (--rev mismatch) ·
            4 E_AUTH/E_SCOPE · 6 E_UNREACHABLE (start it with: ds daemon start)
Discover more: ds schema state · ds describe /controls/keys/0 · docs/microapps.md
";

const SET_HELP: &str = "\
ds set — write a state path in one transaction (needs mutate scope)

Usage:
  ds set <path> <value> [--if-rev N] [--json]
  ds set <path> --file <image-file> [--if-rev N]

Arguments:
  <path>           state path, e.g. /controls/keys/0/image
  <value>          inline JSON when it parses, else a bare scalar string.
                   {} clears an object path. A null value is rejected with E_SCHEMA
                   and exit 1.
  --file f         image file → {\"kind\":\"file\",\"path\":…}; canonicalized, must
                   live under /policy/fs/roots (or /tmp), else exit 4
  --if-rev N       conditional write (default: last-write-wins); mismatch → exit 3
                   with current_rev in the payload
  --json           one object {\"ok\":true,\"rev\":N} (default: human `ok rev=N`)

Example:
  ds set /controls/keys/0/image --json '{\"kind\":\"color\",\"value\":\"#ff0000\"}'

Exit codes: 0 ok · 1 usage/schema (bad value) · 2 E_PATH · 3 E_CONFLICT (--if-rev) ·
            4 E_AUTH/E_SCOPE/E_CAP (--file outside /policy/fs/roots) · 5 E_LIMIT ·
            6 E_UNREACHABLE
Discover more: ds schema state · ds describe /controls/keys/0/image · docs/microapps.md
";

const COLORS_HELP: &str = "\
ds colors — set all 15 key colours in one panel write (DS-111)

Usage:
  ds colors '<json-array>' [--if-rev N] [--json]

Arguments:
  '<json-array>'   JSON array of exactly 15 '#rrggbb' strings, row-major key 0-14
  --if-rev N       conditional write (default: last-write-wins); mismatch -> exit 3
                   with current_rev in the payload
  --json           one object {\"ok\":true,\"rev\":N} (default: human `ok rev=N`)

The daemon composes the 480x272 panel and sends one 02 35 push; no per-key 0x36
reports. Wrong JSON/arity/hex exits 1 before the daemon is contacted.

Example:
  ds colors '[\"#ff0000\",\"#00ff00\",\"#0000ff\", … 15]'

Exit codes: 0 ok · 1 usage/schema · 3 E_CONFLICT (--if-rev) · 4 E_AUTH/E_SCOPE ·
            6 E_UNREACHABLE
Discover more: ds schema state · ds describe /controls/panel/image · docs/microapps.md
";

const WATCH_HELP: &str = "\
ds watch — stream state/topic events on one connection (persistent)

Usage:
  ds watch <path> [--ndjson] [--from-rev N] [--max-events N]

Arguments:
  <path>           state path (/controls, /controls/keys/0) or topic
                   (/app/focus/changed, /deck/control/keys/*)
  --ndjson         one JSON event object per line, no wrapper (default: human
                   `event <path> rev=N`); --json here is a usage error (exit 1)
  --from-rev N     replay from rev N (default: now)
  --max-events N   exit after N events (default: run until Ctrl-C)

Example:
  ds watch /controls --ndjson | head -5

Exit codes: 0 ok (after --max-events or Ctrl-C) · 1 usage (--json on watch) ·
            2 E_PATH · 4 E_AUTH/E_SCOPE · 6 E_UNREACHABLE
Discover more: ds schema ipc · ds describe /controls · docs/microapps.md §events
";

const SCHEMA_HELP: &str = "\
ds schema — print a JSON Schema from schemas/ (embedded; no daemon needed)

Usage:
  ds schema <state|ipc|stream-frame|plugin-manifest|binding|action-metadata|preview> [--json] [--compact]

Arguments:
  <name>           one of: state, ipc, stream-frame, plugin-manifest, binding,
                   action-metadata, preview   (required — no default)
  --json           the same schema as one object (default: pretty JSON)
  --compact        single line

Example:
  ds schema state --compact

Exit codes: 0 ok · 1 usage (unknown name — the error lists the valid names)
Discover more: ds describe <path> answers with these schemas · docs/microapps.md
";

const DESCRIBE_HELP: &str = "\
ds describe — schema + current value + allowed verbs for one path

Usage:
  ds describe <path> [--json] [--compact]

Arguments:
  <path>           any existing state path, e.g. /controls/keys/0 (else exit 2)
  --json           one object {\"path\",\"value\",\"verbs\",\"schema\"[,\"rev\"]}
                   (default: human `path:/rev:/value:/verbs:/schema:` lines)

Example:
  ds describe /controls/keys/0 --json

Exit codes: 0 ok · 1 usage · 2 E_PATH · 4 E_AUTH/E_SCOPE · 6 E_UNREACHABLE
Discover more: ds schema state · ds --help · docs/microapps.md
";

const STATUS_HELP: &str = "\
ds status — daemon health plus client info

Usage:
  ds status [--json] [--compact]

Arguments:
  --json           one JSON status object (default: human
                   `ok running pid=… rev=… device=… socket=… client=ds/…`)

Example:
  ds status --json

Exit codes: 0 ok · 1 usage · 4 E_AUTH · 6 E_UNREACHABLE — daemon down prints:
            start it with: ds daemon start  [exit 6]
Discover more: ds daemon status --help · ds schema ipc · docs/agents.md §3
";

const BIND_HELP: &str = "\
ds bind — append a binding to /bindings, print its daemon-assigned id (DS-041)

Usage:
  ds bind <topic> --do '<step>' [--do '<step>' …] [--when '<cond>'] [--json]

Arguments:
  <topic>          event topic (docs/contracts/actions.md §1), e.g.
                   deck/control/keys.0/down or app/focus/changed; `*` as a whole
                   segment is a wildcard
  --do '<step>'    repeatable; shorthand: set <path> <json> | emit <topic> [payload] |
                   plugin <id.action> [args] | exec <argv…> | sleep <ms> — or a full
                   JSON step object; {\"do\":\"eval\",…} is rejected (exit 1)
  --when '<cond>'  optional JSON condition {\"path\"|\"payload\",\"op\",\"value\"}
  --json           one object {\"ok\":true,\"rev\":N,\"result\":{\"id\":\"bNNNN\"}}
                   (default: human `ok rev=N id=bNNNN`)

Example:
  ds bind app/focus/changed --do 'set /controls/keys/3/image {\"kind\":\"color\",\"value\":\"#00ff00\"}'

Exit codes: 0 ok · 1 usage/schema (bad topic/step) · 2 E_PATH · 4 E_AUTH/E_SCOPE ·
            5 E_LIMIT · 6 E_UNREACHABLE
Discover more: ds schema binding · ds get /bindings --json · docs/microapps.md
";

const UNBIND_HELP: &str = "\
ds unbind — remove bindings from /bindings (DS-041)

Usage:
  ds unbind <id> [--json] [--compact]
  ds unbind --topic <topic> [--json] [--compact]

Arguments:
  <id>             binding id bNNNN (exactly one of <id> / --topic)
  --topic <topic>  remove ALL bindings with this exact topic
  --json           one object {\"ok\":true,\"rev\":N,\"result\":{\"removed\":[…]}}
                   (default: human `ok rev=N removed=b0001`)
  One transaction per call (rev +1, one /bindings event).

Example:
  ds unbind b0001

Exit codes: 0 ok · 1 usage · 2 E_PATH (unknown id / no binding for the topic) ·
            4 E_AUTH/E_SCOPE · 6 E_UNREACHABLE
Discover more: ds schema binding · ds bind --help · docs/microapps.md
";

const DAEMON_HELP: &str = "\
ds daemon — control the ds-daemon process

Usage:
  ds daemon start [--device URI] [--foreground]     spawn ds-daemon, wait for socket
  ds daemon stop                                    stop over the control socket
  ds daemon status                                  status op + socket/pid state
  ds daemon <sub> --help

Exit codes: the top-level table (`ds --help`); daemon down → 6 with the exact
`ds daemon start` hint.
Discover more: ds schema ipc · docs/agents.md §3 · docs/microapps.md
";

const DAEMON_START_HELP: &str = "\
ds daemon start — spawn ds-daemon and wait for the control socket (≤ 5 s)

Usage:
  ds daemon start [--device virtual:PATH|auto] [--foreground] [--data-dir DIR]

Arguments:
  --device URI     virtual:/tmp/vdev.sock (CI/examples), hid: (real deck, DS-021),
                   auto = none in phase 1 (default: no device)
  --foreground     run attached instead of detached
  --data-dir DIR   data dir (default $XDG_RUNTIME_DIR/deckstack, fallback
                   ~/.local/state/deckstack); stderr → <dir>/daemon.log

Example:
  ds daemon start --device virtual:/tmp/vdev.sock --data-dir /tmp/deck

Exit codes: 0 ok · 1 usage (cannot spawn; startup timeout — see <dir>/daemon.log) ·
            6 E_UNREACHABLE · 7 E_DEVICE (hid: permission / vendor app busy)
Discover more: ds schema ipc · ds daemon status --help · docs/agents.md §3
";

const DAEMON_STOP_HELP: &str = "\
ds daemon stop — stop the daemon over the control socket (admin scope)

Usage:
  ds daemon stop [--json]

Arguments:
  --json           one object {\"ok\":true,\"result\":{\"stopping\":true}}
                   (default: human `ok stopping`)

Example:
  ds daemon stop

Exit codes: 0 ok · 1 usage · 4 E_AUTH/E_SCOPE (admin scope needed) · 6 E_UNREACHABLE
Discover more: ds schema ipc · ds daemon status --help · docs/agents.md §3
";

const DAEMON_STATUS_HELP: &str = "\
ds daemon status — daemon status op plus socket/pid state

Usage:
  ds daemon status [--json] [--compact]

Arguments:
  --json           one JSON status object (default: human
                   `ok running pid=… rev=… device=… socket=…`)

Example:
  ds daemon status --json

Exit codes: 0 ok · 1 usage · 4 E_AUTH · 6 E_UNREACHABLE — daemon down prints:
  ds: daemon unreachable at <socket> — start it with: ds daemon start  [exit 6]
Discover more: ds status --help · ds schema ipc · docs/agents.md §3
";

const STREAM_HELP: &str = "\
ds stream — the frame-plane producer verbs (docs/contracts/streaming.md, DS-032)

Usage:
  ds stream push <target> --file f [--loop] [--fps N] [--credits N]   send frames
  ds stream push <target> --dir d --fps N [--loop]                   from a dir
  ds stream stats <target> [--json]                                  telemetry (no rev)
  ds stream stop <target> [--json]                                   stop the producer
  ds stream <push|stats|stop> --help

  <target> is the unified vocabulary (S6): keys | key:0-14 | dial — there is no
  'panel'/'screen' target. Frames ride DSF1 on <data-dir>/stream.sock (DS-032).

Exit codes: the top-level table (`ds --help`); no daemon/stream socket → 6,
backpressure timeout → 5, bad target → 1.
Discover more: ds schema stream-frame · docs/microapps.md §images
";

const STREAM_PUSH_HELP: &str = "\
ds stream push — push frames to a display target over the frame stream socket

Usage:
  ds stream push <target> --file <frame.png|jpg|gif> [--loop] [--fps N] [--credits N]
  ds stream push <target> --dir <frames-dir> --fps N [--loop] [--credits N]

Arguments:
  <target>         keys | key:0-14 | dial (S6); anything else → exit 1
  --file f         one frame (≤ 16 MB on the wire); a video is decoded by an
                   external ffmpeg argv-only spawn (missing ffmpeg → clear error)
  --dir d          every file in filename order at --fps N (capped client-side:
                   ≤ 300 frames, ≤ 30 fps)
  --fps N          1..=30 (required with --dir; default: none)
  --loop           set the loop flag in the frame meta (default: off)
  --credits N      credit window 1..=1024 (default: the daemon's)
  --json           {\"ok\":true,\"target\":…,\"sent\":…,\"frames\":…,…} (default:
                   human `ok target=<t> sent=<n> …`)

Example:
  ds stream push keys --file examples/key.png

Exit codes: 0 ok · 1 usage (bad target/rate) · 4 E_AUTH/E_SCOPE · 5 E_LIMIT
            (backpressure timeout) · 6 E_UNREACHABLE (no daemon/stream socket)
Discover more: ds schema stream-frame · ds stream stats --help · docs/microapps.md
";

const STREAM_STATS_HELP: &str = "\
ds stream stats — print /telemetry/streams/<target> (telemetry: no rev, S1)

Usage:
  ds stream stats <target> [--json] [--compact]

Arguments:
  <target>         keys | key:0-14 | dial (S6)
  --json           the stats object itself (default: human, one line per fact:
                   sent: 960 / dropped: 12 / fps: 32.0 / …)

Example:
  ds stream stats keys --json

Exit codes: 0 ok · 1 usage (bad target) · 2 E_PATH (no stream for the target yet) ·
            4 E_AUTH/E_SCOPE · 6 E_UNREACHABLE
Discover more: ds schema stream-frame · ds describe /telemetry · docs/microapps.md
";

const STREAM_STOP_HELP: &str = "\
ds stream stop — stop the producer of a stream target

Usage:
  ds stream stop <target> [--json] [--compact]

Arguments:
  <target>         keys | key:0-14 | dial (S6)
  --json           {\"ok\":true,\"target\":…,\"stopped\":true} (default: human
                   `ok target=<t> stopped=true`)

Example:
  ds stream stop keys

Exit codes: 0 ok (also when the producer is already gone) · 1 usage (bad target) ·
            4 E_AUTH/E_SCOPE · 6 E_UNREACHABLE
Discover more: ds stream push --help · ds schema stream-frame · docs/microapps.md
";

#[cfg(test)]
mod tests {
    use super::*;

    const VERB_TOPICS: [&str; 17] = [
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
        "colors",
    ];

    const GROUP_TOPICS: [&str; 2] = ["daemon", "stream"];

    /// The lines of the `Example:` block that are actual commands (`  ds …`).
    fn example_commands(t: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut in_example = false;
        for line in t.lines() {
            if line.starts_with("Example:") {
                in_example = true;
                continue;
            }
            if in_example {
                if line.trim().is_empty() {
                    break;
                }
                if line.starts_with("  ds ") {
                    out.push(line.trim_start());
                }
            }
        }
        out
    }

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
            "colors",
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

    #[test]
    fn every_verb_help_carries_the_ds099_content_contract() {
        for topic in VERB_TOPICS {
            let t = text(topic);
            // short: <= 25 lines per verb/group help (top is the index, exempt)
            assert!(
                t.lines().count() <= 25,
                "{topic} help is {} lines (> 25)",
                t.lines().count()
            );
            // the exit codes it can return
            assert!(
                t.contains("Exit codes:"),
                "{topic} help has no Exit codes line"
            );
            // a pointer to the schema/describe discovery verbs
            assert!(
                t.contains("ds schema") || t.contains("ds describe"),
                "{topic} help does not point at `ds schema`/`ds describe`"
            );
            if !GROUP_TOPICS.contains(&topic) {
                // arguments/flags with their defaults
                assert!(
                    t.to_lowercase().contains("default"),
                    "{topic} help documents no defaults"
                );
                // exactly ONE runnable example
                let ex = example_commands(&t);
                assert_eq!(
                    ex.len(),
                    1,
                    "{topic} help must have exactly one example, got {ex:?}"
                );
                assert!(
                    ex[0].starts_with("ds "),
                    "{topic} example is not a ds command: {ex:?}"
                );
            } else {
                // group help keeps one usage line per subcommand
                let usage = t.split("Usage:").nth(1).unwrap_or("");
                for sub in match topic {
                    "daemon" => vec!["ds daemon start", "ds daemon stop", "ds daemon status"],
                    _ => vec!["ds stream push", "ds stream stats", "ds stream stop"],
                } {
                    assert!(
                        usage.contains(sub),
                        "{topic} group help missing one-liner for {sub}"
                    );
                }
            }
        }
        // the top index points at the micro-app authoring route
        let top = text("top");
        assert!(
            top.contains("docs/microapps.md"),
            "top help must point at docs/microapps.md"
        );
        assert!(
            top.contains("examples/microapps/"),
            "top help must point at examples/microapps/"
        );
    }
}

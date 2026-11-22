# Component: ds (the CLI)

Stateless one-shot client of the daemon (R1). Full grammar contract: `docs/contracts/cli.md`.

- **Language:** Rust, sharing the `ds-client` crate with the daemon (ADR-002, D02).
  Why: a static binary starts in ~1–3 ms, so one-shot-per-call beats any interpreter
  (Python ≈30–50 ms startup) for the p50 ≤ 2 ms query budget (D13). Rejected: Python CLI,
  shell functions, reading state files directly. A Python *library* (`deckstack` PyPI
  package wrapping the same protocol) is provided later for composition, but the CLI
  itself is Rust.
- **Public surface:** `ds <group> <verb> [args] [--json|--ndjson|--watch]`; groups:
  `get set bind emit watch events stream plugin daemon token schema describe test migrate`.
  `--help` on every level; `ds schema <name>` prints the JSON Schema from `schemas/`;
  `ds describe <path>` prints the schema + current value + allowed verbs.
- **Invariants:**
  1. Zero state between invocations — no cache files; correctness never depends on a
     previous call.
  2. Exit codes are stable and documented (0 ok, 1 usage, 2 not-found, 3 conflict/rev,
     4 auth, 5 limit, 6 daemon unreachable, 7 device error, 8 internal).
  3. Human output is for humans; `--json` is for machines; never mix.
  4. `ds watch <path>` is the persistent mode: one connection, NDJSON events until
     Ctrl-C or `--max-events N`.
- **Failure modes:** daemon socket missing → exit 6 with "run `ds daemon start`" in the
  error text; rev conflict on `set --if-rev` → exit 3 with the current rev in the payload;
  token file unreadable → exit 4, never fall back to unauthenticated.
- **Dependencies:** `ds-client`, `ds-ipc` (framing), `schemas/` (embedded via include_str
  for `ds schema`).
- **Test command:** `python tests/conformance/run.py` drives the compiled `ds` binary as
  a black box against ds-vdev (R10) — the conformance suite IS the CLI spec executable.
- **Minimum working example:**
  ```sh
  ds get /controls/keys/0 --json
  ds set /controls/keys/0/color '#ff8800'
  ds bind /controls/keys/0/down --do 'emit demo.pressed' --json
  ds watch /controls --ndjson | head -5
  ds stream push keys --file frame.png
  ds plugin list --json
  ds daemon status
  ```

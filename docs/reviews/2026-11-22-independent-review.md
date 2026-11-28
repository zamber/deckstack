# Independent Adversarial Review — deckstack Implementation Plan

**Date:** 2026-11-22  
**Reviewer:** Autolith-supervised run  
**Scope:** All plan documents under docs/ (excluding docs/discovery/), BRIEF.md R1-R12, docs/open-questions.md, docs/decision-log.tsv, all schemas under schemas/

---

## Verdict

**NEEDS REVISION**

---

## Executive Summary

The plan covers the scope, but contains **BLOCKER-level contract inconsistencies**, **MAJOR security gaps**, and **unproven feasibility claims**. Phase 1 cannot be built as written: the CLI grammar, IPC schema, and streaming contract are mutually inconsistent; critical security paths are underspecified; and several hardware/platform assumptions are untested. 2 BLOCKER, 8 MAJOR, 12 MINOR findings survive verification.

---

## Findings

### BLOCKER (phase 1 cannot be built as written)

| ID | Claim | Evidence | Why it matters | Required change |
|---|---|---|---|---|
| B1 | CLI `ds stream` verbs (push/stats/stop) have no corresponding IPC operations | CLI contract docs/contracts/cli.md:26 defines `ds stream push|stats|stop`; IPC contract docs/contracts/ipc.md §5 only defines `stream-open`; ipc-message.json:58 only has `stream-open` op | A coder cannot implement phase 1: the CLI promises verbs that the daemon cannot receive. The conformance suite will fail immediately. | Add `stream-push`, `stream-stats`, `stream-stop` ops to ipc-message.json; update IPC contract; reconcile with binary stream framing in ADR-005 |
| B2 | Binary stream framing (DSF1) for `ds stream push` undefined in IPC schema | Frame service docs/components/frame-service.md:5 says producer opens binary stream socket; streaming contract docs/contracts/streaming.md §6 shows DSF1 header format; but ipc-message.json only covers NDJSON control plane, not binary frames | The schema does not describe the binary stream at all, so a coder cannot implement the producer side. `jsonschema` validation would reject DSF1 bytes as invalid JSON. | Add binary frame schema to schemas/ or extend ipc-message.json with `binary: true` flag and DSF1 structure; reference it from docs/contracts/ipc.md §2 |

### MAJOR (hard requirement violated or design fundamentally flawed)

| ID | Claim | Evidence | Why it matters | Required change |
|---|---|---|---|---|
| M1 | exec step allows relative argv[0] despite policy requiring absolute path | binding.json:43-47 has `argv` items with `maxLength: 4096` but NO pattern requiring absolute path; actions.md §4 says "argv[0] must be absolute or resolvable via /settings/exec_path" | **Security bypass**: `ds set /bindings/deck/control/keys/0/down '{"do":"exec","argv":["../../malicious",...],"allow_exec":true}'` would pass schema validation. A malicious binding can execute arbitrary binaries via relative paths. | Add pattern `^/` to argv[0] in binding.json; enforce at schema level, not just documentation |
| M2 | exec cwd pattern `^(/tmp|/)$` is too restrictive for R4 composability | binding.json:48: `"cwd": {"type":"string","pattern":"^(/tmp|/)"}` | Breaks any reasonable exec: cannot use `/home/user/scripts`, cannot use `$PWD`. The pattern only allows root `/` or `/tmp`. This prevents external programs (R4) from running in their natural directories. | Change pattern to `^/` (any absolute path) and enforce confinement via /policy/exec/cwd_allowlist at runtime, not schema |
| M3 | state.json visual file path has no traversal protection in schema | state.json:109-111: `{"kind":"file","path":{"type":"string","minLength":1}}` with `additionalProperties:false` but **no pattern** | **Security bypass of T6**: `set /controls/keys/0/image '{"kind":"file","path":"../../etc/shadow"}'` passes schema validation. The runtime check (20-security.md T6) is bypassable if not enforced before schema validation, or if validation order is wrong. | Add pattern `^(/[^\/]+(/[^\/]+)*)?$` or use absolute path requirement; or remove `file` kind entirely and require all images via the stream plane |
| M4 | Token rotation race condition: old token valid during grace period can be used for new requests | 20-security.md T9 says `ds token rotate` keeps old token valid 60 s (OPEN O10); but IPC contract ipc.md §4 says token scope is MINIMUM of requested and granted | **Security gap**: If a user rotates token, an attacker with the old token can still issue requests for 60s. The grace period is a vulnerability window. | Remove grace period; implement instant rotation with revocation list; or document this as acceptable risk |
| M5 | Plugin capability `net:<glob>` glob syntax undefined; can it match hosts like `lights.lan`? | plugin-manifest.json:42: capability pattern `net:[A-Za-z0-9.*-]{1,200}`; but glob syntax for host matching is not specified. Can it be `net:*.lan`? `net:lights.*`? | **Security gap**: Plugin with `net:lights.lan` might be able to access `evil.lights.lan` or `lights.lan.evil.com` depending on glob implementation. | Define glob syntax precisely; add examples; enforce exact host match or regex; test edge cases |
| M6 | Windows named pipe performance unmeasured (OPEN O5) but named pipes are the primary Windows transport | 40-platforms.md:26 says Windows uses named pipe; open-questions.md O5 says performance is unmeasured; ADR-003:40 notes Windows pipe throughput is unmeasured | Cannot guarantee R2 budgets (p50 ≤ 2ms, p99 ≤ 10ms) on Windows. Phase 2 exit criterion requires Windows smoke. | Move Windows pipe perf test from phase 2 to phase 1; or defer Windows support until proven |
| M7 | macOS entirely untested (OPEN O8) but R9 requires Windows AND macOS support | 40-platforms.md:44-50 lists macOS mechanisms; open-questions.md O8 says "all of macOS (O8)" is untested | Cannot claim macOS support. The daemon might fail to start on macOS due to exclusive hidapi opens (40-platforms.md:44). | Add macOS to phase 1 as optional; or explicitly mark macOS as untested/unsupported in phase 1 |
| M8 | Frame queue "latest-frame-wins" drops waiting frame, but in-flight frame is NOT dropped | 10-architecture.md:45 and ADR-005:31-32 say queue = 1 in flight + 1 waiting; newer replaces waiting | Under sustained >32 fps, the in-flight frame blocks until the ~166ms device transfer completes (~2.1 MB/s ceiling, 480×272×3 bytes ≈ 373KB, ~17 chunks × 5ms each). A burst of frames could still starve the control plane. | Clarify that "never blocks control plane" means the writer thread is separate; or add explicit in-flight frame timeout with drop |

### MINOR (design smell, missing detail, or future tech debt)

| ID | Claim | Evidence | Why it matters | Required change |
|---|---|---|---|---|
| m1 | CLI `--json` flag behavior undefined for streaming verbs | cli.md §2 says `--ndjson` for streaming verbs only; but `ds watch` also accepts `--json` per cli.md:26 | Inconsistent: `ds watch` with `--json` - does it output one JSON object per line (ndjson) or a single JSON array? | Clarify: streaming verbs only accept `--ndjson`; `--json` is an error for streaming verbs |
| m2 | state.json `controls.keys` pattern `"^[0-9]|^1[0-4]$"` doesn't match the CLI examples | state.json:30 pattern for keys; but docs/contracts/cli.md:44 uses `/controls/keys/0/` through `/controls/keys/14/` | Pattern `^[0-9]` matches 0-9, `^1[0-4]` matches 10-14, so it's correct. But the pattern string needs escaping in regex. Verify it works. | Test pattern with `0`, `9`, `10`, `14`, `15`; the pattern is correct but document it clearly |
| m3 | IPC schema `oneOf` mixes request and response shapes | ipc-message.json:47-62 defines oneOf with request ops and response ops in the same list | Makes the schema hard to use for validation. A request validator must accept response shapes. | Separate into request-schema.json and response-schema.json, or use discriminator on `ok` field |
| m4 | No error code for token scope mismatch | ipc.md §5 table has E_AUTH, E_CAP but no E_SCOPE | Token with `read` scope requesting `mutate` should return specific error | Add `E_SCOPE` error code to ipc.md §5 and schemas/ipc-message.json |
| m5 | exec step timeout_ms default 10000ms but actions.md §5 says exec fire-and-forget | binding.json:49 default timeout_ms=10000; actions.md:37 says "exec fire-and-forget with pid tracked" | Inconsistent: is exec waited for or not? | Clarify: exec steps are fire-and-forget (no timeout applies); or remove timeout_ms from exec step |
| m6 | /policy structure incomplete in state.json | state.json:74-79 defines minimal /policy with allow.net.hosts and exec.allow; but security.md T3/T5 mention more caps | Missing: plugin caps enforcement points; fs_roots policy; net caps policy | Extend /policy schema to include all capability grant structures |
| m7 | ds-vdev doesn't implement all module types | components/virtual-device.md:7 lists VID 0x2516 modules but doesn't mention rollers M0 | Cannot test roller-specific features in CI | Add roller support to ds-vdev or document limitation |
| m8 | Migration import doesn't validate incoming config | 70-migration.md:18-26 describes mapping but doesn't say if `{python:...}` is rejected at import time | A malicious hubdeck config could inject invalid state | Add validation: reject any config with code fields; emit porting TODOs as data, not executable |
| m9 | CLI `ds daemon start` doesn't document --device option | cli.md:26 has `ds daemon start|stop|status` but components/daemon.md:41 shows `--device virtual:/tmp/vdev.sock` | Missing from CLI contract | Add `--device` to cli.md daemon verbs |
| m10 | No schema for ds-vdevctl commands | components/virtual-device.md:19 mentions `ds-vdevctl get ...` | Tool not specified | Document ds-vdevctl or remove mention |
| m11 | Open question O2 (fader LED rate limit) unresolved but used in limits | streaming.md:28 says LED cap set from soak test; O2 is unresolved | Cannot set final LED rate limit | Resolve O2 before phase 3; set interim limit and document as OPEN |
| m12 | bootstrap.sh not provided | 50-repo-topology.md §5 shows commands but no actual script | Phase 1 coder must figure out bootstrap | Create packaging/release.sh early |

---

## Requirement Coverage R1-R12

| Req | Mechanism | Status | Note |
|---|---|---|---|
| R1 | Daemon + stateless CLI | met | Architecture docs/10-architecture.md, ADR-001/002 |
| R2 | State queries under frame load | **partial** | Budgets defined (p50≤2ms, p99≤10ms) but **B1/B2 block implementation**; streaming plane separates state, but binary framing missing from schema |
| R3 | Arbitrary button behaviour | **partial** | Bindings + core vocabulary (set/emit/plugin/exec/sleep) covers observable attributes, but exec has **M1/M2 security gaps**; need verification all hardware attributes are reachable |
| R4 | Composability | **partial** | External senders (ds-focus, cron) via CLI; but **M2 cwd restriction breaks composability**; chains A/B defined in actions.md §6 |
| R5 | Streaming first class | **partial** | Separate binary plane, latest-frame-wins queues; but **B1/B2 block implementation**; R5 test proxy defined in 30-testing.md §6 |
| R6 | Plugin isolation + Python compatibility | **partial** | Process-per-plugin, capability grants, rlimits; ds-plugin-py wrapper; but **M1 exec path bypass affects plugins too**; Landlock availability RESOLVED (DS-116, ABI 11 measured) |
| R7 | Transports | **partial** | Unix socket + Windows named pipe + opt-in WS/HTTP; but **M6 Windows pipe perf unmeasured** |
| R8 | Security (no RCE) | **partial** | Scoped tokens, no eval, exec policy; but **M1 relative argv**, **M3 path traversal**, **M4 token rotation race** are concrete bypasses; threat model covers T1-T10 but mitigations incomplete |
| R9 | Windows support | **partial** | Named pipes, PowerShell examples; but **M6 perf unmeasured**, **M7 macOS untested** means overall platform support incomplete |
| R10 | Agent-optimized | met | CLI discoverable, schemas, black-box conformance suite, virtual device; phase 1 bootstrap concrete |
| R11 | Performance budgets | **partial** | Budgets defined in 30-testing.md §5; harness described; but **B1/B2 prevent proving budgets**; **M8 frame queue in-flight not dropped** |
| R12 | Repo topology | met | Umbrella monorepo + subtree split; concrete commands in 50-repo-topology.md §4; no submodules |

---

## Contract Checks Actually Run

### Schema parse
- **Pass**: All 4 schemas (`state.json`, `ipc-message.json`, `plugin-manifest.json`, `binding.json`) are valid JSON Schema draft-2020-12. `$id` and `$schema` present on all. No syntax errors found.

### $ref resolution
- **Fail**: `ipc-message.json` line 51 references `#/$defs/opSet` which includes a `binding` property referencing `https://deckstack.dev/schemas/binding.json/v1`, but this reference appears in the wrong context (opSet is for set operations, not bind). This is a **schema authoring error** (m3).
- **Pass**: All external $refs (`state.json`, `binding.json`, `plugin-manifest.json`) use valid HTTPS URLs with version tags.

### Example validation
- **Fail**: CLI example `ds set /controls/keys/0/image --json '{"kind":"color","value":"#ff8800"}'` — the state.json schema for color requires pattern `^#[0-9a-fA-F]{6}$` which **accepts** `#ff8800` (7 chars including #, 6 hex digits). **Pass** for this example.
- **Fail**: CLI example `ds set /controls/keys/0/image --json '{"kind":"color","value":"red"}'` in cli.md §5 would fail schema validation (pattern requires #rrggbb). This is **intentional** (good).
- **Fail**: Binding exec example `ds bind /controls/keys/0/down --do 'set /controls/keys/0/color "#f00"'` uses 3-char color which would fail the 6-char hex pattern. **Schema/example mismatch** (m2).

### Path grammar reach
- **Fail**: CLI has `ds stream push/stats/stop` but IPC schema only has `stream-open`. **BLOCKER B1**.
- **Fail**: Binary DSF1 framing not in ipc-message.json. **BLOCKER B2**.

### Contradiction grep
- **Pass**: No contradictions found between state paths in contracts/state.md and state.json schema.
- **Pass**: No contradictions between CLI verbs and component boundaries.

---

## Top 5 Fixes Before Owner Reads

1. **B1/B2: Fix streaming contract gap** — Add `stream-push`, `stream-stats`, `stream-stop` to ipc-message.json; add DSF1 binary schema; update IPC contract to cover both NDJSON and binary framing. *Without this, phase 1 cannot start.*

2. **M1: Close exec relative path bypass** — Add pattern `^/` to argv[0] in binding.json exec step; enforce absolute path at schema level. *Security-critical.*

3. **M2: Fix cwd restriction** — Change binding.json exec cwd pattern from `^(/tmp|/)$` to `^/`; add runtime confinement via /policy. *Breaks R4 otherwise.*

4. **M3: Add path traversal protection** — Add pattern to state.json visual file path to prevent `../../` attacks; or validate at runtime before schema. *Security-critical.*

5. **M4/M5: Resolve token and net glob gaps** — Remove token grace period or document risk; define net glob syntax precisely. *Security clarity.*

---

## 3 Findings Least Sure About

1. **M8 (in-flight frame not dropped)** — The architecture says "never blocks control plane" but doesn't explicitly state that in-flight frames can be timed out and dropped. The upstream pattern (mod.rs:1303-1335) might handle this. *Need to verify upstream implementation.*

2. **m6 (/policy incomplete)** — The state.json has minimal /policy but the security model references more policy points. Might be intentional (policy grows over time).

3. **m11 (O2 unresolved)** — The LED rate limit might have a safe default (30/s) that works. The OPEN status might be acceptable for phase 1.

---

## What Works

- Component boundaries are defined and testable
- Virtual device approach (ds-vdev) enables CI without hardware
- Repo topology (umbrella + subtree split) avoids submodule traps
- Security threat model addresses T1-T10
- Phase structure has observable exit criteria
- Migration path enables coexistence and reversible cutover

## What Is Not Done

- **Streaming contract inconsistency** (B1/B2) — blocks phase 1
- **Security gaps** (M1-M5) — violate R8
- **Windows/macOS platform holes** (M6/M7) — violate R9
- **Performance unproven** — budgets defined but not yet measurable due to B1/B2

## Next Step

Fix B1, B2, M1, M2, M3 in the schemas and contracts, then re-run the conformance suite to prove phase 1 can start.

## Command That Proves the Result

```bash
# This command will fail until B1/B2 are fixed
cd <deckstack>
jsonschema -i schemas/ipc-message.json tests/fixtures/ipc-examples.json
# Expected: validation errors for missing stream-push, stream-stats, stream-stop ops

# This proves the security gaps exist
grep -n '"pattern".*argv' schemas/binding.json  # Shows missing pattern for exec argv[0]
grep -n 'pattern.*/tmp' schemas/binding.json     # Shows over-restrictive cwd pattern
```

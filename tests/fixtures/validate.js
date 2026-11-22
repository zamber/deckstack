// Validate schemas/*.json (parse + $ref resolution) and the example corpora
// (tests/fixtures/ipc-examples.json plus inline state/binding/stream-frame examples,
// including negative tests) against them. Implements the JSON Schema 2020-12 subset
// used by our schemas; external $refs resolve against the local schemas/ directory
// by $id, so no network is needed.
// Run: node tests/fixtures/validate.js   (also driven by validate.sh; validate.py is the
// jsonschema-library twin for CI)
const fs = require("fs");
const path = require("path");
const ROOT = path.join(__dirname, "..", "..");
const SCHEMAS = path.join(ROOT, "schemas");

const registry = new Map(); // $id -> schema
for (const f of fs.readdirSync(SCHEMAS).filter((f) => f.endsWith(".json"))) {
  const s = JSON.parse(fs.readFileSync(path.join(SCHEMAS, f), "utf8"));
  registry.set(s.$id, s);
  console.log(`ok  parses: schemas/${f} ($id ${s.$id})`);
}

let failures = 0;
const errors = [];
const fail = (msg) => { errors.push(msg); };

// returns [docRoot, node]
function resolveRef(ref, docRoot) {
  if (ref.startsWith("#")) {
    let node = docRoot;
    for (const seg of ref.slice(1).split("/").filter(Boolean)) {
      node = node[seg.replace(/~1/g, "/").replace(/~0/g, "~")];
      if (node === undefined) throw new Error("bad local ref " + ref);
    }
    return [docRoot, node];
  }
  const hash = ref.indexOf("#");
  const base = hash === -1 ? ref : ref.slice(0, hash);
  const frag = hash === -1 ? "" : ref.slice(hash);
  const doc = registry.get(base);
  if (!doc) throw new Error("unresolved external $ref: " + ref);
  return frag ? resolveRef("#" + frag, doc) : [doc, doc];
}

function validate(inst, schema, docRoot, where) {
  if (schema.$ref) {
    const [d, node] = resolveRef(schema.$ref, docRoot);
    return validate(inst, node, d, where);
  }
  if (schema.allOf) for (const s of schema.allOf) validate(inst, s, docRoot, where);
  if (schema.oneOf || schema.anyOf) {
    const branches = schema.oneOf || schema.anyOf;
    const want = schema.oneOf ? 1 : null; // oneOf: exactly 1; anyOf: >=1
    let matched = 0;
    for (const s of branches) {
      const mark = errors.length;
      validate(inst, s, docRoot, where);
      if (errors.length === mark) matched++;
      else errors.length = mark; // discard branch errors
    }
    if (want === 1 ? matched !== 1 : matched < 1) fail(`${where}: ${schema.oneOf ? "oneOf" : "anyOf"} matched ${matched} branches`);
  }
  if (schema.not) {
    const mark = errors.length;
    validate(inst, schema.not, docRoot, where);
    if (errors.length === mark) fail(`${where}: 'not' schema matched (must not)`);
    else errors.length = mark; // 'not' did not match: discard its subschema errors only
  }
  if (schema.const !== undefined && inst !== schema.const) fail(`${where}: const ${JSON.stringify(schema.const)} != ${JSON.stringify(inst)}`);
  if (schema.enum && !schema.enum.includes(inst)) fail(`${where}: ${JSON.stringify(inst)} not in enum`);
  if (schema.type) {
    const t = Array.isArray(inst) ? "array" : inst === null ? "null" : typeof inst;
    const ok = schema.type === t || (schema.type === "integer" && t === "number" && Number.isInteger(inst)) || (schema.type === "number" && t === "number");
    if (!ok) fail(`${where}: type '${schema.type}' != '${t}'`);
  }
  if (typeof inst === "string") {
    if (schema.pattern && !new RegExp(schema.pattern).test(inst)) fail(`${where}: ${JSON.stringify(inst)} !~ /${schema.pattern}/`);
    if (schema.minLength !== undefined && inst.length < schema.minLength) fail(`${where}: minLength ${schema.minLength}`);
    if (schema.maxLength !== undefined && inst.length > schema.maxLength) fail(`${where}: maxLength ${schema.maxLength}`);
  }
  if (typeof inst === "number") {
    if (schema.minimum !== undefined && inst < schema.minimum) fail(`${where}: minimum ${schema.minimum}`);
    if (schema.maximum !== undefined && inst > schema.maximum) fail(`${where}: maximum ${schema.maximum}`);
  }
  if (Array.isArray(inst)) {
    if (schema.minItems !== undefined && inst.length < schema.minItems) fail(`${where}: minItems`);
    if (schema.maxItems !== undefined && inst.length > schema.maxItems) fail(`${where}: maxItems`);
    const pre = schema.prefixItems || [];
    inst.forEach((v, i) => {
      if (i < pre.length) validate(v, pre[i], docRoot, `${where}[${i}]`);
      else if (schema.items) validate(v, schema.items, docRoot, `${where}[${i}]`);
    });
    if (schema.uniqueItems) { const seen = new Set(inst.map((x) => JSON.stringify(x))); if (seen.size !== inst.length) fail(`${where}: uniqueItems`); }
  }
  if (inst && typeof inst === "object" && !Array.isArray(inst)) {
    if (schema.required) for (const r of schema.required) if (!(r in inst)) fail(`${where}: missing required '${r}'`);
    const props = schema.properties || {};
    const pats = Object.entries(schema.patternProperties || {});
    for (const [k, v] of Object.entries(inst)) {
      if (props[k]) { validate(v, props[k], docRoot, `${where}.${k}`); continue; }
      let covered = false;
      for (const [p, ps] of pats) if (new RegExp(p).test(k)) { covered = true; validate(v, ps, docRoot, `${where}.${k}`); }
      if (!covered) {
        if (schema.additionalProperties === false) fail(`${where}: additionalProperty '${k}' not allowed`);
        else if (schema.additionalProperties && typeof schema.additionalProperties === "object") validate(v, schema.additionalProperties, docRoot, `${where}.${k}`);
      }
    }
  }
}

// ---- 1. every $ref in every schema resolves locally ----
let refCount = 0;
for (const s of registry.values()) {
  (function w(o) {
    if (o && typeof o === "object" && !Array.isArray(o)) {
      if (typeof o.$ref === "string") {
        refCount++;
        try { resolveRef(o.$ref, s); } catch (e) { fail(e.message); }
      }
      for (const v of Object.values(o)) w(v);
    } else if (Array.isArray(o)) o.forEach(w);
  })(s);
}
if (errors.length === 0) console.log(`ok  ${refCount} $refs checked, all resolve locally`);
else { errors.forEach((e) => fail(e)); }

// helper: run one instance, report
function check(label, inst, schemaId, wantValid) {
  const schema = registry.get(schemaId);
  const mark = errors.length;
  validate(inst, schema, schema, label);
  const got = errors.length === mark;
  if (got === wantValid) console.log(`ok  ${label} (${wantValid ? "valid" : "rejected"} as expected)`);
  else { failures++; console.log(`FAIL ${label}: expected ${wantValid ? "valid" : "invalid"}, got ${got ? "valid" : "invalid"}`); }
  errors.length = mark;
}

// ---- 2. IPC example corpus ----
const corpus = JSON.parse(fs.readFileSync(path.join(ROOT, "tests/fixtures/ipc-examples.json"), "utf8"));
const IPC = "https://deckstack.dev/schemas/ipc-message.json/v1";
corpus.messages.forEach((m, i) => check(`message[${i}] ${m.op ?? "response id=" + m.id}`, m, IPC, true));

// ---- 2b. negative IPC fixtures (DS-093): request/response discrimination via `ok` ----
// A response-class message (has id/result/error, no op) must carry boolean `ok`; a request
// must not; events keep their shape (op without ok). See schemas/ipc-message.json oneOf and
// docs/contracts/ipc.md §4 (m3).
check("response without ok rejected (DS-093)", {"id": 5, "rev": 42}, IPC, false);
check("response with result but no ok rejected (DS-093)", {"id": 3, "result": {"pong": true}}, IPC, false);
check("error response without ok rejected (DS-093)", {"id": 7, "error": {"code": "E_AUTH", "message": "bad token"}}, IPC, false);
check("ok must be boolean (DS-093)", {"id": 5, "ok": "true", "rev": 42}, IPC, false);
// Note: a request shape carrying `ok` (e.g. {"op":"ping","ok":true}) is NOT rejected by the
// normative schema — the response branch of the oneOf accepts any object with boolean `ok`
// (it does not forbid `op`). The request branch's `not` only stops it matching as a request.
// Reported as a schema observation in prs/DS-093.md; not asserted here.
check("event without op rejected (DS-093)", {"path": "/controls/keys/0", "rev": 42}, IPC, false);
check("event shape still valid (DS-093)", {"op": "event", "path": "/controls/keys/0", "rev": 42, "value": {"kind": "color", "value": "#3cc86e"}}, IPC, true);
check("proper response still valid (DS-093)", {"id": 5, "ok": true, "rev": 42}, IPC, true);

// ---- 3. state document example + negatives ----
const STATE = "https://deckstack.dev/schemas/state.json/v1";
const doc = {
  meta: { version: 1, rev: 42, protocol: 1 },
  devices: { ABC123: { kind: "keys", model: "M4315", pid: 510, slot: "1-4-3", connection: "connected", last_seen_rev: 40 } },
  controls: {
    keys: { "0": { image: { kind: "color", value: "#3cc86e" }, label: "Salon", brightness: 80 } },
    dial: { image: { kind: "frame", target: "dial", seq: 7, sha256: "a".repeat(64) } },
    knobs: { "0": { led: { mode: "static", color: "#3cc86e", brightness: 40 } } },
    faders: {}, rollers: {},
  },
  bindings: [
    { id: "b0001", topic: "deck/control/keys.0/down", allow_exec: false, steps: [{ do: "plugin", id: "demo", action: "bump", args: { by: 1 } }] },
    { id: "b0002", topic: "app/focus/changed", when: { payload: "app", op: "eq", value: "code" }, steps: [{ do: "set", path: "/controls/keys/3/image", value: { kind: "color", value: "#00ff00" } }] },
    { id: "b0003", topic: "deck/control/keys.1/down", allow_exec: true, steps: [{ do: "exec", argv: ["/usr/bin/ffmpeg", "-i", "v.mp4"], cwd: "/home/u/scripts" }] },
  ],
  plugins: {},
  streams: { keys: { queue_depth: 2 } },
  settings: {},
  policy: { net: { allow_hosts: ["lights.lan"] }, fs: { roots: ["/home/u/.local/share/deckstack/media"] }, exec: { allow: ["/usr/bin/ffmpeg"], deny: ["/bin/sh", "/bin/bash", "/usr/bin/env"] }, led_rate_per_s: 30 },
  telemetry: { streams: { keys: { sent: 960, dropped: 12, fps: 32.0, bytes: 6700000, queue: 1, in_flight: true, last_sha256: "a".repeat(64), last_seq: 959 } } },
  focus: { app: "code", title: "PLAN.md", pid: 123, at_rev: 42 },
};
check("state document example", doc, STATE, true);
const neg = (label, mutate) => { const bad = JSON.parse(JSON.stringify(doc)); mutate(bad); check(label, bad, STATE, false); };
// S8: key/knob/fader/roller indices are enforced at OBJECT level — the controls maps in
// schemas/state.json carry additionalProperties:false on top of the pattern, so a
// non-matching index ("15", "99", "3" for knobs, …) is rejected by the normative schema
// itself (gap found by DS-093, closed by DS-096).
neg("keys.15 rejected at object level (S8)", (d) => { d.controls.keys["15"] = { label: "x" }; });
neg("keys.99 rejected at object level (S8)", (d) => { d.controls.keys["99"] = { label: "x" }; });
neg("knobs.3 rejected at object level (S8)", (d) => { d.controls.knobs["3"] = { led: { mode: "off", color: "#000000", brightness: 0 } }; });
neg("faders.5 rejected at object level (S8)", (d) => { d.controls.faders["5"] = { value: 50 }; });
neg("rollers.2 rejected at object level (S8)", (d) => { d.controls.rollers["2"] = { value: 50 }; });
neg("streams target 'panel' rejected at object level (S6/S8)", (d) => { d.streams["panel"] = { queue_depth: 2 }; });
neg("telemetry target 'key:15' rejected at object level (S6/S8)", (d) => { d.telemetry.streams["key:15"] = { sent: 1 }; });
neg("unknown control kind rejected at object level (S8)", (d) => { d.controls["m0-base"] = {}; });
neg("file traversal rejected (M3)", (d) => { d.controls.keys["0"].image = { kind: "file", path: "../../etc/shadow" }; });
neg("relative argv[0] rejected (M1)", (d) => { d.bindings[2].steps[0].argv[0] = "sh"; });
neg("exec timeout_ms rejected (S11)", (d) => { d.bindings[2].steps[0].timeout_ms = 5000; });
neg("binding id without b-prefix rejected (S2)", (d) => { d.bindings[0].id = "x1"; });
neg("eval-like step rejected (R8)", (d) => { d.bindings[0].steps[0] = { do: "eval", code: "x" }; });

// ---- 4. stream-frame meta ----
const SF = "https://deckstack.dev/schemas/stream-frame.json/v1";
check("DSF1 meta example", { target: "keys", seq: 7, ts_ms: 1732000000000, format: "png", w: 480, h: 272, frames: 1, crc32: 123456, max_payload_bytes: 16777216 }, SF, true);
check("DSF1 target 'panel' rejected (S6)", { target: "panel", seq: 1, ts_ms: 1, format: "png", crc32: 1, max_payload_bytes: 16777216 }, SF, false);
check("DSF1 301 frames rejected", { target: "keys", seq: 1, ts_ms: 1, format: "gif", frames: 301, crc32: 1, max_payload_bytes: 16777216 }, SF, false);

if (failures + (errors.length > 0 ? 1 : 0) === 0 && errors.length === 0) console.log("ALL CHECKS PASSED");
else { console.log(`${failures} hard failures, ${errors.length} validation errors`); errors.slice(0, 20).forEach((e) => console.log("  err: " + e)); process.exit(1); }

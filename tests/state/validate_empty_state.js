// Validate tests/state/empty-state.json — the canonical empty state document that
// crates/ds-state produces on a fresh start (Store::fresh / empty_document) — against
// schemas/state.json and the schemas it $refs to, plus negative cases the schema
// genuinely rejects. Self-contained (no network); mirrors tests/fixtures/validate.js
// resolution rules: external $refs resolve against the local schemas/ dir by $id.
//
// Run: node tests/state/validate_empty_state.js   (prints ALL CHECKS PASSED, exit 0)
//
// Note (contract gap, reported not worked around): schemas/state.json uses
// patternProperties WITHOUT additionalProperties:false under /controls and /streams,
// so pure JSON-Schema validation admits e.g. /controls/keys/99 even though the schema
// description says '15'/'99' must NOT match (S8). The index vocabulary is therefore
// enforced by ds-state's path layer (crates/ds-state/src/path.rs), not by the schema.
const fs = require("fs");
const path = require("path");
const ROOT = path.join(__dirname, "..", "..");
const SCHEMAS = path.join(ROOT, "schemas");

const registry = new Map();
for (const f of fs.readdirSync(SCHEMAS).filter((f) => f.endsWith(".json"))) {
  const s = JSON.parse(fs.readFileSync(path.join(SCHEMAS, f), "utf8"));
  registry.set(s.$id, s);
}

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

// Returns a list of error strings (empty = valid).
function validate(inst, schema, docRoot) {
  const errors = [];
  const fail = (m) => errors.push(m);
  if (schema.$ref) {
    const [d, node] = resolveRef(schema.$ref, docRoot);
    return validate(inst, node, d);
  }
  if (schema.allOf) for (const s of schema.allOf) errors.push(...validate(inst, s, docRoot));
  if (schema.oneOf || schema.anyOf) {
    const branches = schema.oneOf || schema.anyOf;
    const want = schema.oneOf ? 1 : null;
    let matched = 0;
    for (const s of branches) {
      const sub = validate(inst, s, docRoot);
      if (sub.length === 0) matched++;
    }
    if (want === 1 ? matched !== 1 : matched < 1)
      fail(`${schema.oneOf ? "oneOf" : "anyOf"} matched ${matched} branches`);
    return errors;
  }
  if (schema.const !== undefined && JSON.stringify(inst) !== JSON.stringify(schema.const))
    fail(`const ${JSON.stringify(schema.const)} != ${JSON.stringify(inst)}`);
  if (schema.enum && !schema.enum.some((e) => JSON.stringify(e) === JSON.stringify(inst)))
    fail(`${JSON.stringify(inst)} not in enum`);
  if (schema.type) {
    const t = Array.isArray(inst) ? "array" : inst === null ? "null" : typeof inst;
    const ok =
      schema.type === t ||
      (schema.type === "integer" && t === "number" && Number.isInteger(inst)) ||
      (schema.type === "number" && t === "number");
    if (!ok) fail(`type '${schema.type}' != '${t}'`);
  }
  if (typeof inst === "string") {
    if (schema.pattern && !new RegExp(schema.pattern).test(inst)) fail(`${inst} !~ /${schema.pattern}/`);
    if (schema.minLength !== undefined && inst.length < schema.minLength) fail("minLength");
    if (schema.maxLength !== undefined && inst.length > schema.maxLength) fail("maxLength");
    if (schema.format === "date-time" && isNaN(Date.parse(inst))) fail(`bad date-time ${inst}`);
  }
  if (typeof inst === "number") {
    if (schema.minimum !== undefined && inst < schema.minimum) fail("minimum");
    if (schema.maximum !== undefined && inst > schema.maximum) fail("maximum");
  }
  if (Array.isArray(inst)) {
    if (schema.minItems !== undefined && inst.length < schema.minItems) fail("minItems");
    if (schema.maxItems !== undefined && inst.length > schema.maxItems) fail("maxItems");
    const pre = schema.prefixItems || [];
    inst.forEach((v, i) => {
      if (i < pre.length) errors.push(...validate(v, pre[i], docRoot));
      else if (schema.items) errors.push(...validate(v, schema.items, docRoot));
    });
    if (schema.uniqueItems) {
      const seen = new Set(inst.map((x) => JSON.stringify(x)));
      if (seen.size !== inst.length) fail("uniqueItems");
    }
  }
  if (inst && typeof inst === "object" && !Array.isArray(inst)) {
    if (schema.required) for (const r of schema.required) if (!(r in inst)) fail(`missing required '${r}'`);
    const props = schema.properties || {};
    const pats = Object.entries(schema.patternProperties || {});
    for (const [k, v] of Object.entries(inst)) {
      if (props[k]) { errors.push(...validate(v, props[k], docRoot)); continue; }
      let covered = false;
      for (const [p, ps] of pats) if (new RegExp(p).test(k)) { covered = true; errors.push(...validate(v, ps, docRoot)); }
      if (!covered) {
        if (schema.additionalProperties === false) fail(`additionalProperty '${k}' not allowed`);
        else if (schema.additionalProperties && typeof schema.additionalProperties === "object")
          errors.push(...validate(v, schema.additionalProperties, docRoot));
      }
    }
  }
  return errors;
}

const STATE = registry.get("https://deckstack.dev/schemas/state.json/v1");
let failures = 0;
function check(label, inst, wantValid) {
  const errs = validate(inst, STATE, STATE);
  const got = errs.length === 0;
  if (got === wantValid) console.log(`ok  ${label} (${wantValid ? "valid" : "rejected"} as expected)`);
  else { console.log(`FAIL ${label}: expected ${wantValid ? "valid" : "invalid"}, got ${got ? "valid" : "invalid"}`); errs.slice(0, 5).forEach((e) => console.log(`    ${e}`)); failures++; }
}

// 1. The fixture itself: parses, validates, and is the empty-state invariant.
const fixture = JSON.parse(fs.readFileSync(path.join(__dirname, "empty-state.json"), "utf8"));
check("tests/state/empty-state.json validates against schemas/state.json", fixture, true);
for (const [k, want] of [["controls", {}], ["bindings", []], ["plugins", {}], ["devices", {}], ["streams", {}], ["settings", {}], ["policy", {}], ["telemetry", {}]]) {
  if (JSON.stringify(fixture[k]) === JSON.stringify(want)) console.log(`ok  empty /${k}`);
  else { console.log(`FAIL /${k} is not empty: ${JSON.stringify(fixture[k])}`); failures++; }
}
if (fixture.meta && fixture.meta.version === 1 && fixture.meta.rev === 0 && fixture.meta.protocol === 1)
  console.log("ok  /meta = {version:1, rev:0, protocol:1}");
else { console.log("FAIL /meta shape"); failures++; }

// 2. A filled-in document (the shape the daemon builds via transactions) validates.
const sha = "a".repeat(64);
check("filled document validates", {
  meta: { version: 1, rev: 42, protocol: 1, daemon_pid: 7, started_at: "2026-11-22T10:00:00Z" },
  devices: { ABC123: { kind: "keys", model: "M4315", pid: 510, slot: "1-4-3", connection: "connected", last_seen_rev: 40 } },
  controls: {
    keys: { "0": { image: { kind: "color", value: "#3cc86e" }, label: "Salon", brightness: 80 } },
    dial: { image: { kind: "frame", target: "dial", seq: 7, sha256: sha } },
    knobs: { "0": { led: { mode: "static", color: "#3cc86e", brightness: 40 }, value: 10 } },
    faders: {}, rollers: {},
  },
  bindings: [{ id: "b0001", topic: "deck/control/keys.0/down", allow_exec: false, steps: [{ do: "plugin", id: "demo", action: "bump", args: { by: 1 } }] }],
  plugins: { demo: { status: "running", stats: { restarts: 0, violations: 0, invokes: 3 } } },
  streams: { keys: { queue_depth: 2 } },
  settings: { log: { level: "info" } },
  policy: { net: { allow_hosts: ["lights.lan"] }, exec: { allow: ["/usr/bin/ffmpeg"], deny: ["/bin/sh"] }, led_rate_per_s: 30 },
  telemetry: { streams: { keys: { sent: 960, dropped: 12, fps: 32.0, in_flight: true, last_seq: 959, last_sha256: sha } } },
  focus: { app: "code", title: "PLAN.md", pid: 123, at_rev: 42 },
}, true);

// 3. Negatives the schema genuinely rejects (root is closed, consts, patterns, $refs).
const neg = (label, mutate) => {
  const bad = JSON.parse(JSON.stringify(fixture));
  mutate(bad);
  check(label, bad, false);
};
neg("missing required /policy rejected", (d) => { delete d.policy; });
neg("additional top-level property rejected (root closed)", (d) => { d.bogus = {}; });
neg("meta.version=2 rejected (const 1)", (d) => { d.meta.version = 2; });
neg("rev as string rejected", (d) => { d.meta.rev = "0"; });
neg("bad color rejected", (d) => { d.controls = { keys: { "0": { image: { kind: "color", value: "red" } } } }; });
neg("file image with '..' rejected (M3)", (d) => { d.controls = { keys: { "0": { image: { kind: "file", path: "/a/../../etc/x" } } } }; });
neg("bad binding id rejected (S2, external $ref)", (d) => { d.bindings = [{ id: "x1", topic: "deck/x", steps: [{ do: "sleep", ms: 5 }] }]; });
neg("exec step with relative argv[0] rejected (M1)", (d) => { d.bindings = [{ id: "b0001", topic: "deck/x", allow_exec: true, steps: [{ do: "exec", argv: ["sh", "-c", "x"] }] }]; });
neg("queue_depth=3 rejected", (d) => { d.streams = { keys: { queue_depth: 3 } }; });
// NOTE: stream target 'panel' and keys index '99'/'15' are NOT schema-rejected
// (patternProperties without additionalProperties:false — the reported S8 gap);
// they are rejected by ds-state's path layer instead.
neg("telemetry bad sha256 rejected", (d) => { d.telemetry = { streams: { keys: { last_sha256: "xyz" } } }; });
neg("bad date-time rejected (format asserted)", (d) => { d.meta.started_at = "not a date"; });
neg("unknown device kind rejected", (d) => { d.devices = { X: { kind: "toaster", connection: "connected" } }; });

if (failures === 0) console.log("ALL CHECKS PASSED");
else { console.log(`${failures} FAILURES`); process.exit(1); }

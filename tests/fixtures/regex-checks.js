// Regex/pattern regression tests for the schemas (S8, M3, M1, M5). Run: node tests/fixtures/regex-checks.js
const fs = require("fs");
const path = require("path");
const S = (n) => JSON.parse(fs.readFileSync(path.join(__dirname, "..", "..", "schemas", n), "utf8"));
const state = S("state.json"), bind = S("binding.json"), man = S("plugin-manifest.json"), frame = S("stream-frame.json");
let fail = 0;
function check(name, pat, cases) {
  // patternProperties semantics: re.search; anchored patterns therefore behave as fullmatch
  const rx = new RegExp(pat);
  for (const [input, want] of Object.entries(cases)) {
    const got = rx.test(input);
    const ok = got === want;
    if (!ok) fail++;
    console.log(`${ok ? "ok " : "FAIL"} ${name} ${JSON.stringify(pat)} vs ${JSON.stringify(input)}: match=${got} want=${want}`);
  }
}
const keysPat = Object.keys(state.properties.controls.properties.keys.patternProperties)[0];
check("keys index (S8)", keysPat, { "0": true, "9": true, "10": true, "14": true, "15": false, "99": false, "": false, "1x": false });
const streamsPat = Object.keys(state.properties.streams.patternProperties)[0];
check("streams target (S8)", streamsPat, { keys: true, dial: true, "key:0": true, "key:14": true, "key:15": false, "key:99": false, panel: false });
const frameTargetPat = frame.properties.target.pattern;
check("frame target (S6)", frameTargetPat, { keys: true, dial: true, "key:7": true, "key:15": false, panel: false });
const fileVariant = state.$defs.visual.oneOf.find((v) => v.properties && v.properties.path);
check("file path traversal (M3)", fileVariant.properties.path.pattern, {
  "/home/u/pic.png": true, "/etc/shadow": true, "../../etc/shadow": false,
  "/a/../b": false, "/..": false, "/a..b/x.png": true, "relative.png": false, "/": false,
});
const execStep = bind.$defs.step.oneOf.find((s) => s.properties.do.const === "exec");
check("exec argv[0] absolute (M1/S3)", execStep.properties.argv.prefixItems[0].pattern, {
  "/usr/bin/ffmpeg": true, "sh": false, "../../malicious": false, "/bin/sh": true,
});
check("exec cwd (M2)", execStep.properties.cwd.pattern, {
  "/home/u/scripts": true, "/tmp": true, "/": true, "relative": false, "/a/../b": true, // schema allows; runtime confines to /policy/fs/roots
});
const capPat = man.properties.capabilities.items.pattern;
check("capability (M5/S4)", capPat, {
  "state.read": true, "net:lights.lan": true, "net:*.lan": true, "fs:/home/u/media": true,
  "net:lights.lan.evil.com": true, "exec": true, "shell": false, "net:": false,
});
const topicPat = bind.properties.topic.pattern;
check("binding topic", topicPat, {
  "deck/control/keys.0/down": true, "app/focus/changed": true, "deck/control/keys/*": true, "other/x": false,
});
console.log(fail === 0 ? "ALL REGEX CHECKS PASSED" : `${fail} FAILURES`);
process.exit(fail === 0 ? 0 : 1);

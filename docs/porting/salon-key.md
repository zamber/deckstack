# The Salon key — one end-to-end port (DS-071)

The worked example of `docs/porting/inventory.md` row 2: a Salon deck key, reimplemented from
the old HubDeck behaviour (`presets.default.extends: [salon-pages, salon-controls]` in
`~/.config/hubdeck/config.yaml`, actions resolved by `hubdeck/src/hubdeck/engine.py:1028-1073`)
as **bindings + the lights plugin** — no import, no compat layer, public CLI only.

- Key press (`deck/control/keys.0/down` / `…/up`) recolours key 0 and drives the Salon
  lights through the `lights` plugin (`deckstack-lights`, DS-070).
- A focus event (`app/focus/changed` from `ds-focus`) recolours key 1 — the focus variant of
  the same pattern (`docs/components/focus-sender.md`, Chain B of
  `docs/contracts/actions.md` §6).
- Deployment policy is data: `/policy/net/allow_hosts = ["lights.lan"]` and the plugin's
  `allow_rooms = ["salon"]` (`docs/80-integrations.md`).

Everything below was executed once against `ds-vdev` + `ds-daemon` + the lights plugin with
the fake HueMux (`deckstack-lights/tests/fake_humux.py`) and **exits 0**; the transcript is
quoted at the end. Repeat it with the commands in §4.

## 1. The bindings (the port)

```sh
ds bind deck/control/keys.0/down \
  --do 'set /controls/keys/0/label "SALON"' \
  --do 'set /controls/keys/0/image {"kind":"color","value":"#ff8800"}' \
  --do 'plugin lights.set {"room":"salon","light":"ceiling","on":true,"brightness":80,"color":"#ff8800"}'
ds bind deck/control/keys.0/up \
  --do 'set /controls/keys/0/image {"kind":"color","value":"#3cc86e"}' \
  --do 'plugin lights.set {"room":"salon","light":"ceiling","on":false}'
# focus-event variant: recolor key 1 while the editor is focused
ds bind app/focus/changed --when '{"payload":"app","op":"eq","value":"code"}' \
  --do 'set /controls/keys/1/image {"kind":"color","value":"#00ff00"}'
```

`ds bind <topic> --do '<step>'` appends one element to `/bindings` (DS-041) and prints the
daemon-assigned id (`ok rev=N id=b000N`). Steps run sequentially (DS-042): two `set` steps
(recolour + label the key) then the `plugin lights.set {…}` step. The `plugin` step's args
are validated against the plugin manifest params (`E_SCHEMA` before anything is sent).

## 2. Policy and plugin config (data, not code)

```sh
ds set /policy/net/allow_hosts --json '["lights.lan"]'                 # admin policy (S4/T11)
ds set /plugins/lights/state/config --json \
  '{"bridge_host":"lights.lan","bridge_port":<PORT>,"allow_rooms":["salon"]}'
```

`allow_rooms` is enforced by the plugin: a room outside the list is refused `E_CAP` and **no
bridge request is made** (proven in §5 step 11). `/plugins/lights/config` is honoured when
present; the durable location in this host is `/plugins/lights/state/config` (deckstack-lights
README, "host gaps"). The only network grant is `net:lights.lan` — `lights.lan.evil.com`,
`a.b.lan`, `x.lights.lan` are all refused by the daemon's anchored glob check, and every HTTP
request is performed by the daemon itself (host-mediated), never by the plugin.

## 3. Install (phase-5 surface)

`ds plugin install|invoke` are later-phase verbs in this tree, so install = the documented
copy + generated manifest, invoke = a binding step:

```sh
LIGHTS=<deckstack-lights>                     # the deckstack-lights checkout (separate repo)
DEST=${XDG_DATA_HOME:-$HOME/.local/share}/deckstack/plugins/lights
mkdir -p "$DEST"
cp "$LIGHTS/plugin.py" "$DEST/plugin.py"
python3 "$LIGHTS/make_manifest.py" "$DEST/manifest.json"
# manifest: id=lights, actions=[set], capabilities=[state.read, state.write, net:lights.lan]
```

The plugin host is created lazily on the **first `plugin` step** (the daemon does not scan the
plugin dir at boot), so the first key press that carries a `plugin` step boots the plugin —
that first invoke can be refused `E_DEVICE` while the handshake runs; the next press works.

## 4. Repeatable run (recorded below)

Prereqs: a built umbrella (`cargo build --release --workspace` → `target/release/{ds,ds-daemon,ds-vdev,ds-vdevctl,ds-focus}`;
set `BIN` to that dir, or reuse an existing build), the `deckstack-lights` sibling checkout,
`unshare(1)`, python3. The run pins `lights.lan` → 127.0.0.1 inside an unprivileged
user+mount namespace (same trick as `deckstack-lights/tests/run.py`) so the daemon's
host-mediated DNS resolution reaches the fake HueMux hermetically.

```sh
RUN=/tmp/ds-salon-run; BIN=<deckstack>/target/release; LIGHTS=<deckstack-lights>
rm -rf "$RUN"; mkdir -p "$RUN"
# extract the run script from §6 of this page (between the ```sh fences) into $RUN/run-salon.sh:
awk '/^## 6\./{f=1} f && /^```sh$/{s=1;next} s && /^```$/{exit} s' docs/porting/salon-key.md > "$RUN/run-salon.sh"
printf '%s\n127.0.0.1 lights.lan\n' "$(cat /etc/hosts)" > "$RUN/hosts"
printf '#!/bin/sh\nmount --bind "$RUN/hosts" /etc/hosts || exit 90\nexec sh "$RUN/run-salon.sh"\n' > "$RUN/ns.sh"
chmod +x "$RUN/ns.sh"
RUN="$RUN" BIN="$BIN" LIGHTS="$LIGHTS" \
  unshare --user --map-root-user --mount --propagation private sh "$RUN/ns.sh"
echo "rc=$?"    # -> 0
```

The script (kept verbatim in §6 so the page stays executable):

- §0 starts the fake HueMux (`$LIGHTS/tests/fake_humux.py`) on 127.0.0.1 and records every
  request it receives to `$RUN/humux.requests.json`;
- §1-2 install the plugin and boot `ds-vdev` + `ds daemon start --device virtual:$RUN/vdev.sock`
  on a fresh data dir with `XDG_DATA_HOME=$RUN/xdg`;
- §3 boots the plugin host with a first `plugin` step on key 14 and waits for
  `ds get /plugins/lights/status` = `running`;
- §4-5 set the policy and plugin config from §2;
- §6-7 create the bindings from §1 and press the Salon key with
  `ds-vdevctl --socket $RUN/vdev.sock inject <(echo '{"module":"keys","keys":[0]}')` (then `keys:[]`);
- §8 drives the focus variant with `ds-focus --socket … --inject '{"app":"code","title":"editor","pid":4242}'`
  (the `app/focus/changed` ingest schema requires an integer `pid`);
- §9-12 assert on what the bridge received, what the key state is, and that a refused room
  produced no bridge request; §13 stops the daemon.

## 5. Recorded transcript (this page was written from this run; rc=0)

```
### 2. boot ds-vdev + ds-daemon on a fresh data dir (device virtual:)
ok running pid=1404094 rev=0 device=virtual:/tmp/ds-salon-run/vdev.sock socket=/tmp/ds-salon-run/data/daemon.sock

### 3. boot the plugin host with a first plugin step, then wait for status=running
ok rev=1 id=b0001
lights status: running
manifest id=lights actions=['set'] caps=['state.read', 'state.write', 'net:lights.lan']

### 4. /policy/net/allow_hosts = ["lights.lan"]
{"ok": true, "rev": 5}   value: ["lights.lan"]

### 5. plugin config: allow_rooms = [salon], bridge = lights.lan:<port>
{"ok": true, "rev": 6}   value: {"allow_rooms":["salon"],"bridge_host":"lights.lan","bridge_port":42667}

### 6. the Salon key bindings
ok rev=7 id=b0002   # deck/control/keys.0/down  (set label, set image, plugin lights.set …)
ok rev=8 id=b0003   # deck/control/keys.0/up    (set image, plugin lights.set on:false)
ok rev=9 id=b0004   # app/focus/changed --when payload app eq "code" -> set keys/1 image

### 7. press the Salon key on the virtual device
ds-vdevctl: injected module=keys ... injected 1 events
{"path":"/controls/keys/0","rev":13,"value":{"image":{"kind":"color","value":"#ff8800"},"label":"SALON"}}
{"path":"/plugins/lights/state/last","rev":13,"value":{"action":"lights.set",
  "args":{"room":"salon","light":"ceiling","on":true,"brightness":80,"color":"#ff8800"},
  "bridge":"lights.lan:42667","http_status":200,
  "response":{"ok":true,"room":"salon","light":"ceiling","state":{"on":true,"brightness":80,"color":"#ff8800"}}}}
# key release:
{"path":"/controls/keys/0","rev":16,"value":{"image":{"kind":"color","value":"#3cc86e"},"label":"SALON"}}
{"path":"/plugins/lights/state/last","rev":16,"value":{… "on":false … "http_status":200 …}}
ASSERT OK: /plugins/lights/state/last http_status=200 ok=true args={'light':'ceiling','on':False,'room':'salon'}

### 8. the focus-event variant (ds-focus --inject -> app/focus/changed)
ds-focus: emit app/focus/changed payload={"app":"code","pid":4242,"title":"editor"} subscribers=1
{"path":"/controls/keys/1","rev":17,"value":{"image":{"kind":"color","value":"#00ff00"}}}
{"path":"/focus","rev":17,"value":{"app":"code","pid":4242,"title":"editor","at_rev":17}}

### 9. what the fake HueMux bridge actually received (mediated GET /api/lights/set)
[{"method":"GET","path":"/api/lights/set","query":{"room":"salon","light":"ceiling","on":"true","brightness":"80","color":"#ff8800"}},
 {"method":"GET","path":"/api/lights/set","query":{"room":"salon","light":"ceiling","on":"false"}}]

### 10. the key frames reached the virtual device
key:0 c6ff9cf8…7fee 771B seq=8
key:1 6a3389b2…66b7 773B seq=11

### 11. negative control: room outside allow_rooms is refused E_CAP, NO bridge request
{"path":"/plugins/lights/state/last","value":{"args":{"room":"kitchen","light":"pendant","on":true},
  "refused":{"code":"E_CAP","message":"room 'kitchen' is not in allow_rooms ['salon'] …"}}}
bridge requests before=2 after=2 (must be equal)

### 12. assert the positive run
ASSERT OK: bridge saw 2 set requests; first query={'room':'salon','light':'ceiling','on':'true','brightness':'80','color':'#ff8800'} last query={'room':'salon','light':'ceiling','on':'false'}

### 13. cleanup
ok stopping
RUN COMPLETE rc=0
```

## 6. The run script (verbatim — extract with the awk in §4)

```sh
#!/bin/sh
# DS-071 Salon-key live run — executed inside the lights.lan namespace.
# Expects: RUN, BIN, LIGHTS env vars set. Prints a transcript; exits nonzero on failure.
set -eu

RUN=${RUN:?}
BIN=${BIN:?}
LIGHTS=${LIGHTS:?}
DS="$BIN/ds --socket $RUN/data/daemon.sock --data-dir $RUN/data"
export DS_DAEMON_BIN="$BIN/ds-daemon"
export XDG_DATA_HOME="$RUN/xdg"

step() { echo; echo "### $*"; }

cleanup() {
  $DS daemon stop >/dev/null 2>&1 || true
  [ -n "${VDEV_PID:-}" ] && kill "$VDEV_PID" 2>/dev/null || true
  [ -n "${HUMUX_PID:-}" ] && kill "$HUMUX_PID" 2>/dev/null || true
}
trap cleanup EXIT

step "0. fake HueMux bridge (deckstack-lights/tests/fake_humux.py) on 127.0.0.1, recorded requests -> $RUN/humux.requests.json"
python3 - "$RUN" "$LIGHTS" >"$RUN/humux.out" 2>&1 <<'PYEOF' &
import json, sys, time
run, lights = sys.argv[1], sys.argv[2]
sys.path.insert(0, lights + "/tests")
from fake_humux import FakeHueMux
fake = FakeHueMux()
open(run + "/humux.port", "w").write(str(fake.port))
while True:
    time.sleep(0.2)
    with fake.lock:
        reqs = list(fake.requests)
        state = {"%s/%s" % k: v for k, v in fake.state.items()}
    open(run + "/humux.requests.json", "w").write(json.dumps(reqs, indent=1))
    open(run + "/humux.state.json", "w").write(json.dumps({"ok": True, "state": state}, indent=1))
PYEOF
HUMUX_PID=$!
i=0; while [ ! -f "$RUN/humux.port" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
PORT=$(cat "$RUN/humux.port")
echo "fake HueMux pid=$HUMUX_PID port=$PORT (lights.lan -> 127.0.0.1 in this namespace)"
getent hosts lights.lan || true

step "1. install the lights plugin from the sibling repo (documented install path; ds plugin install is a later-phase verb)"
DEST="$XDG_DATA_HOME/deckstack/plugins/lights"
mkdir -p "$DEST"
cp "$LIGHTS/plugin.py" "$DEST/plugin.py"
python3 "$LIGHTS/make_manifest.py" "$DEST/manifest.json"
ls -l "$DEST"

step "2. boot ds-vdev + ds-daemon on a fresh data dir (device virtual:)"
"$BIN/ds-vdev" --socket "$RUN/vdev.sock" >"$RUN/vdev.out" 2>&1 &
VDEV_PID=$!
i=0; while [ ! -S "$RUN/vdev.sock" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
$DS daemon start --device "virtual:$RUN/vdev.sock" --data-dir "$RUN/data"
$DS daemon status

step "3. boot the plugin host with a first plugin step (the host is created lazily on the first plugin step), then wait for status=running"
$DS bind deck/control/keys.14/down --do "plugin lights.set {\"room\":\"salon\",\"light\":\"ceiling\",\"on\":true}"
printf '%s\n' '{"module":"keys","keys":[14]}' > "$RUN/boot.ndjson"
"$BIN/ds-vdevctl" --socket "$RUN/vdev.sock" inject "$RUN/boot.ndjson"
printf '%s\n' '{"module":"keys","keys":[]}' > "$RUN/boot-up.ndjson"
"$BIN/ds-vdevctl" --socket "$RUN/vdev.sock" inject "$RUN/boot-up.ndjson"
i=0; while [ $i -lt 100 ]; do
  S=$($DS get /plugins/lights/status --json 2>/dev/null | python3 -c 'import sys,json; print(json.load(sys.stdin)["value"])' 2>/dev/null || echo missing)
  [ "$S" = "running" ] && break
  sleep 0.2; i=$((i+1))
done
echo "lights status: $S"
[ "$S" = "running" ] || { echo "FAIL: plugin not running"; $DS get /plugins/lights --json; exit 1; }
$DS get /plugins/lights --json | python3 -c 'import sys,json; v=json.load(sys.stdin)["value"]; print("manifest id=%s actions=%s caps=%s" % (v["manifest"]["id"], [a["id"] for a in v["manifest"]["actions"]], v["manifest"]["capabilities"]))'

step "4. deployment policy as data: /policy/net/allow_hosts = [lights.lan] only"
$DS set /policy/net/allow_hosts --json '["lights.lan"]'
$DS get /policy/net/allow_hosts --json

step "5. plugin config as data: allow_rooms = [salon] only, bridge = lights.lan:$PORT"
$DS set /plugins/lights/state/config --json "{\"bridge_host\":\"lights.lan\",\"bridge_port\":$PORT,\"allow_rooms\":[\"salon\"]}"
$DS get /plugins/lights/state/config --json

step "6. the Salon key bindings (the port itself)"
$DS bind deck/control/keys.0/down \
  --do 'set /controls/keys/0/label "SALON"' \
  --do 'set /controls/keys/0/image {"kind":"color","value":"#ff8800"}' \
  --do "plugin lights.set {\"room\":\"salon\",\"light\":\"ceiling\",\"on\":true,\"brightness\":80,\"color\":\"#ff8800\"}"
$DS bind deck/control/keys.0/up \
  --do 'set /controls/keys/0/image {"kind":"color","value":"#3cc86e"}' \
  --do "plugin lights.set {\"room\":\"salon\",\"light\":\"ceiling\",\"on\":false}"
$DS bind app/focus/changed --when '{"payload":"app","op":"eq","value":"code"}' \
  --do 'set /controls/keys/1/image {"kind":"color","value":"#00ff00"}'
$DS get /bindings --json

step "7. press the Salon key on the virtual device (ds-vdevctl inject key-state report)"
printf '%s\n' '{"module":"keys","keys":[0]}' > "$RUN/press-down.ndjson"
"$BIN/ds-vdevctl" --socket "$RUN/vdev.sock" inject "$RUN/press-down.ndjson"
sleep 1
$DS get /controls/keys/0 --json
$DS get /plugins/lights/state/last --json
printf '%s\n' '{"module":"keys","keys":[]}' > "$RUN/press-up.ndjson"
"$BIN/ds-vdevctl" --socket "$RUN/vdev.sock" inject "$RUN/press-up.ndjson"
sleep 1
$DS get /controls/keys/0 --json
$DS get /plugins/lights/state/last --json
$DS get /plugins/lights/state/last --json | python3 -c 'import sys,json; v=json.load(sys.stdin)["value"]; assert v.get("http_status")==200 and v.get("response",{}).get("ok") is True, v; print("ASSERT OK: /plugins/lights/state/last http_status=200 ok=true args=%s" % v["args"])'

step "8. the focus-event variant (ds-focus --inject -> app/focus/changed)"
"$BIN/ds-focus" --socket "$RUN/data/daemon.sock" --inject '{"app":"code","title":"editor","pid":4242}'
sleep 1
$DS get /controls/keys/1 --json
$DS get /focus --json || true

step "9. what the fake HueMux bridge actually received (mediated GET /api/lights/set)"
cat "$RUN/humux.requests.json"
cat "$RUN/humux.state.json"

step "10. the key frame reached the virtual device"
"$BIN/ds-vdevctl" --socket "$RUN/vdev.sock" frames key:0
"$BIN/ds-vdevctl" --socket "$RUN/vdev.sock" frames key:1

step "11. negative control: a room outside allow_rooms is refused E_CAP with NO bridge request"
BEFORE=$(python3 -c 'import json; print(len(json.load(open("'$RUN'/humux.requests.json"))))')
$DS bind deck/control/keys.2/down --do "plugin lights.set {\"room\":\"kitchen\",\"light\":\"pendant\",\"on\":true}"
printf '%s\n' '{"module":"keys","keys":[2]}' > "$RUN/press-k.ndjson"
"$BIN/ds-vdevctl" --socket "$RUN/vdev.sock" inject "$RUN/press-k.ndjson"
sleep 1
$DS get /plugins/lights/state/last --json
AFTER=$(python3 -c 'import json; print(len(json.load(open("'$RUN'/humux.requests.json"))))')
echo "bridge requests before=$BEFORE after=$AFTER (must be equal)"
[ "$BEFORE" = "$AFTER" ] || { echo "FAIL: refused room produced a bridge request"; exit 1; }

step "12. assert the positive run: salon set reached the bridge with the exact query"
python3 - "$RUN" <<'PYEOF'
import json, sys
run = sys.argv[1]
reqs = json.load(open(run + "/humux.requests.json"))
sets = [r for r in reqs if r["path"] == "/api/lights/set"]
assert sets, "no /api/lights/set request reached the bridge"
q = sets[0]["query"]
assert q == {"room": "salon", "light": "ceiling", "on": "true", "brightness": "80", "color": "#ff8800"}, q
assert sets[-1]["query"] == {"room": "salon", "light": "ceiling", "on": "false"}, sets[-1]
print("ASSERT OK: bridge saw %d set requests; first query=%s last query=%s" % (len(sets), sets[0]["query"], sets[-1]["query"]))
PYEOF

step "13. cleanup"
$DS daemon stop || true
kill $VDEV_PID $HUMUX_PID 2>/dev/null || true
sleep 0.5
echo "RUN COMPLETE rc=0"
```

## 7. Notes / honest limits

- The daemon's host-mediated fetch is plain-http GET/HEAD only, so `lights.set` maps onto
  `GET /api/lights/set?…` — a host-side change (POST/PUT) is a deckstack-lights "host gaps"
  note, not something this port patches.
- `ds plugin install|invoke` are later-phase verbs; the phase-5 surface used here is
  install-by-copy + invoke-by-binding-step, exactly as documented in the lights README.
- The first `plugin` step can be refused `E_DEVICE` while the plugin handshake runs
  (`status: installing`); the next press succeeds. That is why §3 of the script boots the
  host on key 14 first.
- Hardware smoke (real deck + real lights.lan bridge) is a manual run, never CI
  (`docs/90-verification.md`); this page's proof is vdev + fake HueMux, which is what the
  ticket acceptance asks for. Pixel claims on real hardware would need the camera check.

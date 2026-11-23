"""The example deckstack plugin (docs/contracts/plugins.md §7), Python via ds-plugin-py.

A ~40-line counter: `demo.bump` sets key 0's label and image colour and emits a
`plugin/demo/bumped` topic; `demo.get_lamp` shows the host-mediated network path (S4).

Run it through the daemon (see ../run_demo.py or ../README.md). Two practical notes
vs. the contract snippet: state paths are slash-separated (`/controls/keys/0/label` —
the dotted form is the bus-topic form, and the host applies effect paths verbatim
against schemas/state.json), and the on-disk `manifest.json` next to this file must
compare equal to the `register` message, so `command`/`fs_roots`/`resources` are
mirrored from it at startup (see make_manifest.py).
"""

import json
import os
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
_ON_DISK = {}
_mf = os.path.join(_HERE, "manifest.json")
if os.path.exists(_mf):
    with open(_mf, "r", encoding="utf-8") as _f:
        _ON_DISK = json.load(_f)
    # locate the SDK through the manifest's fs_roots (Landlock grants exactly those)
    for _root in _ON_DISK.get("fs_roots", []):
        if os.path.isdir(os.path.join(_root, "ds_plugin_py")):
            sys.path.insert(0, _root)
            break

import ds_plugin_py as sdk

count = {"n": 0}


@sdk.action("bump", params={"type": "object", "properties": {"by": {"type": "integer", "default": 1}}})
def bump(args):
    count["n"] += args.get("by", 1)
    return [sdk.set("/controls/keys/0/label", str(count["n"])),
            sdk.set("/controls/keys/0/image", {"kind": "color", "value": "#ff8800"}),
            sdk.emit("plugin/demo/bumped", {"n": count["n"]})]


@sdk.action("get_lamp")
def get_lamp(args):
    r = sdk.http("GET", "http://lights.lan/api/lamp")     # host-mediated, glob-checked (S4)
    return [sdk.set("/plugins/demo/state/last", r.body)]


MANIFEST = {"id": "demo", "name": "Demo counter", "version": "0.1.0",
            "description": "Contract §7 example: key colour + counter + emit, and a "
                           "host-mediated lights.lan read.",
            "capabilities": ["state.write", "emit", "net:lights.lan"],
            "actions": [bump, get_lamp]}

# keep the register message equal to the on-disk manifest.json
for _k in ("command", "fs_roots", "resources", "events"):
    if _k in _ON_DISK:
        MANIFEST[_k] = _ON_DISK[_k]

if __name__ == "__main__":
    sdk.serve(MANIFEST)

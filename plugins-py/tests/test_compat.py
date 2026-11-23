"""Wrapper translation tests: a fixture old-style (hubdeck base.py:112-272) plugin class
run through ``ds_plugin_py.compat`` against a mock host.
"""

import io
import json
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import ds_plugin_py as sdk  # noqa: E402
from ds_plugin_py import compat  # noqa: E402
from ds_plugin_py import wire  # noqa: E402


class OldCounterPlugin:
    """Fixture old-style plugin: actions/states/values dicts + invoke/adjust/set/watch."""

    actions = {"bump": {"type": "object", "properties": {"by": {"type": "integer"}}}}
    states = {"count": None}
    values = {"brightness": None}

    def __init__(self, ctx=None):
        self.ctx = ctx
        self.count = 0
        self.watched = []

    def invoke(self, name, arg):
        if name == "bump":
            by = (arg or {}).get("by", 1)
            self.count += by
            self.ctx.set_state("count", self.count)
            self.ctx.emit("bumped", {"n": self.count})
            self.ctx.log("info", "bumped by %s" % by)
            return [{"effect": "set", "path": "/controls/keys/0/label", "value": str(self.count)}]
        raise KeyError(name)

    def adjust(self, name, arg):
        self.ctx.set_state(name, (arg or {}).get("value"))

    def set(self, name, value):
        self.ctx.set_state(name, value)

    def watch(self, info):
        self.watched.append(info)

    def watch_states(self, info):
        self.watched.append(("states", info))


def run_compat(plugin_cls, manifest, input_text):
    """Build the compat plugin and run it over scripted mock-host input."""
    inp = io.BytesIO(input_text.encode("utf-8"))
    out = io.BytesIO()

    def read_chunk(n):
        return inp.read(n)

    def write(b):
        out.write(b)

    plugin = compat.build_plugin(plugin_cls, manifest)
    err = None
    try:
        plugin.serve_io(read_chunk, write)
    except sdk.ServeError as e:
        err = e
    assert err is None, err
    return [json.loads(l) for l in out.getvalue().decode().splitlines() if l.strip()]


HELLO = ('{"op":"hello","protocol":1,"plugin_token":"tok","caps":["state.read","state.write","emit"],'
         '"roots":{},"limits":{}}\n')


class CompatTests(unittest.TestCase):
    def test_manifest_for_fixture_class(self):
        m = compat.manifest_for(OldCounterPlugin, id="oldcounter", command=["/usr/bin/python3", "plugin.py"])
        self.assertEqual(m["id"], "oldcounter")
        ids = [a["id"] for a in m["actions"]]
        self.assertEqual(ids, ["bump", "set.count", "set.brightness"])
        bump = m["actions"][0]
        self.assertEqual(bump["params"]["properties"]["by"]["type"], "integer")
        self.assertEqual(m["capabilities"], ["state.read", "state.write", "emit"])
        self.assertIn("keyDown", m["events"])
        self.assertEqual(m["command"], ["/usr/bin/python3", "plugin.py"])

    def test_invoke_translation(self):
        manifest = compat.manifest_for(OldCounterPlugin, id="oldcounter",
                                       command=["/usr/bin/python3", "plugin.py"])
        inp = (HELLO
               + '{"op":"invoke","id":"oldcounter.bump","args":{"by":4},"request_id":7}\n'
               + '{"op":"shutdown"}\n')
        lines = run_compat(OldCounterPlugin, manifest, inp)
        self.assertEqual(lines[0]["op"], "register")
        self.assertEqual(lines[0]["manifest"]["id"], "oldcounter")
        self.assertEqual([a["id"] for a in lines[0]["manifest"]["actions"]],
                         ["bump", "set.count", "set.brightness"])
        eff = {json.dumps(e, sort_keys=True) for e in lines[1]["effects"]}
        self.assertIn(json.dumps({"effect": "set", "path": "/plugins/oldcounter/state/count",
                                  "value": 4}, sort_keys=True), eff)
        self.assertIn(json.dumps({"effect": "emit", "topic": "plugin/oldcounter/bumped",
                                  "payload": {"n": 4}}, sort_keys=True), eff)
        self.assertIn(json.dumps({"effect": "log", "level": "info",
                                  "message": "bumped by 4"}, sort_keys=True), eff)
        self.assertIn(json.dumps({"effect": "set", "path": "/controls/keys/0/label",
                                  "value": "4"}, sort_keys=True), eff)

    def test_set_action_translation(self):
        manifest = compat.manifest_for(OldCounterPlugin, id="oldcounter",
                                       command=["/usr/bin/python3", "plugin.py"])
        inp = (HELLO
               + '{"op":"invoke","id":"oldcounter.set.count","args":{"value":9},"request_id":3}\n'
               + '{"op":"shutdown"}\n')
        lines = run_compat(OldCounterPlugin, manifest, inp)
        eff = lines[1]["effects"]
        self.assertIn({"effect": "set", "path": "/plugins/oldcounter/state/count", "value": 9},
                      eff)

    def test_watch_forwarded(self):
        manifest = compat.manifest_for(OldCounterPlugin, id="oldcounter",
                                       command=["/usr/bin/python3", "plugin.py"])
        inp = (HELLO
               + '{"op":"event","event":"keyDown","topic":"deck/control/keys.0/down","payload":{"key":0}}\n'
               + '{"op":"shutdown"}\n')
        lines = run_compat(OldCounterPlugin, manifest, inp)
        # the event is forwarded to watch/watch_states; the fixture returns no effects
        self.assertEqual(lines[1], {"op": "effects", "effects": []})

    def test_action_params_pydantic_fallback(self):
        class FakeModel:
            @classmethod
            def model_json_schema(cls):
                return {"type": "object", "properties": {"x": {"type": "integer"}}}

        self.assertEqual(compat.action_params(FakeModel),
                         {"type": "object", "properties": {"x": {"type": "integer"}}})
        self.assertEqual(compat.action_params(None), {"type": "object"})
        self.assertEqual(compat.action_params({"type": "array"}), {"type": "array"})
        self.assertEqual(compat.action_params(lambda a: None), {"type": "object"})


if __name__ == "__main__":
    unittest.main()

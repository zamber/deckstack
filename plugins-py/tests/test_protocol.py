"""Protocol-session tests against a mock host (stdlib unittest).

Mirrors the byte-level contract tests of ``crates/ds-plugin-sdk/src/session.rs`` (DS-051):
the same scripted host input must produce the same wire messages.
"""

import io
import json
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import ds_plugin_py as sdk  # noqa: E402
from ds_plugin_py import wire  # noqa: E402


def run_session(plugin, input_text):
    """Run plugin.serve_io over scripted host input; return (error, output lines)."""
    inp = io.BytesIO(input_text.encode("utf-8"))
    out = io.BytesIO()

    def read_chunk(n):
        return inp.read(n)

    def write(b):
        out.write(b)

    err = None
    try:
        plugin.serve_io(read_chunk, write)
    except sdk.ServeError as e:
        err = e
    return err, [json.loads(l) for l in out.getvalue().decode().splitlines() if l.strip()]


def hello_line(caps='["state.read","state.write","emit","net:lights.lan"]'):
    return ('{"op":"hello","protocol":1,"plugin_token":"tok","caps":%s,'
            '"roots":{"fs":["/home/u/media"]},"limits":{"cpu_s":10}}' % caps) + "\n"


def demo_plugin():
    plugin = sdk.Plugin(sdk.Manifest({
        "id": "demo", "name": "Demo", "version": "0.1.0",
        "command": ["/usr/bin/demo"],
        "capabilities": ["state.write", "net:lights.lan"],
    }))

    @plugin.action("bump", {"type": "object",
                            "properties": {"by": {"type": "integer", "default": 1}},
                            "additionalProperties": False})
    def bump(sdk_, args):
        by = args.get("by", 1)
        return [sdk.set("/controls/keys.0/label", str(by)),
                sdk.set("/controls/keys.0/image", {"kind": "color", "value": "#ff8800"})]

    @plugin.action("sync", {"type": "object"})
    def sync(sdk_, _args):
        v = sdk_.get_state("/controls/keys.0")
        rev = sdk_.set_state("/plugins/demo/state/synced", v)
        return [sdk.log("info", "synced at rev %d" % rev)]

    @plugin.action("lamp", {"type": "object"})
    def lamp(sdk_, _args):
        r = sdk_.http("GET", "http://lights.lan/api/lamp")
        return [sdk.set("/plugins/demo/state/last", r.body)]

    @plugin.action("fail", {"type": "object"})
    def fail(_sdk, _args):
        raise sdk.PluginError.cap("not granted")

    @plugin.on_event
    def _ev(sdk_, ev):
        return [sdk.emit("plugin/demo/seen", {"event": ev.event})]

    return plugin


class WireTests(unittest.TestCase):
    def test_effect_builders_match_contract_examples(self):
        self.assertEqual(wire.dumps(sdk.set("/controls/keys.0/label", "1")),
                         '{"effect":"set","path":"/controls/keys.0/label","value":"1"}')
        self.assertEqual(
            wire.dumps(sdk.set("/controls/keys.0/image", {"kind": "color", "value": "#ff8800"})),
            '{"effect":"set","path":"/controls/keys.0/image","value":{"kind":"color","value":"#ff8800"}}')
        self.assertEqual(wire.dumps(sdk.emit("plugin/demo/bumped", {"n": 1})),
                         '{"effect":"emit","topic":"plugin/demo/bumped","payload":{"n":1}}')
        self.assertEqual(wire.dumps(sdk.log("info", "hello")),
                         '{"effect":"log","level":"info","message":"hello"}')
        self.assertEqual(wire.dumps(sdk.sleep(250)), '{"effect":"sleep","ms":250}')

    def test_request_msg_field_order(self):
        msg = wire.request_msg(9, "net", method="GET", url="http://lights.lan/api/x")
        self.assertEqual(wire.dumps(msg),
                         '{"op":"request","request_id":9,"kind":"net","method":"GET","url":"http://lights.lan/api/x"}')
        msg = wire.request_msg(8, "get", path="/controls/keys.0")
        self.assertEqual(wire.dumps(msg),
                         '{"op":"request","request_id":8,"kind":"get","path":"/controls/keys.0"}')
        msg = wire.request_msg(3, "set", path="/plugins/demo/x", value=5)
        self.assertEqual(wire.dumps(msg),
                         '{"op":"request","request_id":3,"kind":"set","path":"/plugins/demo/x","value":5}')

    def test_parse_host_msg_lenient(self):
        self.assertIsNone(wire.parse_host_msg("[1,2]"))
        self.assertIsNone(wire.parse_host_msg('{"no_op":1}'))
        self.assertIsNone(wire.parse_host_msg('{"op":"wat"}'))
        with self.assertRaises(ValueError):
            wire.parse_host_msg("not json")
        h = wire.parse_host_msg(hello_line())
        self.assertIsInstance(h, wire.Hello)
        self.assertEqual(h.protocol, 1)
        self.assertEqual(h.caps, ["state.read", "state.write", "emit", "net:lights.lan"])
        self.assertIsInstance(wire.parse_host_msg('{"op":"shutdown"}'), wire.Shutdown)
        self.assertIsInstance(wire.parse_host_msg('{"op":"stop"}'), wire.Shutdown)
        r = wire.parse_host_msg('{"op":"reply","request_id":9,"ok":true,"status":200,"body":"x"}')
        self.assertTrue(r.ok)
        self.assertEqual(r.status, 200)
        self.assertEqual(r.body, "x")
        # host failure replies may carry a top-level code/message pair
        r = wire.parse_host_msg('{"op":"reply","request_id":9,"ok":false,"code":"E_CAP","message":"no"}')
        self.assertFalse(r.ok)
        self.assertEqual(r.error.code, sdk.ErrorCode.E_CAP)

    def test_error_codes_round_trip_and_exit_codes(self):
        for c in sdk.ErrorCode:
            self.assertEqual(sdk.ErrorCode.parse(c.value), c)
        self.assertIsNone(sdk.ErrorCode.parse("E_NOPE"))
        self.assertEqual(sdk.ErrorCode.E_CAP.exit_code(), 4)
        self.assertEqual(sdk.ErrorCode.E_LIMIT.exit_code(), 5)
        self.assertEqual(sdk.ErrorCode.E_VERSION.exit_code(), 8)
        err = sdk.PluginError.cap("net:lights.lan not granted")
        self.assertEqual(err.to_dict(), {"code": "E_CAP", "message": "net:lights.lan not granted"})
        self.assertEqual(sdk.PluginError.from_dict(err.to_dict()), err)


class SessionTests(unittest.TestCase):
    def test_full_session(self):
        inp = (
            hello_line()
            + '{"op":"event","event":"keyDown","topic":"deck/control/keys.0/down","payload":{"key":0}}\n'
            + '{"op":"invoke","id":"demo.bump","args":{"by":2},"request_id":7}\n'
            + '{"op":"invoke","id":"bump","args":{"by":5},"request_id":8}\n'
            + '{"op":"invoke","id":"demo.sync","args":{},"request_id":9}\n'
            + '{"op":"reply","request_id":1,"ok":true,"value":{"label":"2"},"rev":4}\n'
            + '{"op":"reply","request_id":2,"ok":true,"rev":5}\n'
            + '{"op":"invoke","id":"demo.lamp","args":{},"request_id":10}\n'
            + '{"op":"reply","request_id":3,"ok":true,"status":200,"body":"{\\"state\\":\\"on\\"}"}\n'
            + '{"op":"invoke","id":"demo.fail","args":{},"request_id":11}\n'
            + '{"op":"invoke","id":"demo.nope","args":{},"request_id":12}\n'
            + '{"op":"invoke","id":"demo.bump","args":{"by":"x"},"request_id":13}\n'
            + "not json\n"
            + '{"op":"wat"}\n'
            + '{"op":"shutdown"}\n'
        )
        err, lines = run_session(demo_plugin(), inp)
        self.assertIsNone(err)
        self.assertEqual(len(lines), 12, lines)
        self.assertEqual(lines[0]["op"], "register")
        self.assertEqual(lines[0]["id"], "demo")
        self.assertEqual(lines[0]["manifest"]["actions"][0]["id"], "bump")
        # event -> effects without request_id
        self.assertEqual(lines[1], {"op": "effects", "effects": [
            {"effect": "emit", "topic": "plugin/demo/seen", "payload": {"event": "keyDown"}}]})
        # invoke demo.bump
        self.assertEqual(lines[2], {"op": "effects", "request_id": 7, "effects": [
            {"effect": "set", "path": "/controls/keys.0/label", "value": "2"},
            {"effect": "set", "path": "/controls/keys.0/image",
             "value": {"kind": "color", "value": "#ff8800"}}]})
        self.assertEqual(lines[3]["request_id"], 8)
        self.assertEqual(lines[4], {"op": "request", "request_id": 1, "kind": "get",
                                     "path": "/controls/keys.0"})
        self.assertEqual(lines[5], {"op": "request", "request_id": 2, "kind": "set",
                                     "path": "/plugins/demo/state/synced", "value": {"label": "2"}})
        self.assertEqual(lines[6], {"op": "effects", "request_id": 9, "effects": [
            {"effect": "log", "level": "info", "message": "synced at rev 5"}]})
        self.assertEqual(lines[7], {"op": "request", "request_id": 3, "kind": "net",
                                     "method": "GET", "url": "http://lights.lan/api/lamp"})
        self.assertEqual(lines[8], {"op": "effects", "request_id": 10, "effects": [
            {"effect": "set", "path": "/plugins/demo/state/last", "value": '{"state":"on"}'}]})
        self.assertEqual(lines[9], {"op": "error", "request_id": 11,
                                     "error": {"code": "E_CAP", "message": "not granted"}})
        self.assertEqual(lines[10], {"op": "error", "request_id": 12,
                                      "error": {"code": "E_SCHEMA", "message": "unknown action: demo.nope"}})
        self.assertTrue(lines[11]["error"]["code"] == "E_SCHEMA"
                        and lines[11]["error"]["message"].startswith("args at $.by"),
                        lines[11])

    def test_eof_after_register_is_clean_shutdown(self):
        err, lines = run_session(demo_plugin(), hello_line())
        self.assertIsNone(err)
        self.assertEqual(lines[0]["op"], "register")

    def test_version_mismatch_sends_nothing(self):
        err, lines = run_session(demo_plugin(),
                                 '{"op":"hello","protocol":99,"caps":[]}\n')
        self.assertEqual(lines, [])
        self.assertIsNotNone(err)
        self.assertEqual(err.kind, sdk.ServeError.VERSION)
        self.assertEqual(err.exit_code(), 8)

    def test_first_message_must_be_hello(self):
        err, lines = run_session(demo_plugin(),
                                 '{"op":"invoke","id":"demo.bump","args":{},"request_id":1}\n')
        self.assertEqual(lines, [])
        self.assertEqual(err.kind, sdk.ServeError.EXPECTED_HELLO)

    def test_eof_before_hello(self):
        err, lines = run_session(demo_plugin(), "")
        self.assertEqual(lines, [])
        self.assertEqual(err.kind, sdk.ServeError.UNEXPECTED_EOF)

    def test_oversized_line_is_skipped(self):
        inp = (hello_line()
               + '{"op":"event","event":"' + "x" * (wire.MAX_LINE_BYTES + 100) + '"}\n'
               + '{"op":"invoke","id":"demo.bump","args":{"by":1},"request_id":3}\n')
        err, lines = run_session(demo_plugin(), inp)
        self.assertIsNone(err)
        self.assertTrue(any(l.get("request_id") == 3 for l in lines), lines)

    def test_http_bounds_enforced_before_sending(self):
        plugin = sdk.Plugin(sdk.Manifest({"id": "demo", "name": "d", "version": "0.1.0",
                                           "command": ["/usr/bin/demo"],
                                           "capabilities": ["state.write"]}))

        @plugin.action("bad", {"type": "object"})
        def bad(sdk_, _args):
            return [sdk.set("/x", sdk_.http("GET", "http://x/" + "a" * 5000).status)]

        err, lines = run_session(plugin, hello_line() +
                                 '{"op":"invoke","id":"demo.bad","args":{},"request_id":1}\n')
        self.assertIsNone(err)
        self.assertFalse(any(l.get("kind") == "net" for l in lines), lines)
        self.assertTrue(any(l.get("error", {}).get("code") == "E_LIMIT" for l in lines), lines)

    def test_http_method_not_allowed(self):
        from ds_plugin_py.http import check_request
        with self.assertRaises(sdk.PluginError) as cm:
            check_request("CONNECT", "http://x")
        self.assertEqual(cm.exception.code, sdk.ErrorCode.E_SCHEMA)
        self.assertEqual(check_request("get", "http://lights.lan/api/lamp"), "GET")

    def test_module_level_serve_with_registry(self):
        # the §7 style: decorators + serve(manifest dict with function objects)
        import importlib
        spec = importlib.util.spec_from_file_location(
            "demo_plugin_example", os.path.join(os.path.dirname(HERE), "examples", "demo_plugin.py"))
        mod = importlib.util.module_from_spec(spec)
        # the example imports ds_plugin_py — already importable here
        spec.loader.exec_module(mod)
        manifest = sdk.build_manifest(mod.MANIFEST)
        self.assertEqual([a["id"] for a in manifest["actions"]], ["bump", "get_lamp", "read_state"])
        self.assertEqual(manifest["capabilities"],
                         ["state.read", "state.write", "emit", "net:127.0.0.1"])
        inp = (hello_line()
               + '{"op":"invoke","id":"demo.bump","args":{"by":3},"request_id":7}\n'
               + '{"op":"shutdown"}\n')
        # serve() calls sys.exit — run it in a subprocess-free way via Plugin instead:
        plugin = sdk.Plugin(sdk.Manifest(manifest))
        for e in sdk._REGISTRY.values():
            plugin.actions[e.id] = e
        plugin.event_handler = mod._on_event
        err, lines = run_session(plugin, inp)
        self.assertIsNone(err)
        effects = lines[1]["effects"]
        self.assertIn({"effect": "set", "path": "/plugins/demo/state/count", "value": 3}, effects)
        self.assertIn({"effect": "emit", "topic": "plugin/demo/bumped", "payload": {"n": 3}}, effects)

    def test_manifest_validation(self):
        with self.assertRaises(sdk.ServeError):
            sdk.Manifest({"id": "bad id!", "name": "x", "version": "0.1.0",
                          "command": ["/x"], "capabilities": []}).validate()
        with self.assertRaises(sdk.ServeError):
            sdk.Manifest({"id": "ok", "name": "x", "version": "1.0",
                          "command": ["/x"], "capabilities": []}).validate()
        m = sdk.Manifest({"id": "ok", "name": "x", "version": "1.0.0",
                          "command": ["/x"], "capabilities": ["net:*.lan", "fs:/tmp"],
                          "bogus": 1})
        with self.assertRaises(sdk.ServeError):
            m.validate()
        good = sdk.Manifest({"id": "ok", "name": "x", "version": "1.0.0",
                             "command": ["/x"], "capabilities": ["net:*.lan", "fs:/tmp"],
                             "events": ["keyDown"], "resources": {"cpu_s": 10},
                             "fs_roots": ["/tmp"]})
        good.validate()
        d = good.to_dict()
        self.assertEqual(d["capabilities"], ["net:*.lan", "fs:/tmp"])
        self.assertEqual(d["events"], ["keyDown"])

    def test_params_validation(self):
        schema = {"type": "object",
                  "properties": {"by": {"type": "integer", "minimum": 1}},
                  "required": ["by"], "additionalProperties": False}
        sdk.validate_params(schema, {"by": 2})
        for bad in ({"by": "x"}, {}, {"by": 1, "extra": True}, {"by": 0}, [1]):
            with self.assertRaises(sdk.PluginError) as cm:
                sdk.validate_params(schema, bad)
            self.assertEqual(cm.exception.code, sdk.ErrorCode.E_SCHEMA)


if __name__ == "__main__":
    unittest.main()

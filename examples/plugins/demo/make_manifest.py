#!/usr/bin/env python3
"""Write the demo plugin's on-disk ``manifest.json``.

The daemon schema-validates ``<plugin dir>/manifest.json`` at spawn and requires the
plugin's ``register`` message to compare equal to it (contract §9), so the manifest is
generated from the same ``MANIFEST`` dict ``plugin.py`` serves, plus the launch fields:

- ``command``: ``[sys.executable, "plugin.py"]`` — argv-only, never a shell (§1);
- ``fs_roots``: the directory containing the ``ds_plugin_py`` package (the in-tree
  ``plugins-py/`` here; a pip-installed SDK under a system root needs no entry, see
  packaging/plugin/README.md);
- ``resources``: the schema defaults, spelled out.

Usage: ``python3 make_manifest.py [out.json]`` (default: ``manifest.json`` next to
this script).
"""

import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(REPO, "plugins-py"))
sys.path.insert(0, HERE)

import ds_plugin_py as sdk  # noqa: E402
import plugin as demo  # noqa: E402  (importing registers the @action handlers)


def build(sdk_root):
    manifest = sdk.build_manifest(demo.MANIFEST)
    manifest["command"] = [sys.executable or "/usr/bin/python3", "plugin.py"]
    manifest["fs_roots"] = [sdk_root]
    manifest["resources"] = {"cpu_s": 10, "nproc": 64, "nofile": 64, "as_mb": 1024}
    sdk.Manifest(manifest).validate()
    return manifest


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "manifest.json")
    sdk_root = os.path.join(REPO, "plugins-py")
    manifest = build(sdk_root)
    with open(out, "w", encoding="utf-8") as f:
        f.write(json.dumps(manifest, indent=2) + "\n")
    print("wrote %s (id=%s, actions=%s)" % (
        out, manifest["id"], [a["id"] for a in manifest["actions"]]))


if __name__ == "__main__":
    main()

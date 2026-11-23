# Packaging plugins — install layout and bundle shape (DS-054)

No network publishing in this tree: nothing here uploads anything. This note fixes the
on-disk layout the daemon discovers, the bundle a plugin ships as, and the hooks the
DS-083 release uses (subtree split / wheel). Contract: `docs/contracts/plugins.md`;
manifest schema: `schemas/plugin-manifest.json`.

## Install layout (how the daemon finds a plugin)

```
$XDG_DATA_HOME/deckstack/plugins/<id>/          # default ~/.local/share/deckstack/plugins/<id>/
  manifest.json        # schemas/plugin-manifest.json; dir name must equal manifest.id
  plugin.py | *.so | binary …   # whatever manifest.command[0] executes (argv-only, no shell)
```

- The DS-050 host scans that base dir and starts every `<id>/manifest.json` it finds
  (`load_all`, on daemon start / first plugin step). There is no registry database — the
  directory **is** the registry. Removing the dir (and restarting, or letting the host
  drop it) uninstalls.
- `ds plugin install <src-dir>` is a later-phase CLI verb (exits 1 in this tree,
  `docs/runbook.md` §1). Until it lands, install = validate + copy the source dir to
  `<base>/<id>/` + write `manifest.json` — exactly what `examples/plugins/demo/install.sh`
  does. The host's `install(src)` (copy + start) is the in-daemon equivalent.
- The plugin runs as one sandboxed OS process with rlimits (`resources`) + Landlock:
  read/execute on `/usr /lib /lib64 /bin /sbin /etc /proc`, read-write on its own dir and
  on `fs_roots`, `/dev/null`-class devices, and (ABI ≥ 4) no TCP at all. **The manifest
  and any payload the plugin imports must live under one of those roots** — e.g. the
  in-tree example lists the repo's `plugins-py/` in `fs_roots`; a plugin bundling its own
  copy needs no entry.

## Bundle shape (what a plugin ships as)

A plugin is a directory: `manifest.json` + the executable payload (`plugin.py` +
private modules, a compiled binary, …). For distribution the same directory is the
bundle — a tarball/zip of `<id>/` is sufficient; no build step is required for Python
plugins beyond making `ds-plugin-py` importable (see below). `manifest.json` must be
generated from the code (`sdk.build_manifest` + `command`/`fs_roots`/`resources`), never
hand-synced, because the host requires the `register` message to equal the on-disk file.

## SDK packaging hooks (consumed by DS-083, not executed here)

- `ds-plugin-py` is published to PyPI as a pure-Python wheel built from the `plugins-py/`
  subtree split (`git subtree split --prefix=plugins-py --branch release/plugin-py`,
  then `python -m build plugins-py`) — see `repos.toml` (`deckstack-plugin-py`) and
  `docs/50-repo-topology.md` §4. Import package `ds_plugin_py`, alias `deckstack_plugin`.
- A plugin that depends on the SDK declares it in its own packaging (e.g. a wheel with a
  `ds-plugin-py` requirement) and, when the SDK is not under a system root, lists the
  SDK's install root in `fs_roots` so the sandboxed child can import it.
- Plugin bundles themselves are not published to any index in the alpha; DS-070
  (lights.lan) reuses this layout for a real plugin repo later.

## Versioning

- `manifest.version` is semver `X.Y.Z` (schema-enforced). Bump it on any manifest or
  behaviour change; the daemon exposes it at `/plugins/<id>` (`manifest.version`) and in
  `ds daemon status` via `rev`, so an operator can see which version is running.
- Upgrade = replace the `<id>/` directory contents + restart the daemon (no `ds migrate`,
  by scope); downgrade is the same operation. The host refuses a `register` whose
  manifest differs from the on-disk one (`E_SCHEMA` + kill), so a half-copied upgrade
  fails loudly instead of running stale code.
- The wire protocol version is separate (`hello.protocol: 1`); a future protocol bump
  is a contract change, not a plugin-version change.

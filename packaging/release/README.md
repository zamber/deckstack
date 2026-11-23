# packaging/release — one-command Linux release (DS-083)

```sh
sh packaging/release/release.sh --version 0.1.0 --out /tmp/dsrel
```

Builds the six binaries with `cargo build --release` (ds, ds-daemon, ds-mcp,
ds-focus, ds-vdev, ds-vdevctl), assembles the install tree
(`bin/`, `share/deckstack/schemas/`, systemd user unit, udev rule,
`share/doc/deckstack/runbook.md`, LICENSE/NOTICE, `VERSION`), writes a
deterministic `deckstack-<version>-linux-<arch>.tar.gz` plus `SHA256SUMS`, and
builds an AppImage from `packaging/linux/AppDir/` when `appimagetool` is on PATH
(see `packaging/linux/README.md` for how to fetch it; AppImages need FUSE or
`--appimage-extract-and-run`). Without appimagetool the script says exactly that
and still exits 0 with tar + checksums.

Install from the tar: `packaging/linux/install.sh` (uninstall:
`packaging/linux/uninstall.sh`). CI runs this script on tag pushes:
`.github/workflows/release.yml`.

The script never publishes anything (no tags, no pushes, no uploads) — publishing
is the tag-triggered workflow's job.

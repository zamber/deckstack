# packaging/linux — Linux desktop/AppImage + tar-install layout (DS-083)

Linux-only alpha packaging (docs/00-scope.md). Two consumption paths share one
artifact layout:

```
deckstack-<version>/                  # tar root (what packaging/release/release.sh assembles)
├── VERSION
├── bin/                              # ds ds-daemon ds-mcp ds-focus ds-vdev ds-vdevctl   (0755)
├── share/
│   ├── deckstack/
│   │   ├── schemas/*.json            # contracts, from schemas/ (normative)
│   │   ├── systemd/deckstack.service # systemd USER unit (DS-019, ExecStart=/usr/local/bin/ds-daemon)
│   │   └── udev/99-deckstack.rules   # uaccess rule for MasterHub VID 0x2516 (DS-025)
│   └── doc/deckstack/                # runbook.md, LICENSE, NOTICE
```

## Files

- `install.sh` / `uninstall.sh` — install the unpacked tar layout into a prefix
  (`bin` + `share/deckstack/schemas` + docs under `<prefix>`, the systemd user unit
  into `~/.config/systemd/user/`, the udev rule into `/etc/udev/rules.d/` via sudo
  when needed). See the header comments for flags (`--prefix`, `--no-udev`, …).
- `AppDir/` — the AppImage skeleton consumed by `packaging/release/release.sh` when
  `appimagetool` is on PATH:
  - `AppRun` (execs `usr/bin/ds` relative to the mounted AppDir),
  - `deckstack.desktop` + `deckstack.svg` at the AppDir root (appimagetool requires
    both there), icon also under `usr/share/icons/hicolor/scalable/apps/`.
  - `release.sh` fills `usr/bin/` (the six binaries) and `usr/share/deckstack/…`
    from the same install tree that goes into the tar.

## Building an AppImage by hand

`packaging/release/release.sh --version X.Y.Z --out DIR` builds it automatically when
`appimagetool` is on PATH. To get the tool:

```sh
# https://github.com/AppImage/appimagetool/releases — download e.g.
# appimagetool-x86_64.AppImage, chmod +x, put on PATH as `appimagetool`.
# AppImages need FUSE (libfuse2 on Debian/Ubuntu, fuse2 on Fedora) to run directly;
# without FUSE use: ./appimagetool-x86_64.AppImage --appimage-extract-and-run …
```

Manual equivalent of what release.sh does:

```sh
rm -rf /tmp/AppDir && cp -a packaging/linux/AppDir /tmp/AppDir
install -m 0755 target/release/{ds,ds-daemon,ds-mcp,ds-focus,ds-vdev,ds-vdevctl} /tmp/AppDir/usr/bin/
mkdir -p /tmp/AppDir/usr/share/deckstack
cp -a <unpacked-tar-root>/share/deckstack/schemas /tmp/AppDir/usr/share/deckstack/
ARCH=$(uname -m) appimagetool /tmp/AppDir deckstack-X.Y.Z-$(uname -m).AppImage
```

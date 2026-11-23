#!/bin/sh
# release.sh — build one deckstack Linux-alpha release (DS-083).
#
#   sh packaging/release/release.sh --version 0.1.0 --out /tmp/dsrel
#
# What it does, in order:
#   1. cargo build --release for the six shipped binaries:
#      ds, ds-daemon, ds-mcp, ds-focus, ds-vdev, ds-vdevctl
#      (--skip-build reuses an existing target/release).
#   2. Assembles an install tree  deckstack-<version>/  with
#      bin/, share/deckstack/schemas/, share/deckstack/systemd/deckstack.service,
#      share/deckstack/udev/99-deckstack.rules, share/doc/deckstack/ (runbook,
#      LICENSE, NOTICE), VERSION.
#   3. Self-check: runs bin/ds --version and bin/ds-daemon --version from the tree.
#   4. Writes  <out>/deckstack-<version>-linux-<arch>.tar.gz  (deterministic:
#      sorted names, owner 0, SOURCE_DATE_EPOCH mtime, gzip -n) and
#      <out>/SHA256SUMS covering every artifact.
#   5. AppImage: if `appimagetool` is on PATH, fills packaging/linux/AppDir and
#      builds <out>/deckstack-<version>-linux-<arch>.AppImage (added to
#      SHA256SUMS). If it is NOT on PATH, prints exactly what to install and still
#      exits 0 with the tar + checksums produced (never silently skipped).
#
# Nothing here publishes: no tags, no pushes, no uploads. The tag-triggered
# .github/workflows/release.yml runs this script in CI and attaches the artifacts.
#
# Options:
#   --version X.Y.Z   release version (required; matches the tag vX.Y.Z in CI)
#   --out DIR         output directory for artifacts (required; created if missing)
#   --skip-build      do not run cargo build (reuse target/release)
#   --arch NAME       override the artifact arch suffix (default: uname -m)
#
# AppImage tooling (step 5):
#   appimagetool is NOT bundled. Get it from
#   https://github.com/AppImage/appimagetool/releases (e.g. the
#   appimagetool-x86_64.AppImage "continuous" build), make it executable and put
#   it on PATH as `appimagetool`. AppImages need FUSE to run directly
#   (libfuse2/fuse2); without FUSE use `--appimage-extract-and-run`.
set -eu

VERSION=
OUT=
SKIP_BUILD=0
ARCH=$(uname -m)

usage() { sed -n '2,32p' "$0"; exit "${1:-0}"; }
while [ $# -gt 0 ]; do
  case "$1" in
    --version)    [ $# -ge 2 ] || { echo "release.sh: --version needs a value" >&2; exit 2; }; VERSION=$2; shift 2 ;;
    --out)        [ $# -ge 2 ] || { echo "release.sh: --out needs a value" >&2; exit 2; }; OUT=$2; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --arch)       [ $# -ge 2 ] || { echo "release.sh: --arch needs a value" >&2; exit 2; }; ARCH=$2; shift 2 ;;
    -h|--help)    usage 0 ;;
    *) echo "release.sh: unknown argument: $1" >&2; usage 2 ;;
  esac
done

[ -n "$VERSION" ] || { echo "release.sh: --version X.Y.Z is required" >&2; usage 2; }
[ -n "$OUT" ]     || { echo "release.sh: --out DIR is required" >&2; usage 2; }
case "$VERSION" in *[!0-9A-Za-z.-]*|.|..|*..*) echo "release.sh: bad --version '$VERSION'" >&2; exit 2 ;; esac

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$ROOT"

BINARIES="ds ds-daemon ds-mcp ds-focus ds-vdev ds-vdevctl"

log() { printf 'release.sh: %s\n' "$*"; }
die() { printf 'release.sh: ERROR: %s\n' "$*" >&2; exit 1; }

command -v cargo >/dev/null 2>&1 || die "cargo not found on PATH (needed unless --skip-build)"
[ -f Cargo.toml ] || die "not a deckstack tree: $ROOT has no Cargo.toml"

# ---------------------------------------------------------------- 1. build
if [ "$SKIP_BUILD" = 1 ]; then
  log "skipping build (--skip-build)"
else
  log "cargo build --release ($BINARIES)"
  # shellcheck disable=SC2086
  cargo build --release $(for b in $BINARIES; do printf -- '--bin %s ' "$b"; done)
fi
for b in $BINARIES; do
  [ -x "target/release/$b" ] || die "target/release/$b missing after build"
done

# ---------------------------------------------------------------- 2. install tree
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/dsrel-stage.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT INT TERM
TREE="$STAGE/deckstack-$VERSION"
mkdir -p "$TREE/bin" \
         "$TREE/share/deckstack/schemas" \
         "$TREE/share/deckstack/systemd" \
         "$TREE/share/deckstack/udev" \
         "$TREE/share/doc/deckstack"

for b in $BINARIES; do
  install -m 0755 "target/release/$b" "$TREE/bin/$b"
done
install -m 0644 schemas/*.json                    "$TREE/share/deckstack/schemas/"
install -m 0644 packaging/systemd/deckstack.service "$TREE/share/deckstack/systemd/deckstack.service"
install -m 0644 packaging/udev/99-deckstack.rules    "$TREE/share/deckstack/udev/99-deckstack.rules"
install -m 0644 docs/runbook.md LICENSE NOTICE      "$TREE/share/doc/deckstack/"
printf '%s\n' "$VERSION" > "$TREE/VERSION"

log "install tree assembled: $TREE ($(find "$TREE" -type f | wc -l | tr -d ' ') files)"

# ---------------------------------------------------------------- 3. self-check
log "self-check: bin/ds --version -> $("$TREE/bin/ds" --version)"
log "self-check: bin/ds-daemon --version -> $("$TREE/bin/ds-daemon" --version)"

# ---------------------------------------------------------------- 4. tarball + checksums
mkdir -p "$OUT"
TARBALL="deckstack-$VERSION-linux-$ARCH.tar.gz"
# deterministic tar: sorted names, root ownership, fixed mtime, gzip without timestamp
if [ -z "${SOURCE_DATE_EPOCH:-}" ]; then
  SOURCE_DATE_EPOCH=$(git log -1 --format=%ct 2>/dev/null || date +%s)
  export SOURCE_DATE_EPOCH
fi
log "tar (SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH): $OUT/$TARBALL"
( cd "$STAGE" && tar --sort=name --owner=0 --group=0 --numeric-owner \
    --mtime="@$SOURCE_DATE_EPOCH" -cf - "deckstack-$VERSION" ) | gzip -n -9 > "$OUT/$TARBALL"

SHA="$OUT/SHA256SUMS"
: > "$SHA"
( cd "$OUT" && sha256sum "$TARBALL" ) >> "$SHA"
log "wrote $SHA"

# ---------------------------------------------------------------- 5. AppImage
APPIMAGE="deckstack-$VERSION-linux-$ARCH.AppImage"
if command -v appimagetool >/dev/null 2>&1; then
  APPDIR="$STAGE/AppDir"
  rm -rf "$APPDIR"
  cp -a packaging/linux/AppDir "$APPDIR"
  mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/deckstack" "$APPDIR/usr/share/doc/deckstack"
  for b in $BINARIES; do install -m 0755 "target/release/$b" "$APPDIR/usr/bin/$b"; done
  cp -a "$TREE/share/deckstack/schemas" "$APPDIR/usr/share/deckstack/schemas"
  cp -a "$TREE/share/deckstack/systemd" "$APPDIR/usr/share/deckstack/systemd"
  cp -a "$TREE/share/deckstack/udev"    "$APPDIR/usr/share/deckstack/udev"
  cp -a "$TREE/share/doc/deckstack/."   "$APPDIR/usr/share/doc/deckstack/"
  printf '%s\n' "$VERSION" > "$APPDIR/usr/share/deckstack/VERSION"
  log "appimagetool found: building $OUT/$APPIMAGE"
  # appimagetool wants the target arch in ARCH; run from a writable cwd
  ( cd "$STAGE" && ARCH="$ARCH" appimagetool "$APPDIR" "$OUT/$APPIMAGE" )
  chmod 0755 "$OUT/$APPIMAGE"
  ( cd "$OUT" && sha256sum "$APPIMAGE" ) >> "$SHA"
  log "AppImage built: $OUT/$APPIMAGE"
else
  cat >&2 <<EOF
release.sh: NOTE — 'appimagetool' is not on PATH, so no AppImage was built.
release.sh:          The tarball + SHA256SUMS above are complete and this run still succeeds.
release.sh:          To also produce the AppImage, install appimagetool:
release.sh:            download from https://github.com/AppImage/appimagetool/releases
release.sh:            (e.g. appimagetool-x86_64.AppImage from the 'continuous' release),
release.sh:            chmod +x it and put it on PATH as 'appimagetool', then re-run this script.
release.sh:          AppImages need FUSE to run directly (libfuse2 / fuse2); without FUSE use
release.sh:            ./deckstack-$VERSION-linux-$ARCH.AppImage --appimage-extract-and-run …
release.sh:          The AppDir skeleton used for the build is packaging/linux/AppDir/.
EOF
fi

# ---------------------------------------------------------------- summary
log "verifying checksums:"
( cd "$OUT" && sha256sum -c SHA256SUMS )
log "release artifacts in $OUT:"
ls -l "$OUT"
log "OK — release $VERSION done (tar always; AppImage only when appimagetool is on PATH)"

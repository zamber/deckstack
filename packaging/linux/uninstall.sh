#!/bin/sh
# uninstall.sh — remove a deckstack tar install (DS-083). Reverses install.sh:
# removes the installed files (never whole directories), the systemd user unit
# and the udev rule. Durable state (~/.local/state/deckstack, XDG runtime dir)
# is NOT touched here — back it up first if you want to keep it
# (docs/runbook.md §13/§16).
#
# Usage:
#   sh packaging/linux/uninstall.sh [--prefix /usr/local]
#       [--udev-dir /etc/udev/rules.d] [--unit-dir ~/.config/systemd/user]
#       [--keep-unit] [--keep-udev]
set -eu

PREFIX=/usr/local
UDEV_DIR=/etc/udev/rules.d
UNIT_DIR=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
KEEP_UNIT=0
KEEP_UDEV=0

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix)   [ $# -ge 2 ] || { echo "uninstall.sh: --prefix needs a value" >&2; exit 2; }; PREFIX=$2; shift 2 ;;
    --udev-dir) [ $# -ge 2 ] || { echo "uninstall.sh: --udev-dir needs a value" >&2; exit 2; }; UDEV_DIR=$2; shift 2 ;;
    --unit-dir) [ $# -ge 2 ] || { echo "uninstall.sh: --unit-dir needs a value" >&2; exit 2; }; UNIT_DIR=$2; shift 2 ;;
    --keep-unit) KEEP_UNIT=1; shift ;;
    --keep-udev) KEEP_UDEV=1; shift ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "uninstall.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

AS_ROOT=
if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then AS_ROOT="sudo"; fi

# stop + disable the user unit first (best effort; may not be running/installed)
if [ "$KEEP_UNIT" = 0 ] && command -v systemctl >/dev/null 2>&1; then
  systemctl --user disable --now deckstack.service >/dev/null 2>&1 || true
fi

for b in ds ds-daemon ds-mcp ds-focus ds-vdev ds-vdevctl; do
  if [ -f "$PREFIX/bin/$b" ]; then
    if [ -w "$PREFIX/bin" ]; then rm -f "$PREFIX/bin/$b"; else [ -n "$AS_ROOT" ] && $AS_ROOT rm -f "$PREFIX/bin/$b" || echo "uninstall.sh: cannot remove $PREFIX/bin/$b (no write access, no sudo)" >&2; fi
  fi
done

rm -f "$PREFIX/share/deckstack/schemas/"*.json 2>/dev/null || true
rm -f "$PREFIX/share/doc/deckstack/"* 2>/dev/null || true
rmdir "$PREFIX/share/deckstack/schemas" "$PREFIX/share/deckstack" "$PREFIX/share/doc/deckstack" "$PREFIX/share/doc" 2>/dev/null || true

if [ "$KEEP_UNIT" = 0 ] && [ -f "$UNIT_DIR/deckstack.service" ]; then
  rm -f "$UNIT_DIR/deckstack.service"
  systemctl --user daemon-reload >/dev/null 2>&1 || true
  echo "removed user unit $UNIT_DIR/deckstack.service"
fi

if [ "$KEEP_UDEV" = 0 ] && [ -f "$UDEV_DIR/99-deckstack.rules" ]; then
  if [ -w "$UDEV_DIR" ]; then rm -f "$UDEV_DIR/99-deckstack.rules"; else [ -n "$AS_ROOT" ] && $AS_ROOT rm -f "$UDEV_DIR/99-deckstack.rules" || echo "uninstall.sh: cannot remove $UDEV_DIR/99-deckstack.rules (no sudo)" >&2; fi
  $AS_ROOT udevadm control --reload-rules >/dev/null 2>&1 || true
  echo "removed udev rule $UDEV_DIR/99-deckstack.rules"
fi

echo "uninstall complete (binaries, schemas, docs, unit, udev rule)."
echo "kept on purpose: runtime dir \$XDG_RUNTIME_DIR/deckstack and state ~/.local/state/deckstack — remove manually if unwanted (docs/runbook.md §16)"

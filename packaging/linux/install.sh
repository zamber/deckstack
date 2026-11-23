#!/bin/sh
# install.sh — install an unpacked deckstack tar release (DS-083).
#
# Layout installed (tar root = deckstack-<version>/):
#   bin/*                                   -> <prefix>/bin                       (0755)
#   share/deckstack/schemas/*.json          -> <prefix>/share/deckstack/schemas   (0644)
#   share/deckstack/systemd/deckstack.service -> <user-unit-dir>/deckstack.service (0644)
#   share/deckstack/udev/99-deckstack.rules -> <udev-dir>/99-deckstack.rules     (0644, root)
#   share/doc/deckstack/*                   -> <prefix>/share/doc/deckstack        (0644)
#
# Usage:
#   sh packaging/linux/install.sh --src <unpacked-tar-root> [--prefix /usr/local]
#       [--udev-dir /etc/udev/rules.d] [--unit-dir ~/.config/systemd/user]
#       [--no-udev] [--no-systemd]
#
# Notes:
# - The systemd user unit names /usr/local/bin/ds-daemon in ExecStart (DS-019). If
#   --prefix is not /usr/local, a drop-in is needed after install — see
#   docs/runbook.md §3 (`systemctl --user edit deckstack.service`); the script
#   prints that hint.
# - The udev rule goes to a root-owned dir; the script uses sudo when the dir is
#   not writable by the current user. --no-udev skips it (virtual device only).
# - After install:  systemctl --user daemon-reload && systemctl --user enable --now deckstack.service
#   (or run the daemon by hand: docs/runbook.md §5).
set -eu

SRC=
PREFIX=/usr/local
UDEV_DIR=/etc/udev/rules.d
UNIT_DIR=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
INSTALL_UDEV=1
INSTALL_SYSTEMD=1

usage() { sed -n '2,25p' "$0"; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --src)      [ $# -ge 2 ] || { echo "install.sh: --src needs a value" >&2; exit 2; }; SRC=$2; shift 2 ;;
    --prefix)   [ $# -ge 2 ] || { echo "install.sh: --prefix needs a value" >&2; exit 2; }; PREFIX=$2; shift 2 ;;
    --udev-dir) [ $# -ge 2 ] || { echo "install.sh: --udev-dir needs a value" >&2; exit 2; }; UDEV_DIR=$2; shift 2 ;;
    --unit-dir) [ $# -ge 2 ] || { echo "install.sh: --unit-dir needs a value" >&2; exit 2; }; UNIT_DIR=$2; shift 2 ;;
    --no-udev)    INSTALL_UDEV=0; shift ;;
    --no-systemd) INSTALL_SYSTEMD=0; shift ;;
    -h|--help) usage 0 ;;
    *) echo "install.sh: unknown argument: $1" >&2; usage 2 ;;
  esac
done

[ -n "$SRC" ] || { echo "install.sh: --src <unpacked-tar-root> is required" >&2; exit 2; }
[ -d "$SRC/bin" ] || { echo "install.sh: '$SRC' has no bin/ — pass the unpacked tar root (deckstack-<version>/)" >&2; exit 2; }
[ -f "$SRC/VERSION" ] && echo "install.sh: installing deckstack $(cat "$SRC/VERSION") from $SRC"

AS_ROOT=
if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then AS_ROOT="sudo"; fi

# binaries
mkdir -p "$PREFIX/bin"
for b in "$SRC"/bin/*; do
  install -m 0755 "$b" "$PREFIX/bin/"
done
echo "installed binaries: $(ls "$SRC/bin" | tr '\n' ' ')-> $PREFIX/bin"

# schemas + docs under the prefix
if [ -d "$SRC/share/deckstack/schemas" ]; then
  mkdir -p "$PREFIX/share/deckstack/schemas"
  install -m 0644 "$SRC"/share/deckstack/schemas/*.json "$PREFIX/share/deckstack/schemas/"
fi
if [ -d "$SRC/share/doc/deckstack" ]; then
  mkdir -p "$PREFIX/share/doc/deckstack"
  install -m 0644 "$SRC"/share/doc/deckstack/* "$PREFIX/share/doc/deckstack/"
fi

# systemd user unit
if [ "$INSTALL_SYSTEMD" = 1 ] && [ -f "$SRC/share/deckstack/systemd/deckstack.service" ]; then
  mkdir -p "$UNIT_DIR"
  install -m 0644 "$SRC/share/deckstack/systemd/deckstack.service" "$UNIT_DIR/deckstack.service"
  echo "installed systemd user unit: $UNIT_DIR/deckstack.service"
  echo "  next: systemctl --user daemon-reload && systemctl --user enable --now deckstack.service"
  if [ "$PREFIX" != "/usr/local" ]; then
    echo "  NOTE: prefix is $PREFIX but the unit ExecStart names /usr/local/bin/ds-daemon."
    echo "        Point it at this install with: systemctl --user edit deckstack.service"
    echo "        (docs/runbook.md §3) — ExecStart=$PREFIX/bin/ds-daemon run --device \${DECKSTACK_DEVICE} --data-dir %t/deckstack"
  fi
fi

# udev rule (root-owned)
if [ "$INSTALL_UDEV" = 1 ] && [ -f "$SRC/share/deckstack/udev/99-deckstack.rules" ]; then
  if mkdir -p "$UDEV_DIR" 2>/dev/null && [ -w "$UDEV_DIR" ]; then
    install -m 0644 "$SRC/share/deckstack/udev/99-deckstack.rules" "$UDEV_DIR/99-deckstack.rules"
  else
    [ -n "$AS_ROOT" ] || { echo "install.sh: $UDEV_DIR not writable and no sudo — install the rule manually:" >&2; echo "  sudo install -m 0644 $SRC/share/deckstack/udev/99-deckstack.rules $UDEV_DIR/" >&2; exit 1; }
    $AS_ROOT mkdir -p "$UDEV_DIR"
    $AS_ROOT install -m 0644 "$SRC/share/deckstack/udev/99-deckstack.rules" "$UDEV_DIR/99-deckstack.rules"
  fi
  echo "installed udev rule: $UDEV_DIR/99-deckstack.rules"
  echo "  next: sudo udevadm control --reload-rules && sudo udevadm trigger   (or re-login)"
fi

echo "install complete. Run 'ds --help' (add $PREFIX/bin to PATH) or see $PREFIX/share/doc/deckstack/runbook.md"

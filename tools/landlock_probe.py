#!/usr/bin/env python3
"""Landlock ABI probe (ticket DS-116) — stdlib only, no network, no hardware, no daemon.

Calls landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION) via the raw
syscall (number 444 on x86_64) and prints the kernel's Landlock ABI version:

    Landlock ABI version on this kernel: 11

ABI >= 1 means filesystem rules are available; ABI >= 4 additionally means the
network rules (LANDLOCK_ACCESS_NET_BIND_TCP / CONNECT_TCP) are available, which
the daemon uses as the second-layer net-deny for plugin processes
(crates/ds-daemon/src/plugins/spawn.rs, docs/20-security.md §4, docs/contracts/plugins.md §2).

Exit codes: 0 = Landlock present (ABI printed); 2 = no Landlock on this kernel
(errno printed to stderr). Measured on this machine (kernel 7.3.5-ogc3.1.fc45,
Fedora family of Bazzite): ABI 11.

Usage:
    python3 tools/landlock_probe.py
"""

import ctypes
import os
import sys

# x86_64 syscall number for landlock_create_ruleset(2).
SYS_LANDLOCK_CREATE_RULESET = 444
# landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION) returns the ABI.
LANDLOCK_CREATE_RULESET_VERSION = 1 << 0


def probe_abi():
    """Return the Landlock ABI version, or raise OSError with the errno."""
    libc = ctypes.CDLL(None, use_errno=True)
    libc.syscall.restype = ctypes.c_long
    libc.syscall.argtypes = [
        ctypes.c_long,      # syscall number
        ctypes.c_void_p,    # attr (NULL)
        ctypes.c_size_t,    # size (0)
        ctypes.c_uint,      # flags (LANDLOCK_CREATE_RULESET_VERSION)
    ]
    ctypes.set_errno(0)
    rc = libc.syscall(
        SYS_LANDLOCK_CREATE_RULESET, None, 0, LANDLOCK_CREATE_RULESET_VERSION
    )
    if rc == -1:
        err = ctypes.get_errno()
        raise OSError(err, os.strerror(err))
    return int(rc)


def main():
    if not sys.platform.startswith("linux"):
        print("landlock_probe: not Linux — no Landlock", file=sys.stderr)
        return 2
    machine = os.uname().machine
    if machine != "x86_64":
        print(
            "landlock_probe: syscall 444 is x86_64-only (this machine is %s)" % machine,
            file=sys.stderr,
        )
        return 2
    try:
        abi = probe_abi()
    except OSError as e:
        print(
            "landlock_create_ruleset(VERSION) failed: errno=%d (%s) — "
            "no Landlock on this kernel" % (e.errno, e.strerror),
            file=sys.stderr,
        )
        return 2
    print("Landlock ABI version on this kernel: %d" % abi)
    if abi >= 4:
        print("Landlock net rules (ABI >= 4, BIND_TCP/CONNECT_TCP): available")
    else:
        print("Landlock net rules (ABI >= 4): NOT available (fs rules only)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

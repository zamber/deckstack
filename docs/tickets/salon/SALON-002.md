# SALON-002 — adapt salon-deck to the DS-112 orientation/position model

Status: open. Depends on: DS-112 (merged), DS-120 (merged), DS-121 preview webapp.

## Why

The salon-deck app predates the (slot, orientation) device identity and the
/evices position/orientation state. With DS-112 landed, the app must:

1. key its per-module visuals by logical position (not raw USB serial), so a
   reseat/rotation remaps in place without a restart (DS-120 fixed the daemon side;
   the app must stop relying on daemon restarts);
2. react to `deck/device/<serial>/orientation` (flat 6x4 vs vertical 4x6) and
   transpose its layout / rotate screen content accordingly, live;
3. use the `/preview` surface (DS-113) / preview webapp (DS-121) for debugging
   instead of its own probing;
4. drop its SIGKILL-children workaround once DS-115's watchdog is in the installed
   build.

## Acceptance

On the salon deck: rotate the base flat<->upright and the app layout transposes live;
move a module to another connector and its bindings follow the logical position;
no app restart on hotplug. Evidence: session recording + /tmp capture bundle.

## Links

DS-112, DS-120, DS-113, DS-121, SALON-001 (hardware map in docs/discovery/salon-001/).

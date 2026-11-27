# DS-115: ds-focus can keep running after SIGTERM

Status: fixed by the watchdog-thread option below (ds-focus `install_signal_handlers` now spawns a shutdown watchdog that exits the process once the flag is set, covering every blocking call)
Found: 2026-11-26 (salon-deck stop timeout)

## Symptom

`systemctl --user stop salon-deck` sent SIGTERM to the unit's cgroup, but the child
`ds-focus` stayed alive until systemd's stop timeout aborted the unit
(`State 'final-sigterm' timed out. Aborting.`) and SIGABRT'ed it. The core dump shows
the main thread inside `block_on(Proxy::call(...))` in the zbus backend poll.

## Cause

`install_signal_handlers` only sets an `AtomicBool`; `run()` reads it between
`be.poll()` calls. A D-Bus call that blocks never returns, so the flag is never
observed and the process does not exit.

## Fix options

- watchdog thread that exits the process once the flag is set (covers every blocking
  call), or
- bound each backend call with a timeout and check the flag while waiting.

The app-side workaround (salon-deck SIGKILLs its children after a 1 s grace) landed in
salon-deck 06fd034; this ticket fixes the child itself.

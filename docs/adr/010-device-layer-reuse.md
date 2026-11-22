# ADR-010 — Device layer: reuse mh-protocol/mh-device as dependencies (tension 6)

Status: accepted (planning).

## Context

Tension 6: reuse the existing Rust `mh-protocol`/`mh-device` crates vs a fresh device layer.
The crates are pure protocol + hidapi transport, hardware-verified (`upstream/PROTOCOL.md`),
and already implement the safety-by-construction pattern BRIEF.md demands: builders only
for safe commands + raw-path `is_forbidden` (`upstream/crates/mh-protocol/src/safety.rs:8-36`,
`report.rs:28-30`), software-mode handshake, pipelined writes, rescan, handback.
Licences: upstream `MIT OR Apache-2.0` (`upstream/Cargo.toml:8`, LICENSE-MIT/LICENSE-APACHE);
hubdeck Python is MIT (`hubdeck/pyproject.toml:11`) — all permissive.

## Decision

Depend on `mh-protocol` + `mh-device` as **git cargo dependencies** (pinned rev, upgradable);
do not fork, do not vendor. deckstack is licensed `MIT OR Apache-2.0` to match, with a
`NOTICE.md`/`packaging/NOTICE.md` entry crediting hubdeck-upstream and hubdeck, preserving
their license texts. Bug fixes and protocol facts flow upstream as PRs. `ds-device` adds
only: the virtual transport (ds-vdev), the `/policy` write choke point, per-module
latest-frame-wins writer threads, and the daemon glue.

## Alternatives

- **Fresh device layer** — rejected: re-derives camera-verified facts (key window table
  `upstream/PROTOCOL.md:81`, standard-Huffman requirement `:87-88`, 2.1 MB/s `:265`,
  backlight-0 brick `:61`, hardware-mode read wedge `:182-197`) and re-introduces
  brick-class risk; the protocol knowledge IS the moat (discovery/50 §6).
- **Fork** — rejected: maintenance split with no benefit while upstream is alive.
- **Vendor (copy) the code** — rejected: silently diverges; NOTICE + git dep is the
  honest attribution path.
- **Port the Python device layer** — rejected: same re-derivation problem in another
  language; mh-device is the more complete transport (overlapped writes, notifications).

## Consequences

`crates/ds-device/Cargo.toml` carries `mh-protocol = { git = "…hubdeck-upstream…", rev = "…" }`;
CI runs `cargo publish --dry-run` and a license-check job (NOTICE present, license files
unmodified). If upstream ever goes unmaintained, the fallback is a one-command vendor
(`cargo vendor`) — recorded here as the contingency.

## References

BRIEF.md facts (device safety), task tension 6; discovery/40 §8; decision log D03.

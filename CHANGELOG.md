# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - 2026-10-08

The transport moved to the shared [hidraw](https://github.com/GregDuhamel/hidraw)
crate. Nothing changes for the panel: same discovery, same reports, same
timeouts.

### Changed

- The panel's hidraw nodes are found and driven through `hidraw` v0.1.0
  (`discover` with a USB, vendor and product filter; `Device::write`,
  `read_timeout` and `drain` for the reports) instead of this crate's own
  sysfs walk and `poll(2)` loop. The identity comes from the HID device's
  `uevent` rather than the USB device's `idVendor` and `idProduct`, filtered
  to the USB bus as before.
- The daemon tells an unplugged panel from one that merely stopped answering
  (`hidraw::is_gone`): the reconnection is the same, the log line says
  `LCD unplugged` or `LCD lost`, and the heartbeat thread says which of the
  two ended it.
- `getuid(2)`, which finds this user's logind session, goes through `rustix`.

### Removed

- The `libc` dependency, and with it the crate's two `unsafe` blocks
  (`poll(2)` in `device.rs`, `getuid(2)` in `session.rs`). The crate now
  builds with `unsafe_code = "deny"`.

### Added

- This changelog.
- Unit tests of the device layer on a socket pair: interface selection, the
  report-ID byte, acknowledgements and timeouts, and which errors count as
  the panel being gone.

## [0.4.0] - 2026-10-08

### Changed

- An MPRIS player that vanishes between `ListNames` and `GetAll` is skipped
  instead of taking the others with it; only a transport error drops the bus
  connection, which is then looked up again.
- DejaVu Sans is looked for in the Fedora, Debian and Ubuntu, Arch and Void,
  Gentoo and Alpine, openSUSE and `/usr/local` font directories; a missing
  font is reported with every path tried.
- `poll(2)` on the panel is taken up again with the time left when a signal
  interrupts it (`EINTR`).
- The configured brightness is applied at the end of the handshake, so the
  panel no longer flashes at 100 % before it is set.
- The heartbeat thread logs why it stopped, and the main loop reconnects as
  soon as it has, without waiting for a frame to fail.
- A poisoned mutex is recovered with `PoisonError::into_inner` rather than
  unwrapped.
- The fan band shows six fans at most, and shortens labels that do not fit.
- `rust-version` is 1.88 (let-chains); the release profile strips the binary,
  uses thin LTO and aborts on panic; the clippy pedantic set is on, with the
  product names it should not ask backticks around listed in `clippy.toml`.
- The service unit documents where the release binary goes, and carries a
  `RUST_LOG` placeholder; the README says the same.

## Older releases

Releases before 0.4.0 are described on their
[GitHub releases](https://github.com/GregDuhamel/thermaltaked/releases).

[0.5.0]: https://github.com/GregDuhamel/thermaltaked/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/GregDuhamel/thermaltaked/compare/v0.3.5...v0.4.0

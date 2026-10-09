# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `rust-version` is 1.89: `nalgebra`, `wide` and `safe_arch` (via `imageproc`)
  need it, which the new MSRV job in CI was the first to check.

## [0.6.1] - 2026-10-09

The release binary runs on any x86_64 distribution, and the pieces install
with `make`.

### Changed

- The Release workflow builds for `x86_64-unknown-linux-musl`: the binary is
  statically linked and no longer tied to the runner's glibc, which kept it
  off Debian 12 and Ubuntu 22.04. The workflow checks that the binary has no
  dynamic loader and no shared library, starts it, and has it draw a frame,
  before tagging. The tests run under musl too.
- The service unit runs `/usr/local/bin/thermaltaked`, where the other
  daemons of this machine live, with the `~/.cargo/bin` line kept as a
  comment for `cargo install`.
- The README's setup goes through the Makefile, with the release binary
  beside it and `cargo install` as the alternative.

### Added

- `SHA256SUMS` beside the release assets, checked with
  `sha256sum --check --ignore-missing`.
- A Makefile: `build`, `check` (what CI runs), `install` and `uninstall`
  (root, `/usr/local/bin`), `install-udev` and `uninstall-udev` (root),
  `install-unit` and `uninstall-unit` (as yourself; a running service is
  restarted), `install-config` (as yourself; an existing file is kept) and
  `clean`. Each target refuses to run as the wrong user, and cargo never
  runs under sudo.

## [0.6.0] - 2026-10-09

D-Bus leaves the drawing path, the hwmon tree is walked again when it
changes, and the daemon logs through `log`.

### Changed

- The MPRIS players and logind's `LockedHint` are read by two background
  threads, which publish their latest state for the drawing loop to read;
  the loop no longer touches the bus. Each thread polls once a second and
  sooner on a `PropertiesChanged` signal, and every call carries a 2 s
  timeout. A player that keeps its name but never answers (a frozen tab, a
  process stopped with `SIGSTOP`) costs its thread one timeout, is then left
  alone for 30 s, and never holds a frame back; before, it froze the loop and
  the panel fell back to its own screen. The position moves along between
  two readings, so the progress bar keeps its pace at any refresh rate.
- `Sensors` walks `/sys/class/hwmon` again when a reading's file is gone
  (5 s after the last walk at the soonest) or a sensor is still missing
  (every 30 s): amdgpu registering anew after a GPU reset or a resume, a
  power supply plugged back in, a driver loaded after the daemon. The log
  says what was found, moved or lost.
- Logging goes through `log` and `env_logger`: info by default, `-v` for
  debug and `-vv` for trace, `RUST_LOG` for anything else. Under systemd the
  lines carry journald priorities (`<4>` for a warning) instead of
  timestamps, and the service unit sets `RUST_LOG=info`. Reconnection
  failures are warnings, logged once per failure and once more when it ends;
  screen changes are info; what was found where is debug.
- The weather thread, the cover-art thread and the two new ones share
  `Background<T>`, and the "complain once, say when it is over" reporting
  shares `Complaint`.
- The text helpers of the renderer take a `TextStyle` (size, weight, color)
  instead of three loose arguments; the frames are unchanged pixel for pixel.

### Added

- `Sensors::with_root`, which reads any directory laid out like
  `/sys/class/hwmon`, and the tests that use it: fan selection and order
  from `[fans]`, lone-fan naming, `always_shown`, the CPU, GPU and PSU names
  and the PSU rating, the temperature sources' order, and a chip that
  re-registers or a driver loaded late being found again.
- `thermaltaked info` waits up to 5 s for logind's first word before
  reporting the screen state.

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

[Unreleased]: https://github.com/GregDuhamel/thermaltaked/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/GregDuhamel/thermaltaked/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/GregDuhamel/thermaltaked/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/GregDuhamel/thermaltaked/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/GregDuhamel/thermaltaked/compare/v0.3.5...v0.4.0

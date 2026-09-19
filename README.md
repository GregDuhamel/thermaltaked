# thermaltaked

Rust driver and dashboard daemon for the Thermaltake 3.9" bar LCD
(`264a:233d`, 480x128) on Linux. It talks to the panel through hidraw, with no
libusb or vendor software.

The dashboard shows hostname, kernel, load average, CPU and GPU temperature and
usage, fan speeds, the power drawn from the supply, the date and time, and the
current weather (Open-Meteo).

![The dashboard, drawn from made-up readings](docs/dashboard.png)

While the monitors are off or the session is locked, it shows the date and the
time, and nothing else:

![The sleeping screen, showing only a date and a clock](docs/clock.png)

Both screenshots are real 480x128 frames, rendered from invented readings by
`cargo run --example demo_frame`.

## Setup

```sh
sudo cp udev/70-thermaltake-lcd.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger

cargo install --path .
mkdir -p ~/.config/thermaltaked
cp config.example.toml ~/.config/thermaltaked/config.toml
```

## Usage

```sh
thermaltaked info              # detect the panel, check permissions
thermaltaked test              # color bars
thermaltaked image photo.png   # any image, scaled and cropped
thermaltaked brightness 60
thermaltaked preview out.png   # render the dashboard without the panel
thermaltaked run               # dashboard loop
```

As a user service:

```sh
cp systemd/thermaltaked.service ~/.config/systemd/user/
systemctl --user enable --now thermaltaked
```

## Protocol

Interface 0 takes 440-byte commands `[opcode, 01, 00, 80, ...]`: a handshake
(`85 87 85 87 84 81`), brightness (`12`, value in byte 4) and a heartbeat (`82`)
every 2 s. Interface 1 takes a JPEG split into 1020-byte chunks, each in a
1024-byte report: `[08, packet count, 00, 80]` for the first, `[08, index, 00,
00]` for the rest. See `src/protocol.rs`.

## Configuration

Everything in `config.example.toml` is optional: refresh rate, brightness,
fonts, the weather city, the gauge titles, which fans to show in which order,
and whether to fall back to the clock while nobody is watching. Without a configuration file the dashboard detects the hardware by
itself and leaves the weather out.

## Protocol knowledge

It comes from
[ttlcd](https://github.com/bekindpleaserewind/ttlcd),
[Tower-500-LCD-Controller](https://github.com/JohnathanKong/Tower-500-LCD-Controller)
and [thermaltake-lcd-linux](https://github.com/pcmx1/thermaltake-lcd-linux),
which this project reimplements in Rust rather than copies.

## Releasing

Bump `version` in `Cargo.toml` through a pull request, then run the Release
workflow: it tags that version, builds the binary and publishes it with the
udev rule, the service unit and the example configuration.

## License

GPL-3.0-or-later, see [LICENSE](LICENSE). `ttlcd`, where the protocol was first
described, is GPL-3.0.

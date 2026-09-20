//! Renders the README's screenshots from made-up readings, so the images show
//! the real layouts without anyone's machine in them.
//!
//! Run with: `cargo run --example demo_frame`

use std::time::Duration;

use chrono::{Local, TimeZone};
use image::{Rgb, RgbImage};
use thermaltaked::player::{NowPlaying, Track};
use thermaltaked::render::{ART_SIZE, Dashboard};
use thermaltaked::sensors::{Fan, Names, Snapshot};
use thermaltaked::weather::Weather;

const DASHBOARD: &str = "docs/dashboard.png";
const CLOCK: &str = "docs/clock.png";
const PLAYER: &str = "docs/player.png";

/// Stands in for a downloaded cover, so the screenshot needs no network.
fn cover() -> RgbImage {
    RgbImage::from_fn(ART_SIZE, ART_SIZE, |x, y| {
        let wave = (x * 255 / ART_SIZE) as u8;
        let fade = (y * 255 / ART_SIZE) as u8;
        Rgb([40 + wave / 3, 30 + fade / 4, 90 + wave / 3])
    })
}

fn main() -> anyhow::Result<()> {
    let names = Names {
        cpu: "Ryzen 7 7800X3D".to_owned(),
        gpu: "Radeon RX 7800 XT".to_owned(),
        psu: "CORSAIR RM850x".to_owned(),
    };
    let snapshot = Snapshot {
        names: &names,
        hostname: "workshop".to_owned(),
        kernel: "6.18.2",
        load_average: [0.94, 0.71, 0.55],
        cpu_temp: Some(61.0),
        cpu_usage: Some(23.0),
        gpu_temp: Some(72.0),
        gpu_usage: Some(68.0),
        psu_power: Some(317.0),
        psu_usage: Some(37.0),
        fans: vec![
            Fan {
                label: "front",
                rpm: 880,
            },
            Fan {
                label: "bottom",
                rpm: 910,
            },
            Fan {
                label: "rear",
                rpm: 870,
            },
            Fan {
                label: "top",
                rpm: 905,
            },
            Fan {
                label: "gpu",
                rpm: 1420,
            },
        ],
    };
    let weather = Weather {
        city: "Reykjavik".to_owned(),
        temp: 4.0,
        description: "Neige",
    };
    let now = Local.with_ymd_and_hms(2026, 3, 14, 21, 7, 0).unwrap();

    let config = thermaltaked::config::Config::default();
    let dashboard = Dashboard::new(&config.font_regular, &config.font_bold)?;
    dashboard
        .render(&snapshot, Some(&weather), now)
        .save(DASHBOARD)?;
    dashboard.render_clock(now).save(CLOCK)?;

    let playing = NowPlaying {
        track: Track {
            title: "Signal in the Static".to_owned(),
            artist: "The Long Players".to_owned(),
            album: "Second Wind".to_owned(),
            art_url: None,
        },
        position: Duration::from_secs(97),
        length: Some(Duration::from_secs(271)),
    };
    dashboard
        .render_player(&playing, Some(&cover()), now)
        .save(PLAYER)?;
    println!("wrote {DASHBOARD}, {CLOCK} and {PLAYER}");
    Ok(())
}

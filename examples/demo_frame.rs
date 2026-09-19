//! Renders the README's screenshot from made-up readings, so the image shows
//! the real layout without anyone's machine in it.
//!
//! Run with: `cargo run --example demo_frame`

use chrono::{Local, TimeZone};
use thermaltaked::render::Dashboard;
use thermaltaked::sensors::{Fan, Names, Snapshot};
use thermaltaked::weather::Weather;

const OUTPUT: &str = "docs/dashboard.png";

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
        .save(OUTPUT)?;
    println!("wrote {OUTPUT}");
    Ok(())
}

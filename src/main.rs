use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Local;
use clap::{Parser, Subcommand};
use image::{Rgb, RgbImage};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use thermaltaked::config::Config;
use thermaltaked::device;
use thermaltaked::lcd::Lcd;
use thermaltaked::protocol::{HEIGHT, PRODUCT_ID, VENDOR_ID, WIDTH};
use thermaltaked::render::Dashboard;
use thermaltaked::sensors::Sensors;
use thermaltaked::weather::WeatherFeed;

const RECONNECT_DELAY: Duration = Duration::from_secs(3);
/// /proc/stat needs two samples before a CPU usage can be computed.
const CPU_SAMPLE_DELAY: Duration = Duration::from_millis(250);

#[derive(Parser)]
#[command(
    version,
    about = "Thermaltake 3.9\" bar LCD (264a:233d) dashboard daemon"
)]
struct Cli {
    /// Defaults to ~/.config/thermaltaked/config.toml when it exists.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Detect the LCD without sending anything to it
    Info,
    /// Display color bars
    Test,
    /// Display an image file, scaled and cropped to 480x128
    Image { path: PathBuf },
    /// Set the backlight, 0 to 100
    Brightness { percent: u8 },
    /// Render one dashboard frame to a PNG, without touching the LCD
    Preview { output: PathBuf },
    /// Run the dashboard until interrupted
    Run,
}

fn info() -> anyhow::Result<()> {
    let nodes = device::discover()?;
    if nodes.is_empty() {
        anyhow::bail!("Thermaltake LCD {VENDOR_ID:04x}:{PRODUCT_ID:04x} not found");
    }
    for node in nodes {
        let access = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node.path)
        {
            Ok(_) => "read/write ok".to_owned(),
            Err(error) => format!("not accessible: {error}"),
        };
        println!(
            "interface {}: {} ({access})",
            node.interface,
            node.path.display()
        );
    }
    Ok(())
}

fn color_bars() -> RgbImage {
    const BARS: [[u8; 3]; 8] = [
        [255, 255, 255],
        [255, 255, 0],
        [0, 255, 255],
        [0, 255, 0],
        [255, 0, 255],
        [255, 0, 0],
        [0, 0, 255],
        [0, 0, 0],
    ];
    RgbImage::from_fn(WIDTH, HEIGHT, |x, _| {
        Rgb(BARS[(x * BARS.len() as u32 / WIDTH) as usize])
    })
}

struct Scene {
    dashboard: Dashboard,
    sensors: Sensors,
    weather: Option<WeatherFeed>,
}

impl Scene {
    fn new(config: &Config) -> anyhow::Result<Self> {
        let weather_refresh = Duration::from_secs(config.weather.refresh_minutes.max(1) * 60);
        Ok(Self {
            dashboard: Dashboard::new(&config.font_regular, &config.font_bold)?,
            sensors: Sensors::new(config),
            weather: config
                .weather
                .city
                .clone()
                .map(|city| WeatherFeed::start(city, weather_refresh)),
        })
    }

    fn frame(&mut self) -> RgbImage {
        let weather = self.weather.as_ref().and_then(WeatherFeed::latest);
        self.dashboard
            .render(&self.sensors.snapshot(), weather.as_ref(), Local::now())
    }
}

fn preview(config: &Config, output: &Path) -> anyhow::Result<()> {
    let mut scene = Scene::new(config)?;
    scene.frame();
    // Leaves the weather thread a chance to answer.
    let deadline = Instant::now() + Duration::from_secs(5);
    while scene
        .weather
        .as_ref()
        .is_some_and(|feed| feed.latest().is_none())
        && Instant::now() < deadline
    {
        thread::sleep(CPU_SAMPLE_DELAY);
    }
    thread::sleep(CPU_SAMPLE_DELAY);
    scene
        .frame()
        .save(output)
        .with_context(|| format!("writing {}", output.display()))
}

fn connect(brightness: u8) -> anyhow::Result<Lcd> {
    let mut lcd = Lcd::open()?;
    lcd.set_brightness(brightness)?;
    lcd.start_heartbeat();
    Ok(lcd)
}

/// Delivers a message on SIGINT or SIGTERM, so the main loop can sleep on it
/// instead of polling a flag.
fn stop_requests() -> anyhow::Result<Receiver<()>> {
    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    let (stop, stopped) = mpsc::channel();
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            let _ = stop.send(());
        }
    });
    Ok(stopped)
}

fn run(config: &Config) -> anyhow::Result<()> {
    let stopped = stop_requests()?;
    let refresh = Duration::from_secs_f32(config.refresh_seconds.clamp(0.1, 3600.0));
    let mut scene = Scene::new(config)?;
    let mut lcd = None;

    loop {
        let started = Instant::now();
        let mut pause = refresh;
        if lcd.is_none() {
            match connect(config.brightness) {
                Ok(connected) => {
                    eprintln!("LCD connected");
                    lcd = Some(connected);
                }
                Err(error) => {
                    eprintln!("LCD unavailable: {error:#}");
                    pause = RECONNECT_DELAY;
                }
            }
        }
        if let Some(connected) = &lcd
            && let Err(error) = connected.send_frame(&scene.frame())
        {
            eprintln!("LCD lost: {error:#}");
            lcd = None;
            pause = RECONNECT_DELAY;
        }

        // Sleeps until the next frame is due, unless a signal asks to stop.
        let remaining = pause.saturating_sub(started.elapsed());
        if stopped.recv_timeout(remaining) != Err(RecvTimeoutError::Timeout) {
            return Ok(());
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    match cli.command {
        Command::Info => info(),
        Command::Test => Ok(Lcd::open()?.send_frame(&color_bars())?),
        Command::Image { path } => {
            let image =
                image::open(&path).with_context(|| format!("opening {}", path.display()))?;
            Ok(Lcd::open()?.send_image(&image)?)
        }
        Command::Brightness { percent } => Ok(Lcd::open()?.set_brightness(percent)?),
        Command::Preview { output } => preview(&config, &output),
        Command::Run => run(&config),
    }
}

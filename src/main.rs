use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Local};
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
use thermaltaked::session::{ScreenState, Screens};
use thermaltaked::weather::WeatherFeed;

const RECONNECT_DELAY: Duration = Duration::from_secs(3);
/// Left without a frame for some twenty seconds, the panel falls back to its
/// own screen, so the clock is sent again well before that.
const CLOCK_KEEPALIVE: Duration = Duration::from_secs(5);
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
    Preview {
        output: PathBuf,
        /// Render the sleeping screen, which shows only the date and time
        #[arg(long)]
        clock: bool,
    },
    /// Run the dashboard until interrupted
    Run,
}

fn info() -> anyhow::Result<()> {
    println!("screens: {}", Screens::new().state().label());
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

    fn dashboard_frame(&mut self, now: DateTime<Local>) -> RgbImage {
        let weather = self.weather.as_ref().and_then(WeatherFeed::latest);
        self.dashboard
            .render(&self.sensors.snapshot(), weather.as_ref(), now)
    }

    fn clock_frame(&self, now: DateTime<Local>) -> RgbImage {
        self.dashboard.render_clock(now)
    }
}

fn preview(config: &Config, output: &Path, clock: bool) -> anyhow::Result<()> {
    let mut scene = Scene::new(config)?;
    if clock {
        return scene
            .clock_frame(Local::now())
            .save(output)
            .with_context(|| format!("writing {}", output.display()));
    }
    scene.dashboard_frame(Local::now());
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
        .dashboard_frame(Local::now())
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

fn describe(state: ScreenState) -> String {
    let drawing = if state == ScreenState::Awake {
        "the dashboard"
    } else {
        "the clock"
    };
    format!("{}: drawing {drawing}", state.label())
}

fn run(config: &Config) -> anyhow::Result<()> {
    let stopped = stop_requests()?;
    let screens = Screens::new();
    let refresh = Duration::from_secs_f32(config.refresh_seconds.clamp(0.1, 3600.0));
    let mut scene = Scene::new(config)?;
    let mut lcd = None;
    // The panel is out of reach until its udev rule grants this session access,
    // which can be a while after boot, so the same complaint is logged once.
    let mut complaint = None;
    let mut shown = None;
    let mut clock_sent: Option<Instant> = None;

    loop {
        let started = Instant::now();
        let mut pause = refresh;
        if lcd.is_none() {
            match connect(config.brightness) {
                Ok(connected) => {
                    eprintln!("LCD connected");
                    complaint = None;
                    lcd = Some(connected);
                }
                Err(error) => {
                    let error = format!("LCD unavailable: {error:#}");
                    if complaint.as_ref() != Some(&error) {
                        eprintln!("{error}");
                        complaint = Some(error);
                    }
                    pause = RECONNECT_DELAY;
                }
            }
        }
        let now = Local::now();
        let state = if config.clock_when_away {
            screens.state()
        } else {
            ScreenState::Awake
        };
        if shown != Some(state) {
            eprintln!("{}", describe(state));
            shown = Some(state);
            clock_sent = None;
        }

        // The clock has no seconds to show, so it goes out at the slower pace
        // the panel needs to keep displaying it.
        let due = state == ScreenState::Awake
            || clock_sent.is_none_or(|sent| sent.elapsed() >= CLOCK_KEEPALIVE);
        if let Some(connected) = &lcd
            && due
        {
            let frame = match state {
                ScreenState::Awake => scene.dashboard_frame(now),
                _ => scene.clock_frame(now),
            };
            match connected.send_frame(&frame) {
                Ok(()) => clock_sent = Some(Instant::now()),
                Err(error) => {
                    eprintln!("LCD lost: {error:#}");
                    lcd = None;
                    pause = RECONNECT_DELAY;
                }
            }
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
        Command::Preview { output, clock } => preview(&config, &output, clock),
        Command::Run => run(&config),
    }
}

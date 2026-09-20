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

use thermaltaked::art::CoverArt;
use thermaltaked::config::Config;
use thermaltaked::device;
use thermaltaked::lcd::Lcd;
use thermaltaked::player::{NowPlaying, Players};
use thermaltaked::protocol::{HEIGHT, PRODUCT_ID, VENDOR_ID, WIDTH};
use thermaltaked::render::{ART_SIZE, Dashboard};
use thermaltaked::sensors::Sensors;
use thermaltaked::session::{ScreenState, Screens};
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
    players: Players,
    cover: CoverArt,
    /// The cover last drawn, kept so it is fetched once per track.
    art: Option<(String, RgbImage)>,
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
            players: Players::new(),
            cover: CoverArt::new(),
            art: None,
        })
    }

    fn dashboard_frame(&mut self, now: DateTime<Local>) -> RgbImage {
        let weather = self.weather.as_ref().and_then(WeatherFeed::latest);
        self.dashboard
            .render(&self.sensors.snapshot(), weather.as_ref(), now)
    }

    fn player_frame(&mut self, playing: &NowPlaying, now: DateTime<Local>) -> RgbImage {
        // Neither the player nor the clock draws the dashboard, so its CPU
        // usage would otherwise come back averaged over the whole absence.
        self.sensors.sample_cpu();
        match &playing.track.art_url {
            Some(url) => {
                if self.art.as_ref().is_none_or(|(drawn, _)| drawn != url) {
                    self.art = self.cover.get(url, ART_SIZE).map(|art| (url.clone(), art));
                }
            }
            None => self.art = None,
        }
        let art = self.art.as_ref().map(|(_, art)| art);
        self.dashboard.render_player(playing, art, now)
    }

    fn clock_frame(&mut self, now: DateTime<Local>) -> RgbImage {
        // The dashboard is not drawn while away, so its CPU usage would
        // otherwise come back averaged over the whole absence.
        self.sensors.sample_cpu();
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
    let mut screens = Screens::new();
    let refresh = Duration::from_secs_f32(config.refresh_seconds);
    let mut scene = Scene::new(config)?;
    let mut lcd = None;
    // The panel is out of reach until its udev rule grants this session access,
    // which can be a while after boot, so the same complaint is logged once.
    let mut complaint = None;
    let mut shown = None;

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
        let playing = if config.show_player {
            scene.players.playing()
        } else {
            None
        };
        let showing = match &playing {
            Some(playing) => format!(
                "playing: {} — {}",
                playing.track.artist, playing.track.title
            ),
            None => describe(state),
        };
        if shown.as_ref() != Some(&showing) {
            eprintln!("{showing}");
            shown = Some(showing);
        }

        // Every frame goes out at the same pace, the clock included: left a few
        // seconds without one, the panel drops it for its own screen.
        if let Some(connected) = &lcd {
            let frame = match (&playing, state) {
                (Some(playing), _) => scene.player_frame(playing, now),
                (None, ScreenState::Awake) => scene.dashboard_frame(now),
                (None, _) => scene.clock_frame(now),
            };
            if let Err(error) = connected.send_frame(&frame) {
                eprintln!("LCD lost: {error:#}");
                lcd = None;
                pause = RECONNECT_DELAY;
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

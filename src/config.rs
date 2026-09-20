//! TOML configuration, every field optional.

use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::Context;
use indexmap::IndexMap;
use serde::Deserialize;

use crate::sensors::Names;

/// The panel drops the frame it shows for its own screen when a few seconds
/// pass without a new one, so a slower refresh is not offered.
const MAX_REFRESH_SECONDS: f32 = 2.0;
const MIN_REFRESH_SECONDS: f32 = 0.1;
const DEFAULT_REFRESH_SECONDS: f32 = 1.0;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Seconds between two frames, brought back within 0.1 and 2 on load.
    pub refresh_seconds: f32,
    pub brightness: u8,
    pub font_regular: PathBuf,
    pub font_bold: PathBuf,
    /// Show only the date and time while the monitors are off or the session
    /// is locked, instead of the dashboard.
    pub clock_when_away: bool,
    /// Give the panel over to whatever is playing, for as long as it plays.
    pub show_player: bool,
    pub weather: WeatherConfig,
    /// Gauge titles; whatever is left out is detected from the hardware.
    pub names: Names,
    /// Rated wattage of the power supply, for its load bar. Read from the
    /// model name when unset (`HX1200i`: 1200 W).
    pub psu_rating: Option<f32>,
    /// Display names keyed by `<hwmon chip>/<fanN>`, e.g. `nct6799/fan7`.
    /// When empty, every spinning fan is shown; otherwise only the listed ones,
    /// in the order they are written.
    pub fans: IndexMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WeatherConfig {
    /// Looked up through the Open-Meteo geocoding API. Weather is off when unset.
    pub city: Option<String>,
    pub refresh_minutes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            refresh_seconds: DEFAULT_REFRESH_SECONDS,
            brightness: 100,
            font_regular: "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf".into(),
            font_bold: "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf".into(),
            clock_when_away: true,
            show_player: true,
            weather: WeatherConfig::default(),
            names: Names::default(),
            psu_rating: None,
            fans: IndexMap::new(),
        }
    }
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            city: None,
            refresh_minutes: 15,
        }
    }
}

fn default_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| Path::new(&home).join(".config")))?;
    Some(base.join("thermaltaked/config.toml"))
}

impl Config {
    /// An explicit path must exist; the default one may be absent.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, or does not parse as this configuration.
    pub fn load(explicit: Option<&Path>) -> anyhow::Result<Self> {
        let path = match explicit {
            Some(path) => path.to_owned(),
            None => match default_path().filter(|path| path.exists()) {
                Some(path) => path,
                None => return Ok(Self::default()),
            },
        };
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        let mut config: Self = toml::from_str(text)?;
        let asked = config.refresh_seconds;
        config.refresh_seconds = if asked.is_finite() {
            asked.clamp(MIN_REFRESH_SECONDS, MAX_REFRESH_SECONDS)
        } else {
            DEFAULT_REFRESH_SECONDS
        };
        if config.refresh_seconds != asked {
            eprintln!(
                "refresh_seconds {asked} is out of reach, using {}",
                config.refresh_seconds
            );
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refresh(text: &str) -> f32 {
        Config::parse(text).unwrap().refresh_seconds
    }

    #[test]
    fn refresh_stays_within_what_the_panel_accepts() {
        assert_eq!(refresh("refresh_seconds = 1.5"), 1.5);
        assert_eq!(refresh("refresh_seconds = 60.0"), MAX_REFRESH_SECONDS);
        assert_eq!(refresh("refresh_seconds = 0.0"), MIN_REFRESH_SECONDS);
        assert_eq!(refresh("refresh_seconds = -3.0"), MIN_REFRESH_SECONDS);
        assert_eq!(refresh("refresh_seconds = nan"), DEFAULT_REFRESH_SECONDS);
        assert_eq!(refresh(""), DEFAULT_REFRESH_SECONDS);
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(Config::parse("refresh_secondes = 1.0").is_err());
    }
}

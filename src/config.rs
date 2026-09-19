//! TOML configuration, every field optional.

use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::Context;
use indexmap::IndexMap;
use serde::Deserialize;

use crate::sensors::Names;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Seconds between two frames.
    pub refresh_seconds: f32,
    pub brightness: u8,
    pub font_regular: PathBuf,
    pub font_bold: PathBuf,
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
            refresh_seconds: 1.0,
            brightness: 100,
            font_regular: "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf".into(),
            font_bold: "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf".into(),
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
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

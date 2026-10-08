//! TOML configuration, every field optional.

use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::Context;
use indexmap::IndexMap;
use log::warn;
use serde::Deserialize;

use crate::sensors::Names;

/// The panel drops the frame it shows for its own screen when a few seconds
/// pass without a new one, so a slower refresh is not offered.
const MAX_REFRESH_SECONDS: f32 = 2.0;
const MIN_REFRESH_SECONDS: f32 = 0.1;
const DEFAULT_REFRESH_SECONDS: f32 = 1.0;

/// Where distributions keep DejaVu Sans, the dashboard's font unless the
/// configuration names another.
const FONT_DIRS: [&str; 6] = [
    "/usr/share/fonts/dejavu-sans-fonts", // Fedora
    "/usr/share/fonts/truetype/dejavu",   // Debian, Ubuntu
    "/usr/share/fonts/TTF",               // Arch, Void
    "/usr/share/fonts/dejavu",            // Gentoo, Alpine
    "/usr/share/fonts/truetype",          // openSUSE
    "/usr/local/share/fonts",             // a hand-installed copy
];
const FONT_REGULAR: &str = "DejaVuSans.ttf";
const FONT_BOLD: &str = "DejaVuSans-Bold.ttf";

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Seconds between two frames, brought back within 0.1 and 2 on load.
    pub refresh_seconds: f32,
    pub brightness: u8,
    /// TrueType files; DejaVu Sans, wherever the distribution keeps it,
    /// when unset. See [`Config::fonts`].
    pub font_regular: Option<PathBuf>,
    pub font_bold: Option<PathBuf>,
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
            font_regular: None,
            font_bold: None,
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

/// The first `dirs` entry holding `file`, or an error naming every path tried.
fn find_font(dirs: &[impl AsRef<Path>], file: &str) -> anyhow::Result<PathBuf> {
    let candidates: Vec<PathBuf> = dirs.iter().map(|dir| dir.as_ref().join(file)).collect();
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .with_context(|| {
            let tried: Vec<String> = candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            format!(
                "no {file} found (tried {}): install the DejaVu fonts package, or set \
                 font_regular and font_bold in the configuration",
                tried.join(", ")
            )
        })
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

    /// The regular and bold font files: those configured, or DejaVu Sans
    /// from wherever the distribution keeps it.
    ///
    /// # Errors
    ///
    /// When a font is left unset and DejaVu Sans is in none of the usual
    /// places; the message lists the paths tried.
    pub fn fonts(&self) -> anyhow::Result<(PathBuf, PathBuf)> {
        let find = |configured: &Option<PathBuf>, file| match configured {
            Some(path) => Ok(path.clone()),
            None => find_font(&FONT_DIRS, file),
        };
        Ok((
            find(&self.font_regular, FONT_REGULAR)?,
            find(&self.font_bold, FONT_BOLD)?,
        ))
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        let mut config: Self = toml::from_str(text)?;
        let asked = config.refresh_seconds;
        config.refresh_seconds = if asked.is_finite() {
            asked.clamp(MIN_REFRESH_SECONDS, MAX_REFRESH_SECONDS)
        } else {
            DEFAULT_REFRESH_SECONDS
        };
        // NaN is in no range, so it is reported along with the rest.
        if !(MIN_REFRESH_SECONDS..=MAX_REFRESH_SECONDS).contains(&asked) {
            warn!(
                "refresh_seconds {asked} is out of reach, using {}",
                config.refresh_seconds
            );
        }
        if let Some(rating) = config.psu_rating
            && !(rating.is_finite() && rating > 0.0)
        {
            warn!("psu_rating {rating} is not a wattage, reading the model name instead");
            config.psu_rating = None;
        }
        Ok(config)
    }
}

// The values compared are the very constants and literals that went in.
#[cfg(test)]
#[allow(clippy::float_cmp)]
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
    fn psu_rating_must_be_a_wattage() {
        let rating = |text: &str| Config::parse(text).unwrap().psu_rating;
        assert_eq!(rating("psu_rating = 850"), Some(850.0));
        assert_eq!(rating("psu_rating = 0"), None);
        assert_eq!(rating("psu_rating = -500"), None);
        assert_eq!(rating("psu_rating = inf"), None);
        assert_eq!(rating(""), None);
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(Config::parse("refresh_secondes = 1.0").is_err());
    }

    #[test]
    fn fans_keep_the_order_they_are_written_in() {
        let config = Config::parse("[fans]\n\"nct6799/fan7\" = \"rear\"\n\"amdgpu/fan1\" = \"gpu\"\n\"nct6799/fan1\" = \"front\"")
            .unwrap();
        let keys: Vec<&str> = config.fans.keys().map(String::as_str).collect();
        assert_eq!(keys, ["nct6799/fan7", "amdgpu/fan1", "nct6799/fan1"]);
    }

    #[test]
    fn the_first_directory_holding_the_font_wins() {
        let fonts = tempfile::tempdir().unwrap();
        let (empty, fedora, debian) = (
            fonts.path().join("empty"),
            fonts.path().join("fedora"),
            fonts.path().join("debian"),
        );
        for dir in [&empty, &fedora, &debian] {
            fs::create_dir(dir).unwrap();
        }
        fs::write(fedora.join(FONT_REGULAR), b"ttf").unwrap();
        fs::write(debian.join(FONT_REGULAR), b"ttf").unwrap();
        fs::write(debian.join(FONT_BOLD), b"ttf").unwrap();
        let dirs = [&empty, &fedora, &debian];
        assert_eq!(
            find_font(&dirs, FONT_REGULAR).unwrap(),
            fedora.join(FONT_REGULAR)
        );
        assert_eq!(find_font(&dirs, FONT_BOLD).unwrap(), debian.join(FONT_BOLD));
    }

    #[test]
    fn a_missing_font_names_every_path_tried() {
        let fonts = tempfile::tempdir().unwrap();
        let dirs = [fonts.path().join("a"), fonts.path().join("b")];
        let error = find_font(&dirs, FONT_REGULAR).unwrap_err().to_string();
        for dir in &dirs {
            assert!(
                error.contains(&dir.join(FONT_REGULAR).display().to_string()),
                "{error}"
            );
        }
        assert!(error.contains("font_regular"), "{error}");
    }
}

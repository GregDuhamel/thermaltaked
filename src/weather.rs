//! Current weather from Open-Meteo (no API key), refreshed in the background.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

const GEOCODING_URL: &str = "https://geocoding-api.open-meteo.com/v1/search";
const FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";
const RETRY_DELAY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Weather {
    pub city: String,
    pub temp: f32,
    pub description: &'static str,
}

#[derive(Deserialize)]
struct Geocoding {
    #[serde(default)]
    results: Vec<Place>,
}

#[derive(Deserialize)]
struct Place {
    name: String,
    latitude: f64,
    longitude: f64,
}

#[derive(Deserialize)]
struct Forecast {
    current: Current,
}

#[derive(Deserialize)]
struct Current {
    temperature_2m: f32,
    weather_code: u8,
}

/// WMO weather interpretation codes, as documented by Open-Meteo.
const fn describe(code: u8) -> &'static str {
    match code {
        0 => "Ciel dégagé",
        1 => "Peu nuageux",
        2 => "Nuageux",
        3 => "Couvert",
        45 | 48 => "Brouillard",
        51..=57 => "Bruine",
        61 | 80 => "Pluie faible",
        63 | 81 => "Pluie",
        65 | 82 => "Forte pluie",
        66 | 67 => "Pluie verglaçante",
        71..=77 | 85 | 86 => "Neige",
        95..=99 => "Orage",
        _ => "—",
    }
}

fn locate(city: &str) -> anyhow::Result<Place> {
    let geocoding: Geocoding = ureq::get(GEOCODING_URL)
        .query("name", city)
        .query("count", "1")
        .query("language", "fr")
        .call()?
        .body_mut()
        .read_json()?;
    geocoding
        .results
        .into_iter()
        .next()
        .with_context(|| format!("unknown city {city:?}"))
}

fn fetch(place: &Place) -> anyhow::Result<Weather> {
    let forecast: Forecast = ureq::get(FORECAST_URL)
        .query("latitude", place.latitude.to_string())
        .query("longitude", place.longitude.to_string())
        .query("current", "temperature_2m,weather_code")
        .call()?
        .body_mut()
        .read_json()?;
    Ok(Weather {
        city: place.name.clone(),
        temp: forecast.current.temperature_2m,
        description: describe(forecast.current.weather_code),
    })
}

/// Latest known weather, shared with the refresh thread.
#[derive(Default)]
pub struct WeatherFeed(Arc<Mutex<Option<Weather>>>);

impl WeatherFeed {
    /// Starts the refresh thread and hands back the feed it fills.
    ///
    /// # Panics
    ///
    /// When another thread panicked while holding the latest reading.
    #[must_use]
    pub fn start(city: String, refresh: Duration) -> Self {
        let feed = Self::default();
        let shared = Arc::clone(&feed.0);
        thread::spawn(move || {
            let mut place = None;
            loop {
                if place.is_none() {
                    place = locate(&city)
                        .inspect_err(|error| eprintln!("weather: {error:#}"))
                        .ok();
                }
                // A failed refresh keeps showing the previous reading.
                let delay = match place.as_ref().map(fetch) {
                    Some(Ok(weather)) => {
                        *shared.lock().unwrap() = Some(weather);
                        refresh
                    }
                    Some(Err(error)) => {
                        eprintln!("weather: {error:#}");
                        RETRY_DELAY
                    }
                    None => RETRY_DELAY,
                };
                thread::sleep(delay);
            }
        });
        feed
    }

    /// The last successful reading, if any has arrived yet.
    ///
    /// # Panics
    ///
    /// When another thread panicked while holding the latest reading.
    #[must_use]
    pub fn latest(&self) -> Option<Weather> {
        self.0.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forecast() {
        let json = r#"{"latitude":48.86,"current":{"time":"2026-09-19T18:45","interval":900,"temperature_2m":21.4,"weather_code":2}}"#;
        let forecast: Forecast = serde_json::from_str(json).unwrap();
        assert_eq!(forecast.current.temperature_2m, 21.4);
        assert_eq!(describe(forecast.current.weather_code), "Nuageux");
    }

    #[test]
    fn unknown_city_has_no_results() {
        let geocoding: Geocoding = serde_json::from_str(r#"{"generationtime_ms":0.5}"#).unwrap();
        assert!(geocoding.results.is_empty());
    }
}

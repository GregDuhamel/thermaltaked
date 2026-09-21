//! What is playing, read from any MPRIS player on the session bus.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use zbus::blocking::Connection;
use zbus::blocking::fdo::PropertiesProxy;
use zbus::names::InterfaceName;
use zbus::zvariant::OwnedValue;

const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
/// A session bus is not always there, and players come and go.
const RETRY: Duration = Duration::from_secs(30);

/// One track, as the player describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub title: String,
    /// Artists joined by commas, empty when the player names none.
    pub artist: String,
    pub album: String,
    pub art_url: Option<String>,
}

/// A track and how far into it the player is.
#[derive(Debug, Clone)]
pub struct NowPlaying {
    pub track: Track,
    pub position: Duration,
    pub length: Option<Duration>,
}

fn text_value(value: Option<&OwnedValue>) -> Option<String> {
    let text = value?.downcast_ref::<zbus::zvariant::Str<'_>>().ok()?;
    Some(text.as_str().to_owned())
}

fn text(metadata: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let value = metadata.get(key)?;
    if let Ok(text) = value.downcast_ref::<zbus::zvariant::Str<'_>>() {
        return Some(text.as_str().to_owned());
    }
    // Artists come as an array of strings, however many of them there are.
    let array = value.downcast_ref::<zbus::zvariant::Array>().ok()?;
    let joined: Vec<String> = array
        .iter()
        .filter_map(|item| Some(item.downcast_ref::<&str>().ok()?.to_owned()))
        .collect();
    (!joined.is_empty()).then(|| joined.join(", "))
}

/// MPRIS asks for signed microseconds, but some players send unsigned ones.
fn duration(value: &OwnedValue) -> Option<Duration> {
    if let Ok(micros) = value.downcast_ref::<i64>() {
        return u64::try_from(micros).ok().map(Duration::from_micros);
    }
    value.downcast_ref::<u64>().ok().map(Duration::from_micros)
}

fn micros(metadata: &HashMap<String, OwnedValue>, key: &str) -> Option<Duration> {
    duration(metadata.get(key)?)
}

/// Reads the session bus for a player that is playing. Built once, then asked
/// on every frame.
pub struct Players {
    connection: Option<Connection>,
    last_try: Instant,
    complained: bool,
}

impl Players {
    #[must_use]
    pub fn new() -> Self {
        let mut players = Self {
            connection: None,
            last_try: Instant::now(),
            complained: false,
        };
        players.connect();
        players
    }

    fn connect(&mut self) {
        self.last_try = Instant::now();
        match Connection::session() {
            Ok(connection) => {
                if self.complained {
                    eprintln!("session bus reachable again");
                }
                self.complained = false;
                self.connection = Some(connection);
            }
            Err(error) => {
                if !self.complained {
                    eprintln!("players unavailable: {error}");
                    self.complained = true;
                }
            }
        }
    }

    /// The first player that is playing, if any is.
    #[must_use]
    pub fn playing(&mut self) -> Option<NowPlaying> {
        if self.connection.is_none() && self.last_try.elapsed() >= RETRY {
            self.connect();
        }
        let connection = self.connection.as_ref()?;
        match now_playing(connection) {
            Ok(playing) => playing,
            Err(error) => {
                eprintln!("players lost: {error}");
                self.connection = None;
                self.last_try = Instant::now();
                None
            }
        }
    }
}

impl Default for Players {
    fn default() -> Self {
        Self::new()
    }
}

fn now_playing(connection: &Connection) -> zbus::Result<Option<NowPlaying>> {
    let bus = zbus::blocking::Proxy::new(connection, DBUS, DBUS_PATH, DBUS)?;
    let names: Vec<String> = bus.call("ListNames", &())?;
    let player = InterfaceName::try_from(PLAYER)?;
    for name in names.into_iter().filter(|n| n.starts_with(MPRIS_PREFIX)) {
        // One round trip for the lot, and none of it cached: the position
        // moves along without a signal to announce it.
        let properties = PropertiesProxy::builder(connection)
            .destination(name)?
            .path(MPRIS_PATH)?
            .build()?;
        let all = properties.get_all(player.clone())?;
        if text_value(all.get("PlaybackStatus")).as_deref() != Some("Playing") {
            continue;
        }
        let metadata = all
            .get("Metadata")
            .and_then(|value| HashMap::<String, OwnedValue>::try_from(value.clone()).ok())
            .unwrap_or_default();
        // Some browser tabs play without saying what: nothing worth the panel.
        let Some(title) = text(&metadata, "xesam:title").filter(|title| !title.is_empty()) else {
            continue;
        };
        return Ok(Some(NowPlaying {
            track: Track {
                title,
                artist: text(&metadata, "xesam:artist").unwrap_or_default(),
                album: text(&metadata, "xesam:album").unwrap_or_default(),
                art_url: text(&metadata, "mpris:artUrl"),
            },
            position: all.get("Position").and_then(duration).unwrap_or_default(),
            length: micros(&metadata, "mpris:length"),
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn metadata(entries: Vec<(&str, Value<'_>)>) -> HashMap<String, OwnedValue> {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.try_to_owned().unwrap()))
            .collect()
    }

    #[test]
    fn titles_and_artists_are_read() {
        let metadata = metadata(vec![
            ("xesam:title", Value::from("Adagio for Strings")),
            ("xesam:artist", Value::from(vec!["Tiësto", "Someone Else"])),
        ]);
        assert_eq!(
            text(&metadata, "xesam:title").as_deref(),
            Some("Adagio for Strings")
        );
        assert_eq!(
            text(&metadata, "xesam:artist").as_deref(),
            Some("Tiësto, Someone Else")
        );
        assert_eq!(text(&metadata, "xesam:album"), None);
    }

    #[test]
    fn lengths_come_signed_or_not() {
        let metadata = metadata(vec![
            ("signed", Value::from(418_000_000_i64)),
            ("unsigned", Value::from(418_000_000_u64)),
            ("negative", Value::from(-1_i64)),
        ]);
        assert_eq!(micros(&metadata, "signed"), Some(Duration::from_secs(418)));
        assert_eq!(
            micros(&metadata, "unsigned"),
            Some(Duration::from_secs(418))
        );
        assert_eq!(micros(&metadata, "negative"), None);
    }
}

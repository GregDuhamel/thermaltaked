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

fn micros(metadata: &HashMap<String, OwnedValue>, key: &str) -> Option<Duration> {
    let value = metadata.get(key)?;
    let micros = value.downcast_ref::<i64>().ok()?;
    u64::try_from(micros).ok().map(Duration::from_micros)
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
        let position = all
            .get("Position")
            .and_then(|value| value.downcast_ref::<i64>().ok())
            .unwrap_or_default();
        return Ok(Some(NowPlaying {
            track: Track {
                title: text(&metadata, "xesam:title").unwrap_or_default(),
                artist: text(&metadata, "xesam:artist").unwrap_or_default(),
                album: text(&metadata, "xesam:album").unwrap_or_default(),
                art_url: text(&metadata, "mpris:artUrl"),
            },
            position: Duration::from_micros(position.try_into().unwrap_or_default()),
            length: micros(&metadata, "mpris:length"),
        }));
    }
    Ok(None)
}

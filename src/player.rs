//! What is playing, read from any MPRIS player on the session bus by a
//! thread of its own.
//!
//! The thread asks the bus for its names, then every `org.mpris.MediaPlayer2.*`
//! for all of its player properties, about once a second and sooner when a
//! player announces a change. The drawing loop reads only what the thread
//! last published, and moves the position along on its own between two
//! readings, so a player that stops answering holds up at most the thread,
//! and only for the call's timeout.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, warn};
use zbus::blocking::Connection;
use zbus::blocking::fdo::PropertiesProxy;
use zbus::message::Type;
use zbus::names::InterfaceName;
use zbus::zvariant::OwnedValue;
use zbus::{MatchRule, fdo};

use crate::background::{Background, Publisher};
use crate::bus;
use crate::complaint::Complaint;

const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
/// A session bus is not always there, and players come and go.
const RETRY: Duration = Duration::from_secs(30);
/// The position moves along without a signal to announce it, so the players
/// are asked again this often even when none of them says anything.
const POLL: Duration = Duration::from_secs(1);
/// A player that let a call time out is left alone this long before it is
/// asked again, so one frozen player does not cost every poll its timeout.
const FROZEN_FOR: Duration = Duration::from_secs(30);

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

impl NowPlaying {
    /// The same track, `elapsed` further along: what a player at normal
    /// speed shows that long after this reading, held at the end of the track.
    #[must_use]
    pub fn advanced_by(&self, elapsed: Duration) -> Self {
        let mut position = self.position.saturating_add(elapsed);
        if let Some(length) = self.length {
            position = position.min(length);
        }
        Self {
            track: self.track.clone(),
            position,
            length: self.length,
        }
    }
}

/// What the thread publishes: a reading and when it was taken.
#[derive(Debug, Clone)]
struct Reading {
    playing: Option<NowPlaying>,
    taken: Instant,
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

/// The first player that is playing, as the background thread last found
/// it. Built once, then asked on every frame without touching the bus.
pub struct Players {
    latest: Background<Reading>,
}

impl Players {
    /// Starts the thread that watches the session bus.
    #[must_use]
    pub fn new() -> Self {
        let initial = Reading {
            playing: None,
            taken: Instant::now(),
        };
        Self {
            latest: Background::start("mpris", initial, |publisher| watch(&publisher)),
        }
    }

    /// The first player that is playing, if any is, with its position moved
    /// along since the thread read it.
    #[must_use]
    pub fn playing(&self) -> Option<NowPlaying> {
        let reading = self.latest.latest();
        let playing = reading.playing?;
        Some(playing.advanced_by(reading.taken.elapsed()))
    }
}

impl Default for Players {
    fn default() -> Self {
        Self::new()
    }
}

/// `PropertiesChanged` from any player: they all live at the same path,
/// whatever their name.
fn changes_rule() -> zbus::Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(Type::Signal)
        .interface(PROPERTIES)?
        .member("PropertiesChanged")?
        .path(MPRIS_PATH)?
        .build())
}

/// The thread: connects, then reads the players again on every nudge and
/// at least every [`POLL`], until the bus goes away, when it starts over.
fn watch(publisher: &Publisher<Reading>) {
    let mut complaint = Complaint::default();
    while !publisher.abandoned() {
        let connected = bus::session().and_then(|connection| {
            let nudges = bus::nudges(&connection, changes_rule()?, "mpris-signals")?;
            Ok((connection, nudges))
        });
        let (connection, nudges) = match connected {
            Ok(connected) => connected,
            Err(error) => {
                complaint.raise(format!("players unavailable: {error}"));
                thread::sleep(RETRY);
                continue;
            }
        };
        complaint.withdraw("session bus reachable again");
        let mut frozen = Frozen::default();
        let error = follow(&connection, &nudges, &mut frozen, publisher);
        if publisher.abandoned() {
            return;
        }
        // The reading stands until the bus is back, which could mislead:
        // nothing plays on a bus that is not there.
        publisher.publish(Reading {
            playing: None,
            taken: Instant::now(),
        });
        complaint.raise(format!("players lost: {error}"));
        thread::sleep(RETRY);
    }
}

/// Reads the players until the bus fails, and says how.
fn follow(
    connection: &Connection,
    nudges: &Receiver<()>,
    frozen: &mut Frozen,
    publisher: &Publisher<Reading>,
) -> zbus::Error {
    loop {
        match now_playing(connection, frozen) {
            Ok(playing) => publisher.publish(Reading {
                playing,
                taken: Instant::now(),
            }),
            Err(error) => return error,
        }
        if publisher.abandoned() {
            // Lets the caller wind down through its usual path.
            return zbus::Error::Failure("nobody reads the players anymore".to_owned());
        }
        match nudges.recv_timeout(POLL) {
            // A burst of changes is read once.
            Ok(()) => while nudges.try_recv().is_ok() {},
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return zbus::Error::Failure("the signal stream ended".to_owned());
            }
        }
    }
}

/// Players that let a call time out, and when they did.
#[derive(Default)]
struct Frozen(HashMap<String, Instant>);

impl Frozen {
    /// Whether `name` is still being left alone.
    fn holds(&mut self, name: &str) -> bool {
        match self.0.get(name) {
            Some(since) if since.elapsed() < FROZEN_FOR => true,
            Some(_) => {
                self.0.remove(name);
                false
            }
            None => false,
        }
    }

    fn add(&mut self, name: String) {
        warn!(
            "player {name} is not answering, ignored for {}s",
            FROZEN_FOR.as_secs()
        );
        self.0.insert(name, Instant::now());
    }
}

/// Every property of the player interface, in one round trip and none of it
/// cached: the position moves along without a signal to announce it.
fn properties(connection: &Connection, name: &str) -> fdo::Result<HashMap<String, OwnedValue>> {
    let player = InterfaceName::from_static_str_unchecked(PLAYER);
    let properties = PropertiesProxy::builder(connection)
        .destination(name.to_owned())?
        .path(MPRIS_PATH)?
        .build()?;
    properties.get_all(player)
}

/// What a player is playing, from its properties, when it is.
fn playing_from(all: &HashMap<String, OwnedValue>) -> Option<NowPlaying> {
    if text_value(all.get("PlaybackStatus")).as_deref() != Some("Playing") {
        return None;
    }
    let metadata = all
        .get("Metadata")
        .and_then(|value| HashMap::<String, OwnedValue>::try_from(value.clone()).ok())
        .unwrap_or_default();
    // Some browser tabs play without saying what: nothing worth the panel.
    let title = text(&metadata, "xesam:title").filter(|title| !title.is_empty())?;
    Some(NowPlaying {
        track: Track {
            title,
            artist: text(&metadata, "xesam:artist").unwrap_or_default(),
            album: text(&metadata, "xesam:album").unwrap_or_default(),
            art_url: text(&metadata, "mpris:artUrl"),
        },
        position: all.get("Position").and_then(duration).unwrap_or_default(),
        length: micros(&metadata, "mpris:length"),
    })
}

/// Fails only when the bus itself is out of reach: a player that leaves
/// between `ListNames` and its own answer is skipped, not held against the
/// others, and one that never answers is noted and left alone for a while.
fn now_playing(connection: &Connection, frozen: &mut Frozen) -> zbus::Result<Option<NowPlaying>> {
    let bus = zbus::blocking::Proxy::new(connection, DBUS, DBUS_PATH, DBUS)?;
    let names: Vec<String> = bus.call("ListNames", &())?;
    for name in names.into_iter().filter(|n| n.starts_with(MPRIS_PREFIX)) {
        if frozen.holds(&name) {
            continue;
        }
        let all = match properties(connection, &name) {
            Ok(all) => all,
            Err(fdo::Error::ZBus(error)) if bus::timed_out(&error) => {
                frozen.add(name);
                continue;
            }
            Err(fdo::Error::ZBus(error)) if bus::lost(&error) => return Err(error),
            Err(error) => {
                debug!("player {name} skipped: {error}");
                continue;
            }
        };
        if let Some(playing) = playing_from(&all) {
            return Ok(Some(playing));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Dict, Signature, Value};

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

    /// The player's properties as `GetAll` returns them, the metadata being a
    /// dictionary of variants.
    fn properties(status: &str, metadata: Vec<(&str, Value<'_>)>) -> HashMap<String, OwnedValue> {
        let mut dict = Dict::new(&Signature::Str, &Signature::Variant);
        for (key, value) in metadata {
            dict.add(key, value).unwrap();
        }
        let mut all = HashMap::new();
        all.insert(
            "PlaybackStatus".to_owned(),
            Value::from(status).try_to_owned().unwrap(),
        );
        all.insert(
            "Metadata".to_owned(),
            Value::Dict(dict).try_to_owned().unwrap(),
        );
        all.insert(
            "Position".to_owned(),
            Value::from(97_000_000_i64).try_to_owned().unwrap(),
        );
        all
    }

    #[test]
    fn a_playing_player_is_read_whole() {
        let all = properties(
            "Playing",
            vec![
                ("xesam:title", Value::from("Adagio for Strings")),
                ("xesam:artist", Value::from(vec!["Tiësto"])),
                ("mpris:length", Value::from(418_000_000_i64)),
            ],
        );
        let playing = playing_from(&all).unwrap();
        assert_eq!(playing.track.title, "Adagio for Strings");
        assert_eq!(playing.track.artist, "Tiësto");
        assert_eq!(playing.track.album, "");
        assert_eq!(playing.position, Duration::from_secs(97));
        assert_eq!(playing.length, Some(Duration::from_secs(418)));
    }

    #[test]
    fn paused_and_untitled_players_are_passed_over() {
        let paused = properties("Paused", vec![("xesam:title", Value::from("Anything"))]);
        assert!(playing_from(&paused).is_none());
        let untitled = properties("Playing", vec![("xesam:title", Value::from(""))]);
        assert!(playing_from(&untitled).is_none());
    }

    #[test]
    fn the_position_moves_along_between_readings() {
        let all = properties(
            "Playing",
            vec![
                ("xesam:title", Value::from("Adagio for Strings")),
                ("mpris:length", Value::from(100_000_000_i64)),
            ],
        );
        let playing = playing_from(&all).unwrap();
        assert_eq!(
            playing.advanced_by(Duration::from_millis(1500)).position,
            Duration::from_millis(98_500)
        );
        // Held at the end rather than running past it.
        assert_eq!(
            playing.advanced_by(Duration::from_secs(10)).position,
            Duration::from_secs(100)
        );
        let endless = NowPlaying {
            length: None,
            ..playing
        };
        assert_eq!(
            endless.advanced_by(Duration::from_secs(10)).position,
            Duration::from_secs(107)
        );
    }

    #[test]
    fn a_frozen_player_is_left_alone_for_a_while() {
        let mut frozen = Frozen::default();
        assert!(!frozen.holds("org.mpris.MediaPlayer2.spotify"));
        frozen.add("org.mpris.MediaPlayer2.spotify".to_owned());
        assert!(frozen.holds("org.mpris.MediaPlayer2.spotify"));
        assert!(!frozen.holds("org.mpris.MediaPlayer2.vlc"));
        // Long enough ago to be worth another try.
        let long_ago = Instant::now()
            .checked_sub(FROZEN_FOR + Duration::from_secs(1))
            .unwrap();
        frozen
            .0
            .insert("org.mpris.MediaPlayer2.spotify".to_owned(), long_ago);
        assert!(!frozen.holds("org.mpris.MediaPlayer2.spotify"));
        assert!(frozen.0.is_empty());
    }
}

//! Whether anyone is looking: the monitors' power state and the session lock.
//!
//! The monitors are read from sysfs on every frame, which costs nothing. The
//! lock comes from logind over D-Bus, which a thread of its own asks about
//! once a second and whenever the session announces a change; the drawing
//! loop reads only what that thread last published.

use std::env;
use std::fs;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use log::debug;
use zbus::MatchRule;
use zbus::blocking::proxy::Builder as ProxyBuilder;
use zbus::blocking::{Connection, Proxy};
use zbus::message::Type;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;

use crate::background::{Background, Publisher};
use crate::bus;
use crate::complaint::Complaint;

/// Connectors live here, one directory per output, `enabled` telling which
/// ones drive a monitor and `dpms` whether that monitor is powered.
const CONNECTORS: &str = "/sys/class/drm";

const LOGIND: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const GRAPHICAL_TYPES: [&str; 2] = ["wayland", "x11"];
/// A service can start before anyone logs in, so a missing session is worth
/// coming back to rather than giving up on.
const RETRY: Duration = Duration::from_secs(30);
/// logind announces the lock, but the hint is read again this often anyway,
/// in case an announcement was missed.
const POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenState {
    /// Monitors on and session unlocked: the dashboard is worth drawing.
    Awake,
    /// Every monitor is powered down.
    Asleep,
    /// The session is locked, whether or not the monitors are still on.
    Locked,
}

impl ScreenState {
    /// Two words for a log line or `thermaltaked info`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Awake => "awake",
            Self::Asleep => "monitors off",
            Self::Locked => "session locked",
        }
    }
}

/// True when monitors are attached and all of them are powered down.
fn monitors_asleep(connectors: &Path) -> bool {
    let mut attached = 0;
    let mut asleep = 0;
    for entry in fs::read_dir(connectors).into_iter().flatten().flatten() {
        let connector = entry.path();
        let read = |file: &str| {
            Some(
                fs::read_to_string(connector.join(file))
                    .ok()?
                    .trim()
                    .to_owned(),
            )
        };
        if read("enabled").as_deref() != Some("enabled") {
            continue;
        }
        attached += 1;
        if read("dpms").as_deref() == Some("Off") {
            asleep += 1;
        }
    }
    attached > 0 && attached == asleep
}

/// Reads the state the dashboard reacts to. Built once, then asked on every
/// frame: the monitors come from sysfs, the lock from the logind thread.
pub struct Screens {
    locked: Background<bool>,
}

fn graphical_session(connection: &Connection) -> zbus::Result<OwnedObjectPath> {
    let manager = Proxy::new(connection, LOGIND, MANAGER_PATH, MANAGER)?;
    if let Ok(id) = env::var("XDG_SESSION_ID")
        && let Ok(path) = manager.call::<_, _, OwnedObjectPath>("GetSession", &(id.as_str()))
    {
        return Ok(path);
    }
    // No session id to go by, so take this user's first graphical session.
    let uid = rustix::process::getuid().as_raw();
    let sessions: Vec<(String, u32, String, String, OwnedObjectPath)> =
        manager.call("ListSessions", &())?;
    sessions
        .into_iter()
        .filter(|session| session.1 == uid)
        .find(|session| {
            Proxy::new(connection, LOGIND, session.4.clone(), SESSION)
                .and_then(|session| session.get_property::<String>("Type"))
                .is_ok_and(|kind| GRAPHICAL_TYPES.contains(&kind.as_str()))
        })
        .map(|session| session.4)
        .ok_or_else(|| zbus::Error::Failure("no graphical session".to_owned()))
}

impl Screens {
    /// Starts the thread that follows the session lock.
    #[must_use]
    pub fn new() -> Self {
        Self {
            locked: Background::start("logind", false, |publisher| watch(&publisher)),
        }
    }

    /// Waits for the thread's first word on the lock, or `timeout`. Returns
    /// whether it came: until it does, the session passes for unlocked.
    #[must_use]
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        self.locked.wait_published(timeout)
    }

    #[must_use]
    pub fn state(&self) -> ScreenState {
        if self.locked.latest() {
            ScreenState::Locked
        } else if monitors_asleep(Path::new(CONNECTORS)) {
            ScreenState::Asleep
        } else {
            ScreenState::Awake
        }
    }
}

impl Default for Screens {
    fn default() -> Self {
        Self::new()
    }
}

/// `PropertiesChanged` on the session object: the sender is left out of the
/// rule, since a well-known name there would never match the unique name
/// the signal carries.
fn changes_rule(session: &OwnedObjectPath) -> zbus::Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(Type::Signal)
        .interface(PROPERTIES)?
        .member("PropertiesChanged")?
        .path(session.clone())?
        .build())
}

/// The thread: finds the session, then reads `LockedHint` on every nudge and
/// at least every [`POLL`], until logind or the bus fails, when it starts
/// over. Until it finds the session the lock goes unnoticed, which is worth
/// one line in the log and no more: the monitors still say most of it.
fn watch(publisher: &Publisher<bool>) {
    let mut complaint = Complaint::default();
    while !publisher.abandoned() {
        let connected = bus::system().and_then(|connection| {
            let path = graphical_session(&connection)?;
            let nudges = bus::nudges(&connection, changes_rule(&path)?, "logind-signals")?;
            // Not cached: each read asks logind, and a stale hint is worse
            // than a round trip a second.
            let session: Proxy<'static> = ProxyBuilder::new(&connection)
                .destination(LOGIND)?
                .path(path)?
                .interface(SESSION)?
                .cache_properties(CacheProperties::No)
                .build()?;
            Ok((session, nudges))
        });
        let (session, nudges) = match connected {
            Ok(connected) => connected,
            Err(error) => {
                complaint.raise(format!("lock state unavailable: {error}"));
                thread::sleep(RETRY);
                continue;
            }
        };
        complaint.withdraw("lock state readable again");
        let error = follow(&session, &nudges, publisher);
        if publisher.abandoned() {
            return;
        }
        // Unlocked until logind says otherwise, as at the start.
        publisher.publish(false);
        complaint.raise(format!("lock state lost: {error}"));
        thread::sleep(RETRY);
    }
}

/// Reads the hint until the session or the bus fails, and says how.
fn follow(session: &Proxy<'_>, nudges: &Receiver<()>, publisher: &Publisher<bool>) -> zbus::Error {
    loop {
        match session.get_property::<bool>("LockedHint") {
            Ok(locked) => {
                if locked != publisher.latest() {
                    debug!(
                        "logind: session {}",
                        if locked { "locked" } else { "unlocked" }
                    );
                }
                publisher.publish(locked);
            }
            Err(error) => return error,
        }
        if publisher.abandoned() {
            return zbus::Error::Failure("nobody reads the lock state anymore".to_owned());
        }
        match nudges.recv_timeout(POLL) {
            Ok(()) => while nudges.try_recv().is_ok() {},
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return zbus::Error::Failure("the signal stream ended".to_owned());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `/sys/class/drm` lookalike: one directory per connector,
    /// holding the two files the state is read from. Each case gets its own
    /// root, since the tests run side by side.
    fn connectors(outputs: &[(&str, &str, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for (name, enabled, dpms) in outputs {
            let connector = root.path().join(name);
            fs::create_dir_all(&connector).unwrap();
            fs::write(connector.join("enabled"), format!("{enabled}\n")).unwrap();
            fs::write(connector.join("dpms"), format!("{dpms}\n")).unwrap();
        }
        root
    }

    #[test]
    fn asleep_only_when_every_monitor_is_off() {
        let both_off = connectors(&[
            ("card1-DP-2", "enabled", "Off"),
            ("card1-DP-3", "enabled", "Off"),
            ("card1-HDMI-A-1", "disabled", "Off"),
        ]);
        assert!(monitors_asleep(both_off.path()));

        let one_on = connectors(&[
            ("card1-DP-2", "enabled", "Off"),
            ("card1-DP-3", "enabled", "On"),
        ]);
        assert!(!monitors_asleep(one_on.path()));
    }

    #[test]
    fn no_monitor_at_all_is_not_sleep() {
        let unplugged = connectors(&[("card1-DP-1", "disabled", "Off")]);
        assert!(!monitors_asleep(unplugged.path()));
        assert!(!monitors_asleep(Path::new("/nonexistent")));
    }

    #[test]
    fn the_session_rule_names_its_path_and_not_its_sender() {
        let path = OwnedObjectPath::try_from("/org/freedesktop/login1/session/_32").unwrap();
        let rule = changes_rule(&path).unwrap();
        assert!(rule.sender().is_none());
        assert_eq!(
            rule.member().map(ToString::to_string).as_deref(),
            Some("PropertiesChanged")
        );
        assert_eq!(rule.msg_type(), Some(Type::Signal));
    }
}

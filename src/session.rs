//! Whether anyone is looking: the monitors' power state and the session lock.

use std::env;
use std::fs;
use std::path::Path;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

/// Connectors live here, one directory per output, `enabled` telling which
/// ones drive a monitor and `dpms` whether that monitor is powered.
const CONNECTORS: &str = "/sys/class/drm";

const LOGIND: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";
const GRAPHICAL_TYPES: [&str; 2] = ["wayland", "x11"];

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
/// frame: the monitors come from sysfs, the lock from logind over D-Bus.
pub struct Screens {
    session: Option<Proxy<'static>>,
}

fn graphical_session(connection: &Connection) -> zbus::Result<OwnedObjectPath> {
    let manager = Proxy::new(connection, LOGIND, MANAGER_PATH, MANAGER)?;
    if let Ok(id) = env::var("XDG_SESSION_ID")
        && let Ok(path) = manager.call::<_, _, OwnedObjectPath>("GetSession", &(id.as_str()))
    {
        return Ok(path);
    }
    // No session id to go by, so take this user's first graphical session.
    let uid = unsafe { libc::getuid() };
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
    /// Without a reachable logind the lock goes unnoticed, which is worth one
    /// line on stderr and no more: the monitors still say most of it.
    #[must_use]
    pub fn new() -> Self {
        let session = Connection::system()
            .and_then(|connection| {
                let path = graphical_session(&connection)?;
                Proxy::new(&connection, LOGIND, path, SESSION)
            })
            .inspect_err(|error| eprintln!("lock state unavailable: {error}"))
            .ok();
        Self { session }
    }

    fn locked(&self) -> bool {
        self.session
            .as_ref()
            .and_then(|session| session.get_property::<bool>("LockedHint").ok())
            .unwrap_or(false)
    }

    #[must_use]
    pub fn state(&self) -> ScreenState {
        if self.locked() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `/sys/class/drm` lookalike: one directory per connector,
    /// holding the two files the state is read from. Each case gets its own
    /// root, since the tests run side by side.
    fn connectors(case: &str, outputs: &[(&str, &str, &str)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("thermaltaked-drm-{case}"));
        let _ = fs::remove_dir_all(&root);
        for (name, enabled, dpms) in outputs {
            let connector = root.join(name);
            fs::create_dir_all(&connector).unwrap();
            fs::write(connector.join("enabled"), format!("{enabled}\n")).unwrap();
            fs::write(connector.join("dpms"), format!("{dpms}\n")).unwrap();
        }
        root
    }

    #[test]
    fn asleep_only_when_every_monitor_is_off() {
        let both_off = connectors(
            "both-off",
            &[
                ("card1-DP-2", "enabled", "Off"),
                ("card1-DP-3", "enabled", "Off"),
                ("card1-HDMI-A-1", "disabled", "Off"),
            ],
        );
        assert!(monitors_asleep(&both_off));

        let one_on = connectors(
            "one-on",
            &[
                ("card1-DP-2", "enabled", "Off"),
                ("card1-DP-3", "enabled", "On"),
            ],
        );
        assert!(!monitors_asleep(&one_on));
    }

    #[test]
    fn no_monitor_at_all_is_not_sleep() {
        let unplugged = connectors("unplugged", &[("card1-DP-1", "disabled", "Off")]);
        assert!(!monitors_asleep(&unplugged));
        assert!(!monitors_asleep(Path::new("/nonexistent")));
    }
}

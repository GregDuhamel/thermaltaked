//! What the D-Bus threads share: bounded calls, a nudge whenever a signal
//! arrives, and telling a peer that will not answer from a bus that is gone.

use std::io::ErrorKind;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use log::debug;
use zbus::MatchRule;
use zbus::blocking::connection::Builder;
use zbus::blocking::{Connection, MessageIterator};

/// How long a call waits for its answer. A player stopped with `SIGSTOP`, or
/// a browser tab that froze, keeps its name on the bus and answers nothing:
/// without this, the thread asking would wait on it for good.
pub const METHOD_TIMEOUT: Duration = Duration::from_secs(2);

/// A session bus connection whose calls give up after [`METHOD_TIMEOUT`].
///
/// # Errors
///
/// When there is no session bus to reach, or the handshake fails.
pub fn session() -> zbus::Result<Connection> {
    Builder::session()?.method_timeout(METHOD_TIMEOUT).build()
}

/// A system bus connection whose calls give up after [`METHOD_TIMEOUT`].
///
/// # Errors
///
/// When there is no system bus to reach, or the handshake fails.
pub fn system() -> zbus::Result<Connection> {
    Builder::system()?.method_timeout(METHOD_TIMEOUT).build()
}

/// Whether a call failed because its peer never answered in time. The bus
/// itself is fine, and the peer is the one to leave alone for a while.
#[must_use]
pub fn timed_out(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::InputOutput(io) if io.kind() == ErrorKind::TimedOut)
}

/// Whether the connection itself is gone, which is worth opening a new one,
/// rather than one peer having failed.
#[must_use]
pub fn lost(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::InputOutput(io) if io.kind() != ErrorKind::TimedOut)
}

/// Turns every message matching `rule` into a nudge on the channel returned,
/// from a thread called `name`. The channel closes when the connection ends.
///
/// A thread blocked on the bus cannot also keep a deadline, so the signals
/// are listened for apart from the thread that acts on them, which can then
/// wait for a nudge with a timeout and poll when none comes.
///
/// # Errors
///
/// When the bus refuses the match rule.
pub fn nudges(
    connection: &Connection,
    rule: MatchRule<'_>,
    name: &str,
) -> zbus::Result<Receiver<()>> {
    let iterator = MessageIterator::for_match_rule(rule, connection, None)?;
    let (nudge, nudged) = mpsc::channel();
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            for message in iterator {
                // An error ends the stream with the connection; a nudge nobody
                // takes means the thread acting on them has moved on.
                if message.is_err() || nudge.send(()).is_err() {
                    break;
                }
            }
            debug!("signal listener ended");
        })
        .map_err(|error| zbus::Error::Failure(format!("starting the {name} thread: {error}")))?;
    Ok(nudged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn io_error(kind: ErrorKind) -> zbus::Error {
        zbus::Error::InputOutput(Arc::new(std::io::Error::from(kind)))
    }

    #[test]
    fn a_timeout_is_the_peer_and_not_the_bus() {
        let timeout = io_error(ErrorKind::TimedOut);
        assert!(timed_out(&timeout));
        assert!(!lost(&timeout));
        let broken = io_error(ErrorKind::BrokenPipe);
        assert!(!timed_out(&broken));
        assert!(lost(&broken));
        let refused = zbus::Error::Failure("no such name".to_owned());
        assert!(!timed_out(&refused));
        assert!(!lost(&refused));
    }
}

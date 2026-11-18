//! What the D-Bus threads share: bounded calls, a nudge whenever a signal
//! arrives, and telling a peer that will not answer from a bus that is gone.
//!
//! zbus 6 has no blocking API: every call is a future, and a thread runs it
//! with [`block_on`], which drives the connections built on that thread while
//! it waits. Between two calls, while the thread sleeps on its channel, a
//! helper thread of zbus's runtime keeps the connection's socket read. No
//! executor is set up anywhere: each thread is its own.

use std::future::{Future, poll_fn};
use std::io::ErrorKind;
use std::pin::Pin;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use futures_core::Stream;
use log::debug;
use zbus::connection::Builder;
use zbus::{Connection, MatchRule, MessageStream};

/// How long a call waits for its answer. A player stopped with `SIGSTOP`, or
/// a browser tab that froze, keeps its name on the bus and answers nothing:
/// without this, the thread asking would wait on it for good.
pub const METHOD_TIMEOUT: Duration = Duration::from_secs(2);

/// Runs `future` to completion on the calling thread, driving the connections
/// built on it meanwhile. Each D-Bus thread wraps its calls in this, one at
/// a time; nothing else runs the future, so the thread may sleep or wait on
/// a channel between two calls without holding anything up.
///
/// Not for a thread already inside it: zbus panics there, since the inner
/// call would wait on the very thread it runs on.
pub fn block_on<F: Future>(future: F) -> F::Output {
    zbus::block_on(future)
}

/// A session bus connection whose calls give up after [`METHOD_TIMEOUT`].
///
/// # Errors
///
/// When there is no session bus to reach, or the handshake fails.
pub fn session() -> zbus::Result<Connection> {
    block_on(Builder::session().method_timeout(METHOD_TIMEOUT).build())
}

/// A system bus connection whose calls give up after [`METHOD_TIMEOUT`].
///
/// # Errors
///
/// When there is no system bus to reach, or the handshake fails.
pub fn system() -> zbus::Result<Connection> {
    block_on(Builder::system().method_timeout(METHOD_TIMEOUT).build())
}

/// Closes `connection` for every holder of it, which ends its message
/// streams and so the listener thread reading one. A connection the bus
/// already dropped has nothing left to close, and says so only in the debug
/// log.
pub fn close(connection: Connection) {
    if let Err(error) = block_on(connection.close()) {
        debug!("closing the connection: {error}");
    }
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

/// The next item of `stream`, or `None` once it has ended. What
/// `StreamExt::next` would be, without the crate it comes from.
async fn next<S: Stream + Unpin>(stream: &mut S) -> Option<S::Item> {
    poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

/// Turns every message matching `rule` into a nudge on the channel returned,
/// from a thread called `name`. The channel closes when the stream ends,
/// which it does with the connection: when the bus drops it, or when the
/// thread acting on the nudges is done with it and calls [`close`].
///
/// A thread waiting for a message cannot also keep a deadline, so the
/// signals are listened for apart from the thread that acts on them, which
/// can then wait for a nudge with a timeout and poll when none comes.
///
/// # Errors
///
/// When the bus refuses the match rule, or does not answer in time.
pub fn nudges(
    connection: &Connection,
    rule: MatchRule<'_>,
    name: &str,
) -> zbus::Result<Receiver<()>> {
    let mut stream = block_on(MessageStream::for_match_rule(rule, connection, None))?;
    let (nudge, nudged) = mpsc::channel();
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            block_on(async {
                // An error ends the stream with the connection; a nudge nobody
                // takes means the thread acting on them has moved on.
                while let Some(Ok(_)) = next(&mut stream).await {
                    if nudge.send(()).is_err() {
                        break;
                    }
                }
            });
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

    /// A stream of what it is given, one item per poll, then the end.
    struct Items(Vec<u8>);

    impl Stream for Items {
        type Item = u8;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<u8>> {
            std::task::Poll::Ready(self.0.pop())
        }
    }

    #[test]
    fn a_stream_is_taken_item_by_item_to_its_end() {
        let mut items = Items(vec![3, 2, 1]);
        let taken = block_on(async {
            let mut taken = Vec::new();
            while let Some(item) = next(&mut items).await {
                taken.push(item);
            }
            taken
        });
        assert_eq!(taken, [1, 2, 3]);
    }
}

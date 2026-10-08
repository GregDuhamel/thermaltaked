//! A value kept up to date by a thread of its own, read without ever waiting
//! on that thread.
//!
//! The drawing loop must never block on anything slower than a sysfs read:
//! left a few seconds without a frame, the panel drops to its own screen. So
//! whatever comes from the network or from D-Bus is fetched by a background
//! thread, which publishes its latest result here, and the loop reads only
//! that. A thread that stalls leaves the previous value in place, which is as
//! good as it gets until it answers again.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

struct Slot<T> {
    value: T,
    /// Whether anything has been published since the initial value.
    published: bool,
}

struct Shared<T> {
    slot: Mutex<Slot<T>>,
    /// Signalled on the first publication, for readers that would rather
    /// wait a moment than draw the initial value.
    first: Condvar,
}

impl<T> Shared<T> {
    fn lock(&self) -> MutexGuard<'_, Slot<T>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The reader's end: the latest value the thread published.
pub struct Background<T> {
    shared: Arc<Shared<T>>,
}

/// The thread's end: where it puts each new value.
pub struct Publisher<T> {
    shared: Arc<Shared<T>>,
}

impl<T: Clone + Send + 'static> Background<T> {
    /// Starts a thread called `name` running `work`, and hands back the end
    /// it publishes to. `initial` is what readers see until it does.
    ///
    /// The thread should return once [`Publisher::abandoned`] says so; it is
    /// never joined, so one blocked in a call ends with the process instead.
    ///
    /// # Panics
    ///
    /// When the operating system refuses to start a thread.
    #[must_use]
    pub fn start(name: &str, initial: T, work: impl FnOnce(Publisher<T>) + Send + 'static) -> Self {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot {
                value: initial,
                published: false,
            }),
            first: Condvar::new(),
        });
        let publisher = Publisher {
            shared: Arc::clone(&shared),
        };
        thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || work(publisher))
            .unwrap_or_else(|error| panic!("starting the {name} thread: {error}"));
        Self { shared }
    }

    /// The latest value, cloned out from under the lock so the thread is
    /// never held up by a reader either.
    #[must_use]
    pub fn latest(&self) -> T {
        self.shared.lock().value.clone()
    }

    /// Waits until the thread has published once, or `timeout` has passed.
    /// Returns whether it has.
    #[must_use]
    pub fn wait_published(&self, timeout: Duration) -> bool {
        let slot = self.shared.lock();
        let (slot, _) = self
            .shared
            .first
            .wait_timeout_while(slot, timeout, |slot| !slot.published)
            .unwrap_or_else(PoisonError::into_inner);
        slot.published
    }
}

impl<T> Publisher<T> {
    /// Makes `value` what readers see from now on.
    pub fn publish(&self, value: T) {
        let mut slot = self.shared.lock();
        slot.value = value;
        slot.published = true;
        drop(slot);
        self.shared.first.notify_all();
    }

    /// True once the [`Background`] end is gone: nothing reads anymore, so
    /// the thread may as well return.
    #[must_use]
    pub fn abandoned(&self) -> bool {
        Arc::strong_count(&self.shared) == 1
    }
}

impl<T: Clone> Publisher<T> {
    /// What readers currently see.
    #[must_use]
    pub fn latest(&self) -> T {
        self.shared.lock().value.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn readers_see_the_initial_value_until_something_is_published() {
        let (release, released) = mpsc::channel::<()>();
        let background = Background::start("test", 0, move |publisher| {
            released.recv().unwrap();
            publisher.publish(42);
        });
        assert_eq!(background.latest(), 0);
        assert!(!background.wait_published(Duration::from_millis(10)));
        release.send(()).unwrap();
        assert!(background.wait_published(Duration::from_secs(5)));
        assert_eq!(background.latest(), 42);
    }

    #[test]
    fn the_thread_learns_when_nobody_reads_anymore() {
        let (report, reported) = mpsc::channel();
        let background = Background::start("test", (), move |publisher| {
            while !publisher.abandoned() {
                thread::sleep(Duration::from_millis(5));
            }
            report.send(()).unwrap();
        });
        drop(background);
        assert!(reported.recv_timeout(Duration::from_secs(5)).is_ok());
    }
}

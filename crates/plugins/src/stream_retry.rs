//! Refresh-authorized reconnects, not an idle-stream watchdog. Each failed
//! attempt waits for a successful state refresh before a retry is scheduled.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::glib;

const FIRST_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Default)]
struct Backoff {
    failures: u32,
    deadline: Option<Instant>,
}

impl Backoff {
    fn failed(&mut self, now: Instant) {
        let factor = 1_u32 << self.failures.min(5);
        self.deadline = Some(now + (FIRST_DELAY * factor).min(MAX_DELAY));
        self.failures = self.failures.saturating_add(1);
    }

    fn delay(&self, now: Instant) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn begin_attempt(&mut self) {
        // Keep the failure count until a valid frame proves recovery. Merely
        // starting a subprocess (or getting an unchanged snapshot) is not success.
        self.deadline = None;
    }
}

#[derive(Default)]
struct State {
    backoff: Cell<Backoff>,
    timer: RefCell<Option<glib::SourceId>>,
}

impl State {
    fn cancel(&self) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Default)]
pub(crate) struct StreamRetry(Rc<State>);

impl StreamRetry {
    /// Stop a pending reconnect on unmap, destroy, backend error, or shutdown.
    pub(crate) fn cancel(&self) {
        self.0.cancel();
    }

    /// A new mapped session, changed stream, or valid frame starts a new budget.
    pub(crate) fn reset(&self) {
        self.cancel();
        self.0.backoff.set(Backoff::default());
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> bool {
        self.0.timer.borrow().is_some()
    }

    pub(crate) fn failed(&self) {
        self.cancel();
        let mut backoff = self.0.backoff.get();
        backoff.failed(Instant::now());
        self.0.backoff.set(backoff);
    }

    /// Called only by a mapped view after a successful refresh (or map). At
    /// most one timer is armed, and repeated refreshes cannot bypass backoff.
    /// A failed retry needs another successful refresh; it never loops alone.
    pub(crate) fn ready(&self, retry: impl FnOnce() + 'static) -> bool {
        let Some(delay) = self.0.backoff.get().delay(Instant::now()) else {
            return true;
        };
        if self.0.timer.borrow().is_none() {
            let weak = Rc::downgrade(&self.0);
            *self.0.timer.borrow_mut() = Some(glib::timeout_add_local_once(delay, move || {
                let Some(state) = weak.upgrade() else { return };
                state.timer.borrow_mut().take();
                let mut backoff = state.backoff.get();
                backoff.begin_attempt();
                state.backoff.set(backoff);
                retry();
            }));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_stream_recovers_without_resetting_on_unchanged_refresh() {
        let now = Instant::now();
        let mut backoff = Backoff::default();
        backoff.failed(now);
        assert_eq!(backoff.delay(now), Some(FIRST_DELAY));
        // Any number of successful snapshots still sees the same deadline.
        assert_eq!(
            backoff.delay(now + Duration::from_millis(200)),
            Some(Duration::from_millis(800))
        );
        assert_eq!(backoff.delay(now + FIRST_DELAY), Some(Duration::ZERO));
        backoff.begin_attempt();
        assert_eq!(backoff.delay(now + FIRST_DELAY), None);
        backoff.failed(now + FIRST_DELAY);
        assert_eq!(
            backoff.delay(now + FIRST_DELAY),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn repeated_failures_have_bounded_backoff_and_frames_reset_it() {
        let now = Instant::now();
        let mut backoff = Backoff::default();
        for expected in [1, 2, 4, 8, 16, 30, 30, 30] {
            backoff.failed(now);
            assert_eq!(backoff.delay(now), Some(Duration::from_secs(expected)));
            backoff.begin_attempt();
        }
        backoff.failures = u32::MAX;
        backoff.failed(now);
        assert_eq!(backoff.delay(now), Some(MAX_DELAY));
        backoff = Backoff::default();
        assert_eq!(backoff.delay(now), None);
        backoff.failed(now);
        assert_eq!(backoff.delay(now), Some(FIRST_DELAY));
    }

    #[test]
    fn healthy_idle_stream_has_no_retry_deadline() {
        let backoff = Backoff::default();
        let now = Instant::now();
        assert_eq!(backoff.delay(now), None);
        assert_eq!(backoff.delay(now + Duration::from_secs(86400)), None);
    }
}

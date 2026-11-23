//! When the record is written: a change only marks it dirty, and a dirty record is written at most
//! every [`INTERVAL`]. Quitting and an in-app update write it at once.
use std::time::{Duration, Instant};

/// The shortest time between two writes.
pub(crate) const INTERVAL: Duration = Duration::from_secs(5);

/// The state of saving, one for the whole app.
#[derive(Debug)]
pub(crate) struct Saver {
    /// The setting is on and this is a real run (not a demo scene, not a test).
    enabled: bool,
    /// A launch is rebuilding the windows from the record; what is on screen is not yet the session.
    restoring: bool,
    /// The app is quitting and the final record was written.
    frozen: bool,
    dirty: bool,
    last: Option<Instant>,
}

impl Saver {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled: enabled && !cfg!(test),
            restoring: false,
            frozen: false,
            dirty: false,
            last: None,
        }
    }

    /// Saving and restoring are in use. A demo scene or a test is never.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// The setting changed. Turning it off drops what was waiting to be written.
    pub(crate) fn set_enabled(&mut self, on: bool) {
        self.enabled = on && !cfg!(test);
        // Turned on: the record of the last run may be stale or gone, so write one soon.
        self.dirty = self.enabled;
    }

    pub(crate) fn restoring(&self) -> bool {
        self.restoring
    }

    pub(crate) fn begin_restore(&mut self) {
        self.restoring = true;
    }

    /// The windows are rebuilt: what is on screen is the session again, and worth writing.
    pub(crate) fn end_restore(&mut self) {
        self.restoring = false;
        self.mark();
    }

    /// Nothing is written from now on (the final record was written).
    pub(crate) fn freeze(&mut self) {
        self.frozen = true;
    }

    fn allowed(&self) -> bool {
        self.enabled && !self.restoring && !self.frozen
    }

    /// Something worth saving changed.
    pub(crate) fn mark(&mut self) {
        if self.allowed() {
            self.dirty = true;
        }
    }

    /// Whether the record should be written now.
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.allowed()
            && self.dirty
            && self
                .last
                .is_none_or(|at| now.saturating_duration_since(at) >= INTERVAL)
    }

    /// Whether a write that must happen now (quit, update) may: whatever the interval or the dirty flag.
    pub(crate) fn may_write_now(&self) -> bool {
        self.allowed()
    }

    pub(crate) fn written(&mut self, now: Instant) {
        self.dirty = false;
        self.last = Some(now);
    }

    /// The write failed: it stays dirty and is tried again after the interval.
    pub(crate) fn failed(&mut self, now: Instant) {
        self.last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Saver::new` never enables itself in tests (that is the point), so these build one by hand.
    fn on() -> Saver {
        Saver {
            enabled: true,
            restoring: false,
            frozen: false,
            dirty: false,
            last: None,
        }
    }

    fn at(seconds: u64) -> Instant {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *START.get_or_init(Instant::now) + Duration::from_secs(seconds)
    }

    #[test]
    fn nothing_is_written_until_something_changed() {
        let saver = on();
        assert!(!saver.due(at(0)) && !saver.due(at(100)));
    }

    #[test]
    fn the_first_change_is_written_at_the_next_look_and_then_at_most_every_five_seconds() {
        let mut saver = on();
        saver.mark();
        assert!(saver.due(at(1)));
        saver.written(at(1));
        assert!(!saver.due(at(2)), "clean again");
        saver.mark();
        for seconds in 2..6 {
            assert!(
                !saver.due(at(seconds)),
                "{seconds}s: written {}s ago",
                seconds - 1
            );
        }
        assert!(saver.due(at(6)));
        // A burst of changes is one write.
        for _ in 0..50 {
            saver.mark();
        }
        saver.written(at(6));
        assert!(!saver.due(at(7)));
    }

    #[test]
    fn a_failed_write_is_tried_again_after_the_interval_not_at_every_look() {
        let mut saver = on();
        saver.mark();
        saver.failed(at(10));
        assert!(!saver.due(at(11)));
        assert!(saver.due(at(15)), "still dirty");
        saver.written(at(15));
        assert!(!saver.due(at(30)));
    }

    #[test]
    fn nothing_is_marked_or_written_while_restoring_or_when_off_or_after_the_final_write() {
        let mut saver = on();
        saver.begin_restore();
        assert!(saver.restoring());
        saver.mark();
        assert!(!saver.due(at(100)) && !saver.may_write_now());
        saver.end_restore();
        assert!(!saver.restoring());
        assert!(
            saver.due(at(100)),
            "what the restore built is written once it is done"
        );

        let mut off = Saver::new(true);
        assert!(!off.enabled(), "a test never saves");
        off.mark();
        assert!(!off.due(at(100)) && !off.may_write_now());

        let mut saver = on();
        saver.mark();
        saver.freeze();
        assert!(!saver.due(at(100)) && !saver.may_write_now());
    }

    #[test]
    fn turning_the_setting_off_forgets_what_was_waiting_and_on_asks_for_a_write() {
        let mut saver = on();
        saver.mark();
        saver.set_enabled(false);
        assert!(!saver.enabled() && !saver.due(at(100)) && !saver.may_write_now());
        saver.mark();
        assert!(!saver.due(at(100)));
        // `set_enabled` itself keeps tests from saving, so turn it on by hand.
        saver.enabled = true;
        saver.mark();
        assert!(saver.due(at(100)));
    }

    #[test]
    fn a_write_at_quit_does_not_wait_for_the_interval() {
        let mut saver = on();
        saver.mark();
        saver.written(at(1));
        assert!(!saver.due(at(2)) && saver.may_write_now());
    }
}

//! How the queue tells a running item to stop.

use std::sync::atomic::{AtomicU8, Ordering};

use tokio::sync::Notify;

use super::PauseReason;

/// Why a running item stops early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Interrupt {
    /// Keep the temporary file and stop.
    Pause(PauseReason),
    /// Remove the temporary file and stop.
    Cancel,
}

const RUN: u8 = 0;
const PAUSE_USER: u8 = 1;
const PAUSE_DISCONNECTED: u8 = 2;
const CANCEL: u8 = 3;

/// Shared between the queue and the task running an item.
pub(crate) struct Control {
    signal: AtomicU8,
    changed: Notify,
}

impl Control {
    pub fn new() -> Self {
        Self { signal: AtomicU8::new(RUN), changed: Notify::new() }
    }

    /// What the task has been asked to do, if anything.
    pub fn interrupted(&self) -> Option<Interrupt> {
        match self.signal.load(Ordering::Acquire) {
            PAUSE_USER => Some(Interrupt::Pause(PauseReason::User)),
            PAUSE_DISCONNECTED => Some(Interrupt::Pause(PauseReason::Disconnected)),
            CANCEL => Some(Interrupt::Cancel),
            _ => None,
        }
    }

    /// Asks the task to stop (or, with `None`, to carry on after all). A cancellation is final.
    pub fn set(&self, interrupt: Option<Interrupt>) {
        let value = match interrupt {
            None => RUN,
            Some(Interrupt::Pause(PauseReason::User)) => PAUSE_USER,
            Some(Interrupt::Pause(PauseReason::Disconnected)) => PAUSE_DISCONNECTED,
            Some(Interrupt::Cancel) => CANCEL,
        };
        let mut current = self.signal.load(Ordering::Acquire);
        while current != CANCEL {
            match self.signal.compare_exchange_weak(current, value, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        self.changed.notify_waiters();
    }

    /// Resolves once the task has been told to cancel.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.signal.load(Ordering::Acquire) == CANCEL {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_signal_can_be_set_and_taken_back_but_a_cancel_is_final() {
        let control = Control::new();
        assert_eq!(control.interrupted(), None);
        control.set(Some(Interrupt::Pause(PauseReason::User)));
        assert_eq!(control.interrupted(), Some(Interrupt::Pause(PauseReason::User)));
        control.set(None);
        assert_eq!(control.interrupted(), None, "resumed before the task noticed");
        control.set(Some(Interrupt::Pause(PauseReason::Disconnected)));
        assert_eq!(control.interrupted(), Some(Interrupt::Pause(PauseReason::Disconnected)));
        control.set(Some(Interrupt::Cancel));
        control.set(None);
        control.set(Some(Interrupt::Pause(PauseReason::User)));
        assert_eq!(control.interrupted(), Some(Interrupt::Cancel));
    }

    #[tokio::test]
    async fn waiting_for_a_cancel_wakes_up_when_it_comes() {
        let control = Arc::new(Control::new());
        let waiting = tokio::spawn({
            let control = control.clone();
            async move { control.cancelled().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        control.set(Some(Interrupt::Pause(PauseReason::User)));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished(), "a pause is not a cancel");
        control.set(Some(Interrupt::Cancel));
        tokio::time::timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap();
        control.cancelled().await;
    }
}

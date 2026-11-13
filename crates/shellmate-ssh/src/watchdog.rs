//! The connect watchdog: a time budget for the non-interactive part of a connection attempt.
//!
//! The clock runs while the attempt is talking to the network (TCP connect, handshake,
//! authentication round trips, every jump hop, opening the shell) and stands still while a
//! [`HostKeyVerifier`](crate::HostKeyVerifier) or [`Prompter`](crate::Prompter) call waits for
//! the user. When the budget is used up the attempt is abandoned with [`SshError::Timeout`].

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::error::SshError;

struct State {
    /// Running time used so far (while the clock was going).
    spent: Duration,
    /// When the clock last started; `None` while it is stopped.
    since: Option<Instant>,
    /// Number of live [`Pause`] guards.
    pauses: usize,
}

struct Inner {
    limit: Duration,
    state: Mutex<State>,
    changed: Notify,
}

/// A shared, pausable time budget.
#[derive(Clone)]
pub(crate) struct Watchdog {
    inner: Arc<Inner>,
}

/// While alive, the watchdog's clock stands still.
pub(crate) struct Pause {
    inner: Arc<Inner>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Watchdog {
    /// A watchdog with `limit` of running time, started now.
    pub fn new(limit: Duration) -> Self {
        let state = State { spent: Duration::ZERO, since: Some(Instant::now()), pauses: 0 };
        Self { inner: Arc::new(Inner { limit, state: Mutex::new(state), changed: Notify::new() }) }
    }

    /// The configured budget.
    pub fn limit(&self) -> Duration {
        self.inner.limit
    }

    /// Stops the clock until the returned guard is dropped (guards nest).
    pub fn pause(&self) -> Pause {
        {
            let mut state = lock(&self.inner.state);
            state.pauses += 1;
            if let Some(since) = state.since.take() {
                state.spent += since.elapsed();
            }
        }
        self.inner.changed.notify_waiters();
        Pause { inner: self.inner.clone() }
    }

    /// Resolves once the whole budget has been used up.
    pub async fn expired(&self) {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let remaining = {
                let state = lock(&self.inner.state);
                state.since.map(|since| {
                    let used = state.spent + since.elapsed();
                    self.inner.limit.saturating_sub(used)
                })
            };
            match remaining {
                Some(left) if left.is_zero() => return,
                Some(left) => {
                    tokio::select! {
                        () = tokio::time::sleep(left) => {}
                        () = &mut changed => {}
                    }
                }
                // Paused: nothing to count down until the clock restarts.
                None => changed.await,
            }
        }
    }

    /// Runs `work`, giving up with [`SshError::Timeout`] when the budget runs out first.
    pub async fn guard<T>(&self, work: impl Future<Output = Result<T, SshError>>) -> Result<T, SshError> {
        tokio::select! {
            outcome = work => outcome,
            () = self.expired() => Err(SshError::Timeout { limit: self.limit() }),
        }
    }
}

impl Drop for Pause {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.inner.state);
            state.pauses -= 1;
            if state.pauses == 0 {
                state.since = Some(Instant::now());
            }
        }
        self.inner.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn within(limit_ms: u64, work: impl Future<Output = ()>) -> Result<(), SshError> {
        let watchdog = Watchdog::new(Duration::from_millis(limit_ms));
        watchdog
            .guard(async {
                work.await;
                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn work_that_finishes_in_time_is_untouched() {
        assert!(within(500, tokio::time::sleep(Duration::from_millis(20))).await.is_ok());
    }

    #[tokio::test]
    async fn work_that_takes_too_long_times_out() {
        let started = Instant::now();
        let err = within(100, tokio::time::sleep(Duration::from_secs(30))).await.unwrap_err();
        assert!(matches!(err, SshError::Timeout { limit } if limit == Duration::from_millis(100)), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_paused_clock_does_not_count() {
        let watchdog = Watchdog::new(Duration::from_millis(150));
        let outcome = watchdog
            .guard(async {
                {
                    let _pause = watchdog.pause();
                    tokio::time::sleep(Duration::from_millis(600)).await;
                }
                Ok(())
            })
            .await;
        assert!(outcome.is_ok(), "waiting for the user is free");
    }

    #[tokio::test]
    async fn running_time_before_and_after_a_pause_adds_up() {
        let watchdog = Watchdog::new(Duration::from_millis(250));
        let outcome = watchdog
            .guard(async {
                tokio::time::sleep(Duration::from_millis(150)).await;
                drop(watchdog.pause());
                {
                    let _pause = watchdog.pause();
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                // 150 ms already used + this stall exceeds the 250 ms budget.
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            })
            .await;
        assert!(matches!(outcome, Err(SshError::Timeout { .. })));
    }

    #[tokio::test]
    async fn pauses_nest() {
        let watchdog = Watchdog::new(Duration::from_millis(100));
        let outcome = watchdog
            .guard(async {
                let outer = watchdog.pause();
                let inner = watchdog.pause();
                drop(inner);
                tokio::time::sleep(Duration::from_millis(400)).await;
                drop(outer);
                Ok(())
            })
            .await;
        assert!(outcome.is_ok(), "the clock only restarts when every guard is gone");
    }

    #[tokio::test]
    async fn a_pause_that_starts_while_the_timer_is_armed_stops_it() {
        // The timer is already counting down when a prompt opens at 60 ms.
        let watchdog = Watchdog::new(Duration::from_millis(120));
        let outcome = watchdog
            .guard(async {
                tokio::time::sleep(Duration::from_millis(60)).await;
                let _pause = watchdog.pause();
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok(())
            })
            .await;
        assert!(outcome.is_ok());
    }
}

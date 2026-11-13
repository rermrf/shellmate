//! Auto-reconnect of an SSH connection as a plain state machine (no UI, no timers, no I/O).
//!
//! One [`Reconnect`] belongs to one connection, however many tabs share it: every tab reports
//! the loss, only the first one starts the loop. The owner feeds [`Input`]s in and carries out
//! the [`Output`]s: start a timer, start an attempt, mark the tabs connected, give up.
//!
//! After an unexpected loss the attempts are 1…5, preceded by waits of 1, 2, 4, 8 and 16 seconds.
//! Enter during a wait retries at once. After the fifth failure the machine gives up; a retry
//! from there starts over with a fresh counter. A first connect is a single attempt without
//! retries; a connection that has been up once reconnects automatically from then on.

use std::time::Duration;

use shellmate_ssh::SshError;

/// Number of automatic attempts after a loss.
pub const MAX_ATTEMPTS: u32 = 5;

/// Wait before attempt 1, 2, 3, 4, 5.
pub const BACKOFF: [Duration; MAX_ATTEMPTS as usize] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];

/// Where the machine stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Nothing is going on: the connection is up (or was never started).
    Idle,
    /// Waiting before attempt `attempt` (1-based).
    Waiting { attempt: u32 },
    /// Attempt `attempt` is running.
    Connecting { attempt: u32 },
    /// Gave up; a retry starts over.
    Failed,
}

/// Why an attempt failed, as far as retrying is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Likely temporary (network, server restarting): try again after the next wait.
    Retry(String),
    /// Retrying cannot help or the user said no (cancelled prompt, rejected credentials).
    Stop(String),
}

/// Sorts an SSH error into [`Failure::Retry`] (network trouble) or [`Failure::Stop`].
pub fn classify(err: &SshError) -> Failure {
    let text = err.to_string();
    match err {
        _ if err.is_cancelled() => Failure::Stop(text),
        SshError::Jump { source, .. } => classify(source),
        SshError::Connect { .. }
        | SshError::ConnectTimeout { .. }
        | SshError::Timeout { .. }
        | SshError::Handshake(_)
        | SshError::Disconnected(_)
        | SshError::Channel(_) => Failure::Retry(text),
        _ => Failure::Stop(text),
    }
}

/// What happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// The connection broke. Reported once per tab on it; only the first counts.
    Lost,
    /// The wait before the current attempt is over.
    TimerElapsed,
    /// Enter or a click on the tab: retry now (during a wait) or start over (after giving up).
    RetryNow,
    /// Start an attempt (from idle or after giving up).
    Start,
    /// The running attempt succeeded.
    Succeeded,
    /// The running attempt failed.
    Failed(Failure),
}

/// What the owner has to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// Start a timer of `delay`, then send [`Input::TimerElapsed`]. Replaces any earlier timer.
    Wait { attempt: u32, delay: Duration },
    /// Start connecting (cancel any timer).
    Attempt { attempt: u32 },
    /// Connected. `reconnected` is true if this healed a lost connection.
    Connected { reconnected: bool },
    /// No more automatic attempts. `attempts` is how many were made in this round.
    GaveUp { reason: String, attempts: u32 },
}

/// How tabs show the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// First connection in progress.
    Connecting,
    /// Connected.
    Connected,
    /// Lost; attempt `n` of 5 is pending or running ("重连 n/5").
    Reconnecting(u32),
    /// Gave up ("连接失败").
    Failed,
}

/// The reconnect loop of one connection.
#[derive(Clone, Debug)]
pub struct Reconnect {
    phase: Phase,
    /// The connection has been up: a loss starts the automatic loop.
    auto: bool,
}

impl Reconnect {
    /// A connection whose first attempt is starting now (no retries if it fails).
    pub fn initial() -> Self {
        Self { phase: Phase::Connecting { attempt: 1 }, auto: false }
    }

    /// A connection that is up.
    #[cfg(test)]
    pub fn connected() -> Self {
        Self { phase: Phase::Idle, auto: true }
    }

    /// The current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether the connection has been up at least once.
    pub fn has_connected(&self) -> bool {
        self.auto
    }

    /// How tabs should present the connection.
    pub fn view(&self) -> View {
        match (self.phase, self.auto) {
            (Phase::Failed, _) => View::Failed,
            (Phase::Idle, true) => View::Connected,
            (_, false) => View::Connecting,
            (Phase::Waiting { attempt } | Phase::Connecting { attempt }, true) => View::Reconnecting(attempt),
        }
    }

    /// Advances the machine.
    pub fn step(&mut self, input: Input) -> Vec<Output> {
        match (self.phase, input) {
            (Phase::Idle, Input::Lost) if self.auto => self.wait(1),
            (Phase::Waiting { attempt }, Input::TimerElapsed | Input::RetryNow) => self.attempt(attempt),
            (Phase::Idle | Phase::Failed, Input::Start) | (Phase::Failed, Input::RetryNow) => self.attempt(1),
            (Phase::Connecting { .. }, Input::Succeeded) => {
                let reconnected = self.auto;
                self.phase = Phase::Idle;
                self.auto = true;
                vec![Output::Connected { reconnected }]
            }
            (Phase::Connecting { attempt }, Input::Failed(Failure::Retry(reason))) => {
                if self.auto && attempt < MAX_ATTEMPTS { self.wait(attempt + 1) } else { self.give_up(reason, attempt) }
            }
            (Phase::Connecting { attempt }, Input::Failed(Failure::Stop(reason))) => self.give_up(reason, attempt),
            // Everything else is stale or redundant: another tab reporting the same loss, a timer
            // that was overtaken by Enter, Enter while an attempt is already running.
            _ => Vec::new(),
        }
    }

    fn wait(&mut self, attempt: u32) -> Vec<Output> {
        self.phase = Phase::Waiting { attempt };
        vec![Output::Wait { attempt, delay: BACKOFF[attempt as usize - 1] }]
    }

    fn attempt(&mut self, attempt: u32) -> Vec<Output> {
        self.phase = Phase::Connecting { attempt };
        vec![Output::Attempt { attempt }]
    }

    fn give_up(&mut self, reason: String, attempts: u32) -> Vec<Output> {
        self.phase = Phase::Failed;
        vec![Output::GaveUp { reason, attempts }]
    }
}

// ---- terminal wording ------------------------------------------------------------------------

/// `1 秒`
pub fn seconds(delay: Duration) -> String {
    format!("{} 秒", delay.as_secs())
}

/// Printed when the connection is lost or an attempt failed and the next one is scheduled.
pub fn waiting_line(attempt: u32, delay: Duration, last_failure: Option<&str>) -> String {
    let why = last_failure.map(|reason| format!("上次失败：{reason} · ")).unwrap_or_default();
    format!("\x1b[33m↻ {}后自动重连（第 {attempt}/{MAX_ATTEMPTS} 次）· {why}按回车立即重试", seconds(delay))
}

/// Printed when an attempt starts.
pub fn attempt_line(attempt: u32) -> String {
    format!("\x1b[33m↻ 正在重连（第 {attempt}/{MAX_ATTEMPTS} 次）")
}

/// Printed on a terminal that got a new shell after a lost connection. `manual`: the user
/// asked for it (Enter after the attempts ran out, or after the remote shell exited).
pub fn healed_line(manual: bool) -> String {
    if manual {
        "\x1b[32m✓ 已重新连接".to_string()
    } else {
        "\x1b[32m✓ 已自动重连\x1b[0m\r\n\x1b[2m  新开了一个 shell 通道，原来运行的程序已被中断；滚动区已保留"
            .to_string()
    }
}

/// What the terminal says when the loop gives up. After a connection that had been up, the
/// failure is announced; a first connect just says why it did not work.
pub fn gave_up_text(reason: &str, attempts: u32, retried: bool) -> String {
    match (retried, attempts) {
        (false, _) => reason.to_string(),
        (true, 1) => format!("连接失败：{reason}"),
        (true, n) => format!("连接失败：重连 {n} 次均未成功（{reason}）"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retry(reason: &str) -> Input {
        Input::Failed(Failure::Retry(reason.into()))
    }

    fn stop(reason: &str) -> Input {
        Input::Failed(Failure::Stop(reason.into()))
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A machine that has been connected and just lost the connection.
    fn lost() -> Reconnect {
        let mut machine = Reconnect::connected();
        assert_eq!(machine.step(Input::Lost), [Output::Wait { attempt: 1, delay: secs(1) }]);
        machine
    }

    // ---- schedule ----

    #[test]
    fn the_backoff_is_1_2_4_8_16_seconds() {
        assert_eq!(BACKOFF, [secs(1), secs(2), secs(4), secs(8), secs(16)]);
    }

    #[test]
    fn five_failures_walk_the_whole_schedule_and_give_up() {
        let mut machine = lost();
        let expected_delays = [2, 4, 8, 16];
        for attempt in 1..=5u32 {
            assert_eq!(machine.phase(), Phase::Waiting { attempt });
            assert_eq!(machine.view(), View::Reconnecting(attempt));
            assert_eq!(machine.step(Input::TimerElapsed), [Output::Attempt { attempt }]);
            assert_eq!(machine.phase(), Phase::Connecting { attempt });
            assert_eq!(machine.view(), View::Reconnecting(attempt), "the badge stays while the attempt runs");
            let out = machine.step(retry("超时"));
            if attempt < 5 {
                let delay = secs(expected_delays[attempt as usize - 1]);
                assert_eq!(out, [Output::Wait { attempt: attempt + 1, delay }]);
            } else {
                assert_eq!(out, [Output::GaveUp { reason: "超时".into(), attempts: 5 }]);
            }
        }
        assert_eq!(machine.phase(), Phase::Failed);
        assert_eq!(machine.view(), View::Failed);
    }

    #[test]
    fn a_success_on_any_attempt_heals_and_resets_the_counter() {
        for succeed_on in 1..=5u32 {
            let mut machine = lost();
            for attempt in 1..succeed_on {
                machine.step(Input::TimerElapsed);
                machine.step(retry("x"));
                assert_eq!(machine.phase(), Phase::Waiting { attempt: attempt + 1 });
            }
            machine.step(Input::TimerElapsed);
            assert_eq!(
                machine.step(Input::Succeeded),
                [Output::Connected { reconnected: true }],
                "attempt {succeed_on}"
            );
            assert_eq!((machine.phase(), machine.view()), (Phase::Idle, View::Connected));

            // The next loss starts from attempt 1 again.
            assert_eq!(machine.step(Input::Lost), [Output::Wait { attempt: 1, delay: secs(1) }]);
        }
    }

    // ---- Enter / retry ----

    #[test]
    fn enter_during_a_wait_retries_immediately() {
        let mut machine = lost();
        assert_eq!(machine.step(Input::RetryNow), [Output::Attempt { attempt: 1 }]);
        assert_eq!(machine.phase(), Phase::Connecting { attempt: 1 });
        // The timer that was running fires anyway: stale, ignored.
        assert_eq!(machine.step(Input::TimerElapsed), []);
        assert_eq!(machine.phase(), Phase::Connecting { attempt: 1 });
    }

    #[test]
    fn enter_keeps_the_attempt_number_of_the_wait_it_cuts_short() {
        let mut machine = lost();
        machine.step(Input::TimerElapsed);
        machine.step(retry("x"));
        machine.step(Input::TimerElapsed);
        machine.step(retry("x"));
        assert_eq!(machine.phase(), Phase::Waiting { attempt: 3 });
        assert_eq!(machine.step(Input::RetryNow), [Output::Attempt { attempt: 3 }]);
    }

    #[test]
    fn enter_while_an_attempt_is_running_does_nothing() {
        let mut machine = lost();
        machine.step(Input::TimerElapsed);
        assert_eq!(machine.step(Input::RetryNow), []);
        assert_eq!(machine.phase(), Phase::Connecting { attempt: 1 });
    }

    #[test]
    fn retrying_after_giving_up_starts_with_a_fresh_counter_and_loops_again() {
        let mut machine = lost();
        for _ in 0..5 {
            machine.step(Input::TimerElapsed);
            machine.step(retry("x"));
        }
        assert_eq!(machine.phase(), Phase::Failed);
        assert_eq!(machine.step(Input::RetryNow), [Output::Attempt { attempt: 1 }]);
        assert_eq!(machine.view(), View::Reconnecting(1), "a connection that was up keeps reconnecting");
        assert_eq!(machine.step(retry("again")), [Output::Wait { attempt: 2, delay: secs(2) }]);
    }

    #[test]
    fn a_retry_that_succeeds_after_giving_up_is_a_reconnect() {
        let mut machine = lost();
        for _ in 0..5 {
            machine.step(Input::TimerElapsed);
            machine.step(retry("x"));
        }
        machine.step(Input::RetryNow);
        assert_eq!(machine.step(Input::Succeeded), [Output::Connected { reconnected: true }]);
    }

    // ---- stopping ----

    #[test]
    fn a_cancelled_or_rejected_attempt_ends_auto_reconnect_at_once() {
        let mut machine = lost();
        machine.step(Input::TimerElapsed);
        assert_eq!(machine.step(stop("已取消认证")), [Output::GaveUp { reason: "已取消认证".into(), attempts: 1 }]);
        assert_eq!((machine.phase(), machine.view()), (Phase::Failed, View::Failed));
        // Late reports of the same loss change nothing.
        assert_eq!(machine.step(Input::Lost), []);
        assert_eq!(machine.step(Input::TimerElapsed), []);
    }

    // ---- one loop per connection ----

    #[test]
    fn every_tab_reports_the_loss_but_only_one_loop_runs() {
        let mut machine = Reconnect::connected();
        assert_eq!(machine.step(Input::Lost).len(), 1, "the first tab starts the loop");
        for _ in 0..4 {
            assert_eq!(machine.step(Input::Lost), [], "the other tabs of the connection do not");
        }
        assert_eq!(machine.phase(), Phase::Waiting { attempt: 1 });
        machine.step(Input::TimerElapsed);
        assert_eq!(machine.step(Input::Lost), [], "not while an attempt runs either");
        machine.step(Input::Succeeded);
        assert_eq!(machine.step(Input::Lost).len(), 1, "a new loss after healing starts a new loop");
    }

    // ---- first connect ----

    #[test]
    fn a_first_connect_that_fails_is_not_retried() {
        let mut machine = Reconnect::initial();
        assert_eq!((machine.phase(), machine.view()), (Phase::Connecting { attempt: 1 }, View::Connecting));
        assert_eq!(machine.step(retry("连接被拒绝")), [Output::GaveUp { reason: "连接被拒绝".into(), attempts: 1 }]);
        assert_eq!(machine.view(), View::Failed);
    }

    #[test]
    fn a_first_connect_that_succeeds_is_not_a_reconnect_but_arms_the_loop() {
        let mut machine = Reconnect::initial();
        assert_eq!(machine.step(Input::Succeeded), [Output::Connected { reconnected: false }]);
        assert_eq!(machine.view(), View::Connected);
        assert_eq!(machine.step(Input::Lost), [Output::Wait { attempt: 1, delay: secs(1) }]);
    }

    #[test]
    fn retrying_a_failed_first_connect_is_another_single_attempt() {
        let mut machine = Reconnect::initial();
        machine.step(retry("x"));
        assert_eq!(machine.step(Input::RetryNow), [Output::Attempt { attempt: 1 }]);
        assert_eq!(machine.view(), View::Connecting, "no 重连 state: it never connected");
        assert_eq!(machine.step(retry("x")), [Output::GaveUp { reason: "x".into(), attempts: 1 }]);
    }

    #[test]
    fn a_loss_before_the_first_connect_finished_does_not_start_the_loop() {
        let mut machine = Reconnect::initial();
        assert_eq!(machine.step(Input::Lost), []);
        assert_eq!(machine.phase(), Phase::Connecting { attempt: 1 });
    }

    #[test]
    fn start_begins_an_attempt_from_idle_or_after_giving_up() {
        let mut connected = Reconnect::connected();
        assert_eq!(connected.step(Input::Start), [Output::Attempt { attempt: 1 }]);
        assert_eq!(connected.step(Input::Start), [], "not while one is running");
        assert_eq!(connected.step(Input::Succeeded), [Output::Connected { reconnected: true }]);
        assert_eq!(connected.step(Input::Start), [Output::Attempt { attempt: 1 }], "again after it ended");
        connected.step(stop("x"));
        assert_eq!(connected.step(Input::Start), [Output::Attempt { attempt: 1 }], "and after giving up");
    }

    // ---- views ----

    #[test]
    fn the_view_follows_the_phase() {
        let mut machine = Reconnect::initial();
        assert_eq!(machine.view(), View::Connecting);
        machine.step(Input::Succeeded);
        assert_eq!(machine.view(), View::Connected);
        machine.step(Input::Lost);
        assert_eq!(machine.view(), View::Reconnecting(1));
        machine.step(Input::TimerElapsed);
        assert_eq!(machine.view(), View::Reconnecting(1));
        machine.step(retry("x"));
        assert_eq!(machine.view(), View::Reconnecting(2));
        machine.step(Input::TimerElapsed);
        machine.step(Input::Succeeded);
        assert_eq!(machine.view(), View::Connected);
    }

    // ---- error classification ----

    #[test]
    fn network_errors_are_retried_and_everything_else_stops() {
        use shellmate_core::AuthMethod;
        let retry_errors = [
            SshError::Connect { host: "h".into(), port: 22, reason: "refused".into() },
            SshError::ConnectTimeout { host: "h".into(), port: 22, secs: 15 },
            SshError::Timeout { limit: secs(20) },
            SshError::Handshake("eof".into()),
            SshError::Disconnected("gone".into()),
            SshError::Channel("no".into()),
        ];
        for err in retry_errors {
            assert!(matches!(classify(&err), Failure::Retry(_)), "{err}");
        }
        let stop_errors = [
            SshError::AuthRejected(AuthMethod::Password),
            SshError::AuthCancelled,
            SshError::HostKeyRejected,
            SshError::HostKeyChanged,
            SshError::NoAuthMethod,
            SshError::Agent("none".into()),
            SshError::Unsupported("x".into()),
        ];
        for err in stop_errors {
            assert!(matches!(classify(&err), Failure::Stop(_)), "{err}");
        }
    }

    #[test]
    fn a_jump_error_is_classified_by_its_cause() {
        let jump = |source| SshError::Jump { host: "bastion".into(), port: 22, source: Box::new(source) };
        assert!(matches!(classify(&jump(SshError::Handshake("x".into()))), Failure::Retry(_)));
        assert!(matches!(classify(&jump(SshError::AuthCancelled)), Failure::Stop(_)));
        let failure = classify(&jump(SshError::HostKeyChanged));
        assert!(matches!(&failure, Failure::Stop(text) if text.starts_with("跳板机 bastion:22")), "{failure:?}");
    }

    #[test]
    fn the_reason_text_is_the_errors_message() {
        let err = SshError::Timeout { limit: secs(20) };
        assert_eq!(classify(&err), Failure::Retry("连接超时（20 秒内没有响应）".into()));
    }

    // ---- wording ----

    #[test]
    fn terminal_wording() {
        assert_eq!(waiting_line(1, secs(1), None), "\x1b[33m↻ 1 秒后自动重连（第 1/5 次）· 按回车立即重试");
        assert_eq!(
            waiting_line(3, secs(4), Some("连接被拒绝")),
            "\x1b[33m↻ 4 秒后自动重连（第 3/5 次）· 上次失败：连接被拒绝 · 按回车立即重试"
        );
        assert_eq!(attempt_line(2), "\x1b[33m↻ 正在重连（第 2/5 次）");
        assert!(healed_line(false).starts_with("\x1b[32m✓ 已自动重连"));
        assert_eq!(healed_line(true), "\x1b[32m✓ 已重新连接");
        assert_eq!(gave_up_text("连接被拒绝", 5, true), "连接失败：重连 5 次均未成功（连接被拒绝）");
        assert_eq!(gave_up_text("已取消认证", 1, true), "连接失败：已取消认证");
        assert_eq!(gave_up_text("无法连接", 1, false), "无法连接");
    }
}

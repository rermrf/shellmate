//! Transfer speed smoothed over the last few seconds.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a stretch the speed is averaged over.
pub const WINDOW: Duration = Duration::from_secs(3);

/// Remembers how many bytes were done at recent moments and derives a speed from them.
#[derive(Clone, Debug)]
pub struct SpeedMeter {
    window: Duration,
    samples: VecDeque<(Instant, u64)>,
}

impl Default for SpeedMeter {
    fn default() -> Self {
        Self::new(WINDOW)
    }
}

impl SpeedMeter {
    /// A meter averaging over `window`.
    pub fn new(window: Duration) -> Self {
        Self { window, samples: VecDeque::new() }
    }

    /// Notes that `total` bytes were done at `now`.
    pub fn record(&mut self, now: Instant, total: u64) {
        self.samples.push_back((now, total));
        self.prune(now);
    }

    /// Bytes per second over the window ending at `now`. Falls towards zero when nothing arrives.
    pub fn bytes_per_second(&mut self, now: Instant) -> f64 {
        self.prune(now);
        let (Some(&(first_at, first)), Some(&(_, last))) = (self.samples.front(), self.samples.back()) else {
            return 0.0;
        };
        let elapsed = now.saturating_duration_since(first_at).as_secs_f64();
        if elapsed < 0.05 { 0.0 } else { last.saturating_sub(first) as f64 / elapsed }
    }

    /// Forgets everything (a paused transfer starts afresh when it resumes).
    pub fn reset(&mut self) {
        self.samples.clear();
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&(at, _)) = self.samples.front() {
            // Keep the newest sample that is older than the window: it anchors the average.
            let second_is_old =
                self.samples.get(1).is_some_and(|(next, _)| now.saturating_duration_since(*next) >= self.window);
            if now.saturating_duration_since(at) >= self.window && second_is_old {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn a_steady_flow_gives_its_rate() {
        let start = Instant::now();
        let mut meter = SpeedMeter::default();
        for second in 0..=5u64 {
            meter.record(at(start, second * 1000), second * 1_000_000);
        }
        let speed = meter.bytes_per_second(at(start, 5000));
        assert!((speed - 1_000_000.0).abs() < 50_000.0, "{speed}");
    }

    #[test]
    fn only_the_last_seconds_count() {
        let start = Instant::now();
        let mut meter = SpeedMeter::default();
        // Fast for ten seconds, then slow for four.
        for second in 0..=10u64 {
            meter.record(at(start, second * 1000), second * 10_000_000);
        }
        for second in 11..=14u64 {
            meter.record(at(start, second * 1000), 100_000_000 + (second - 10) * 100_000);
        }
        let speed = meter.bytes_per_second(at(start, 14_000));
        assert!(speed < 1_000_000.0, "the fast past is forgotten: {speed}");
    }

    #[test]
    fn a_stalled_transfer_decays_to_zero() {
        let start = Instant::now();
        let mut meter = SpeedMeter::default();
        meter.record(at(start, 0), 0);
        meter.record(at(start, 1000), 1_000_000);
        let moving = meter.bytes_per_second(at(start, 1000));
        let stalled = meter.bytes_per_second(at(start, 2500));
        let long_stalled = meter.bytes_per_second(at(start, 20_000));
        assert!(moving > 900_000.0);
        assert!(stalled < moving && stalled > 0.0);
        assert_eq!(long_stalled, 0.0);
    }

    #[test]
    fn too_little_data_is_no_speed() {
        let start = Instant::now();
        let mut meter = SpeedMeter::default();
        assert_eq!(meter.bytes_per_second(start), 0.0);
        meter.record(start, 500);
        assert_eq!(meter.bytes_per_second(start), 0.0, "a single sample says nothing");
        meter.reset();
        assert_eq!(meter.bytes_per_second(at(start, 5000)), 0.0);
    }
}

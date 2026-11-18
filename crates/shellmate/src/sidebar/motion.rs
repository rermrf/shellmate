use super::{RAIL_WIDTH, SIDEBAR_WIDTH};
use std::time::{Duration, Instant};

const DURATION: Duration = Duration::from_millis(220);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SidebarFrame {
    pub width: f32,
    // Only one layout paints: positive is the full sidebar, negative is the rail.
    pub content: f32,
}

impl SidebarFrame {
    pub fn endpoint(open: bool) -> Self {
        if open {
            Self { width: SIDEBAR_WIDTH, content: 1. }
        } else {
            Self { width: RAIL_WIDTH, content: -1. }
        }
    }

    pub fn openness(self) -> f32 {
        (self.width - RAIL_WIDTH) / (SIDEBAR_WIDTH - RAIL_WIDTH)
    }

    fn interpolate(self, end: Self, elapsed: Duration, duration: Duration) -> Self {
        if elapsed >= duration {
            return end;
        }
        let time = elapsed.as_secs_f32() / duration.as_secs_f32();
        let ease_out = 1. - (1. - time).powi(3);
        let fade_out = if self.content * end.content < 0. { 0.4 * self.content.abs() } else { 0. };
        let content = if time < fade_out {
            self.content * (1. - time / fade_out)
        } else {
            let start = if fade_out > 0. { 0. } else { self.content };
            start + (end.content - start) * ((time - fade_out) / (1. - fade_out))
        };
        Self { width: self.width + (end.width - self.width) * ease_out, content }
    }
}

struct Flight {
    start: SidebarFrame,
    end: SidebarFrame,
    since: Instant,
    duration: Duration,
}

pub(crate) struct SidebarMotion {
    pub frame: SidebarFrame,
    flight: Option<Flight>,
}

impl SidebarMotion {
    pub fn settled(open: bool) -> Self {
        Self { frame: SidebarFrame::endpoint(open), flight: None }
    }

    pub fn retarget(&mut self, open: bool, now: Instant, animate: bool) {
        let end = SidebarFrame::endpoint(open);
        if !animate || self.frame == end {
            *self = Self::settled(open);
            return;
        }
        let distance = (self.frame.openness() - end.openness()).abs()
            .max((self.frame.content - end.content).abs() / 2.);
        self.flight = Some(Flight { start: self.frame, end, since: now, duration: DURATION.mul_f32(distance) });
    }

    pub fn advance(&mut self, now: Instant, reduce_motion: bool) {
        if let Some(flight) = &self.flight {
            let elapsed = now.saturating_duration_since(flight.since);
            if reduce_motion || elapsed >= flight.duration {
                self.frame = flight.end;
                self.flight = None;
            } else {
                self.frame = flight.start.interpolate(flight.end, elapsed, flight.duration);
            }
        }
    }

    pub fn running(&self) -> bool {
        self.flight.is_some()
    }

    #[cfg(debug_assertions)]
    pub fn frozen_midpoint(open: bool) -> Self {
        let frame = SidebarFrame::endpoint(!open).interpolate(SidebarFrame::endpoint(open), DURATION / 2, DURATION);
        Self { frame, flight: None }
    }

    #[cfg(debug_assertions)]
    pub fn fixture_grid_width_offset(&self, open: bool) -> f32 {
        if self.running() || self.frame == SidebarFrame::endpoint(open) {
            0.
        } else {
            self.frame.width - SidebarFrame::endpoint(!open).width
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_eases_out_and_content_changes_without_overlap() {
        let start = SidebarFrame::endpoint(true);
        let end = SidebarFrame::endpoint(false);
        let sample = |ms| start.interpolate(end, Duration::from_millis(ms), DURATION);
        assert_eq!(sample(0), start);
        assert!(sample(44).content > 0.);
        assert!(sample(88).content.abs() < 1e-6);
        assert!(sample(110).content < 0.);
        assert_eq!(sample(110).width, 102.);
        assert_eq!(sample(220), end);
        assert_eq!(sample(300), end);
        assert!((1..=220).all(|ms| sample(ms).width <= sample(ms - 1).width));
    }

    #[test]
    fn repeated_reversals_continue_from_the_displayed_frame() {
        let start = Instant::now();
        let mut motion = SidebarMotion::settled(true);
        for (ms, open) in [(0, false), (48, true), (65, false), (179, true)] {
            let now = start + Duration::from_millis(ms);
            motion.advance(now, false);
            let displayed = motion.frame;
            motion.retarget(open, now, true);
            motion.advance(now, false);
            assert_eq!(displayed, motion.frame);
            motion.advance(now + Duration::from_millis(1), false);
            assert_eq!(motion.frame.width > displayed.width, open);
        }
        motion.advance(start + DURATION * 2, false);
        assert_eq!(motion.frame, SidebarFrame::endpoint(true));
        assert!(!motion.running());
    }

    #[test]
    fn restoration_and_reduced_motion_do_not_schedule_frames() {
        let now = Instant::now();
        let mut motion = SidebarMotion::settled(false);
        assert_eq!(motion.frame, SidebarFrame::endpoint(false));
        assert!(!motion.running());
        motion.retarget(true, now, false);
        assert_eq!(motion.frame, SidebarFrame::endpoint(true));
        assert!(!motion.running());
        motion.retarget(false, now, true);
        motion.advance(now, true);
        assert_eq!(motion.frame, SidebarFrame::endpoint(false));
        assert!(!motion.running());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn both_midpoint_fixtures_are_pinned_to_the_real_transition() {
        let now = Instant::now();
        for open in [false, true] {
            let mut playback = SidebarMotion::settled(!open);
            playback.retarget(open, now, true);
            playback.advance(now + DURATION / 2, false);
            let mut fixture = SidebarMotion::frozen_midpoint(open);
            fixture.advance(now + DURATION * 20, true);
            assert_eq!(fixture.frame, playback.frame);
            assert!(!fixture.running());
            let viewport = 1440.;
            assert_eq!(viewport - fixture.frame.width + fixture.fixture_grid_width_offset(open),
                viewport - SidebarFrame::endpoint(!open).width);
            assert_eq!(playback.fixture_grid_width_offset(open), 0.);
            assert_eq!(SidebarMotion::settled(open).fixture_grid_width_offset(open), 0.);
        }
    }
}

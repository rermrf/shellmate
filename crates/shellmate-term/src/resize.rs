//! Backend resize pacing; the local grid remains free to reflow every frame.
use crate::{PtyDims, TerminalSize};
use std::time::Duration;

const INTERVAL: Duration = Duration::from_millis(100);

pub(crate) fn layout_resize(current: TerminalSize, measured: TerminalSize, held: bool, pending: bool) -> Option<TerminalSize> {
    (!held && (current != measured || pending)).then_some(measured)
}

#[derive(Default)]
pub(crate) struct ResizeThrottle {
    dragging: bool,
    final_pending: bool,
    last_sent: Option<Duration>,
    sent: Option<PtyDims>,
    pending: bool,
}

impl ResizeThrottle {
    pub fn set_dragging(&mut self, dragging: bool) {
        if self.dragging && !dragging {
            self.final_pending = true;
        }
        self.dragging = dragging;
    }

    pub fn pending(&self) -> bool {
        self.pending || self.final_pending
    }

    pub fn update(&mut self, dims: PtyDims, now: Duration) -> Option<PtyDims> {
        self.pending = self.sent != Some(dims);
        if !self.pending && !self.final_pending {
            return None;
        }
        if self.dragging
            && self
                .last_sent
                .is_some_and(|last| now.saturating_sub(last) < INTERVAL)
        {
            return None;
        }
        self.sent = Some(dims);
        self.last_sent = Some(now);
        self.pending = false;
        self.final_pending = false;
        Some(dims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dims(cols: u16) -> PtyDims {
        PtyDims {
            cols,
            ..Default::default()
        }
    }

    #[test]
    fn interactive_resize_is_throttled_and_release_flushes_latest() {
        let mut throttle = ResizeThrottle::default();
        let at = Duration::from_millis;
        assert_eq!(throttle.update(dims(120), at(0)), Some(dims(120)));
        throttle.set_dragging(true);
        for time in 1..100 {
            assert_eq!(throttle.update(dims(100), at(time)), None);
        }
        assert_eq!(throttle.update(dims(90), at(100)), Some(dims(90)));
        assert_eq!(throttle.update(dims(85), at(150)), None);
        assert!(throttle.pending());
        throttle.set_dragging(false);
        assert_eq!(throttle.update(dims(82), at(151)), Some(dims(82)));
        assert!(!throttle.pending());
        assert_eq!(throttle.update(dims(82), at(152)), None);
    }

    #[test]
    fn stationary_drag_flushes_pending_and_normal_resize_is_immediate() {
        let mut throttle = ResizeThrottle::default();
        let at = Duration::from_millis;
        throttle.update(dims(100), at(0));
        throttle.set_dragging(true);
        assert_eq!(throttle.update(dims(90), at(20)), None);
        assert_eq!(throttle.update(dims(90), at(100)), Some(dims(90)));
        throttle.set_dragging(false);
        assert_eq!(throttle.update(dims(90), at(101)), Some(dims(90)));
        assert_eq!(throttle.update(dims(80), at(102)), Some(dims(80)));
    }

    #[test]
    fn held_layout_sends_only_the_final_measurement_even_after_reversal() {
        let grid = |cols| TerminalSize { cols, ..Default::default() };
        let mut current = grid(120);
        let mut throttle = ResizeThrottle::default();
        throttle.update(current.dims(), Duration::ZERO);
        throttle.set_dragging(true);
        throttle.set_dragging(false);
        let mut sent = Vec::new();
        for (frame, (held, cols)) in [(true, 118), (true, 111), (true, 116), (true, 125), (false, 130), (false, 130)].into_iter().enumerate() {
            if let Some(next) = layout_resize(current, grid(cols), held, throttle.pending()) {
                current = next;
                sent.extend(throttle.update(next.dims(), Duration::from_millis(frame as u64 * 64)));
            }
            if held { assert_eq!(current, grid(120)); }
        }
        assert_eq!(sent, [grid(130).dims()]);
        assert!(!throttle.pending());
    }
}

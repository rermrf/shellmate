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
    fn drawer_open_and_swap_each_deliver_one_final_pty_resize() {
        let mut throttle = ResizeThrottle::default();
        let grid = |cols| TerminalSize { cols, ..Default::default() };
        let mut current = grid(140);
        throttle.update(current.dims(), Duration::ZERO);
        for widths in [[132, 109, 102, 100], [101, 103, 104, 105]] {
            let initial = current;
            let mut sent = Vec::new();
            for (index, cols) in widths.into_iter().chain([widths[3]]).enumerate() {
                let held = index < 3;
                if let Some(next) = layout_resize(current, grid(cols), held, throttle.pending()) {
                    current = next;
                    sent.extend(throttle.update(next.dims(), Duration::ZERO));
                }
                if held { assert_eq!(current, initial); }
            }
            assert_eq!(sent, [grid(widths[3]).dims()]);
        }
    }

    /// The terminal area of a window measured with the cell metrics of a font size (0.6 em wide cells,
    /// line height 20/13 of the size, rounded like the element does).
    fn measured_at(points: f32) -> TerminalSize {
        let cell = gpui::px(points * 0.6);
        let line = gpui::px((points * 20. / 13.).round());
        TerminalSize::from_geometry(1200., 600., cell, line)
    }

    #[test]
    fn a_new_font_size_reflows_the_grid_and_tells_the_backend_once() {
        let mut current = measured_at(13.);
        let mut throttle = ResizeThrottle::default();
        assert_eq!(throttle.update(current.dims(), Duration::ZERO), Some(current.dims()));
        let mut sent = Vec::new();
        for points in [14., 15., 13., 9., 28.] {
            let measured = measured_at(points);
            let next = layout_resize(current, measured, false, throttle.pending())
                .unwrap_or_else(|| panic!("{points} pt must be applied"));
            assert_eq!(next, measured);
            assert_ne!((next.cols, next.rows), (current.cols, current.rows), "{points} pt changes the grid");
            sent.push(throttle.update(next.dims(), Duration::ZERO).expect("the backend hears of every new size"));
            assert_eq!(throttle.update(next.dims(), Duration::ZERO), None, "and only once");
            current = next;
        }
        let cells: Vec<_> = sent.iter().map(|d| (d.cols, d.rows)).collect();
        assert_eq!(cells, [(142, 27), (133, 26), (153, 30), (222, 42), (71, 13)]);
        // Larger type means fewer cells in the same window.
        assert!(measured_at(28.).cols < measured_at(13.).cols && measured_at(28.).rows < measured_at(13.).rows);
        assert!(measured_at(9.).cols > measured_at(13.).cols && measured_at(9.).rows > measured_at(13.).rows);
    }

    #[test]
    fn cell_metrics_alone_are_enough_to_resize_when_the_grid_keeps_its_shape() {
        // Same columns and rows, different pixel size: the PTY's pixel dimensions still change.
        let before = TerminalSize { cols: 100, rows: 30, cell_width: gpui::px(7.8), line_height: gpui::px(20.) };
        let after = TerminalSize { cell_width: gpui::px(8.4), line_height: gpui::px(22.), ..before };
        assert_eq!((before.cols, before.rows), (after.cols, after.rows));
        let mut throttle = ResizeThrottle::default();
        throttle.update(before.dims(), Duration::ZERO);
        assert_eq!(layout_resize(before, after, false, throttle.pending()), Some(after));
        let sent = throttle.update(after.dims(), Duration::ZERO).expect("pixel size changed");
        assert_eq!((sent.cols, sent.rows), (100, 30));
        assert_ne!((sent.pixel_width, sent.pixel_height), (before.dims().pixel_width, before.dims().pixel_height));
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

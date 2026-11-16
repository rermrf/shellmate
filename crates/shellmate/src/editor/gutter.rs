//! Pixel-to-line mapping for the editor's unwrapped, unfolded line-number gutter.
use std::ops::Range;

pub const TOP: f32 = 16.;
pub const LINE: f32 = 21.;
pub const RIGHT: f32 = 12.;

pub fn width(lines: usize, digit_width: f32) -> f32 {
    let digits = lines.max(1).ilog10() + 1;
    (digits as f32 * digit_width + RIGHT * 2.).max(52.)
}

pub fn visible(scroll_y: f32, height: f32, lines: usize) -> Range<usize> {
    let top = TOP + scroll_y;
    let start = ((-top / LINE).floor().max(0.) as usize).min(lines);
    let end = (((height - top) / LINE).ceil().max(0.) as usize).min(lines);
    start..end.max(start)
}

pub fn row_at(y: f32, scroll_y: f32, lines: usize) -> usize {
    (((y - TOP - scroll_y) / LINE).floor().max(0.) as usize).min(lines.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_numbers_follow_partial_rows_and_scroll_to_end() {
        assert_eq!(visible(0., 100., 100), 0..4);
        assert_eq!(visible(-26., 100., 100), 0..6);
        assert_eq!(visible(-37., 100., 100), 1..6);
        assert_eq!(visible(-37., 100., 3), 1..3);
        assert_eq!(visible(-10_000., 100., 3), 3..3);
        assert_eq!(visible(0., 100., 0), 0..0);
    }

    #[test]
    fn pointer_hit_testing_uses_the_same_scroll_origin() {
        assert_eq!(row_at(16., 0., 100), 0);
        assert_eq!(row_at(37., 0., 100), 1);
        assert_eq!(row_at(0., -37., 100), 1);
        assert_eq!(row_at(500., -37., 3), 2);
        assert_eq!(row_at(-20., 0., 3), 0);
        assert_eq!(row_at(20., 0., 0), 0);
    }

    #[test]
    fn large_files_keep_all_line_number_digits_visible() {
        assert_eq!(width(19, 7.8), 52.);
        assert!(width(100_000, 7.8) >= 6. * 7.8 + RIGHT * 2.);
    }
}

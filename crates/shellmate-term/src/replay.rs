//! Replay timeline shared by the UI and deterministic emulator tests.
pub mod timestamps;
use crate::cast::{Cast, Data};
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line},
    term::{Term, cell::Flags},
};
use std::ops::Range;
pub struct Timeline {
    pub cast: Cast,
    next: usize,
    pub position: f64,
}
impl Timeline {
    pub fn new(cast: Cast) -> Self {
        Self {
            cast,
            next: 0,
            position: 0.,
        }
    }
    /// Seeking explicitly replays from the beginning; normal playback only applies new events.
    pub fn advance(&mut self, time: f64, seek: bool) -> (bool, Range<usize>) {
        let reset = seek || time < self.position;
        let start = if reset { 0 } else { self.next };
        self.position = time.clamp(0., self.cast.duration);
        self.next = self.cast.events.partition_point(|e| e.at <= self.position);
        (reset, start..self.next)
    }
}
pub fn screen_text<T>(term: &Term<T>) -> String {
    let mut text = String::new();
    for line in 0..term.screen_lines() {
        let mut row = String::new();
        for c in 0..term.columns() {
            let cell = &term.grid()[Line(line as i32)][Column(c)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            row.push(cell.c);
            if let Some(extra) = cell.zerowidth() {
                row.extend(extra);
            }
        }
        text.push_str(row.trim_end());
        if !term.grid()[Line(line as i32)][Column(term.columns() - 1)]
            .flags
            .contains(Flags::WRAPLINE)
        {
            text.push('\n');
        }
    }
    text.trim_end().to_string()
}
/// Displayed cells ending at the visible viewport, with up to `limit` physical
/// rows. Soft wraps are joined, just like history capture, so wrapped secrets
/// remain recognizable to a downstream redactor. No hidden primary grid or
/// backend input buffer is read.
pub fn recent_text<T>(term: &Term<T>, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let grid = term.grid();
    let bottom = term.screen_lines() as i32 - 1 - grid.display_offset() as i32;
    let top = (bottom - limit.min(i32::MAX as usize) as i32 + 1).max(-(grid.history_size() as i32));
    let mut text = String::new();
    for line in top..=bottom {
        let mut row = String::new();
        for c in 0..term.columns() {
            let cell = &grid[Line(line)][Column(c)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            row.push(cell.c);
            if let Some(extra) = cell.zerowidth() {
                row.extend(extra)
            }
        }
        text.push_str(row.trim_end());
        if !grid[Line(line)][Column(term.columns() - 1)]
            .flags
            .contains(Flags::WRAPLINE)
        {
            text.push('\n');
        }
    }
    text.trim_end().to_owned()
}
pub const FONT_SIZE: f32 = 13.;
pub const LINE_HEIGHT: f32 = 20.;
pub const TIME_FONT_SIZE: f32 = 12.5;
pub const TIME_GAP: f32 = 14.;
pub const TIME_WIDTH: f32 = 64.;
pub const TIME_GUTTER: f32 = TIME_WIDTH + TIME_GAP;

/// Both terminal cells and the fixed gutter use this content-relative row origin.
pub fn row_y(line: i32, history: usize, scroll_y: f32) -> f32 {
    (line as f32 + history as f32) * LINE_HEIGHT + scroll_y
}

pub fn scrollback_content_size(
    grid: crate::TerminalSize,
    history: usize,
) -> gpui::Size<gpui::Pixels> {
    let mut extent = content_size(grid);
    extent.height += grid.line_height * history as f32;
    extent
}

/// Replay metrics never depend on the available viewport or live font size.
pub fn grid_size(recorded: crate::TerminalSize, cell_width: gpui::Pixels) -> crate::TerminalSize {
    crate::TerminalSize {
        cell_width,
        line_height: gpui::px(LINE_HEIGHT),
        ..recorded
    }
}

pub fn content_size(grid: crate::TerminalSize) -> gpui::Size<gpui::Pixels> {
    gpui::size(
        grid.cell_width * grid.cols as f32,
        grid.line_height * grid.rows as f32,
    )
}

pub(crate) fn wheel_delta(
    mut delta: gpui::Point<gpui::Pixels>,
    shift: bool,
) -> gpui::Point<gpui::Pixels> {
    if shift {
        delta.x = if delta.x != gpui::px(0.) {
            delta.x
        } else {
            delta.y
        };
        delta.y = gpui::px(0.);
    }
    delta
}

/// Positive scroll positions, independent of the UI scroll handle's negative offsets.
#[derive(Debug)]
pub struct FollowCursor {
    following: bool,
}

impl Default for FollowCursor {
    fn default() -> Self {
        Self { following: true }
    }
}

impl FollowCursor {
    pub fn restart(&mut self) {
        self.following = true;
    }
    pub fn manual_scroll(&mut self) {
        self.following = false;
    }

    pub fn position(
        &self,
        current: [f32; 2],
        viewport: [f32; 2],
        content: [f32; 2],
        cursor: [f32; 2],
        cell: [f32; 2],
    ) -> [f32; 2] {
        let mut result = current;
        for axis in 0..2 {
            if self.following && viewport[axis] > 0. {
                if cursor[axis] < result[axis] {
                    result[axis] = cursor[axis];
                } else if cursor[axis] + cell[axis] > result[axis] + viewport[axis] {
                    result[axis] = cursor[axis] + cell[axis] - viewport[axis];
                }
            }
            result[axis] = result[axis].clamp(0., (content[axis] - viewport[axis]).max(0.));
        }
        result
    }
}
pub fn apply_data<T: alacritty_terminal::event::EventListener>(
    term: &mut Term<T>,
    parser: &mut alacritty_terminal::vte::ansi::Processor,
    data: &Data,
) {
    match data {
        Data::Output(text) => parser.advance(term, text.as_bytes()),
        Data::Resize(c, r) => term.resize(crate::session_log::Size {
            cols: *c as usize,
            rows: *r as usize,
        }),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cast::Header,
        session_log::{Silent, Size},
    };
    use alacritty_terminal::{term::Config, vte::ansi::Processor};
    #[test]
    fn gutter_rows_align_with_terminal_at_fixed_replay_metrics() {
        for (history, line, scroll) in [
            (0, 0, 0.),
            (8, -8, -7.5),
            (8, -3, -81.),
            (10_000, 23, -200_000.),
        ] {
            let y = row_y(line, history, scroll);
            assert_eq!(y, (line + history as i32) as f32 * 20. + scroll);
            assert_eq!(row_y(line + 1, history, scroll) - y, 20.);
        }
        assert_eq!(TIME_FONT_SIZE, 12.5);
        assert_eq!(TIME_GUTTER - TIME_WIDTH, 14.);
        let grid = grid_size(crate::TerminalSize::default(), gpui::px(7.8));
        assert_eq!(
            scrollback_content_size(grid, 10).height - content_size(grid).height,
            gpui::px(200.)
        );
    }
    #[test]
    fn ai_context_uses_displayed_cells_and_scrollback_only() {
        let mut term = Term::new(Config::default(), &Size { cols: 80, rows: 3 }, Silent);
        let mut parser: Processor = Processor::new();
        for n in 0..50 {
            parser.advance(&mut term, format!("line-{n}\r\n").as_bytes());
        }
        let text = recent_text(&term, 40);
        assert!(text.contains("line-49"));
        assert!(!text.contains("line-9\n"));
        assert!(text.lines().count() <= 40);
        parser.advance(&mut term, b"\x1b[?1049halt-screen");
        let alt = recent_text(&term, 40);
        assert!(alt.contains("alt-screen"));
        assert!(!alt.contains("line-49"));
    }
    #[test]
    fn ai_context_joins_soft_wrapped_tokens() {
        let mut term = Term::new(Config::default(), &Size { cols: 10, rows: 4 }, Silent);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"sk-123456789abcdefghijklmnop");
        assert_eq!(recent_text(&term, 40), "sk-123456789abcdefghijklmnop");
    }
    #[test]
    fn seek_equals_sequential_screen_and_cursor() {
        let input=b"{\"version\":2,\"width\":10,\"height\":3}\n[0,\"o\",\"hello\\r\\n\"]\n[1,\"o\",\"\\u001b[31mworld\"]\n[2,\"r\",\"6x4\"]\n[3,\"o\",\"abcdefghi\\r\\n\"]\n[4,\"o\",\"\\u001b[?1049halt\"]\n[5,\"o\",\"\\u001b[?1049l\\rX\"]\n";
        let cast = Cast::read(&input[..]).unwrap();
        let build = |h: &Header| {
            Term::new(
                Config::default(),
                &Size {
                    cols: h.cols as usize,
                    rows: h.rows as usize,
                },
                Silent,
            )
        };
        let mut sequential = build(&cast.header);
        let mut p = Processor::new();
        let mut next = 0;
        for time in [0., 0.5, 1., 2., 3., 4., 5.] {
            while next < cast.events.len() && cast.events[next].at <= time {
                apply_data(&mut sequential, &mut p, &cast.events[next].data);
                next += 1;
            }
            let mut timeline = Timeline::new(cast.clone());
            let (_, range) = timeline.advance(time, true);
            let mut sought = build(&cast.header);
            let mut q = Processor::new();
            for e in &cast.events[range] {
                apply_data(&mut sought, &mut q, &e.data);
            }
            assert_eq!(screen_text(&sequential), screen_text(&sought));
            assert_eq!(sequential.grid().cursor.point, sought.grid().cursor.point);
        }
    }
    #[test]
    fn replay_keeps_font_grid_and_scroll_extents() {
        for live_font in [8., 14., 32.] {
            let style = crate::TerminalStyle {
                font_family: "terminal-family".into(),
                fallbacks: vec!["fallback".into()],
                font_size: gpui::px(live_font),
                line_height: 1.8,
                palette: crate::TerminalPalette::light(),
            }
            .for_replay();
            assert_eq!(style.font_family.as_ref(), "terminal-family");
            assert_eq!(style.fallbacks, ["fallback"]);
            assert_eq!(style.font_size, gpui::px(13.));
            assert_eq!((style.font_size * style.line_height).round(), gpui::px(20.));
        }
        for (cols, rows) in [(20, 5), (80, 24), (142, 36), (300, 100)] {
            let grid = grid_size(
                crate::TerminalSize {
                    cols,
                    rows,
                    ..Default::default()
                },
                gpui::px(7.8),
            );
            assert_eq!((grid.cols, grid.rows), (cols, rows));
            assert_eq!(FONT_SIZE, 13.);
            assert_eq!(grid.line_height, gpui::px(20.));
            let extent = content_size(grid);
            assert_eq!(
                extent,
                gpui::size(gpui::px(7.8 * cols as f32), gpui::px(20. * rows as f32))
            );
            let end = FollowCursor::default().position(
                [0.; 2],
                [800., 480.],
                [extent.width.into(), extent.height.into()],
                [7.8 * (cols - 1) as f32, 20. * (rows - 1) as f32],
                [7.8, 20.],
            );
            assert!((end[0] - (7.8 * cols as f32 - 800.).max(0.)).abs() < 0.001);
            assert_eq!(end[1], (20. * rows as f32 - 480.).max(0.));
        }
    }

    #[test]
    fn follow_cursor_pauses_on_manual_scroll_and_seek_restarts_it() {
        let mut follow = FollowCursor::default();
        let position = |f: &FollowCursor, current, cursor| {
            f.position(current, [80., 40.], [160., 100.], cursor, [8., 20.])
        };
        assert_eq!(position(&follow, [0., 0.], [120., 80.]), [48., 60.]);
        assert_eq!(position(&follow, [48., 60.], [16., 20.]), [16., 20.]);
        follow.manual_scroll();
        assert_eq!(position(&follow, [0., 0.], [120., 80.]), [0., 0.]);
        follow.restart();
        assert_eq!(position(&follow, [0., 0.], [120., 80.]), [48., 60.]);
    }

    #[test]
    fn shift_wheel_accepts_mouse_and_macos_translated_horizontal_deltas() {
        let p = |x, y| gpui::point(gpui::px(x), gpui::px(y));
        assert_eq!(wheel_delta(p(0., -20.), true), p(-20., 0.));
        assert_eq!(wheel_delta(p(-20., 0.), true), p(-20., 0.));
        assert_eq!(wheel_delta(p(4., -20.), false), p(4., -20.));
    }
}

//! Replay timeline shared by the UI and deterministic emulator tests.
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
pub fn recent_text<T>(term:&Term<T>,limit:usize)->String{
    if limit==0{return String::new()}
    let grid=term.grid();let bottom=term.screen_lines() as i32-1-grid.display_offset() as i32;
    let top=(bottom-limit.min(i32::MAX as usize)as i32+1).max(-(grid.history_size() as i32));
    let mut text=String::new();for line in top..=bottom{let mut row=String::new();for c in 0..term.columns(){let cell=&grid[Line(line)][Column(c)];if cell.flags.intersects(Flags::WIDE_CHAR_SPACER|Flags::LEADING_WIDE_CHAR_SPACER){continue}row.push(cell.c);if let Some(extra)=cell.zerowidth(){row.extend(extra)}}text.push_str(row.trim_end());if !grid[Line(line)][Column(term.columns()-1)].flags.contains(Flags::WRAPLINE){text.push('\n');}}
    text.trim_end().to_owned()
}
pub fn fit_font(base: f32, width: f32, height: f32, cols: u16, rows: u16, cell: f32, line: f32) -> f32 {
    (base
        * (width / (cols as f32 * cell))
            .min(height / (rows as f32 * line))
            .min(1.))
    .max(8.)
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
    fn ai_context_uses_displayed_cells_and_scrollback_only(){
        let mut term=Term::new(Config::default(),&Size{cols:80,rows:3},Silent);let mut parser:Processor=Processor::new();
        for n in 0..50{parser.advance(&mut term,format!("line-{n}\r\n").as_bytes());}
        let text=recent_text(&term,40);assert!(text.contains("line-49"));assert!(!text.contains("line-9\n"));assert!(text.lines().count()<=40);
        parser.advance(&mut term,b"\x1b[?1049halt-screen");let alt=recent_text(&term,40);assert!(alt.contains("alt-screen"));assert!(!alt.contains("line-49"));
    }
    #[test]
    fn ai_context_joins_soft_wrapped_tokens(){let mut term=Term::new(Config::default(),&Size{cols:10,rows:4},Silent);let mut parser:Processor=Processor::new();parser.advance(&mut term,b"sk-123456789abcdefghijklmnop");assert_eq!(recent_text(&term,40),"sk-123456789abcdefghijklmnop");}
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
    fn fit_scales_but_never_below_eight() {
        assert_eq!(fit_font(13., 800., 480., 80, 24, 8., 20.), 13.);
        assert_eq!(fit_font(13., 320., 200., 80, 24, 8., 20.), 8.);
    }
}

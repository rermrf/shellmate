//! Row identities follow emulator scrolling, including eviction from bounded history.
//! Intercept ANSI printing, rather than comparing frames: identical rewrites and
//! multiple lines inside a single output event must retain their first write time.
use alacritty_terminal::{
    event::EventListener,
    grid::{Dimensions, GridCell},
    index::{Column, Line},
    term::{Term, TermMode, cell::Flags},
    vte::ansi::*,
};
use std::{collections::VecDeque, ops::Range};

#[derive(Clone, Debug, PartialEq)]
struct Row {
    id: u64,
    first: Option<f64>,
    continuation: bool,
}

#[derive(Default)]
pub struct LineTimes {
    rows: VecDeque<Row>,
    primary: VecDeque<Row>,
    primary_lines: VecDeque<(String, Row)>,
    primary_resized: bool,
    next_id: u64,
    region: Range<usize>,
}

impl LineTimes {
    fn fresh(&mut self) -> Row {
        let id = self.next_id;
        self.next_id += 1;
        Row {
            id,
            first: None,
            continuation: false,
        }
    }

    pub fn reset<T>(&mut self, term: &Term<T>) {
        *self = Self::default();
        self.region = 0..term.screen_lines();
        for _ in 0..term.total_lines() {
            let row = self.fresh();
            self.rows.push_back(row);
        }
    }

    /// The grid uses negative indices for scrollback; each slot holds a stable
    /// absolute identity, which is never reused when old history is evicted.
    pub fn first_write<T>(&self, term: &Term<T>, line: Line) -> Option<f64> {
        let index = (term.history_size() as i32 + line.0) as usize;
        self.rows
            .get(index)
            .and_then(|row| (!row.continuation).then_some(row.first).flatten())
    }

    pub fn label<T>(&self, term: &Term<T>, line: Line, timestamp: Option<i64>) -> Option<String> {
        // Erased rows and soft-wrap continuations have no visible label.
        let row = &term.grid()[line];
        if !(0..term.columns()).any(|c| row[Column(c)].c != ' ') {
            return None;
        }
        let at = self.first_write(term, line)?;
        Some(match timestamp {
            Some(start) => {
                crate::archive::local_time(start.saturating_add(at.floor() as i64), "%H:%M:%S")
            }
            None => crate::archive::mmss(at),
        })
    }

    pub fn advance<T: EventListener>(
        &mut self,
        term: &mut Term<T>,
        parser: &mut Processor,
        bytes: &[u8],
        at: f64,
    ) {
        if self.rows.is_empty() {
            self.reset(term);
        }
        parser.advance(
            &mut Timed {
                term,
                times: self,
                at,
            },
            bytes,
        );
    }

    fn trim<T>(&mut self, term: &Term<T>) {
        while self.rows.len() > term.total_lines() {
            self.rows.pop_front();
        }
    }

    fn scroll_up<T>(&mut self, term: &Term<T>, origin: usize, count: usize) {
        let count = count.min(self.region.end - origin);
        // The terminal has already scrolled. Recover the previous history size
        // from our slots so bounded history does not affect row identity.
        let history = self.rows.len() - term.screen_lines();
        for index in 0..count {
            let row = self.fresh();
            if origin == 0 && !term.mode().contains(TermMode::ALT_SCREEN) {
                self.rows.insert(history + self.region.end + index, row);
            } else {
                self.rows.remove(history + origin);
                self.rows.insert(history + self.region.end - 1, row);
            }
        }
        self.trim(term);
    }

    fn scroll_down<T>(&mut self, term: &Term<T>, origin: usize, count: usize) {
        let history = term.history_size();
        for _ in 0..count.min(self.region.end - origin) {
            self.rows.remove(history + self.region.end - 1);
            let row = self.fresh();
            self.rows.insert(history + origin, row);
        }
    }

    fn logical_lines<T>(&self, term: &Term<T>) -> VecDeque<(String, Row)> {
        let mut lines = VecDeque::new();
        let mut text = String::new();
        let mut first = None;
        for line in -(term.history_size() as i32)..term.screen_lines() as i32 {
            let index = (term.history_size() as i32 + line) as usize;
            first = first.or_else(|| self.rows.get(index).cloned());
            let (part, wrap) = row_text(term, Line(line));
            text.push_str(&part);
            if !wrap {
                if let Some(row) = first.take() {
                    lines.push_back((std::mem::take(&mut text), row));
                }
                text.clear();
            }
        }
        if let Some(row) = first {
            lines.push_back((text, row));
        }
        lines
    }

    /// Reflow changes physical rows but not logical lines or their write times.
    pub fn resize<T: EventListener>(&mut self, term: &mut Term<T>, size: crate::TerminalSize) {
        if self.rows.is_empty() {
            self.reset(term);
        }
        let previous = self.logical_lines(term);
        if term.mode().contains(TermMode::ALT_SCREEN) {
            self.primary_resized = true;
        }
        term.resize(size);
        self.region = 0..term.screen_lines();
        self.reflow(term, previous);
    }

    fn reflow<T>(&mut self, term: &Term<T>, mut previous: VecDeque<(String, Row)>) {
        self.rows.clear();
        let mut text = String::new();
        let mut group = Vec::new();
        for line in -(term.history_size() as i32)..term.screen_lines() as i32 {
            let (part, wrap) = row_text(term, Line(line));
            text.push_str(&part);
            let mut row = self.fresh();
            row.continuation = !group.is_empty();
            group.push(row);
            if !wrap || line == term.screen_lines() as i32 - 1 {
                if let Some((_, original)) = previous
                    .iter()
                    .position(|(old, _)| old.trim_end() == text.trim_end())
                    .and_then(|i| previous.drain(..=i).next_back())
                {
                    group[0] = original;
                }
                self.rows.extend(group.drain(..));
                text.clear();
            }
        }
    }
}

fn row_text<T>(term: &Term<T>, line: Line) -> (String, bool) {
    let mut text = String::new();
    for col in 0..term.columns() {
        let cell = &term.grid()[line][Column(col)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        text.push(cell.c);
        if let Some(extra) = cell.zerowidth() {
            text.extend(extra);
        }
    }
    let wrap = term.grid()[line][Column(term.columns() - 1)]
        .flags
        .contains(Flags::WRAPLINE);
    if !wrap {
        text.truncate(text.trim_end().len());
    }
    (text, wrap)
}

struct Timed<'a, T> {
    term: &'a mut Term<T>,
    times: &'a mut LineTimes,
    at: f64,
}

// Keep all non-printing ANSI behavior in the real emulator, including queries,
// character sets, color overrides and synchronized output.
macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {$(
        fn $name(&mut self, $($arg: $ty),*) { self.term.$name($($arg),*); }
    )*};
}

impl<T: EventListener> Timed<'_, T> {
    fn alt_changed(&mut self, was_alt: bool) {
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        if was_alt == alt {
            return;
        }
        if alt {
            self.times.primary = std::mem::take(&mut self.times.rows);
            for _ in 0..self.term.screen_lines() {
                let row = self.times.fresh();
                self.times.rows.push_back(row);
            }
        } else {
            self.times.rows = std::mem::take(&mut self.times.primary);
            let previous = std::mem::take(&mut self.times.primary_lines);
            if std::mem::take(&mut self.times.primary_resized) {
                self.times.reflow(self.term, previous);
            }
            self.times.trim(self.term);
            while self.times.rows.len() < self.term.total_lines() {
                let row = self.times.fresh();
                self.times.rows.push_back(row);
            }
        }
    }
    fn fed_line(&mut self, previous_line: usize) {
        if previous_line + 1 == self.times.region.end {
            self.times.scroll_up(self.term, self.times.region.start, 1);
        }
    }
}

impl<T: EventListener> Handler for Timed<'_, T> {
    fn input(&mut self, c: char) {
        let before = self.term.grid().cursor.clone();
        self.term.input(c);
        let after = &self.term.grid().cursor;
        // Locate the printed cell (wide spacers and combining marks included).
        let mut col = after.point.column.0;
        if !after.input_needs_wrap {
            col = col.saturating_sub(1);
        }
        if self.term.grid()[after.point.line][Column(col)]
            .flags
            .contains(Flags::WIDE_CHAR_SPACER)
        {
            col = col.saturating_sub(1);
        }
        let cell = &self.term.grid()[after.point.line][Column(col)];
        let combining = cell
            .zerowidth()
            .is_some_and(|extra| extra.last() == Some(&c));
        let wrapped = !combining
            && self.term.mode().contains(TermMode::LINE_WRAP)
            && (before.input_needs_wrap
                || after.point.line != before.point.line
                || after.point.column < before.point.column);
        let line = after.point.line;
        if wrapped {
            self.fed_line(before.point.line.0 as usize);
        }
        let index = (self.term.history_size() as i32 + line.0) as usize;
        if let Some(row) = self.times.rows.get_mut(index) {
            if wrapped {
                row.continuation = true;
            }
            if !c.is_whitespace() {
                row.first.get_or_insert(self.at);
            }
        }
    }
    fn linefeed(&mut self) {
        let line = self.term.grid().cursor.point.line.0 as usize;
        self.term.linefeed();
        self.fed_line(line);
    }
    fn newline(&mut self) {
        let line = self.term.grid().cursor.point.line.0 as usize;
        self.term.newline();
        self.fed_line(line);
    }
    fn scroll_up(&mut self, count: usize) {
        self.term.scroll_up(count);
        self.times
            .scroll_up(self.term, self.times.region.start, count);
    }
    fn scroll_down(&mut self, count: usize) {
        self.term.scroll_down(count);
        self.times
            .scroll_down(self.term, self.times.region.start, count);
    }
    fn insert_blank_lines(&mut self, count: usize) {
        let line = self.term.grid().cursor.point.line.0 as usize;
        self.term.insert_blank_lines(count);
        if self.times.region.contains(&line) {
            self.times.scroll_down(self.term, line, count);
        }
    }
    fn delete_lines(&mut self, count: usize) {
        let line = self.term.grid().cursor.point.line.0 as usize;
        self.term.delete_lines(count);
        if self.times.region.contains(&line) {
            self.times.scroll_up(self.term, line, count);
        }
    }
    fn reverse_index(&mut self) {
        let line = self.term.grid().cursor.point.line.0 as usize;
        self.term.reverse_index();
        if line == self.times.region.start {
            self.times.scroll_down(self.term, line, 1);
        }
    }
    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        self.term.set_scrolling_region(top, bottom);
        let bottom = bottom.unwrap_or(self.term.screen_lines());
        if top < bottom {
            self.times.region = top.saturating_sub(1).min(self.term.screen_lines())
                ..bottom.min(self.term.screen_lines());
        }
    }
    fn set_private_mode(&mut self, mode: PrivateMode) {
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        if !alt && mode == PrivateMode::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor) {
            self.times.primary_lines = self.times.logical_lines(self.term);
        }
        self.term.set_private_mode(mode);
        self.alt_changed(alt);
    }
    fn unset_private_mode(&mut self, mode: PrivateMode) {
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        self.term.unset_private_mode(mode);
        self.alt_changed(alt);
    }
    fn reset_state(&mut self) {
        self.term.reset_state();
        self.times.reset(self.term);
    }
    fn clear_screen(&mut self, mode: ClearMode) {
        let last = (0..self.term.screen_lines()).rfind(|&line| {
            (0..self.term.columns())
                .any(|col| !self.term.grid()[Line(line as i32)][Column(col)].is_empty())
        });
        let all = matches!(mode, ClearMode::All);
        self.term.clear_screen(mode);
        if all && !self.term.mode().contains(TermMode::ALT_SCREEN) {
            let region = self.times.region.clone();
            self.times.region = 0..self.term.screen_lines();
            self.times
                .scroll_up(self.term, 0, last.map_or(0, |line| line + 1));
            self.times.region = region;
        }
        self.times.trim(self.term);
    }
    forward! {
        set_title(arg0: Option<String>);
        set_cursor_style(arg0: Option<CursorStyle>);
        set_cursor_shape(shape: CursorShape);
        goto(line: i32, col: usize);
        goto_line(line: i32);
        goto_col(col: usize);
        insert_blank(arg0: usize);
        move_up(arg0: usize);
        move_down(arg0: usize);
        identify_terminal(intermediate: Option<char>);
        device_status(arg0: usize);
        move_forward(col: usize);
        move_backward(col: usize);
        move_down_and_cr(row: usize);
        move_up_and_cr(row: usize);
        put_tab(count: u16);
        backspace();
        carriage_return();
        bell();
        substitute();
        set_horizontal_tabstop();
        erase_chars(arg0: usize);
        delete_chars(arg0: usize);
        move_backward_tabs(count: u16);
        move_forward_tabs(count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(mode: LineClearMode);
        clear_tabs(mode: TabulationClearMode);
        set_tabs(interval: u16);
        terminal_attribute(attr: Attr);
        set_mode(mode: Mode);
        unset_mode(mode: Mode);
        report_mode(mode: Mode);
        report_private_mode(mode: PrivateMode);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(arg0: CharsetIndex);
        configure_charset(arg0: CharsetIndex, arg1: StandardCharset);
        set_color(arg0: usize, arg1: Rgb);
        dynamic_color_sequence(arg0: String, arg1: usize, arg2: &str);
        reset_color(arg0: usize);
        clipboard_store(arg0: u8, arg1: &[u8]);
        clipboard_load(arg0: u8, arg1: &str);
        decaln();
        push_title();
        pop_title();
        text_area_size_pixels();
        text_area_size_chars();
        set_hyperlink(arg0: Option<Hyperlink>);
        report_keyboard_mode();
        push_keyboard_mode(mode: KeyboardModes);
        pop_keyboard_modes(to_pop: u16);
        set_keyboard_mode(mode: KeyboardModes, behavior: KeyboardModesApplyBehavior);
        set_modify_other_keys(mode: ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(char_path: ScpCharPath, update_mode: ScpUpdateMode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cast::{Cast, Data},
        replay::Timeline,
        session_log::{Silent, Size},
    };
    use alacritty_terminal::term::Config;

    struct Replay {
        term: Term<Silent>,
        parser: Processor,
        times: LineTimes,
    }
    impl Replay {
        fn new(cols: usize, rows: usize, history: usize) -> Self {
            let term = Term::new(
                Config {
                    scrolling_history: history,
                    ..Default::default()
                },
                &Size { cols, rows },
                Silent,
            );
            let mut times = LineTimes::default();
            times.reset(&term);
            Self {
                term,
                parser: Processor::new(),
                times,
            }
        }
        fn feed(&mut self, at: f64, text: &str) {
            self.times
                .advance(&mut self.term, &mut self.parser, text.as_bytes(), at);
        }
        fn first(&self, line: i32) -> Option<f64> {
            self.times.first_write(&self.term, Line(line))
        }
        fn event(&mut self, event: &crate::cast::Event) {
            match &event.data {
                Data::Output(text) => self.feed(event.recorded_at, text),
                Data::Resize(cols, rows) => self.times.resize(
                    &mut self.term,
                    crate::TerminalSize {
                        cols: *cols,
                        rows: *rows,
                        ..Default::default()
                    },
                ),
            }
        }
    }

    #[test]
    fn first_write_times_follow_absolute_lines_past_history_capacity() {
        let mut r = Replay::new(12, 2, 3);
        for n in 0..9 {
            r.feed(n as f64 * 10., &format!("same-{n}\r\n"));
            assert_eq!(r.first(1), None);
            assert_eq!(r.first(0), Some(n as f64 * 10.));
            for back in 1..=r.term.history_size() {
                assert_eq!(r.first(-(back as i32)), Some((n - back) as f64 * 10.));
            }
        }
        let ids: Vec<_> = r.times.rows.iter().map(|row| row.id).collect();
        assert_eq!(ids, [5, 6, 7, 8, 9]);
        r.feed(100., "tail");
        assert_eq!(r.first(1), Some(100.));
    }

    #[test]
    fn rewrites_keep_first_time_and_control_sequences_do_not_stamp() {
        let mut r = Replay::new(40, 3, 10);
        r.feed(1., "\x1b[31m\r\x1b[0m");
        assert_eq!(r.first(0), None);
        r.feed(7., "root@host:~# ");
        r.feed(9., "pwd\r\n");
        r.feed(10., "\x1b[1;1H\x1b[2Kroot@host:~# ls");
        assert_eq!(r.first(0), Some(7.));
        assert_eq!(r.first(1), None);
        r.feed(11., "\rroot@host:~# ls");
        assert_eq!(r.first(0), Some(7.));
    }

    #[test]
    fn soft_wraps_and_blank_rows_have_no_labels_including_cjk_and_combining() {
        let mut r = Replay::new(6, 2, 10);
        r.feed(1., "abcde中");
        r.feed(2., "文e\u{301}fg\r\nlast");
        assert_eq!(r.first(-2), Some(1.));
        assert_eq!(r.first(-1), None);
        assert_eq!(r.first(0), None);
        assert_eq!(r.first(1), Some(2.));
        r.feed(3., "\r\x1b[2K");
        assert_eq!(r.times.label(&r.term, Line(1), None), None);
    }

    #[test]
    fn batched_prints_scrolls_and_repeat_characters_are_all_observed() {
        let mut r = Replay::new(4, 2, 2);
        r.feed(5., "a\x1b[10b\r\nb\r\nc");
        assert_eq!(r.term.history_size(), 2);
        assert_eq!(r.first(-2), None);
        assert_eq!(r.first(-1), None);
        assert_eq!(r.first(0), Some(5.));
        assert_eq!(r.first(1), Some(5.));
    }

    #[test]
    fn timestamps_use_original_offsets_and_elapsed_fallback() {
        let bytes = b"{\"version\":2,\"width\":20,\"height\":3,\"timestamp\":1700000000}\n[0,\"o\",\"first\\r\\n\"]\n[67.8,\"o\",\"second\"]\n";
        let cast = Cast::read(&bytes[..]).unwrap();
        assert_eq!(cast.events[1].at, 2.);
        assert_eq!(cast.events[1].recorded_at, 67.8);
        let mut r = Replay::new(20, 3, 10);
        for event in &cast.events {
            r.event(event);
        }
        assert_eq!(
            r.times.label(&r.term, Line(1), cast.header.timestamp),
            Some(crate::archive::local_time(1700000067, "%H:%M:%S"))
        );
        assert_eq!(
            r.times.label(&r.term, Line(1), None).as_deref(),
            Some("01:07")
        );
    }

    #[test]
    fn seek_rebuilds_identical_times_and_row_identities() {
        let bytes = b"{\"version\":2,\"width\":8,\"height\":2}\n[0,\"o\",\"prompt$ \"]\n[7,\"o\",\"ls\\r\\none\\r\\ntwo\"]\n[9,\"o\",\"\\rtwo!\\r\\n\"]\n[12,\"r\",\"12x3\"]\n[18,\"o\",\"\\u001b[?1049halt\\u001b[?1049lfinal\"]\n";
        let cast = Cast::read(&bytes[..]).unwrap();
        let mut sequential = Replay::new(8, 2, 10);
        let mut timeline = Timeline::new(cast.clone());
        for time in [0., 1., 2., 4., 6., 8.] {
            let (_, range) = timeline.advance(time, false);
            for event in &cast.events[range] {
                sequential.event(event);
            }
            let mut seek = Timeline::new(cast.clone());
            let (_, range) = seek.advance(time, true);
            let mut sought = Replay::new(8, 2, 10);
            for event in &cast.events[range] {
                sought.event(event);
            }
            assert_eq!(sequential.times.rows, sought.times.rows);
            assert_eq!(sequential.times.primary, sought.times.primary);
            assert_eq!(
                crate::replay::screen_text(&sequential.term),
                crate::replay::screen_text(&sought.term)
            );
        }
    }

    #[test]
    fn resize_reflows_times_and_alt_screen_does_not_pollute_primary() {
        let mut r = Replay::new(8, 3, 10);
        r.feed(1., "abcdefghijk\r\n");
        r.feed(2., "last");
        r.feed(3., "\x1b[?1049halt\x1b[?1049l");
        assert_eq!(r.first(0), Some(1.));
        assert_eq!(r.first(1), None);
        assert_eq!(r.first(2), Some(2.));
        r.times.resize(
            &mut r.term,
            crate::TerminalSize {
                cols: 16,
                rows: 3,
                ..Default::default()
            },
        );
        assert_eq!(r.first(0), Some(1.));
        assert_eq!(r.first(1), Some(2.));
    }

    #[test]
    fn resize_during_alt_screen_preserves_hidden_primary_line_times() {
        let mut r = Replay::new(8, 3, 10);
        r.feed(1., "abcdefghijk\r\n");
        r.feed(2., "last");
        let original_id = r.times.rows[0].id;
        r.feed(3., "\x1b[?1049halt");
        r.times.resize(
            &mut r.term,
            crate::TerminalSize {
                cols: 16,
                rows: 4,
                ..Default::default()
            },
        );
        r.feed(4., "\x1b[?1049l");
        assert_eq!(r.first(0), Some(1.));
        assert_eq!(r.first(1), Some(2.));
        assert_eq!(r.times.rows[0].id, original_id);
        assert_eq!(r.times.rows.len(), r.term.total_lines());
        r.feed(5., "\r\nnext");
        assert_eq!(r.first(2), Some(5.));
    }

    #[test]
    fn scrolling_regions_move_times_with_their_rows() {
        let mut r = Replay::new(10, 4, 10);
        for n in 0..4 {
            r.feed(n as f64, &format!("\x1b[{};1Hrow-{n}", n + 1));
        }
        r.feed(9., "\x1b[2;4r\x1b[4;1H\nnew");
        assert_eq!(
            (r.first(0), r.first(1), r.first(2), r.first(3)),
            (Some(0.), Some(2.), Some(3.), Some(9.))
        );
        r.feed(10., "\x1b[2;1H\x1b[Linsert");
        assert_eq!(
            (r.first(1), r.first(2), r.first(3)),
            (Some(10.), Some(2.), Some(3.))
        );
    }
}

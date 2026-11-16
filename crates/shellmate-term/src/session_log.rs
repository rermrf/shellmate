//! Finalized primary-screen lines. A bounded shadow emulator follows the same byte stream.
use crate::archive;
use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    index::{Column, Line},
    term::{Config, Term, TermDamage, TermMode, cell::Flags},
    vte::ansi::{ClearMode, Handler, Processor},
};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
#[derive(Clone)]
pub(crate) struct Silent;
impl EventListener for Silent {
    fn send_event(&self, _: Event) {}
}
#[derive(Clone, Copy)]
pub(crate) struct Size {
    pub cols: usize,
    pub rows: usize,
}
impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}
#[derive(Default)]
struct Boundaries {
    clear: bool,
    reset: bool,
    erased: bool,
}
impl Handler for Boundaries {
    fn clear_screen(&mut self, mode: ClearMode) {
        self.clear = matches!(mode, ClearMode::All | ClearMode::Saved);
        self.erased = matches!(mode, ClearMode::All);
    }
    fn reset_state(&mut self) {
        self.reset = true;
    }
}
/// A finalized line carries its last content-change time, independent of when it is written out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub text: String,
    pub written_at: i64,
}
impl LogLine {
    pub fn new(text: impl Into<String>, written_at: i64) -> Self {
        Self {
            text: text.into(),
            written_at,
        }
    }
}

#[derive(Clone, Default)]
struct RowStamp {
    text: String,
    wrapped: bool,
    written_at: i64,
}
impl RowStamp {
    fn update(&mut self, text: String, wrapped: bool, now: i64) {
        if self.text != text {
            self.text = text;
            self.written_at = now;
        }
        self.wrapped = wrapped;
    }
}

pub struct LineTracker {
    term: Term<Silent>,
    parser: Processor,
    observer: Processor,
    boundaries: Boundaries,
    pending: String,
    pending_at: Option<i64>,
    rows: VecDeque<RowStamp>,
    suppressed: VecDeque<Option<String>>,
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
        if !cell.c.is_control() {
            text.push(cell.c);
        }
        if let Some(extra) = cell.zerowidth() {
            text.extend(extra.iter().filter(|c| !c.is_control()));
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
impl LineTracker {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = Size {
            cols: cols as usize,
            rows: rows as usize,
        };
        Self {
            term: Term::new(
                Config {
                    scrolling_history: 4096,
                    ..Default::default()
                },
                &size,
                Silent,
            ),
            parser: Processor::new(),
            observer: Processor::new(),
            boundaries: Boundaries::default(),
            pending: String::new(),
            pending_at: None,
            rows: vec![RowStamp::default(); rows as usize].into(),
            suppressed: VecDeque::new(),
        }
    }
    pub fn seed<T>(&mut self, term: &Term<T>) {
        if term.mode().contains(TermMode::ALT_SCREEN) {
            self.term.swap_alt();
        }
        *self.term.grid_mut() = term.grid().clone();
        self.term.grid_mut().clear_history();
        self.rows = vec![RowStamp::default(); self.term.screen_lines()].into();
        self.refresh_rows(archive::now());
    }
    /// Only compare damaged rows; cursor motion and color changes leave timestamps untouched.
    fn refresh_rows(&mut self, now: i64) {
        let lines: Vec<_> = match self.term.damage() {
            TermDamage::Full => (0..self.rows.len()).collect(),
            TermDamage::Partial(lines) => lines.map(|l| l.line).collect(),
        };
        self.term.reset_damage();
        for line in lines {
            if line >= self.rows.len() {
                continue;
            }
            let (text, wrapped) = row_text(&self.term, Line(line as i32));
            self.rows[line].update(text, wrapped, now);
        }
    }
    fn line(&mut self, text: String, wrap: bool, skip: bool, written_at: i64, out: &mut Vec<LogLine>) {
        if !skip {
            self.pending.push_str(&text);
            if !text.is_empty() {
                self.pending_at = Some(self.pending_at.map_or(written_at, |t| t.max(written_at)));
            }
        }
        if !wrap && !self.pending.is_empty() {
            if !self.pending.trim().is_empty() {
                out.push(LogLine::new(
                    std::mem::take(&mut self.pending),
                    self.pending_at.take().unwrap_or(0),
                ));
            } else {
                self.pending.clear();
                self.pending_at = None;
            }
        }
    }
    fn screen(&mut self) -> Vec<LogLine> {
        let mut out = vec![];
        let mut marked = VecDeque::new();
        for line in 0..self.term.screen_lines() {
            let (text, wrap) = row_text(&self.term, Line(line as i32));
            let skip = self.suppressed.get(line).and_then(|s| s.as_ref()) == Some(&text);
            marked.push_back(Some(text.clone()));
            self.line(text, wrap, skip, self.rows[line].written_at, &mut out);
        }
        if !self.pending.trim().is_empty() {
            out.push(LogLine::new(
                std::mem::take(&mut self.pending),
                self.pending_at.take().unwrap_or(0),
            ));
        }
        self.suppressed = marked;
        out
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<LogLine> {
        self.feed_at(bytes, archive::now())
    }
    /// Injectable wall clock. Finalization never consults it; only changed content does.
    pub fn feed_at(&mut self, bytes: &[u8], now: i64) -> Vec<LogLine> {
        let mut out = vec![];
        for byte in bytes {
            self.observer
                .advance(&mut self.boundaries, std::slice::from_ref(byte));
            let clear = std::mem::take(&mut self.boundaries.clear);
            let erased = std::mem::take(&mut self.boundaries.erased);
            let reset = std::mem::take(&mut self.boundaries.reset);
            let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
            if reset && alt {
                self.term.swap_alt();
                out.extend(self.screen());
                self.term.swap_alt();
            } else if !alt && (clear || reset) {
                out.extend(self.screen());
            }
            self.parser.advance(&mut self.term, std::slice::from_ref(byte));
            if !self.term.mode().contains(TermMode::ALT_SCREEN) {
                let n = self.term.grid().history_size();
                if !clear && !reset {
                    for index in -(n as i32)..0 {
                        let (text, wrap) = row_text(&self.term, Line(index));
                        let skip = self.suppressed.pop_front().flatten().as_ref() == Some(&text);
                        let mut stamp = self.rows.pop_front().unwrap_or_default();
                        stamp.update(text.clone(), wrap, now);
                        self.rows.push_back(RowStamp::default());
                        self.line(text, wrap, skip, stamp.written_at, &mut out);
                    }
                }
                self.term.grid_mut().clear_history();
                if reset || erased {
                    self.suppressed.clear();
                    self.rows = vec![RowStamp::default(); self.term.screen_lines()].into();
                }
                self.refresh_rows(now);
            }
        }
        out
    }
    pub fn resize(&mut self, cols: u16, rows: u16) -> Vec<LogLine> {
        // Reflow moves characters, not their write times. Match logical lines before/after resizing
        // so shrinking, growing, and soft-wrap changes do not stamp old output with resize time.
        let mut previous = VecDeque::new();
        let mut text = String::new();
        let mut time = 0;
        for row in &self.rows {
            text.push_str(&row.text);
            time = time.max(row.written_at);
            if !row.wrapped {
                previous.push_back((std::mem::take(&mut text), time));
                time = 0;
            }
        }
        if !text.is_empty() {
            previous.push_back((text, time));
        }
        self.term.resize(Size {
            cols: cols as usize,
            rows: rows as usize,
        });
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            // Resize the hidden primary grid, inspecting only our private shadow emulator.
            self.term.swap_alt();
            let result = self.reflow_stamps(previous);
            self.term.swap_alt();
            return result;
        }
        self.reflow_stamps(previous)
    }
    fn reflow_stamps(&mut self, mut previous: VecDeque<(String, i64)>) -> Vec<LogLine> {
        let history = self.term.grid().history_size();
        let mut stamps = VecDeque::new();
        let mut logical = String::new();
        let mut group = Vec::new();
        for line in -(history as i32)..self.term.screen_lines() as i32 {
            let (text, wrapped) = row_text(&self.term, Line(line));
            logical.push_str(&text);
            group.push((text, wrapped));
            if !wrapped || line == self.term.screen_lines() as i32 - 1 {
                // A resize can discard blank rows, so align to the next identical logical line.
                let time = previous
                    .iter()
                    .position(|(text, _)| text.trim_end() == logical.trim_end())
                    .and_then(|i| previous.drain(..=i).next_back())
                    .map_or(0, |(_, time)| time);
                stamps.extend(group.drain(..).map(|(text, wrapped)| RowStamp {
                    text,
                    wrapped,
                    written_at: time,
                }));
                logical.clear();
            }
        }
        let mut out = vec![];
        for _ in 0..history {
            if let Some(row) = stamps.pop_front() {
                self.line(row.text, row.wrapped, false, row.written_at, &mut out);
            }
        }
        self.rows = stamps;
        self.term.grid_mut().clear_history();
        self.term.reset_damage();
        out
    }
    pub fn finish(&mut self) -> Vec<LogLine> {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            self.term.swap_alt();
        }
        self.screen()
    }
}
pub struct LogWriter {
    root: PathBuf,
    host: String,
    date: String,
    writer: Option<BufWriter<File>>,
}
impl LogWriter {
    pub fn new(root: PathBuf, host: &str) -> Self {
        Self {
            root,
            host: archive::safe_label(host),
            date: String::new(),
            writer: None,
        }
    }
    pub fn lines(&mut self, lines: &[LogLine]) -> io::Result<()> {
        for line in lines {
            let date = archive::local_time(line.written_at, "%Y-%m-%d");
            if self.date != date {
                self.flush()?;
                let dir = self.root.join(&self.host);
                archive::create_dir(&dir)?;
                self.writer = Some(BufWriter::new(archive::open_file(
                    &dir.join(format!("{date}.log")),
                    true,
                )?));
                self.date = date;
            }
            let time = archive::local_time(line.written_at, "%H:%M:%S");
            writeln!(self.writer.as_mut().expect("opened"), "{time}  {}", line.text)?;
        }
        Ok(())
    }
    pub fn flush(&mut self) -> io::Result<()> {
        if let Some(w) = &mut self.writer {
            w.flush()?;
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct LogFile {
    pub host: String,
    pub date: String,
    pub path: PathBuf,
    pub size: u64,
}
pub fn list(root: &Path) -> io::Result<Vec<LogFile>> {
    let mut result = vec![];
    if !root.exists() {
        return Ok(result);
    }
    for dir in std::fs::read_dir(root)? {
        let dir = dir?;
        if !dir.file_type()?.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(dir.path())? {
            let file = file?;
            if file.file_type()?.is_file() && file.path().extension().is_some_and(|s| s == "log") {
                result.push(LogFile {
                    host: dir.file_name().to_string_lossy().into_owned(),
                    date: file.file_name().to_string_lossy().into_owned(),
                    path: file.path(),
                    size: file.metadata()?.len(),
                });
            }
        }
    }
    result.sort_by(|a, b| b.date.cmp(&a.date).then(a.host.cmp(&b.host)));
    Ok(result)
}
pub fn tail(path: &Path) -> io::Result<(Vec<String>, bool)> {
    const LIMIT: u64 = 5 * 1024 * 1024;
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let truncated = length > LIMIT;
    if truncated {
        file.seek(SeekFrom::End(-(LIMIT as i64)))?;
    }
    let mut bytes = Vec::new();
    file.take(LIMIT).read_to_end(&mut bytes)?;
    if truncated && let Some(i) = bytes.iter().position(|b| *b == b'\n') {
        bytes.drain(..=i);
    }
    Ok((
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::to_string)
            .collect(),
        truncated,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn texts(lines: &[LogLine]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }
    #[test]
    fn scroll_clear_progress_alt_and_wrap() {
        let mut t = LineTracker::new(12, 3);
        let mut lines = t.feed(b"first\r\nsecond\r\nthird\r\nfourth");
        assert_eq!(texts(&lines), ["first"]);
        lines.extend(t.feed(b"\r\nprogress 1%\rprogress 99%\r\n\x1b[2J\x1b[H"));
        lines.extend(t.feed(b"after clear\x1b[?1049hALT SECRET\r\n\x1b[2J\x1b[?1049l"));
        lines.extend(t.finish());
        assert_eq!(
            texts(&lines),
            [
                "first",
                "second",
                "third",
                "fourth",
                "progress 99%",
                "after clear"
            ]
        );
    }
    #[test]
    fn wrapped_lines_join_across_scrolloff() {
        let mut t = LineTracker::new(8, 2);
        let mut rows = t.feed(b"abcdefghijklmnopqrstuvwx\r\nlast\r\n");
        rows.extend(t.finish());
        assert_eq!(texts(&rows), ["abcdefghijklmnopqrstuvwx", "last"]);
    }
    #[test]
    fn saved_clear_then_viewport_clear_is_not_duplicated() {
        for end in ["\x1b[3J\x1b[2J\x1b[H", "\x1bc"] {
            let mut t = LineTracker::new(40, 3);
            let mut rows = t.feed(format!("line one\r\nline two{end}").as_bytes());
            rows.extend(t.finish());
            assert_eq!(texts(&rows), ["line one", "line two"]);
        }
    }
    #[test]
    fn a_new_identical_line_after_clear_is_logged_again() {
        let mut t = LineTracker::new(40, 3);
        let mut rows = t.feed(b"same\x1b[2J\x1b[Hsame");
        rows.extend(t.finish());
        assert_eq!(texts(&rows), ["same", "same"]);
    }

    #[test]
    fn close_in_alt_keeps_only_primary() {
        let mut t = LineTracker::new(40, 3);
        t.feed(b"primary\x1b[?1049hsecret alternate");
        assert_eq!(texts(&t.finish()), ["primary"]);
    }
    #[test]
    fn tail_limits_bytes_and_drops_partial_first_line() {
        let dir = std::env::temp_dir().join(format!(
            "shellmate-log-tail-{}",
            shellmate_core::HostId::generate()
        ));
        archive::create_dir(&dir).unwrap();
        let path = dir.join("large.log");
        let mut file = archive::open_file(&path, false).unwrap();
        file.write_all(&vec![b'x'; 5 * 1024 * 1024 + 17]).unwrap();
        file.write_all(b"\nlast line\n").unwrap();
        drop(file);
        let (lines, truncated) = tail(&path).unwrap();
        assert!(truncated);
        assert_eq!(lines, ["last line"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn clears_split_across_chunks_and_osc_content_are_distinguished() {
        let mut t = LineTracker::new(40, 3);
        let mut rows = vec![];
        for byte in b"first\x1b]0;title\x07\r\nsecond\x1b[2J\x1b[Hafter" {
            rows.extend(t.feed(&[*byte]));
        }
        rows.extend(t.finish());
        assert_eq!(texts(&rows), ["first", "second", "after"]);
    }

    #[test]
    fn scroll_and_close_keep_the_times_content_was_written() {
        let mut tracker = LineTracker::new(40, 2);
        assert!(tracker.feed_at(b"first\r\n", 100).is_empty());
        let lines = tracker.feed_at(b"second\r\n", 110);
        assert_eq!(lines, [LogLine::new("first", 100)]);
        tracker.feed_at(b"third", 120);
        assert_eq!(
            tracker.finish(),
            [LogLine::new("second", 110), LogLine::new("third", 120)]
        );
    }
    #[test]
    fn progress_uses_final_redraw_time_not_newline_or_close() {
        let mut tracker = LineTracker::new(40, 3);
        tracker.feed_at(b"progress 1%", 100);
        tracker.feed_at(b"\rprogress 50%", 110);
        tracker.feed_at(b"\rprogress 99%", 120);
        tracker.feed_at(b"\r\n", 130);
        assert_eq!(tracker.finish(), [LogLine::new("progress 99%", 120)]);
    }
    #[test]
    fn clear_preserves_per_line_times_and_cursor_or_color_changes_are_not_writes() {
        let mut tracker = LineTracker::new(40, 3);
        tracker.feed_at(b"first\r\n", 100);
        tracker.feed_at(b"second", 110);
        tracker.feed_at(b"\r\x1b[31m\x1b[0m", 120);
        let lines = tracker.feed_at(b"\x1b[3J\x1b[2J\x1b[H", 150);
        assert_eq!(lines, [LogLine::new("first", 100), LogLine::new("second", 110)]);
        tracker.feed_at(b"after", 160);
        assert_eq!(tracker.finish(), [LogLine::new("after", 160)]);
    }
    #[test]
    fn wrapped_lines_keep_the_latest_changed_row_time() {
        let mut tracker = LineTracker::new(8, 2);
        tracker.feed_at(b"abcdefgh", 100);
        tracker.feed_at(b"ijklmnop", 110);
        tracker.feed_at(b"qrstuvwx", 120);
        let lines = tracker.feed_at(b"\r\nlast\r\n", 130);
        assert_eq!(lines, [LogLine::new("abcdefghijklmnopqrstuvwx", 120)]);
        assert_eq!(tracker.finish(), [LogLine::new("last", 130)]);
    }
    #[test]
    fn alternate_screen_and_resize_do_not_change_primary_write_times() {
        let mut tracker = LineTracker::new(20, 3);
        tracker.feed_at(b"first\r\nsecond", 100);
        tracker.feed_at(b"\x1b[?1049hsecret\r\nmore\x1b[?1049l", 200);
        assert!(tracker.resize(8, 5).is_empty());
        assert_eq!(
            tracker.finish(),
            [LogLine::new("first", 100), LogLine::new("second", 100)]
        );
    }
    #[test]
    fn identical_rows_keep_separate_times_when_scrolled_and_resized() {
        let mut tracker = LineTracker::new(12, 3);
        tracker.feed_at(b"same\r\n", 100);
        tracker.feed_at(b"same\r\n", 110);
        let mut lines = tracker.feed_at(b"last\r\n", 120);
        lines.extend(tracker.resize(6, 4));
        lines.extend(tracker.finish());
        assert_eq!(
            lines,
            [
                LogLine::new("same", 100),
                LogLine::new("same", 110),
                LogLine::new("last", 120)
            ]
        );
    }
    #[test]
    fn reflowed_wraps_keep_write_time_in_both_layouts() {
        let mut tracker = LineTracker::new(8, 3);
        tracker.feed_at(b"abcdefgh", 100);
        tracker.feed_at(b"ijklmnop\r\nlast", 110);
        let mut lines = tracker.resize(20, 2);
        lines.extend(tracker.resize(6, 5));
        lines.extend(tracker.finish());
        assert_eq!(
            lines,
            [LogLine::new("abcdefghijklmnop", 110), LogLine::new("last", 110)]
        );
    }

    #[test]
    fn finalized_batch_is_archived_by_each_lines_write_date() {
        let dir = std::env::temp_dir().join(format!(
            "shellmate-log-dates-{}",
            shellmate_core::HostId::generate()
        ));
        let mut tracker = LineTracker::new(40, 3);
        tracker.feed_at(b"before\r\n", 1700000000);
        tracker.feed_at(b"after", 1700086400);
        let mut writer = LogWriter::new(dir.clone(), "host");
        writer.lines(&tracker.finish()).unwrap();
        writer.flush().unwrap();
        for (time, text) in [(1700000000, "before"), (1700086400, "after")] {
            let path = dir
                .join("host")
                .join(format!("{}.log", archive::local_time(time, "%Y-%m-%d")));
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                format!("{}  {text}\n", archive::local_time(time, "%H:%M:%S"))
            );
        }
        drop(writer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rollover_append_and_close_flush() {
        let dir = std::env::temp_dir().join(format!("shellmate-log-{}", shellmate_core::HostId::generate()));
        let mut w = LogWriter::new(dir.clone(), "../host");
        w.lines(&[LogLine::new("before", 1700000000)]).unwrap();
        w.lines(&[LogLine::new("after", 1700086400)]).unwrap();
        w.flush().unwrap();
        let files = list(&dir).unwrap();
        assert_eq!(files.len(), 2);
        assert!(tail(&files[0].path).unwrap().0[0].ends_with("  after"));
        drop(w);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

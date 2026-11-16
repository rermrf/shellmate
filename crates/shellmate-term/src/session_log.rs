//! Finalized primary-screen lines. A bounded shadow emulator follows the same byte stream.
use crate::archive;
use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    index::{Column, Line},
    term::{Config, Term, TermMode, cell::Flags},
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
pub struct LineTracker {
    term: Term<Silent>,
    parser: Processor,
    observer: Processor,
    boundaries: Boundaries,
    pending: String,
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
            suppressed: VecDeque::new(),
        }
    }
    pub fn seed<T>(&mut self, term: &Term<T>) {
        if term.mode().contains(TermMode::ALT_SCREEN) {
            self.term.swap_alt();
        }
        *self.term.grid_mut() = term.grid().clone();
        self.term.grid_mut().clear_history();
    }
    fn line(&mut self, text: String, wrap: bool, skip: bool, out: &mut Vec<String>) {
        if !skip {
            self.pending.push_str(&text);
        }
        if !wrap && !self.pending.is_empty() {
            if !self.pending.trim().is_empty() {
                out.push(std::mem::take(&mut self.pending));
            } else {
                self.pending.clear();
            }
        }
    }
    fn screen(&mut self) -> Vec<String> {
        let mut out = vec![];
        let mut marked = VecDeque::new();
        for line in 0..self.term.screen_lines() {
            let (text, wrap) = row_text(&self.term, Line(line as i32));
            let skip = self.suppressed.get(line).and_then(|s| s.as_ref()) == Some(&text);
            marked.push_back(Some(text.clone()));
            self.line(text, wrap, skip, &mut out);
        }
        if !self.pending.trim().is_empty() {
            out.push(std::mem::take(&mut self.pending));
        }
        self.suppressed = marked;
        out
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
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
                        self.line(text, wrap, skip, &mut out);
                    }
                }
                self.term.grid_mut().clear_history();
                if reset || erased {
                    self.suppressed.clear();
                }
            }
        }
        out
    }
    pub fn resize(&mut self, cols: u16, rows: u16) -> Vec<String> {
        self.term.resize(Size {
            cols: cols as usize,
            rows: rows as usize,
        });
        let mut out = vec![];
        if !self.term.mode().contains(TermMode::ALT_SCREEN) {
            let n = self.term.grid().history_size();
            for line in -(n as i32)..0 {
                let (text, wrap) = row_text(&self.term, Line(line));
                self.line(text, wrap, false, &mut out);
            }
            self.term.grid_mut().clear_history();
        }
        out
    }
    pub fn finish(&mut self) -> Vec<String> {
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
    pub fn lines(&mut self, lines: &[String], unix: i64) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let date = archive::local_time(unix, "%Y-%m-%d");
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
        let time = archive::local_time(unix, "%H:%M:%S");
        for line in lines {
            writeln!(self.writer.as_mut().expect("opened"), "{time}  {line}")?;
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
    #[test]
    fn scroll_clear_progress_alt_and_wrap() {
        let mut t = LineTracker::new(12, 3);
        let mut lines = t.feed(b"first\r\nsecond\r\nthird\r\nfourth");
        assert_eq!(lines, ["first"]);
        lines.extend(t.feed(b"\r\nprogress 1%\rprogress 99%\r\n\x1b[2J\x1b[H"));
        lines.extend(t.feed(b"after clear\x1b[?1049hALT SECRET\r\n\x1b[2J\x1b[?1049l"));
        lines.extend(t.finish());
        assert_eq!(
            lines,
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
        assert_eq!(rows, ["abcdefghijklmnopqrstuvwx", "last"]);
    }
    #[test]
    fn saved_clear_then_viewport_clear_is_not_duplicated() {
        for end in ["\x1b[3J\x1b[2J\x1b[H", "\x1bc"] {
            let mut t = LineTracker::new(40, 3);
            let mut rows = t.feed(format!("line one\r\nline two{end}").as_bytes());
            rows.extend(t.finish());
            assert_eq!(rows, ["line one", "line two"]);
        }
    }
    #[test]
    fn a_new_identical_line_after_clear_is_logged_again() {
        let mut t = LineTracker::new(40, 3);
        let mut rows = t.feed(b"same\x1b[2J\x1b[Hsame");
        rows.extend(t.finish());
        assert_eq!(rows, ["same", "same"]);
    }

    #[test]
    fn close_in_alt_keeps_only_primary() {
        let mut t = LineTracker::new(40, 3);
        t.feed(b"primary\x1b[?1049hsecret alternate");
        assert_eq!(t.finish(), ["primary"]);
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
        assert_eq!(rows, ["first", "second", "after"]);
    }

    #[test]
    fn rollover_append_and_close_flush() {
        let dir = std::env::temp_dir().join(format!("shellmate-log-{}", shellmate_core::HostId::generate()));
        let mut w = LogWriter::new(dir.clone(), "../host");
        w.lines(&["before".into()], 1700000000).unwrap();
        w.lines(&["after".into()], 1700086400).unwrap();
        w.flush().unwrap();
        let files = list(&dir).unwrap();
        assert_eq!(files.len(), 2);
        assert!(tail(&files[0].path).unwrap().0[0].ends_with("  after"));
        drop(w);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

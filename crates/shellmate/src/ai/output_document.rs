//! Immutable, disk-backed readable output. Indexing/reading/copying run off the UI thread.
use shellmate_core::ai_output::{self, Completeness, SnapshotInfo};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    ops::Range,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use unicode_width::UnicodeWidthChar;

#[path = "output_document/clean.rs"]
mod clean;

pub type Cancel = Arc<AtomicBool>;
/// Invalidated synchronously when the conversation changes, even if its last frame retains views.
#[derive(Default)]
pub struct Lifetime(pub Cancel);
impl Drop for Lifetime {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
pub fn check(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
    } else {
        Ok(())
    }
}
#[derive(Clone, Copy, Debug)]
struct Mark {
    row: usize,
    column: usize,
    byte: u64,
}
pub struct Document {
    file: tempfile::NamedTempFile,
    marks: Vec<Mark>,
    pub rows: usize,
    pub columns: usize,
    pub bytes: u64,
    pub info: SnapshotInfo,
    clean_boundary: bool,
}
#[derive(Clone, Debug)]
pub struct Segment {
    pub row: usize,
    pub column: usize,
    pub text: String,
    /// Rendered-text byte offsets mapped to document bytes (tabs expand only for painting).
    pub carets: Vec<(usize, u64)>,
}
impl Segment {
    pub fn selected_range(&self, range: &Range<u64>) -> Option<Range<usize>> {
        let first = self.carets.first()?;
        let last = self.carets.last()?;
        if range.end <= first.1 || range.start >= last.1 {
            return None;
        }
        let start = self
            .carets
            .iter()
            .find(|(_, b)| *b >= range.start)
            .unwrap_or(last)
            .0;
        let end = self
            .carets
            .iter()
            .find(|(_, b)| *b >= range.end)
            .unwrap_or(last)
            .0;
        (end > start).then_some(start..end)
    }
}
fn width(c: char, column: usize) -> usize {
    if c == '\t' {
        4 - column % 4
    } else {
        c.width().unwrap_or(0)
    }
}
impl Document {
    pub fn refresh(
        root: &Path,
        conversation: &str,
        id: &str,
        previous: Option<&Arc<Self>>,
        cancel: &AtomicBool,
    ) -> io::Result<Arc<Self>> {
        if let Some(previous) = previous {
            let info = ai_output::inspect(root, conversation, id)?;
            if info == previous.info {
                return Ok(previous.clone());
            }
            if previous.clean_boundary && previous.info.completeness == Completeness::Complete {
                let mut raw = tempfile::tempfile()?;
                if let Some(info) = ai_output::snapshot_since(
                    root,
                    conversation,
                    id,
                    previous.info.retained_bytes,
                    &mut raw,
                )? {
                    check(cancel)?;
                    raw.rewind()?;
                    let file = tempfile::NamedTempFile::new()?;
                    std::fs::copy(previous.file.path(), file.path())?;
                    let mut writer = BufWriter::new(file.reopen()?);
                    writer.seek(SeekFrom::End(0))?;
                    let mut build = Builder {
                        writer,
                        marks: previous.marks.clone(),
                        row: previous.rows,
                        column: 0,
                        bytes: previous.bytes,
                        columns: previous.columns,
                        last_column: 0,
                        newline: previous.bytes > 0,
                    };
                    let clean_boundary = clean::normalize(raw, &mut build, cancel)?;
                    build.writer.flush()?;
                    check(cancel)?;
                    return Ok(Arc::new(Self {
                        file,
                        marks: build.marks,
                        rows: build.row + usize::from(!build.newline && build.bytes > 0),
                        columns: build.columns,
                        bytes: build.bytes,
                        info,
                        clean_boundary,
                    }));
                }
            }
        }
        Self::load(root, conversation, id, cancel)
    }
    pub fn load(
        root: &Path,
        conversation: &str,
        id: &str,
        cancel: &AtomicBool,
    ) -> io::Result<Arc<Self>> {
        check(cancel)?;
        let mut raw = tempfile::tempfile()?;
        let info = ai_output::snapshot(root, conversation, id, &mut raw)?;
        raw.rewind()?;
        Self::from_reader(raw, info, cancel)
    }
    pub fn inline(text: &str, complete: bool, cancel: &AtomicBool) -> io::Result<Arc<Self>> {
        Self::from_reader(
            text.as_bytes(),
            SnapshotInfo {
                total_bytes: text.len() as u64,
                retained_bytes: text.len() as u64,
                completeness: if complete {
                    Completeness::Complete
                } else {
                    Completeness::Unknown
                },
                finished: true,
            },
            cancel,
        )
    }
    fn from_reader(
        reader: impl Read,
        info: SnapshotInfo,
        cancel: &AtomicBool,
    ) -> io::Result<Arc<Self>> {
        let file = tempfile::NamedTempFile::new()?;
        let mut build = Builder {
            writer: BufWriter::new(file.reopen()?),
            marks: vec![Mark {
                row: 0,
                column: 0,
                byte: 0,
            }],
            row: 0,
            column: 0,
            bytes: 0,
            columns: 0,
            last_column: 0,
            newline: false,
        };
        let clean_boundary = clean::normalize(reader, &mut build, cancel)?;
        build.writer.flush()?;
        check(cancel)?;
        Ok(Arc::new(Self {
            file,
            marks: build.marks,
            rows: build.row + usize::from(!build.newline && build.bytes > 0),
            columns: build.columns,
            bytes: build.bytes,
            info,
            clean_boundary,
        }))
    }
    /// A bounded horizontal slice. Even a 64 MiB line never enters a text shaper whole.
    pub fn segment(&self, row: usize, column: usize, columns: usize) -> io::Result<Segment> {
        let mut result = Segment {
            row,
            column: 0,
            text: String::new(),
            carets: vec![],
        };
        if row >= self.rows {
            return Ok(result);
        }
        let mark = self.marks[self
            .marks
            .partition_point(|m| (m.row, m.column) <= (row, column))
            .saturating_sub(1)];
        let mut file = BufReader::new(self.file.reopen()?);
        file.seek(SeekFrom::Start(mark.byte))?;
        let mut offset = mark.byte;
        for _ in mark.row..row {
            offset += file.skip_until(b'\n')? as u64;
        }
        let mut col = if mark.row == row { mark.column } else { 0 };
        let end = column.saturating_add(columns.max(1));
        while let Some(c) = read_char(&mut file)? {
            if c == '\n' {
                break;
            }
            let n = width(c, col);
            if col + n >= column {
                if result.carets.is_empty() {
                    result.column = col;
                }
                result.carets.push((result.text.len(), offset));
                if c == '\t' {
                    result.text.extend(std::iter::repeat_n(' ', n));
                } else {
                    result.text.push(c);
                }
            }
            offset += c.len_utf8() as u64;
            col += n;
            if col >= end {
                break;
            }
        }
        result.carets.push((result.text.len(), offset));
        Ok(result)
    }
    pub fn region(
        &self,
        rows: &[usize],
        column: usize,
        columns: usize,
        cancel: &AtomicBool,
    ) -> io::Result<Vec<Segment>> {
        rows.iter()
            .map(|row| {
                check(cancel)?;
                self.segment(*row, column, columns)
            })
            .collect()
    }
    pub fn copy(&self, range: Range<u64>, cancel: &AtomicBool) -> io::Result<String> {
        check(cancel)?;
        let start = range.start.min(self.bytes);
        let end = range.end.min(self.bytes).max(start);
        let mut file = self.file.reopen()?;
        file.seek(SeekFrom::Start(start))?;
        let mut text = Vec::with_capacity((end - start).min(1024 * 1024) as usize);
        let mut buffer = [0; 65536];
        let mut left = end - start;
        while left > 0 {
            check(cancel)?;
            let n = file.read(&mut buffer[..left.min(65536) as usize])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "output snapshot incomplete",
                ));
            }
            text.extend_from_slice(&buffer[..n]);
            left -= n as u64;
        }
        String::from_utf8(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
    pub fn visible_row(&self, row: usize, expanded: bool) -> Option<usize> {
        if expanded || self.rows <= 8 {
            (row < self.rows).then_some(row)
        } else {
            match row {
                0..=2 => Some(row),
                3 => None,
                4..=7 => Some(self.rows - 8 + row),
                _ => None,
            }
        }
    }
}
fn read_char(reader: &mut impl Read) -> io::Result<Option<char>> {
    let mut bytes = [0; 4];
    if reader.read(&mut bytes[..1])? == 0 {
        return Ok(None);
    }
    let size = match bytes[0] {
        0..=127 => 1,
        192..=223 => 2,
        224..=239 => 3,
        _ => 4,
    };
    reader.read_exact(&mut bytes[1..size])?;
    Ok(std::str::from_utf8(&bytes[..size])
        .ok()
        .and_then(|s| s.chars().next()))
}
struct Builder {
    writer: BufWriter<File>,
    marks: Vec<Mark>,
    row: usize,
    column: usize,
    bytes: u64,
    columns: usize,
    last_column: usize,
    newline: bool,
}
impl Builder {
    fn character(&mut self, c: char) -> io::Result<()> {
        if (self.column == 0 && self.row.is_multiple_of(128))
            || self.column.saturating_sub(self.last_column) >= 512
        {
            if self.marks.last().is_none_or(|m| m.byte != self.bytes) {
                self.marks.push(Mark {
                    row: self.row,
                    column: self.column,
                    byte: self.bytes,
                });
            }
            self.last_column = self.column;
        }
        let mut buf = [0; 4];
        self.writer.write_all(c.encode_utf8(&mut buf).as_bytes())?;
        self.bytes += c.len_utf8() as u64;
        self.newline = c == '\n';
        if self.newline {
            self.row += 1;
            self.column = 0;
            self.last_column = 0;
        } else {
            self.column += width(c, self.column);
            self.columns = self.columns.max(self.column);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "output_document/tests.rs"]
mod tests;

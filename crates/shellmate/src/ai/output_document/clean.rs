//! Streaming ANSI/UTF-8/CR normalization. Very long logical lines spill to a temporary file.
use super::{Builder, check};
use std::{
    fs::File,
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    sync::atomic::AtomicBool,
};

#[derive(Default)]
enum Escape {
    #[default]
    Text,
    Esc,
    Csi,
    String,
    StringEsc,
    Intermediate,
    C1String,
}
#[derive(Default)]
struct Line {
    small: Vec<char>,
    disk: Option<BufWriter<File>>,
    cursor: usize,
    len: usize,
}
impl Line {
    fn push(&mut self, c: char) -> io::Result<()> {
        if self.disk.is_none() && self.small.len() >= 16384 {
            let mut file = BufWriter::new(tempfile::tempfile()?);
            for c in self.small.drain(..) {
                file.write_all(&(c as u32).to_le_bytes())?;
            }
            file.seek(SeekFrom::Start(self.cursor as u64 * 4))?;
            self.disk = Some(file);
        }
        if let Some(file) = &mut self.disk {
            file.write_all(&(c as u32).to_le_bytes())?;
        } else if self.cursor < self.small.len() {
            self.small[self.cursor] = c;
        } else {
            self.small.push(c);
        }
        self.cursor += 1;
        self.len = self.len.max(self.cursor);
        Ok(())
    }
    fn carriage_return(&mut self) -> io::Result<()> {
        self.cursor = 0;
        if let Some(file) = &mut self.disk {
            file.rewind()?;
        }
        Ok(())
    }
    fn emit(&mut self, build: &mut Builder, cancel: &AtomicBool) -> io::Result<()> {
        if let Some(mut file) = self.disk.take() {
            file.flush()?;
            file.rewind()?;
            let mut reader = BufReader::new(file.into_inner().map_err(|e| e.into_error())?);
            let mut buf = [0; 4];
            for i in 0..self.len {
                if i.is_multiple_of(16384) {
                    check(cancel)?;
                }
                reader.read_exact(&mut buf)?;
                build.character(char::from_u32(u32::from_le_bytes(buf)).unwrap_or('\u{fffd}'))?;
            }
        } else {
            for c in self.small.drain(..) {
                build.character(c)?;
            }
        }
        self.cursor = 0;
        self.len = 0;
        Ok(())
    }
}
#[derive(Default)]
struct Cleaner {
    escape: Escape,
    line: Line,
}
impl Cleaner {
    fn character(&mut self, c: char, build: &mut Builder, cancel: &AtomicBool) -> io::Result<()> {
        use Escape::*;
        match self.escape {
            Esc => {
                self.escape = match c {
                    '[' => Csi,
                    ']' | 'P' | '^' | '_' => String,
                    ' '..='/' => Intermediate,
                    _ => Text,
                }
            }
            Csi => {
                if ('@'..='~').contains(&c) {
                    self.escape = Text;
                }
            }
            Intermediate => {
                if ('0'..='~').contains(&c) {
                    self.escape = Text;
                }
            }
            String => {
                if c == '\x07' {
                    self.escape = Text;
                } else if c == '\x1b' {
                    self.escape = StringEsc;
                }
            }
            StringEsc => {
                self.escape = if c == '\\' || c == '\x07' {
                    Text
                } else if c == '\x1b' {
                    StringEsc
                } else {
                    String
                }
            }
            C1String => {
                if c == '\x07' || c == '\u{9c}' {
                    self.escape = Text;
                }
            }
            Text => match c {
                '\x1b' => self.escape = Esc,
                '\u{9b}' => self.escape = Csi,
                '\u{9d}' => self.escape = C1String,
                '\r' => self.line.carriage_return()?,
                '\n' => {
                    self.line.emit(build, cancel)?;
                    build.character('\n')?;
                }
                '\t' => self.line.push(c)?,
                c if !c.is_control() => self.line.push(c)?,
                _ => {}
            },
        }
        Ok(())
    }
}
pub(super) fn normalize(
    mut reader: impl Read,
    build: &mut Builder,
    cancel: &AtomicBool,
) -> io::Result<bool> {
    let mut cleaner = Cleaner::default();
    let mut buf = [0; 65536];
    let mut pending = Vec::with_capacity(65540);
    loop {
        check(cancel)?;
        let n = reader.read(&mut buf)?;
        pending.extend_from_slice(&buf[..n]);
        let mut at = 0;
        while at < pending.len() {
            match std::str::from_utf8(&pending[at..]) {
                Ok(s) => {
                    for c in s.chars() {
                        cleaner.character(c, build, cancel)?;
                    }
                    at = pending.len();
                }
                Err(e) => {
                    for c in std::str::from_utf8(&pending[at..at + e.valid_up_to()])
                        .unwrap()
                        .chars()
                    {
                        cleaner.character(c, build, cancel)?;
                    }
                    at += e.valid_up_to();
                    if let Some(len) = e.error_len() {
                        cleaner.character('\u{fffd}', build, cancel)?;
                        at += len;
                    } else if n == 0 {
                        cleaner.character('\u{fffd}', build, cancel)?;
                        at = pending.len();
                    } else {
                        break;
                    }
                }
            }
        }
        pending.drain(..at);
        if n == 0 {
            break;
        }
    }
    let boundary = cleaner.line.len == 0 && matches!(cleaner.escape, Escape::Text);
    cleaner.line.emit(build, cancel)?;
    Ok(boundary)
}

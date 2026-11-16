//! Session display transforms. Raw mode is byte-for-byte, including split UTF-8.
use shellmate_core::Newline;
/// Transform Enter from the terminal/send path; preserve existing CRLF and all
/// non-newline bytes. State handles a CRLF split between writes.
#[derive(Default)]
pub struct Sender {
    cr: bool,
}
impl Sender {
    pub fn feed(&mut self, bytes: &[u8], newline: Newline) -> Vec<u8> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' && self.cr {
                self.cr = false;
                continue;
            }
            self.cr = b == b'\r';
            if b == b'\r' || b == b'\n' {
                out.extend(newline.bytes());
            } else {
                out.push(b);
            }
        }
        out
    }
}
#[derive(Default)]
pub struct Display {
    pub hex: bool,
    pub timestamps: bool,
    line_start: bool,
    started: bool,
    hex_pending: Vec<u8>,
    offset: usize,
}
impl Display {
    pub fn feed(&mut self, bytes: &[u8], time: &str) -> Vec<u8> {
        if self.hex {
            self.hex_pending.extend(bytes);
            let mut out = Vec::new();
            while self.hex_pending.len() >= 16 {
                let row = self.hex_pending.drain(..16).collect::<Vec<_>>();
                out.extend(self.hex_row(&row, time));
            }
            return out;
        }
        if !self.timestamps {
            return bytes.to_vec();
        }
        let mut out = Vec::new();
        for &b in bytes {
            if (!self.started || self.line_start) && b != b'\r' && b != b'\n' {
                out.extend(format!("[{time}] ").bytes());
                self.line_start = false;
                self.started = true;
            }
            out.push(b);
            if b == b'\n' {
                self.line_start = true;
            }
        }
        out
    }
    /// Flush a partial HEX row on idle/close/option change, never discard bytes.
    pub fn flush(&mut self, time: &str) -> Vec<u8> {
        if self.hex_pending.is_empty() {
            return Vec::new();
        }
        let row = std::mem::take(&mut self.hex_pending);
        self.hex_row(&row, time)
    }
    fn hex_row(&mut self, row: &[u8], time: &str) -> Vec<u8> {
        let mut text = if self.timestamps {
            format!("[{time}] ")
        } else {
            String::new()
        };
        text.push_str(&format!("{:08X}  ", self.offset));
        self.offset += row.len();
        for i in 0..16 {
            if let Some(b) = row.get(i) {
                text.push_str(&format!("{b:02X} "));
            } else {
                text.push_str("   ");
            }
            if i == 7 {
                text.push(' ');
            }
        }
        text.push_str(" |");
        for b in row {
            text.push(if (32..127).contains(b) {
                *b as char
            } else {
                '.'
            });
        }
        text.push_str("|\r\n");
        text.into_bytes()
    }
    pub fn options(&mut self, hex: bool, timestamps: bool) {
        self.hex = hex;
        self.timestamps = timestamps;
        self.started = false;
        self.line_start = true;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_utf8_and_newlines() {
        let mut d = Display::default();
        let bytes = "中文 ✓".as_bytes();
        let mut out = Vec::new();
        for b in bytes {
            out.extend(d.feed(&[*b], "00:00:00.000"));
        }
        assert_eq!(out, bytes);
        for n in [Newline::CrLf, Newline::Lf, Newline::Cr] {
            let mut s = Sender::default();
            assert_eq!(s.feed(b"a\r", n), [b"a", n.bytes()].concat());
            assert!(s.feed(b"\n", n).is_empty());
            assert_eq!(s.feed(b"b\nc", n), [b"b", n.bytes(), b"c"].concat());
        }
    }
    #[test]
    fn timestamp_chunks_and_hex_ascii() {
        let mut d = Display::default();
        d.options(false, true);
        assert_eq!(d.feed(b"ab", "09:41:00.001"), b"[09:41:00.001] ab");
        assert_eq!(d.feed(b"c\r\nd", "09:41:01.002"), b"c\r\n[09:41:01.002] d");
        d.options(true, false);
        assert!(d.feed(&[0, 65, 255], "").is_empty());
        let row = String::from_utf8(d.flush("")).unwrap();
        assert!(row.starts_with("00000000  00 41 FF "));
        assert!(row.ends_with("|.A.|\r\n"));
        assert!(d.flush("").is_empty());
    }
}

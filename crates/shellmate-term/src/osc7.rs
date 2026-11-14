//! Finds the working directory a shell reports with OSC 7 (`ESC ] 7 ; file://host/path BEL`).
//!
//! alacritty's parser ignores this sequence, so the terminal scans the byte stream itself. The
//! scanner only looks: the bytes go to the parser unchanged. The sequence may be cut at any byte
//! between two reads, so the state is kept across chunks.

/// The longest OSC body kept; anything longer is not a sane directory report.
const MAX_BODY: usize = 8 * 1024;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Outside any escape sequence.
    Ground,
    /// After `ESC`.
    Escape,
    /// Inside `ESC ]`, collecting the body.
    Osc,
    /// After an `ESC` inside the body: `\` ends it (the string terminator).
    OscEscape,
}

/// Incremental OSC 7 scanner.
#[derive(Debug)]
pub struct Osc7Scanner {
    state: State,
    body: Vec<u8>,
    last: Option<String>,
}

impl Default for Osc7Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Osc7Scanner {
    /// A scanner at the start of a stream.
    pub fn new() -> Self {
        Self { state: State::Ground, body: Vec::new(), last: None }
    }

    /// Feeds the next chunk of output and returns the directories reported in it that differ
    /// from the one reported before (shells repeat the report at every prompt).
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            if self.state == State::Ground {
                // Most output has no escapes at all: skip to the next one.
                match rest.iter().position(|&b| b == ESC) {
                    Some(at) => {
                        rest = &rest[at + 1..];
                        self.state = State::Escape;
                    }
                    None => break,
                }
                continue;
            }
            let byte = rest[0];
            rest = &rest[1..];
            self.step(byte, &mut found);
        }
        found
    }

    fn step(&mut self, byte: u8, found: &mut Vec<String>) {
        self.state = match (self.state, byte) {
            (State::Ground, _) => State::Ground,
            (State::Escape, b']') => {
                self.body.clear();
                State::Osc
            }
            (State::Escape, ESC) => State::Escape,
            (State::Escape, _) => State::Ground,
            (State::Osc, BEL) => self.finish(found),
            (State::Osc, ESC) => State::OscEscape,
            (State::Osc, CAN | SUB) => State::Ground,
            (State::Osc, _) if self.body.len() >= MAX_BODY => State::Ground,
            (State::Osc, _) => {
                self.body.push(byte);
                State::Osc
            }
            (State::OscEscape, b'\\') => self.finish(found),
            (State::OscEscape, b']') => {
                // The old string was cut short by a new OSC.
                self.body.clear();
                State::Osc
            }
            (State::OscEscape, ESC) => State::Escape,
            (State::OscEscape, _) => State::Ground,
        };
    }

    /// The sequence ended properly: report it if it is a new OSC 7.
    fn finish(&mut self, found: &mut Vec<String>) -> State {
        if let Some(payload) = self.body.strip_prefix(b"7;")
            && let Some(path) = parse_uri(payload)
            && self.last.as_deref() != Some(path.as_str())
        {
            self.last = Some(path.clone());
            found.push(path);
        }
        self.body.clear();
        State::Ground
    }
}

/// The path of a `file://host/path` URI, percent-decoded. `None` if it is not one, or the path
/// is not valid text.
fn parse_uri(payload: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(payload).ok()?;
    let rest = text.get(..7).filter(|scheme| scheme.eq_ignore_ascii_case("file://")).map(|_| &text[7..])?;
    // The host (empty, `localhost` or the machine's name) is not needed: the report comes from
    // the machine the terminal is connected to.
    let path = &rest[rest.find('/')?..];
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let decoded = percent_decode(path);
    let decoded = String::from_utf8(decoded).ok()?;
    (!decoded.chars().any(char::is_control)).then_some(decoded)
}

/// `%E4%B8%AD` → the bytes of `中`. A `%` not followed by two hex digits stays as it is.
fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        if bytes[at] == b'%'
            && let (Some(high), Some(low)) =
                (bytes.get(at + 1).and_then(|b| hex(*b)), bytes.get(at + 2).and_then(|b| hex(*b)))
        {
            out.push((high * 16 + low) as u8);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<String> {
        let mut scanner = Osc7Scanner::new();
        chunks.iter().flat_map(|chunk| scanner.feed(chunk)).collect()
    }

    #[test]
    fn a_bel_terminated_report_is_found() {
        assert_eq!(scan(&[b"\x1b]7;file://web-01/home/tester/src\x07"]), ["/home/tester/src"]);
    }

    #[test]
    fn a_report_ended_by_the_string_terminator_is_found() {
        assert_eq!(scan(&[b"\x1b]7;file://web-01/var/log\x1b\\"]), ["/var/log"]);
    }

    #[test]
    fn the_host_may_be_empty_or_localhost() {
        assert_eq!(scan(&[b"\x1b]7;file:///tmp\x07"]), ["/tmp"]);
        assert_eq!(scan(&[b"\x1b]7;file://localhost/etc\x07"]), ["/etc"]);
        assert_eq!(scan(&[b"\x1b]7;FILE://Host/usr\x07"]), ["/usr"], "the scheme is case-insensitive");
    }

    #[test]
    fn percent_encoding_is_decoded() {
        assert_eq!(scan(&[b"\x1b]7;file://h/home/me/my%20docs\x07"]), ["/home/me/my docs"]);
        assert_eq!(scan(&[b"\x1b]7;file://h/a%23b%25c%3Fd\x07"]), ["/a#b%c?d"]);
    }

    #[test]
    fn chinese_paths_work_encoded_or_raw() {
        assert_eq!(
            scan(&["\x1b]7;file://h/home/%E7%94%A8%E6%88%B7/%E6%96%87%E6%A1%A3\x07".as_bytes()]),
            ["/home/用户/文档"]
        );
        assert_eq!(scan(&["\x1b]7;file://h/home/用户\x07".as_bytes()]), ["/home/用户"]);
    }

    #[test]
    fn a_report_split_at_any_byte_is_put_together() {
        let sequence = "\x1b]7;file://h/home/%E7%94%A8%E6%88%B7/a b\x1b\\".as_bytes();
        for cut in 1..sequence.len() {
            let (first, second) = sequence.split_at(cut);
            assert_eq!(scan(&[first, second]), ["/home/用户/a b"], "cut at {cut}");
        }
        // One byte at a time, with ordinary output around it.
        let mut stream = b"prompt$ ".to_vec();
        stream.extend_from_slice(sequence);
        stream.extend_from_slice(b" next");
        let bytes: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(scan(&bytes), ["/home/用户/a b"]);
    }

    #[test]
    fn reports_between_ordinary_output_are_found_in_order() {
        let stream = b"ls\r\n\x1b]7;file://h/a\x07file1\r\n\x1b[32mgreen\x1b[0m\x1b]7;file://h/b\x07$ ";
        assert_eq!(scan(&[stream]), ["/a", "/b"]);
    }

    #[test]
    fn a_repeated_report_is_only_reported_when_it_changes() {
        let mut scanner = Osc7Scanner::new();
        assert_eq!(scanner.feed(b"\x1b]7;file://h/a\x07"), ["/a"]);
        assert!(scanner.feed(b"\x1b]7;file://h/a\x07").is_empty(), "the next prompt repeats it");
        assert_eq!(scanner.feed(b"\x1b]7;file://h/b\x07"), ["/b"]);
        assert_eq!(scanner.feed(b"\x1b]7;file://h/a\x07"), ["/a"], "going back is a change");
    }

    #[test]
    fn other_sequences_are_ignored() {
        // Title, hyperlink, colors, CSI, a lone ESC.
        let stream = b"\x1b]0;title\x07\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\\x1b[31mred\x1b\x1b]11;?\x07\x1bX";
        assert!(scan(&[stream]).is_empty());
        assert!(scan(&[b"\x1b]17;file://h/a\x07"]).is_empty(), "a number that only ends in 7");
        assert!(scan(&[b"\x1b]7file://h/a\x07"]).is_empty(), "no semicolon");
    }

    #[test]
    fn invalid_reports_are_ignored() {
        for bad in [
            &b"\x1b]7;\x07"[..],
            b"\x1b]7;/just/a/path\x07",
            b"\x1b]7;http://h/a\x07",
            b"\x1b]7;file://hostonly\x07",
            b"\x1b]7;file://h/a%00b\x07",
            b"\x1b]7;file://h/a%0Ab\x07",
            b"\x1b]7;file://h/%FF%FE\x07",
            b"\x1b]7;file://h/\xFF\xFE\x07",
        ] {
            assert!(scan(&[bad]).is_empty(), "{:?}", String::from_utf8_lossy(bad));
        }
        // The scanner is back in business afterwards.
        assert_eq!(scan(&[b"\x1b]7;file://h/bad%00\x07\x1b]7;file://h/ok\x07"]), ["/ok"]);
    }

    #[test]
    fn a_stray_percent_stays_a_percent() {
        assert_eq!(scan(&[b"\x1b]7;file://h/100%\x07"]), ["/100%"]);
        assert_eq!(scan(&[b"\x1b]7;file://h/a%zzb%4\x07"]), ["/a%zzb%4"]);
    }

    #[test]
    fn an_interrupted_sequence_does_not_swallow_the_next_one() {
        // CAN cancels; a fresh ESC ] restarts; an ESC without ] abandons.
        assert_eq!(scan(&[b"\x1b]7;file://h/lost\x18\x1b]7;file://h/found\x07"]), ["/found"]);
        assert_eq!(scan(&[b"\x1b]7;file://h/lost\x1b]7;file://h/found\x07"]), ["/found"]);
        assert_eq!(scan(&[b"\x1b]7;file://h/lost\x1bA\x1b]7;file://h/found\x07"]), ["/found"]);
        assert_eq!(scan(&[b"\x1b\x1b]7;file://h/doubled\x07"]), ["/doubled"]);
    }

    #[test]
    fn an_endless_body_is_dropped() {
        let mut long = b"\x1b]7;file://h/".to_vec();
        long.extend(std::iter::repeat_n(b'a', MAX_BODY + 10));
        long.push(BEL);
        long.extend_from_slice(b"\x1b]7;file://h/short\x07");
        assert_eq!(scan(&[&long]), ["/short"]);
    }

    #[test]
    fn plain_output_costs_nothing_and_reports_nothing() {
        let mut scanner = Osc7Scanner::new();
        assert!(scanner.feed(b"hello \xE4\xB8\xAD world\r\n").is_empty());
        assert!(scanner.feed(b"").is_empty());
        assert_eq!(scanner.state, State::Ground);
    }

    #[test]
    fn a_query_or_fragment_is_not_part_of_the_path() {
        assert_eq!(scan(&[b"\x1b]7;file://h/a/b?x=1#frag\x07"]), ["/a/b"]);
    }
}

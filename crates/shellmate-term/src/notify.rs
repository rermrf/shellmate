//! Finds the desktop notifications a program asks for in its output.
//!
//! Two de-facto conventions are understood, on every backend:
//!
//! * OSC 9: `ESC ] 9 ; <text> BEL`. ConEmu also uses this number for its own sub-commands
//!   (`9;4;1;50` is a progress bar); those start with a number followed by `;` or the end, and
//!   are not notifications.
//! * OSC 777: `ESC ] 777 ; notify ; <title> ; <body> BEL`. Other sub-commands are ignored.
//!
//! alacritty's parser ignores both, so the screen never shows them. The scanner only looks: the
//! bytes go to the parser unchanged. A sequence may be cut at any byte between two reads, so the
//! state is kept across chunks.

/// The longest notification kept (title and body together); the rest is cut off.
pub const MAX_TEXT: usize = 1024;
/// The OSC payload kept while scanning: the text plus room for the `777;notify;` prefix and
/// the separator. Anything beyond is dropped, but the sequence is still followed to its end.
const MAX_RAW: usize = MAX_TEXT + 16;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// What a program asked to be told to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// The headline, when the program gave one (OSC 777).
    pub title: Option<String>,
    pub body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Outside any escape sequence.
    Ground,
    /// After `ESC`.
    Escape,
    /// Inside `ESC ]`, collecting the payload.
    Osc,
    /// After an `ESC` inside the payload: `\` ends it (the string terminator).
    OscEscape,
}

/// Incremental scanner for OSC 9 and OSC 777.
#[derive(Debug)]
pub struct NotifyScanner {
    state: State,
    body: Vec<u8>,
}

impl Default for NotifyScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl NotifyScanner {
    /// A scanner at the start of a stream.
    pub fn new() -> Self {
        Self { state: State::Ground, body: Vec::new() }
    }

    /// Feeds the next chunk of output and returns the notifications completed in it.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Notification> {
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

    fn step(&mut self, byte: u8, found: &mut Vec<Notification>) {
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
            // Like the parser, drop the other C0 controls and DEL inside the string.
            (State::Osc, 0..=0x1f | 0x7f) => State::Osc,
            (State::Osc, _) => {
                if self.body.len() < MAX_RAW {
                    self.body.push(byte);
                }
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

    /// The sequence ended properly: report it if it is a notification.
    fn finish(&mut self, found: &mut Vec<Notification>) -> State {
        if let Some(notification) = parse(&self.body) {
            found.push(notification);
        }
        self.body.clear();
        State::Ground
    }
}

fn parse(payload: &[u8]) -> Option<Notification> {
    if let Some(text) = payload.strip_prefix(b"9;") {
        if conemu_command(text) {
            return None;
        }
        return build(None, text);
    }
    let rest = payload.strip_prefix(b"777;notify;")?;
    // `title;body`; the body may contain further semicolons. One field alone is the message.
    match rest.iter().position(|&b| b == b';') {
        Some(at) => build(Some(&rest[..at]), &rest[at + 1..]),
        None => build(None, rest),
    }
}

/// `4;1;50` and `4` (progress) and the like: ConEmu's sub-commands, which are a number followed
/// by `;` or by nothing.
fn conemu_command(text: &[u8]) -> bool {
    let digits = text.iter().take_while(|b| b.is_ascii_digit()).count();
    digits > 0 && text.get(digits).is_none_or(|&b| b == b';')
}

fn build(title: Option<&[u8]>, body: &[u8]) -> Option<Notification> {
    let title = title.map(clean).filter(|title| !title.is_empty());
    let body = clean(body);
    // A title with no message is the message.
    let (title, body) = match (title, body.is_empty()) {
        (Some(title), true) => (None, title),
        (title, _) => (title, body),
    };
    if body.is_empty() {
        return None;
    }
    let title = title.map(|title| truncate(title, MAX_TEXT));
    let room = MAX_TEXT - title.as_ref().map_or(0, String::len);
    Some(Notification { title, body: truncate(body, room) })
}

/// The text with every control character (C0, DEL, C1) and every bidirectional override removed:
/// what a remote program prints must not be able to reorder the words around it.
fn clean(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).chars().filter(|c| !c.is_control() && !is_bidi_control(*c)).collect()
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// At most `max` bytes, cut at a character boundary.
fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<Notification> {
        let mut scanner = NotifyScanner::new();
        chunks.iter().flat_map(|chunk| scanner.feed(chunk)).collect()
    }

    fn body(text: &str) -> Notification {
        Notification { title: None, body: text.into() }
    }

    fn titled(title: &str, text: &str) -> Notification {
        Notification { title: Some(title.into()), body: text.into() }
    }

    #[test]
    fn osc_9_ended_by_bel_or_the_string_terminator() {
        assert_eq!(scan(&[b"\x1b]9;build finished\x07"]), [body("build finished")]);
        assert_eq!(scan(&[b"\x1b]9;build finished\x1b\\"]), [body("build finished")]);
    }

    #[test]
    fn osc_777_has_a_title_and_a_body() {
        assert_eq!(scan(&[b"\x1b]777;notify;Backup;done in 5 s\x07"]), [titled("Backup", "done in 5 s")]);
        assert_eq!(scan(&[b"\x1b]777;notify;Backup;done\x1b\\"]), [titled("Backup", "done")]);
        // Semicolons after the first belong to the body.
        assert_eq!(scan(&[b"\x1b]777;notify;t;a;b;c\x07"]), [titled("t", "a;b;c")]);
        // A title alone is the message; an empty title is none.
        assert_eq!(scan(&[b"\x1b]777;notify;only one field\x07"]), [body("only one field")]);
        assert_eq!(scan(&[b"\x1b]777;notify;headline;\x07"]), [body("headline")]);
        assert_eq!(scan(&[b"\x1b]777;notify;;message\x07"]), [body("message")]);
    }

    #[test]
    fn other_777_sub_commands_and_numbers_are_not_notifications() {
        assert!(scan(&[b"\x1b]777;preexec\x07\x1b]777;precmd;x\x07\x1b]777;notifyme;a;b\x07"]).is_empty());
        assert!(scan(&[b"\x1b]7777;notify;a;b\x07\x1b]99;hello\x07\x1b]19;hello\x07\x1b]9hello\x07"]).is_empty());
        assert!(scan(&[b"\x1b]777;notify\x07\x1b]777;notify;\x07\x1b]777;notify;;\x07"]).is_empty());
    }

    #[test]
    fn conemu_sub_commands_are_ignored() {
        // Progress bars, taskbar states, sleeps, ...: a number, then `;` or the end.
        for ignored in ["9;4;1;50", "9;4;0;0", "9;4", "9;1;500", "9;12", "9;3;Title", "9;5"] {
            let sequence = format!("\x1b]{ignored}\x07");
            assert!(scan(&[sequence.as_bytes()]).is_empty(), "{ignored}");
        }
        // A number inside a sentence, or followed by anything but `;`, is a message.
        assert_eq!(scan(&[b"\x1b]9;4 files copied\x07"]), [body("4 files copied")]);
        assert_eq!(scan(&[b"\x1b]9;4.5 GB\x07"]), [body("4.5 GB")]);
        assert_eq!(scan(&[b"\x1b]9;v2;done\x07"]), [body("v2;done")]);
        assert_eq!(scan(&[b"\x1b]9;4x\x07"]), [body("4x")]);
    }

    #[test]
    fn an_empty_message_is_not_a_notification() {
        assert!(scan(&[b"\x1b]9;\x07\x1b]9;\x1b\\"]).is_empty());
        assert!(scan(&[b"\x1b]9;\x01\x7f\x07"]).is_empty());
    }

    #[test]
    fn a_sequence_split_at_any_byte_is_put_together() {
        let sequence = "\x1b]777;notify;完成;任务 ✓\x1b\\".as_bytes();
        for cut in 1..sequence.len() {
            let (first, second) = sequence.split_at(cut);
            assert_eq!(scan(&[first, second]), [titled("完成", "任务 ✓")], "cut at {cut}");
        }
        // One byte at a time, with ordinary output around it.
        let mut stream = b"prompt$ ".to_vec();
        stream.extend_from_slice("\x1b]9;一条消息\x07".as_bytes());
        stream.extend_from_slice(b" next");
        let bytes: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(scan(&bytes), [body("一条消息")]);
    }

    #[test]
    fn several_in_one_chunk_come_out_in_order() {
        let stream = b"a\x1b]9;one\x07b\x1b[31mred\x1b[0m\x1b]777;notify;two;2\x1b\\c\x1b]9;three\x07";
        assert_eq!(scan(&[stream]), [body("one"), titled("two", "2"), body("three")]);
    }

    #[test]
    fn ordinary_sequences_are_ignored() {
        // Title, hyperlink, directory, colors, CSI, a lone ESC.
        let stream = b"\x1b]0;title\x07\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\\x1b]7;file://h/a\x07\x1b[31mred\x1b\x1b]11;?\x07\x1bX";
        assert!(scan(&[stream]).is_empty());
    }

    #[test]
    fn control_characters_are_removed() {
        // C0 inside the string are dropped like the parser does; C1 and the bidi overrides are filtered after decoding.
        assert_eq!(scan(&[b"\x1b]9;a\x01b\x08c\nd\re\tf\x7fg\x07"]), [body("abcdefg")]);
        assert_eq!(scan(&["\x1b]9;x\u{85}y\u{9b}z\x07".as_bytes()]), [body("xyz")]);
        assert_eq!(scan(&["\x1b]9;a\u{202e}b\u{2066}c\u{200f}d\x07".as_bytes()]), [body("abcd")]);
        assert_eq!(scan(&[b"\x1b]777;notify;ti\x02tle;bo\x03dy\x07"]), [titled("title", "body")]);
    }

    #[test]
    fn a_long_message_is_cut_at_one_kibibyte() {
        let long = "a".repeat(5000);
        let sequence = format!("\x1b]9;{long}\x07");
        let found = scan(&[sequence.as_bytes()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].body.len(), MAX_TEXT);
        // The cut never splits a character: 3-byte glyphs, 1024 is not a multiple of 3.
        let wide = "好".repeat(2000);
        let sequence = format!("\x1b]9;{wide}\x1b\\");
        let found = scan(&[sequence.as_bytes()]);
        assert_eq!(found[0].body, "好".repeat(341));
        // Title and body share the limit.
        let sequence = format!("\x1b]777;notify;{};{}\x07", "t".repeat(300), "b".repeat(2000));
        let found = scan(&[sequence.as_bytes()]);
        let title = found[0].title.as_deref().unwrap();
        assert_eq!((title.len(), found[0].body.len()), (300, MAX_TEXT - 300));
        // An endless string neither grows the buffer nor eats what follows it.
        let mut scanner = NotifyScanner::new();
        scanner.feed(b"\x1b]9;");
        for _ in 0..100 {
            scanner.feed(&[b'x'; 4096]);
        }
        assert!(scanner.body.len() <= MAX_RAW);
        assert_eq!(scanner.feed(b"\x07\x1b]9;next\x07"), [Notification { title: None, body: "x".repeat(MAX_TEXT) }, body("next")]);
    }

    #[test]
    fn an_interrupted_sequence_does_not_swallow_the_next_one() {
        // CAN cancels; a fresh ESC ] restarts; an ESC without ] abandons.
        assert_eq!(scan(&[b"\x1b]9;lost\x18\x1b]9;found\x07"]), [body("found")]);
        assert_eq!(scan(&[b"\x1b]9;lost\x1b]9;found\x07"]), [body("found")]);
        assert_eq!(scan(&[b"\x1b]9;lost\x1bA\x1b]9;found\x07"]), [body("found")]);
        assert_eq!(scan(&[b"\x1b\x1b]9;doubled\x07"]), [body("doubled")]);
    }

    #[test]
    fn plain_output_costs_nothing_and_reports_nothing() {
        let mut scanner = NotifyScanner::new();
        assert!(scanner.feed("hello 中 world\r\n\x07".as_bytes()).is_empty());
        assert!(scanner.feed(b"").is_empty());
        assert_eq!(scanner.state, State::Ground);
    }

    #[test]
    fn invalid_utf8_does_not_stop_the_scanner() {
        let found = scan(&[b"\x1b]9;caf\xe9\x07\x1b]9;ok\x07"]);
        assert_eq!(found, [body("caf\u{fffd}"), body("ok")]);
    }
}

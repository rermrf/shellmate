//! Screen-based command capture. No keystroke text is retained.
use alacritty_terminal::{
    event::EventListener,
    grid::Dimensions,
    index::{Column, Line, Point},
    term::{Term, TermMode, cell::Flags},
    vte::ansi::Processor,
};

#[derive(Default)]
pub(crate) struct Capture {
    start: Option<Start>,
    semantic: bool,
    input_phase: bool,
    last: Option<String>,
    osc: Osc133,
}

struct Start {
    offset: usize,
    prefix: String,
}

impl Capture {
    pub fn advance<T: EventListener>(
        &mut self,
        term: &mut Term<T>,
        parser: &mut Processor,
        bytes: &[u8],
    ) -> Vec<String> {
        let marks = self.osc.feed(bytes);
        let mut at = 0;
        let mut found = Vec::new();
        for (end, mark) in marks {
            parser.advance(term, &bytes[at..end]);
            at = end;
            if term.mode().contains(TermMode::ALT_SCREEN) {
                self.start = None;
                continue;
            }
            self.semantic = true;
            match mark {
                b'A' | b'D' => {
                    self.start = None;
                    self.input_phase = false;
                }
                b'B' => {
                    self.input_phase = true;
                    self.start = Some(start(term));
                }
                b'C' => {
                    if let Some(command) = self.finish(term) {
                        found.push(command);
                    }
                    self.input_phase = false;
                }
                _ => {}
            }
        }
        parser.advance(term, &bytes[at..]);
        if term.mode().contains(TermMode::ALT_SCREEN) {
            self.start = None;
        }
        found
    }

    /// Invoked before writing user input. Enter reads only the already echoed grid.
    pub fn input<T: EventListener>(&mut self, term: &Term<T>, bytes: &[u8]) -> Option<String> {
        if term.mode().contains(TermMode::ALT_SCREEN) || (self.semantic && !self.input_phase) {
            self.start = None;
            return None;
        }
        if bytes == b"\x03" {
            self.start = None;
            return None;
        }
        if bytes == b"\r" {
            return self.finish(term);
        }
        if self.start.is_none() {
            self.start = Some(start(term));
        }
        None
    }

    fn finish<T: EventListener>(&mut self, term: &Term<T>) -> Option<String> {
        let start = self.start.take()?;
        let line = logical_line(term);
        // A changed prompt or unrelated output invalidates the heuristic, rather than saving it.
        if !line.starts_with(&start.prefix) {
            return None;
        }
        let raw: String = line.chars().skip(start.offset).collect();
        if !recordable(&raw) {
            return None;
        }
        let command = raw.trim().to_string();
        if self.last.as_ref() == Some(&command) {
            return None;
        }
        self.last = Some(command.clone());
        Some(command)
    }
}

fn first_row<T>(term: &Term<T>) -> Line {
    let mut row = term.grid().cursor.point.line;
    let bottom = -(term.grid().history_size() as i32);
    while row.0 > bottom
        && term.grid()[Line(row.0 - 1)][Column(term.columns() - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    {
        row -= 1;
    }
    row
}

fn text_until<T>(term: &Term<T>, end: Point) -> String {
    let mut text = String::new();
    for row in first_row(term).0..=end.line.0 {
        let cols = if row == end.line.0 {
            end.column.0
        } else {
            term.columns()
        };
        for col in 0..cols {
            let cell = &term.grid()[Line(row)][Column(col)];
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
    }
    text
}

fn start<T>(term: &Term<T>) -> Start {
    let mut point = term.grid().cursor.point;
    if term.grid().cursor.input_needs_wrap {
        point.column.0 += 1;
    }
    let prefix = text_until(term, point);
    Start {
        offset: prefix.chars().count(),
        prefix,
    }
}

fn logical_line<T>(term: &Term<T>) -> String {
    let mut end = term.grid().cursor.point.line;
    while end.0 + 1 < term.screen_lines() as i32
        && term.grid()[end][Column(term.columns() - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    {
        end += 1;
    }
    text_until(term, Point::new(end, Column(term.columns())))
}

/// Conservative credential suppression. False positives are preferable to persisting a secret.
pub fn recordable(raw: &str) -> bool {
    if raw.starts_with(char::is_whitespace) || raw.trim().chars().count() <= 1 {
        return false;
    }
    let lower = raw.to_lowercase();
    let words: Vec<_> = lower.split_whitespace().collect();
    if ["password", "token", "secret", "api_key"]
        .iter()
        .any(|key| lower.contains(&format!("{key}=")))
    {
        return false;
    }
    if words
        .iter()
        .any(|w| w.starts_with("-p") && w.len() > 2 && !w.starts_with("--"))
    {
        return false;
    }
    if lower.contains("sshpass") && words.iter().any(|w| *w == "-p" || w.starts_with("-p")) {
        return false;
    }
    if lower.contains("curl")
        && words
            .windows(2)
            .any(|w| matches!(w[0], "-u" | "--user") && w[1].contains(':'))
    {
        return false;
    }
    if lower.contains("curl")
        && words
            .iter()
            .any(|w| (w.starts_with("-u") || w.starts_with("--user=")) && w.contains(':'))
    {
        return false;
    }
    true
}

#[derive(Default)]
struct Osc133 {
    state: u8,
    body: Vec<u8>,
}
impl Osc133 {
    fn feed(&mut self, bytes: &[u8]) -> Vec<(usize, u8)> {
        let mut marks = Vec::new();
        for (i, &b) in bytes.iter().enumerate() {
            match (self.state, b) {
                (0, 0x1b) => self.state = 1,
                (1, b']') => {
                    self.body.clear();
                    self.state = 2;
                }
                (1, 0x1b) => {}
                (2, 7) | (3, b'\\') => {
                    if self.body.starts_with(b"133;")
                        && self.body.len() >= 5
                        && (self.body.len() == 5 || self.body[5] == b';')
                        && b"ABCD".contains(&self.body[4])
                    {
                        marks.push((i + 1, self.body[4]));
                    }
                    self.state = 0;
                }
                (2, 0x1b) => self.state = 3,
                (2, 0x18 | 0x1a) => self.state = 0,
                (2, _) if self.body.len() < 1024 => self.body.push(b),
                _ => self.state = 0,
            }
        }
        marks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::{
        event::{Event, EventListener},
        term::Config,
    };
    struct Listener;
    impl EventListener for Listener {
        fn send_event(&self, _: Event) {}
    }
    struct Shell {
        term: Term<Listener>,
        parser: Processor,
        capture: Capture,
        found: Vec<String>,
    }
    impl Shell {
        fn new(cols: u16, rows: u16) -> Self {
            Self {
                term: Term::new(
                    Config::default(),
                    &crate::TerminalSize {
                        cols,
                        rows,
                        ..Default::default()
                    },
                    Listener,
                ),
                parser: Processor::new(),
                capture: Capture::default(),
                found: vec![],
            }
        }
        fn output(&mut self, text: &str) {
            self.found.extend(
                self.capture
                    .advance(&mut self.term, &mut self.parser, text.as_bytes()),
            );
        }
        fn key(&mut self, text: &str) {
            self.found.extend(self.capture.input(&self.term, text.as_bytes()));
        }
    }
    #[test]
    fn shell_editing_table() {
        for (key, echoed, expected) in [
            ("u", "uptime", "uptime"),
            ("\x1b[A", "ls -la", "ls -la"),
            ("c", "cat Cargo.toml", "cat Cargo.toml"),
            ("e", "echo 用户你好", "echo 用户你好"),
        ] {
            let mut s = Shell::new(80, 24);
            s.output("user@host:~$ ");
            s.key(key);
            s.output(echoed);
            s.key("\r");
            assert_eq!(s.found, [expected]);
        }
    }
    #[test]
    fn tab_completion_uses_final_screen_not_keys() {
        let mut s = Shell::new(80, 24);
        s.output("$ ");
        s.key("c");
        s.output("cat Car");
        s.key("\t");
        s.output("go.toml ");
        s.key("\r");
        assert_eq!(s.found, ["cat Cargo.toml"]);
    }
    #[test]
    fn wraps_and_scrolls_and_handles_wide_characters() {
        for command in [
            "echo abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz",
            "echo 一二三四五六七八九十",
        ] {
            let mut s = Shell::new(12, 3);
            s.output("$ ");
            s.key("e");
            s.output(command);
            s.key("\r");
            assert_eq!(s.found, [command]);
        }
    }
    #[test]
    fn echo_off_password_is_never_captured() {
        let mut s = Shell::new(80, 24);
        s.output("Password: ");
        s.key("super-secret-password");
        s.key("\r");
        assert!(s.found.is_empty());
    }
    #[test]
    fn alternate_screen_and_space_and_duplicates_and_cancel() {
        let mut s = Shell::new(80, 24);
        s.output("\x1b[?1049h");
        s.key(":");
        s.output(":write file");
        s.key("\r");
        s.output("\x1b[?1049l");
        for command in [" secret command", "x", "", "uptime", "uptime", "df -h", "uptime"] {
            s.output("\r\n$ ");
            s.key("x");
            s.output(command);
            s.key("\r");
        }
        s.output("\r\n$ ");
        s.key("x");
        s.output("cancelled");
        s.key("\x03");
        s.key("\r");
        assert_eq!(s.found, ["uptime", "df -h", "uptime"]);
    }
    #[test]
    fn redraw_and_cursor_edit_read_to_end_of_line() {
        let mut s = Shell::new(80, 24);
        s.output("user$ ");
        s.key("\x1b[A");
        s.output("\r\x1b[Kuser$ echo recalled\x1b[5D");
        s.key("\r");
        assert_eq!(s.found, ["echo recalled"]);
    }
    #[test]
    fn osc133_fragmented_marks_override_heuristic_and_ignore_running_program_input() {
        let mut s = Shell::new(80, 24);
        for byte in b"\x1b]133;A\x1b\\host$ \x1b]133;B\x07" {
            s.output(std::str::from_utf8(std::slice::from_ref(byte)).unwrap());
        }
        s.output("echo semantic");
        s.key("\r");
        s.output("\x1b]133;C\x07\r\nPassword: ");
        s.key("x");
        s.output("echoed program input");
        s.key("\r");
        s.output("\x1b]133;D;0\x07\r\n\x1b]133;A\x07$ \x1b]133;B\x07df -h\x1b]133;C\x07");
        assert_eq!(s.found, ["echo semantic", "df -h"]);
    }
    #[test]
    fn prompt_at_right_margin_and_command_after_scroll() {
        let mut s = Shell::new(8, 3);
        s.output("123456$ ");
        s.key("u");
        s.output("uptime");
        s.key("\r");
        assert_eq!(s.found, ["uptime"]);
    }
    #[test]
    fn changed_prompt_is_not_saved_as_a_command() {
        let mut s = Shell::new(80, 24);
        s.output("old$ ");
        s.key("e");
        s.output("\r\x1b[Knew$ echo hi");
        s.key("\r");
        assert!(s.found.is_empty());
    }
    #[test]
    fn osc_marks_in_one_chunk_and_st_terminators_use_their_exact_grid_position() {
        let mut s = Shell::new(80, 24);
        s.output("\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\uptime\x1b]133;C\x1b\\\r\n0.5\r\n\x1b]133;D;0\x1b\\\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\df -h\x1b]133;C\x1b\\");
        assert_eq!(s.found, ["uptime", "df -h"]);
    }
    #[test]
    fn credential_filter_table() {
        for command in [
            "mysql -pSECRET",
            "PASSWORD=abc cmd",
            "export TOKEN=abc",
            "SECRET=x",
            "API_KEY=x curl foo",
            "curl -u user:pass url",
            "curl --user=user:pass url",
            "curl -uuser:pass url",
            "sshpass -p abc ssh host",
            " TOKEN=x",
            "x",
            "",
        ] {
            assert!(!recordable(command), "{command}");
        }
        for command in [
            "mysql -p",
            "pwd",
            "curl https://example.test",
            "echo hello",
            "ssh host",
            "export LANG=en_US.UTF-8",
        ] {
            assert!(recordable(command), "{command}");
        }
    }
}

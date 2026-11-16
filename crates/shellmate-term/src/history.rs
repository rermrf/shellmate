//! Screen-based command capture. No keystroke text is retained.
use std::time::{Duration, Instant};

use alacritty_terminal::{
    event::EventListener,
    grid::Dimensions,
    index::{Column, Line, Point},
    term::{Term, TermMode, cell::Flags},
    vte::ansi::Processor,
};

const SUBMIT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(crate) struct Capture {
    start: Option<Start>,
    pending: Option<Pending>,
    semantic: bool,
    input_phase: bool,
    last: Option<String>,
    osc: Osc133,
}

struct Pending {
    since: Instant,
    // OSC 133 C often follows CRLF. Preserve the screen before LF, but do not publish until C.
    line: Option<String>,
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
        self.advance_at(term, parser, bytes, Instant::now())
    }

    fn advance_at<T: EventListener>(
        &mut self,
        term: &mut Term<T>,
        parser: &mut Processor,
        bytes: &[u8],
        now: Instant,
    ) -> Vec<String> {
        self.expire(now);
        let marks = self.osc.feed(bytes);
        let mut at = 0;
        let mut found = Vec::new();
        for (end, mark) in marks {
            self.advance_segment(term, parser, &bytes[at..end], &mut found);
            at = end;
            if term.mode().contains(TermMode::ALT_SCREEN) {
                self.cancel();
                continue;
            }
            self.semantic = true;
            match mark {
                b'A' | b'D' => {
                    self.cancel();
                    self.input_phase = false;
                }
                b'B' => {
                    self.cancel();
                    self.input_phase = true;
                    self.start = Some(start(term));
                }
                b'C' => {
                    let line = self
                        .pending
                        .as_mut()
                        .and_then(|p| p.line.take())
                        .unwrap_or_else(|| logical_line(term));
                    if let Some(command) = self.finish(line) {
                        found.push(command);
                    }
                    self.input_phase = false;
                }
                _ => {}
            }
        }
        self.advance_segment(term, parser, &bytes[at..], &mut found);
        found
    }

    fn advance_segment<T: EventListener>(
        &mut self,
        term: &mut Term<T>,
        parser: &mut Processor,
        bytes: &[u8],
        found: &mut Vec<String>,
    ) {
        let mut at = 0;
        while at < bytes.len() {
            if self.pending.is_none() {
                // Command output keeps the normal bulk parser path.
                parser.advance(term, &bytes[at..]);
                break;
            }
            let before = self
                .pending
                .as_ref()
                .filter(|p| p.line.is_none())
                .map(|_| (term.grid().cursor.point.line, first_row(term), logical_line(term)));
            let byte = bytes[at];
            parser.advance(term, &bytes[at..at + 1]);
            at += 1;
            if term.mode().contains(TermMode::ALT_SCREEN) {
                self.cancel();
                continue;
            }
            if let Some((cursor_row, first, line)) = before {
                let after = logical_line(term);
                // Inspect the parser's actual effect: an LF inside an OSC string is not a
                // submitted line. Soft wrapping/scrolling extends the same logical line.
                let line_feed =
                    byte == b'\n' && (term.grid().cursor.point.line != cursor_row || after != line);
                let moved_off = first_row(term) != first && !after.starts_with(line.trim_end());
                if line_feed || moved_off {
                    if self.semantic {
                        if let Some(pending) = &mut self.pending {
                            pending.line = Some(line);
                        }
                    } else if let Some(command) = self.finish(line) {
                        found.push(command);
                    }
                }
            }
        }
        if term.mode().contains(TermMode::ALT_SCREEN) {
            self.cancel();
        }
    }

    /// Enter only arms a deadline. The shell must finish echoing/erasing/redrawing its input
    /// before a stream boundary can publish history. Input received while pending is not stored.
    pub fn input<T: EventListener>(&mut self, term: &Term<T>, bytes: &[u8]) {
        self.input_at(term, bytes, Instant::now());
    }

    fn input_at<T: EventListener>(&mut self, term: &Term<T>, bytes: &[u8], now: Instant) {
        self.expire(now);
        if term.mode().contains(TermMode::ALT_SCREEN) || (self.semantic && !self.input_phase) {
            self.cancel();
            return;
        }
        if bytes == b"\x03" {
            self.cancel();
            return;
        }
        if bytes == b"\r" {
            if self.start.is_some() && self.pending.is_none() {
                self.pending = Some(Pending {
                    since: now,
                    line: None,
                });
            }
            return;
        }
        if self.start.is_none() {
            self.start = Some(start(term));
        }
    }

    fn expire(&mut self, now: Instant) {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| now.saturating_duration_since(p.since) >= SUBMIT_TIMEOUT)
        {
            self.cancel();
        }
    }

    fn cancel(&mut self) {
        self.start = None;
        self.pending = None;
    }

    fn finish(&mut self, line: String) -> Option<String> {
        self.pending = None;
        let start = self.start.take()?;
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
    if mysql_password(raw) {
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

/// Recognize the executable, not arbitrary arguments such as `find . -name mysql -print`.
/// This is deliberately a small shell-command recognizer, not an evaluator.
fn mysql_password(raw: &str) -> bool {
    const CLIENTS: &[&str] = &[
        "mysql",
        "mariadb",
        "mysqldump",
        "mysqladmin",
        "mysqlimport",
        "mysqlshow",
        "mysqlcheck",
        "mysqlpump",
    ];
    raw.split([';', '|', '&', '\n']).any(|clause| {
        let words: Vec<_> = clause
            .split_whitespace()
            .map(|w| w.trim_matches(['\'', '"']))
            .collect();
        let mut i = 0;
        while let Some(word) = words.get(i) {
            if word.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            }) {
                i += 1;
                continue;
            }
            let executable = word.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
            match executable.as_str() {
                "sudo" | "env" | "command" | "exec" => {
                    i += 1;
                    while let Some(option) = words.get(i).filter(|w| w.starts_with('-')) {
                        i += 1;
                        if *option == "--" {
                            break;
                        }
                        if matches!(
                            *option,
                            "-u" | "-g"
                                | "-h"
                                | "-p"
                                | "-C"
                                | "-T"
                                | "-R"
                                | "-D"
                                | "--user"
                                | "--group"
                                | "--host"
                                | "--prompt"
                                | "--unset"
                                | "--chdir"
                        ) {
                            i += 1;
                        }
                    }
                }
                client if CLIENTS.contains(&client) => {
                    // MySQL's uppercase -P is a port, not a password.
                    return words[i + 1..].iter().any(|w| w.starts_with("-p") && w.len() > 2);
                }
                _ => return false,
            }
        }
        false
    })
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
            self.capture.input(&self.term, text.as_bytes());
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
            s.output("\r\n");
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
        s.output("\r\n");
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
            s.output("\r\n");
            assert_eq!(s.found, [command]);
        }
    }
    #[test]
    fn echo_off_password_is_never_captured() {
        let mut s = Shell::new(80, 24);
        s.output("Password: ");
        s.key("super-secret-password");
        s.key("\r");
        s.output("\r\n");
        assert!(s.found.is_empty());
    }
    #[test]
    fn alternate_screen_and_space_and_duplicates_and_cancel() {
        let mut s = Shell::new(80, 24);
        s.output("\x1b[?1049h");
        s.key(":");
        s.output(":write file");
        s.key("\r");
        s.output("\r\n");
        s.output("\x1b[?1049l");
        for command in [" secret command", "x", "", "uptime", "uptime", "df -h", "uptime"] {
            s.output("\r\n$ ");
            s.key("x");
            s.output(command);
            s.key("\r");
            s.output("\r\n");
        }
        s.output("\r\n$ ");
        s.key("x");
        s.output("cancelled");
        s.key("\x03");
        s.key("\r");
        s.output("\r\n");
        assert_eq!(s.found, ["uptime", "df -h", "uptime"]);
    }
    #[test]
    fn redraw_and_cursor_edit_read_to_end_of_line() {
        let mut s = Shell::new(80, 24);
        s.output("user$ ");
        s.key("\x1b[A");
        s.output("\r\x1b[Kuser$ echo recalled\x1b[5D");
        s.key("\r");
        s.output("\r\n");
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
        s.output("\r\n");
        s.output("\x1b]133;C\x07\r\nPassword: ");
        s.key("x");
        s.output("echoed program input");
        s.key("\r");
        s.output("\r\n");
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
        s.output("\r\n");
        assert_eq!(s.found, ["uptime"]);
    }
    #[test]
    fn changed_prompt_is_not_saved_as_a_command() {
        let mut s = Shell::new(80, 24);
        s.output("old$ ");
        s.key("e");
        s.output("\r\x1b[Knew$ echo hi");
        s.key("\r");
        s.output("\r\n");
        assert!(s.found.is_empty());
    }
    #[test]
    fn osc_marks_in_one_chunk_and_st_terminators_use_their_exact_grid_position() {
        let mut s = Shell::new(80, 24);
        s.output("\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\uptime\x1b]133;C\x1b\\\r\n0.5\r\n\x1b]133;D;0\x1b\\\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\df -h\x1b]133;C\x1b\\");
        assert_eq!(s.found, ["uptime", "df -h"]);
    }
    #[test]
    fn enter_waits_for_the_last_echoed_characters() {
        let mut s = Shell::new(80, 24);
        s.output("$ ");
        s.key("ls -la");
        s.output("ls -l");
        s.key("\r");
        assert!(s.found.is_empty());
        s.output("a\r\noutput\r\n$ ");
        assert_eq!(s.found, ["ls -la"]);
    }

    #[test]
    fn delayed_echo_can_soft_wrap_and_scroll_after_enter() {
        let mut s = Shell::new(12, 3);
        let command = "echo abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz";
        s.output("$ ");
        s.key(command);
        s.output(&command[..5]);
        s.key("\r");
        for byte in command[5..].bytes().chain(b"\r\n".iter().copied()) {
            s.output(std::str::from_utf8(&[byte]).unwrap());
        }
        assert_eq!(s.found, [command]);
    }

    #[test]
    fn zsh_suggestion_is_erased_before_the_line_is_captured() {
        let mut s = Shell::new(80, 24);
        s.output("$ ");
        s.key("echo yes");
        s.output("echo yes\x1b[90m and unwanted\x1b[0m\x1b[13D");
        s.key("\r");
        assert!(s.found.is_empty());
        s.output("\x1b[K\r\n");
        assert_eq!(s.found, ["echo yes"]);
    }

    #[test]
    fn fish_redraw_and_chunk_boundaries_do_not_change_capture() {
        for fragmented in [false, true] {
            let mut s = Shell::new(80, 24);
            s.output("fish> ");
            s.key("echo fish");
            s.output("echo fi");
            s.key("\r");
            let output = "\r\x1b[Kfish> echo fish\r\nfish\r\nfish> ";
            if fragmented {
                for byte in output.bytes() {
                    s.output(std::str::from_utf8(&[byte]).unwrap());
                }
            } else {
                s.output(output);
            }
            assert_eq!(s.found, ["echo fish"]);
        }
    }

    #[test]
    fn capture_precedes_clear_and_cursor_motion_without_lf() {
        for suffix in ["\r\n\x1b[2J\x1b[H", "\x1b[E\x1b[2J\x1b[H"] {
            let mut s = Shell::new(80, 24);
            s.output("$ ");
            s.key("clear");
            s.output("clear");
            s.key("\r");
            s.output(suffix);
            assert_eq!(s.found, ["clear"]);
        }
    }

    #[test]
    fn timeout_cancellation_and_alternate_screen_discard_pending_input() {
        let mut s = Shell::new(80, 24);
        s.output("$ ");
        s.key("echo late");
        s.output("echo late");
        let now = Instant::now();
        s.capture.input_at(&s.term, b"\r", now);
        // Advance the clock deterministically, with no output or sleep.
        assert!(
            s.capture
                .advance_at(&mut s.term, &mut s.parser, b"", now + SUBMIT_TIMEOUT)
                .is_empty()
        );
        assert!(s.capture.pending.is_none());
        s.output("\r\n");
        assert!(s.found.is_empty());
        for cancel in ["\x03", "alternate"] {
            let mut s = Shell::new(80, 24);
            s.output("$ ");
            s.key("echo cancelled");
            s.output("echo cancelled");
            s.key("\r");
            if cancel == "alternate" {
                s.output("\x1b[?1049h\x1b[?1049l");
            } else {
                s.key(cancel);
            }
            s.output("\r\n");
            assert!(s.found.is_empty());
        }
    }

    #[test]
    fn semantic_c_is_authoritative_even_after_lf_and_clear() {
        let mut s = Shell::new(80, 24);
        s.output("\x1b]133;A\x07$ \x1b]133;B\x07");
        s.key("clear");
        s.output("cle");
        s.key("\r");
        s.output("ar\r\n\x1b[2J\x1b[H");
        assert!(s.found.is_empty());
        s.output("\x1b]133;C\x07");
        assert_eq!(s.found, ["clear"]);
    }

    #[test]
    fn mysql_family_passwords_and_common_p_flags() {
        for client in [
            "mysql",
            "mariadb",
            "mysqldump",
            "mysqladmin",
            "mysqlimport",
            "mysqlshow",
            "mysqlcheck",
            "mysqlpump",
        ] {
            for prefix in [
                "",
                "sudo ",
                "sudo -u root ",
                "FOO=bar ",
                "env FOO=bar ",
                "sudo env FOO=bar ",
            ] {
                for path in ["", "/usr/local/bin/"] {
                    assert!(!recordable(&format!("{prefix}{path}{client} -pSECRET")));
                }
            }
        }
        for command in [
            "find . -print",
            "find / -perm -4000",
            "find . -path ./x -prune",
            "ssh -p2222 host",
            "ps -p1234 -o pid,cmd",
            "mkdir -pv a/b",
            "gcc -pthread",
            "git log -p2",
            "find . -name mysql -print",
            "find . -path /bin/mysql -prune",
            "mysql -P3306 -p",
        ] {
            assert!(recordable(command), "{command}");
        }
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

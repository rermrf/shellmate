use shellmate_core::Secret;
use std::time::Duration;
/// Bounded, one-shot prompt recognizer. Never captures submitted input/history.
/// Manual input cancels it. Elapsed time is measured from TCP connection,
/// with a 10-second window from the first server bytes and a 30-second cap.
/// Both clocks are injectable; negotiation bytes count as server activity.
pub struct AutoLogin {
    user: String,
    password: Option<Secret>,
    tail: String,
    user_sent: bool,
    password_sent: bool,
    text: PromptText,
    first_bytes: Option<Duration>,
}
impl AutoLogin {
    pub fn new(user: String, password: Option<Secret>) -> Self {
        Self {
            user,
            password,
            tail: String::new(),
            user_sent: false,
            password_sent: false,
            text: PromptText::default(),
            first_bytes: None,
        }
    }
    pub fn cancel(&mut self) {
        self.password = None;
        self.tail.clear();
    }
    /// Call on every nonempty network read, before stripping Telnet negotiation.
    pub fn received(&mut self, elapsed: Duration) {
        if elapsed >= self.deadline() {
            self.cancel();
        }
        self.first_bytes.get_or_insert(elapsed);
    }
    /// Absolute elapsed deadline; subsequent reads never extend either window.
    pub fn deadline(&self) -> Duration {
        self.first_bytes.map_or(Duration::from_secs(30), |t| {
            t.saturating_add(Duration::from_secs(10))
                .min(Duration::from_secs(30))
        })
    }
    pub fn pending(&self) -> bool {
        self.password.is_some()
    }
    pub fn feed(&mut self, bytes: &[u8], elapsed: Duration) -> Vec<Secret> {
        if !bytes.is_empty() {
            self.received(elapsed);
        }
        if elapsed >= self.deadline() {
            self.cancel();
        }
        if self.password.is_none() {
            return Vec::new();
        }
        for b in self.text.feed(bytes).bytes() {
            if b == b'\n' || b == b'\r' {
                self.tail.clear();
            } else if b.is_ascii() {
                self.tail.push((b as char).to_ascii_lowercase());
            }
        }
        if self.tail.len() > 512 {
            self.tail.drain(..self.tail.len() - 512);
        }
        let prompt = self.tail.trim();
        let mut replies = Vec::new();
        if !self.user_sent && (prompt.ends_with("login:") || prompt.ends_with("username:")) {
            self.user_sent = true;
            self.tail.clear();
            replies.push(Secret::new(format!("{}\r\n", self.user)));
        } else if !self.password_sent
            && (prompt.ends_with("password:")
                || (prompt.contains("password for ") && prompt.ends_with(':')))
        {
            self.password_sent = true;
            let password = self.password.take().unwrap();
            replies.push(Secret::new(format!("{}\r\n", password.expose())));
            self.tail.clear();
        }
        replies
    }
    pub fn password_sent(&self) -> bool {
        self.password_sent
    }
}
/// Ignore ANSI decoration and OSC title strings, including chunk boundaries.
#[derive(Default)]
struct PromptText {
    state: u8,
}
impl PromptText {
    fn feed(&mut self, bytes: &[u8]) -> String {
        let mut out = Vec::new();
        for &b in bytes {
            match self.state {
                0 if b == 0x1b => self.state = 1,
                0 => out.push(b),
                1 => {
                    self.state = match b {
                        b'[' => 2,
                        b']' => 3,
                        _ => 0,
                    }
                }
                2 => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = 0;
                    }
                }
                3 => match b {
                    7 => self.state = 0,
                    0x1b => self.state = 4,
                    _ => {}
                },
                _ => self.state = if b == b'\\' { 0 } else { 3 },
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}
/// Text used for line/prompt matching, not a full VT screen emulator. Remove
/// CSI/OSC decoration, then keep the last CR redraw of each LF-delimited line.
/// A trailing CR (including CRLF) only moves the cursor; it does not erase the
/// line. Preserve it until this step so `"\rvalue\r\n"` still yields `"value"`.
pub fn display_text(text: &str) -> String {
    PromptText::default()
        .feed(text.as_bytes())
        .split('\n')
        .map(|line| {
            line.trim_end_matches('\r')
                .rsplit('\r')
                .next()
                .unwrap_or("")
        })
        .collect::<Vec<_>>()
        .join("\n")
}
/// Conventional Unix/router prompts for the editor's probe. This is heuristic:
/// unknown prompts remain inconclusive, and no probe command is ever executed.
pub fn authentication_result(text: &str) -> Option<bool> {
    let text = display_text(text);
    let lower = text.to_ascii_lowercase();
    if [
        "login incorrect",
        "authentication failed",
        "access denied",
        "login failed",
    ]
    .iter()
    .any(|e| lower.contains(e))
    {
        return Some(false);
    }
    let last = text.lines().last().unwrap_or("").trim_end();
    if last.ends_with('$') || last.ends_with('#') || last.ends_with('>') {
        Some(true)
    } else {
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn login() -> AutoLogin {
        AutoLogin::new("admin".into(), Some(Secret::new("test-password")))
    }
    #[test]
    fn ansi_prompts_split_at_every_byte() {
        let mut a = login();
        let mut replies = Vec::new();
        for byte in b"\x1b]0;device\x07\x1b[1mUsername:\x1b[0m" {
            replies.extend(a.feed(&[*byte], Duration::ZERO));
        }
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].expose(), "admin\r\n");
        assert_eq!(
            authentication_result("\x1b[32muser@router:~#\x1b[0m "),
            Some(true)
        );
    }
    #[test]
    fn test_connection_prompt_result() {
        assert_eq!(
            authentication_result("Welcome\r\nadmin@router:~# "),
            Some(true)
        );
        assert_eq!(
            authentication_result("Login incorrect\r\nlogin:"),
            Some(false)
        );
        assert_eq!(authentication_result("Password:"), None);
        assert_eq!(authentication_result("custom appliance prompt"), None);
    }
    #[test]
    fn decorated_redrawn_prompts_match_visible_text() {
        let mut a = login();
        let mut replies = Vec::new();
        for byte in b"old prompt\x1b[?2004l\r\x1b]0;title\x1b\\\x1b[32mUsername:\x1b[0m " {
            replies.extend(a.feed(&[*byte], Duration::ZERO));
        }
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].expose(), "admin\r\n");
        assert_eq!(
            authentication_result("Login incorrect\r\x1b[32muser@router:~#\x1b[0m \x1b[?2004h"),
            Some(true)
        );
        assert_eq!(authentication_result("user@router:~#\rPassword:"), None);
        assert_eq!(
            authentication_result("\x1b]0;title\x07\x1b[31mLogin incorrect\x1b[0m\r\nlogin:"),
            Some(false)
        );
    }
    #[test]
    fn once_chunked_and_common_prompts() {
        let mut a = login();
        assert!(a.feed(b"User", Duration::ZERO).is_empty());
        assert_eq!(a.feed(b"NAME: ", Duration::ZERO)[0].expose(), "admin\r\n");
        assert!(a.feed(b"login:", Duration::ZERO).is_empty());
        assert_eq!(
            a.feed(b"\nPassword for admin:", Duration::ZERO)[0].expose(),
            "test-password\r\n"
        );
        assert!(a.feed(b"Password:", Duration::ZERO).is_empty());
    }
    #[test]
    fn delayed_first_bytes_get_a_full_ten_seconds() {
        let mut a = login();
        assert_eq!(a.deadline(), Duration::from_secs(30));
        assert!(a.feed(&[], Duration::from_secs(10)).is_empty());
        a.received(Duration::from_secs(12)); // inetutils negotiation after reverse DNS
        assert_eq!(a.deadline(), Duration::from_secs(22));
        assert_eq!(
            a.feed(b"login:", Duration::from_secs(13))[0].expose(),
            "admin\r\n"
        );
        assert_eq!(
            a.feed(b"Password:", Duration::from_millis(21999))[0].expose(),
            "test-password\r\n"
        );
        assert!(!a.pending());
    }
    #[test]
    fn negotiation_starts_window_and_later_bytes_do_not_extend_it() {
        let mut a = login();
        a.received(Duration::from_secs(4)); // only IACs, no display bytes yet
        assert!(a.feed(&[], Duration::from_secs(4)).is_empty());
        a.received(Duration::from_secs(12));
        assert_eq!(a.deadline(), Duration::from_secs(14));
        assert!(a.feed(b"login:", Duration::from_secs(14)).is_empty());
        assert!(!a.pending());
    }
    #[test]
    fn overall_cap_includes_silence_and_truncates_a_late_window() {
        let mut a = login();
        a.received(Duration::from_secs(25));
        assert_eq!(a.deadline(), Duration::from_secs(30));
        assert_eq!(
            a.feed(b"login:", Duration::from_millis(29999))[0].expose(),
            "admin\r\n"
        );
        assert!(a.feed(b"Password:", Duration::from_secs(30)).is_empty());
        assert!(!a.pending());
        assert!(login().feed(b"login:", Duration::from_secs(30)).is_empty());
        assert!(
            login()
                .feed(b"Password:", Duration::from_secs(40))
                .is_empty()
        );
    }

    #[test]
    fn mismatch_timeout_absent_and_manual() {
        assert!(login().feed(b"PIN:", Duration::ZERO).is_empty());
        let mut expired = login();
        expired.received(Duration::ZERO);
        assert!(expired.feed(b"login:", Duration::from_secs(10)).is_empty());
        assert!(
            AutoLogin::new("a".into(), None)
                .feed(b"Password:", Duration::ZERO)
                .is_empty()
        );
        let mut a = login();
        a.cancel();
        assert!(a.feed(b"Password:", Duration::ZERO).is_empty());
    }
}

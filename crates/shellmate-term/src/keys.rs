//! Maps gpui keystrokes to the byte sequences a terminal application expects.
//!
//! Plain printable characters are *not* handled here: they arrive through the
//! platform input handler so that IMEs (Chinese/Japanese input) keep working.

use std::borrow::Cow;

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

/// Returns the escape sequence for `keystroke`, or `None` when the key should be
/// left to key bindings (⌘-shortcuts) or text input.
pub fn to_esc_str(keystroke: &Keystroke, mode: &TermMode) -> Option<Cow<'static, str>> {
    let m = &keystroke.modifiers;
    let key = keystroke.key.as_str();
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    if m.platform && m.control && !m.alt && !m.shift && key == "f" { return None; }

    // macOS editing conventions: ⌘← / ⌘→ / ⌘⌫ act on the whole line.
    if m.platform && !m.control && !m.alt {
        return match key {
            "left" => Some("\x01".into()),
            "right" => Some("\x05".into()),
            "backspace" => Some("\x15".into()),
            _ => None,
        };
    }
    // Arrows / navigation with modifiers use the xterm `1;<mod>` form.
    let modifier_code = 1 + (m.shift as u8) + 2 * (m.alt as u8) + 4 * (m.control as u8);
    let csi_mod = |final_byte: char| -> Cow<'static, str> { format!("\x1b[1;{modifier_code}{final_byte}").into() };
    let tilde_mod = |code: u8| -> Cow<'static, str> {
        if modifier_code == 1 { format!("\x1b[{code}~").into() } else { format!("\x1b[{code};{modifier_code}~").into() }
    };

    match key {
        "up" | "down" | "right" | "left" => {
            let final_byte = match key {
                "up" => 'A',
                "down" => 'B',
                "right" => 'C',
                _ => 'D',
            };
            // ⌥← / ⌥→ jump by word like macOS text fields (readline / zle bindings).
            if m.alt && !m.control && !m.shift && matches!(key, "left" | "right") {
                return Some(if key == "left" { "\x1bb" } else { "\x1bf" }.into());
            }
            if modifier_code > 1 {
                return Some(csi_mod(final_byte));
            }
            return Some(if app_cursor { format!("\x1bO{final_byte}") } else { format!("\x1b[{final_byte}") }.into());
        }
        "home" | "end" => {
            let final_byte = if key == "home" { 'H' } else { 'F' };
            if modifier_code > 1 {
                return Some(csi_mod(final_byte));
            }
            return Some(if app_cursor { format!("\x1bO{final_byte}") } else { format!("\x1b[{final_byte}") }.into());
        }
        "pageup" => return Some(tilde_mod(5)),
        "pagedown" => return Some(tilde_mod(6)),
        "insert" => return Some(tilde_mod(2)),
        "delete" => return Some(tilde_mod(3)),
        _ => {}
    }

    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return function_key(n, modifier_code);
    }

    match (key, m.control, m.alt, m.shift) {
        ("enter", false, false, _) => return Some("\r".into()),
        ("enter", false, true, _) => return Some("\x1b\r".into()),
        ("backspace", false, false, _) => return Some("\x7f".into()),
        ("backspace", true, false, _) => return Some("\x08".into()),
        ("backspace", false, true, _) => return Some("\x1b\x7f".into()),
        ("tab", false, false, false) => return Some("\t".into()),
        ("tab", false, false, true) => return Some("\x1b[Z".into()),
        ("escape", false, false, _) => return Some("\x1b".into()),
        ("space", true, false, _) => return Some("\0".into()),
        _ => {}
    }

    if m.control && !m.alt {
        return control_char(key).map(|c| Cow::Owned(c.to_string()));
    }
    if m.control && m.alt {
        return control_char(key).map(|c| Cow::Owned(format!("\x1b{c}")));
    }
    None
}

fn control_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let byte = match c {
        'a'..='z' => c as u8 - b'a' + 1,
        '@' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '7' => 0x1f,
        '/' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    };
    Some(byte as char)
}

fn function_key(n: u8, modifier_code: u8) -> Option<Cow<'static, str>> {
    let ss3 = |c: char| -> Cow<'static, str> {
        if modifier_code == 1 { format!("\x1bO{c}").into() } else { format!("\x1b[1;{modifier_code}{c}").into() }
    };
    let tilde = |code: u8| -> Cow<'static, str> {
        if modifier_code == 1 { format!("\x1b[{code}~").into() } else { format!("\x1b[{code};{modifier_code}~").into() }
    };
    Some(match n {
        1 => ss3('P'),
        2 => ss3('Q'),
        3 => ss3('R'),
        4 => ss3('S'),
        5 => tilde(15),
        6 => tilde(17),
        7 => tilde(18),
        8 => tilde(19),
        9 => tilde(20),
        10 => tilde(21),
        11 => tilde(23),
        12 => tilde(24),
        _ => return None,
    })
}

/// Wraps pasted text for the shell: bracketed paste when the app enabled it,
/// otherwise newline normalisation so multi-line pastes become carriage returns.
pub fn paste_payload(text: &str, mode: &TermMode) -> String {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        // Strip ESC so pasted content can't terminate the bracket early.
        format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn menubar_shortcuts_never_encode_shell_input() {
        for key in ["cmd-n", "cmd-d", "cmd-y", "cmd-,", "cmd-h", "alt-cmd-h", "cmd-m", "ctrl-cmd-f",
            "alt-cmd-1", "alt-cmd-2", "alt-cmd-3", "alt-cmd-4"] {
            assert_eq!(esc(key), None, "{key}");
        }
        assert_eq!(esc("ctrl-r").as_deref(), Some("\x12"));
    }
    use super::*;

    fn ks(s: &'static str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    fn esc(s: &'static str) -> Option<String> {
        to_esc_str(&ks(s), &TermMode::default()).map(|c| c.into_owned())
    }

    #[test]
    fn plain_text_is_left_to_ime() {
        assert_eq!(esc("a"), None);
        assert_eq!(esc("shift-a"), None);
        assert_eq!(esc("space"), None);
    }

    #[test]
    fn editing_keys() {
        assert_eq!(esc("enter").as_deref(), Some("\r"));
        assert_eq!(esc("backspace").as_deref(), Some("\x7f"));
        assert_eq!(esc("tab").as_deref(), Some("\t"));
        assert_eq!(esc("shift-tab").as_deref(), Some("\x1b[Z"));
        assert_eq!(esc("escape").as_deref(), Some("\x1b"));
        assert_eq!(esc("delete").as_deref(), Some("\x1b[3~"));
    }

    #[test]
    fn control_letters() {
        assert_eq!(esc("ctrl-c").as_deref(), Some("\x03"));
        assert_eq!(esc("ctrl-a").as_deref(), Some("\x01"));
        assert_eq!(esc("ctrl-[").as_deref(), Some("\x1b"));
        assert_eq!(esc("ctrl-space").as_deref(), Some("\0"));
    }

    #[test]
    fn arrows_respect_app_cursor_mode() {
        assert_eq!(esc("up").as_deref(), Some("\x1b[A"));
        let app = TermMode::APP_CURSOR;
        assert_eq!(to_esc_str(&ks("up"), &app).as_deref(), Some("\x1bOA"));
        assert_eq!(esc("ctrl-right").as_deref(), Some("\x1b[1;5C"));
        assert_eq!(esc("shift-up").as_deref(), Some("\x1b[1;2A"));
    }

    #[test]
    fn mac_conventions() {
        assert_eq!(esc("alt-left").as_deref(), Some("\x1bb"));
        assert_eq!(esc("alt-right").as_deref(), Some("\x1bf"));
        assert_eq!(esc("cmd-left").as_deref(), Some("\x01"));
        assert_eq!(esc("cmd-backspace").as_deref(), Some("\x15"));
        assert_eq!(esc("cmd-c"), None);
        assert_eq!(esc("cmd-v"), None);
    }

    #[test]
    fn function_keys() {
        assert_eq!(esc("f1").as_deref(), Some("\x1bOP"));
        assert_eq!(esc("f5").as_deref(), Some("\x1b[15~"));
        assert_eq!(esc("f12").as_deref(), Some("\x1b[24~"));
    }

    #[test]
    fn paste_normalises_newlines_or_brackets() {
        assert_eq!(paste_payload("a\nb\r\nc", &TermMode::default()), "a\rb\rc");
        assert_eq!(
            paste_payload("x\x1by", &TermMode::BRACKETED_PASTE),
            "\x1b[200~xy\x1b[201~"
        );
    }
}

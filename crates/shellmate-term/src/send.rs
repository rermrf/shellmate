//! Reviewed command delivery. Each backend write is an ordered frame.
use alacritty_terminal::term::TermMode;

pub(crate) fn send(
    command: &str,
    mode: &TermMode,
    mut write: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> Result<(), &'static str> {
    validate(command, mode)?;
    let paste = crate::keys::paste_payload(command, mode);
    write(paste.as_bytes()).map_err(|_| "命令粘贴失败，未发送回车")?;
    write(b"\r").map_err(|_| "回车发送失败，请检查会话状态")
}

/// Preflight a fan-out before any target receives bytes.
pub(crate) fn validate(command: &str, mode: &TermMode) -> Result<(), &'static str> {
    if command.contains(['\n', '\r']) && !mode.contains(TermMode::BRACKETED_PASTE) {
        return Err("当前终端未启用括号粘贴，无法安全发送多行命令");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paste_and_return_are_separate_ordered_frames() {
        for (mode, text, expected) in [
            (TermMode::default(), "uptime", "uptime"),
            (TermMode::BRACKETED_PASTE, "uptime", "\x1b[200~uptime\x1b[201~"),
            (
                TermMode::BRACKETED_PASTE,
                "echo one\necho two",
                "\x1b[200~echo one\necho two\x1b[201~",
            ),
        ] {
            let mut frames = vec![];
            send(text, &mode, |frame| {
                frames.push(frame.to_vec());
                Ok(())
            })
            .unwrap();
            assert_eq!(frames, [expected.as_bytes(), b"\r"]);
        }
    }
    #[test]
    fn a_failed_paste_never_sends_return() {
        let mut calls = 0;
        assert!(
            send("uptime", &TermMode::default(), |_| {
                calls += 1;
                Err(std::io::ErrorKind::BrokenPipe.into())
            })
            .is_err()
        );
        assert_eq!(calls, 1);
    }
    #[test]
    fn refuses_unbracketed_multiline_without_writing_any_bytes() {
        for text in ["one\ntwo", "one\rtwo", "one\r\ntwo"] {
            assert!(send(text, &TermMode::default(), |_| panic!("must not write")).is_err());
        }
    }
}

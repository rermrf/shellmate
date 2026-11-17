//! Paths and wall-clock formatting shared by recordings and session logs.
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Reproduce only the active prompt row. This goes straight to the cast writer,
/// never through the output tap (which also feeds the plain-text log).
pub fn cursor_line_seed<T>(term: &alacritty_terminal::term::Term<T>) -> Option<String> {
    use alacritty_terminal::{
        grid::Dimensions,
        index::Column,
        term::{TermMode, cell::Flags},
        vte::ansi::Color,
    };
    use std::fmt::Write;
    if term.mode().contains(TermMode::ALT_SCREEN) {
        return None;
    }
    let cursor = term.grid().cursor.point;
    let row = &term.grid()[cursor.line];
    let last = (0..term.columns()).rfind(|&col| {
        let cell = &row[Column(col)];
        cell.c != ' ' || cell.zerowidth().is_some()
    })?;
    let mut seed = String::new();
    for col in 0..=last.max(cursor.column.0) {
        let cell = &row[Column(col)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        seed.push_str("\x1b[0");
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::UNDERLINE, "4"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
            (Flags::DOUBLE_UNDERLINE, "4:2"),
            (Flags::UNDERCURL, "4:3"),
            (Flags::DOTTED_UNDERLINE, "4:4"),
            (Flags::DASHED_UNDERLINE, "4:5"),
        ] {
            if cell.flags.contains(flag) {
                write!(seed, ";{code}").unwrap();
            }
        }
        for (color, base) in [(cell.fg, 30), (cell.bg, 40)] {
            match color {
                Color::Spec(rgb) => {
                    write!(seed, ";{};2;{};{};{}", base + 8, rgb.r, rgb.g, rgb.b).unwrap()
                }
                Color::Indexed(index) => write!(seed, ";{};5;{index}", base + 8).unwrap(),
                Color::Named(name) => {
                    let index = name as usize;
                    if index < 8 {
                        write!(seed, ";{}", base + index).unwrap();
                    } else if index < 16 {
                        write!(seed, ";{}", base + 60 + index - 8).unwrap();
                    }
                    // Default foreground/background are restored by SGR 0.
                }
            }
        }
        seed.push('m');
        seed.push(cell.c);
        if let Some(extra) = cell.zerowidth() {
            seed.extend(extra);
        }
    }
    seed.push_str("\x1b[0m\r");
    if cursor.column.0 != 0 {
        write!(seed, "\x1b[{}C", cursor.column.0).unwrap();
    }
    Some(seed)
}
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
pub fn local_time(unix: i64, format: &str) -> String {
    let timestamp = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: all pointers reference live, correctly sized buffers.
    if unsafe { libc::localtime_r(&timestamp, &mut tm) }.is_null() {
        return String::new();
    }
    let Ok(format) = std::ffi::CString::new(format) else {
        return String::new();
    };
    let mut buf = [0u8; 128];
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), format.as_ptr(), &tm) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}
pub fn safe_label(label: &str) -> String {
    let mut text: String = label
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '.') {
                '_'
            } else {
                c
            }
        })
        .take(80)
        .collect();
    text = text.trim().to_string();
    while text.len() > 200 {
        text.pop();
    }
    if text.is_empty() { "host".into() } else { text }
}
pub fn data_root() -> PathBuf {
    std::env::var_os("SHELLMATE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::home_dir()
                .unwrap_or_default()
                .join("Library/Application Support/Shellmate")
        })
}
pub fn log_root() -> PathBuf {
    std::env::var_os("SHELLMATE_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::home_dir()
                .unwrap_or_default()
                .join("Library/Logs/Shellmate")
        })
}
pub fn create_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut dir = fs::DirBuilder::new();
    dir.recursive(true).mode(0o700);
    dir.create(path)
}
pub fn open_file(path: &Path, append: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.write(true).mode(0o600);
    if append {
        options.create(true).append(true);
    } else {
        options.create_new(true);
    }
    options.open(path)
}
pub fn recording_file(root: &Path, label: &str, unix: i64) -> io::Result<(PathBuf, File)> {
    let dir = root.join("recordings").join(safe_label(label));
    create_dir(&dir)?;
    let name = local_time(unix, "%Y%m%d-%H%M%S");
    for index in 0..10000 {
        let suffix = if index == 0 {
            String::new()
        } else {
            format!("-{index}")
        };
        let path = dir.join(format!("{name}{suffix}.cast"));
        match open_file(&path, false) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "录制文件名已被占用",
    ))
}
pub fn mmss(seconds: f64) -> String {
    let seconds = seconds.max(0.) as u64;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_log::{Silent, Size};
    use alacritty_terminal::{
        index::{Column, Line},
        term::{Config, Term},
        vte::ansi::Processor,
    };

    fn seed_roundtrip(text: &str, cols: usize) -> (Term<Silent>, Term<Silent>) {
        let mut source = Term::new(Config::default(), &Size { cols, rows: 3 }, Silent);
        Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new()
            .advance(&mut source, text.as_bytes());
        let seed = cursor_line_seed(&source).unwrap();
        let mut header = crate::cast::Header::new(cols as u16, 3);
        header.timestamp = Some(1700000000);
        let mut writer = crate::cast::Writer::new(Vec::new(), &header).unwrap();
        writer.output(0., seed.as_bytes()).unwrap();
        let (bytes, _) = writer.finish(0.).unwrap();
        let cast = crate::cast::Cast::read(bytes.as_slice()).unwrap();
        assert_eq!(cast.header, header);
        assert_eq!(cast.events[0].at, 0.);
        assert_eq!(cast.events[0].recorded_at, 0.);
        let crate::cast::Data::Output(output) = &cast.events[0].data else {
            panic!("output event");
        };
        let mut target = Term::new(Config::default(), &Size { cols, rows: 3 }, Silent);
        Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new()
            .advance(&mut target, output.as_bytes());
        for col in 0..cols {
            assert_eq!(
                source.grid()[source.grid().cursor.point.line][Column(col)],
                target.grid()[Line(0)][Column(col)],
                "column {col}"
            );
        }
        assert_eq!(
            source.grid().cursor.point.column,
            target.grid().cursor.point.column
        );
        (source, target)
    }

    #[test]
    fn seed_mixed_sgr_cells_cursor_and_cast_roundtrip() {
        seed_roundtrip(
            "old row\r\n\x1b[1;2;3;4;7;9;31;104mA\x1b[0;38;5;196;48;5;22mB\x1b[0;38;2;1;2;3;48;2;4;5;6mC\x1b[0m\"\\tail\r\x1b[2C",
            30,
        );
        seed_roundtrip("root@host:~# ", 30);
        seed_roundtrip("abc\r", 30);
        seed_roundtrip("abc\x1b[7C", 30);
    }

    #[test]
    fn seed_cjk_wide_and_combining_cells_roundtrip() {
        seed_roundtrip("\x1b[1;38;5;42m中文\x1b[0m e\u{301}$ ", 20);
        seed_roundtrip("ab中文", 6);
    }

    #[test]
    fn seed_empty_and_alt_screen_are_omitted() {
        let mut term = Term::new(Config::default(), &Size { cols: 20, rows: 3 }, Silent);
        assert!(cursor_line_seed(&term).is_none());
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"   ");
        assert!(cursor_line_seed(&term).is_none());
        parser.advance(&mut term, b"prompt\x1b[?1049halt");
        assert!(cursor_line_seed(&term).is_none());
    }

    #[test]
    fn seeded_first_prompt_is_learned_before_input_and_with_later_prompts() {
        let (_, mut replay) = seed_roundtrip("root@serv2147439633:~# ", 80);
        let mut highlighter = crate::prompt::PromptHighlighter::default();
        highlighter.learn_replay_seed(&replay);
        highlighter.learn_replay(&replay);
        assert!(highlighter.row(&replay, Line(0)).is_some());
        Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new()
            .advance(&mut replay, b"pwd\r\n/root\r\nroot@serv2147439633:~# ");
        highlighter.learn_replay(&replay);
        assert!(highlighter.row(&replay, Line(0)).is_some());
        assert!(crate::replay::screen_text(&replay).starts_with("root@serv2147439633:~# pwd"));
    }

    #[test]
    fn unicode_labels_fit_a_filesystem_component() {
        assert!(safe_label(&"😀".repeat(200)).len() <= 200);
    }
    #[test]
    fn labels_cannot_escape_the_archive_directory() {
        for raw in [
            "..",
            "../etc",
            "/tmp/x",
            ".hidden",
            "a..b",
            "a\0b",
            " ",
            "数据库/生产",
            "a\\b",
        ] {
            let s = safe_label(raw);
            assert!(!s.starts_with('.'));
            assert!(!s.contains(['/', '\\', '\0']));
            assert!(!s.contains(".."));
            assert!(!s.is_empty());
            assert_eq!(Path::new(&s).components().count(), 1);
        }
    }
}

//! Paths and wall-clock formatting shared by recordings and session logs.
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
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
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "录制文件名已被占用"))
}
pub fn mmss(seconds: f64) -> String {
    let seconds = seconds.max(0.) as u64;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}
#[cfg(test)]
mod tests {
    use super::*;
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

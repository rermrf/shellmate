//! Terminal control cleanup and bounded head/tail excerpts; content is not redacted.
use std::time::Duration;

pub const STORED_LIMIT: usize = 8 * 1024;
const HALF: usize = 4 * 1024;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    stdout: String,
    stderr: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub disconnected: bool,
    pub truncated: bool,
    pub output_id: Option<String>,
    pub task_id: Option<String>,
    pub message: Option<String>,
    pub successful: bool,
    pub total_bytes: u64,
}
impl ToolOutput {
    pub fn stdout(&self) -> &str {
        &self.stdout
    }
    pub fn stderr(&self) -> &str {
        &self.stderr
    }
    pub fn wire_text(&self) -> String {
        if let Some(id) = &self.output_id {
            if let Some(message) = &self.message {
                return message.clone();
            }
            return format!(
                "输出 id {id} · 总计 {} 字节 · 退出码 {} · {} ms\n{}",
                self.total_bytes,
                self.exit_code
                    .map_or_else(|| "未知".into(), |n| n.to_string()),
                self.duration_ms,
                self.stdout
            );
        }
        format!(
            "退出码：{}\n耗时：{} ms\n超时：{}\n截断：{}\n<不可信标准输出>\n{}\n</不可信标准输出>\n<不可信标准错误>\n{}\n</不可信标准错误>",
            self.exit_code
                .map_or_else(|| "未知".into(), |n| n.to_string()),
            self.duration_ms,
            self.timed_out,
            self.truncated,
            self.stdout,
            self.stderr
        )
    }
    /// Hard byte cap includes labels and the omission marker.
    pub fn stored(&self) -> String {
        if self.output_id.is_some() {
            return stored_text(&self.wire_text());
        }
        stored_text(&format!(
            "标准输出：\n{}\n标准错误：\n{}",
            self.stdout, self.stderr
        ))
    }
}
pub fn shape(
    stdout: impl AsRef<[u8]>,
    stderr: impl AsRef<[u8]>,
    exit: Option<i32>,
    duration: Duration,
    timed_out: bool,
    truncated: bool,
) -> ToolOutput {
    let stdout = clean(stdout.as_ref());
    let stderr = clean(stderr.as_ref());
    let clipped = stdout.len() > 2 * HALF || stderr.len() > 2 * HALF;
    ToolOutput {
        stdout: clip(&stdout, HALF, HALF),
        stderr: clip(&stderr, HALF, HALF),
        exit_code: exit,
        duration_ms: duration.as_millis().min(u64::MAX as u128) as u64,
        timed_out,
        disconnected: false,
        truncated: truncated || clipped,
        output_id: None,
        task_id: None,
        message: None,
        successful: exit == Some(0) && !timed_out,
        total_bytes: (stdout.len() + stderr.len()) as u64,
    }
}
/// Model output uses character ceilings; the persisted UI excerpt keeps its 8 KiB cap.
#[allow(clippy::too_many_arguments)]
pub fn spooled(
    id: &str,
    command: &str,
    bytes: &[u8],
    total: u64,
    exit: Option<i32>,
    duration: Duration,
    limits: crate::agent::ExecLimits,
) -> ToolOutput {
    let text = clean(bytes);
    let success = successful(command, exit);
    let ceiling = limits.output_chars().min(128_000);
    let truncated = text.chars().count() > ceiling || total > bytes.len() as u64;
    let stdout = if success {
        if truncated {
            format!(
                "{}\n完整输出已保存，用 read_output 查看或搜索",
                text.chars().take(2000).collect::<String>()
            )
        } else {
            text
        }
    } else {
        char_excerpt(&char_excerpt(&text, limits.output_chars()), 10_000)
    };
    ToolOutput {
        stdout,
        stderr: String::new(),
        exit_code: exit,
        duration_ms: duration.as_millis().min(u64::MAX as u128) as u64,
        timed_out: false,
        disconnected: false,
        truncated,
        output_id: Some(id.into()),
        task_id: None,
        message: None,
        successful: success,
        total_bytes: total,
    }
}
pub fn char_excerpt(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.into();
    }
    let marker = "\n[省略中间输出]\n";
    let keep = max.saturating_sub(marker.chars().count());
    let head: String = text.chars().take(keep / 2).collect();
    let tail: String = text.chars().skip(count - (keep - keep / 2)).collect();
    format!("{head}{marker}{tail}")
}
pub fn successful(command: &str, exit: Option<i32>) -> bool {
    if exit == Some(0) {
        return true;
    }
    if exit != Some(1) {
        return false;
    }
    let words = crate::policy::command_words(command);
    matches!(
        words.first().map(String::as_str),
        Some("grep" | "rg" | "egrep" | "fgrep" | "find" | "diff" | "test" | "[")
    ) || (words.first().is_some_and(|s| s == "git")
        && words
            .get(1)
            .is_some_and(|s| matches!(s.as_str(), "diff" | "grep")))
}
/// Also used at the store boundary, which must never trust caller-created Step strings.
pub fn stored_text(text: &str) -> String {
    let text = clean(text.as_bytes());
    if text.len() <= STORED_LIMIT {
        return text;
    }
    // Reserve the marker first; recalculate until the cap includes all its digits.
    let mut keep = STORED_LIMIT - 64;
    loop {
        let result = clip(&text, keep / 2, keep - keep / 2);
        if result.len() <= STORED_LIMIT {
            return result;
        }
        keep -= result.len() - STORED_LIMIT;
    }
}
fn clip(text: &str, head: usize, tail: usize) -> String {
    if text.len() <= head + tail {
        return text.into();
    }
    let mut start = head.min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = text.len().saturating_sub(tail);
    while !text.is_char_boundary(end) {
        end += 1;
    }
    format!(
        "{}\n[省略 {} 字节]\n{}",
        &text[..start],
        end - start,
        &text[end..]
    )
}
pub fn clean(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars().peekable();
    let mut plain = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.peek() == Some(&'\\')) {
                            if c == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(c) if (' '..='/').contains(&c) => {
                    for c in chars.by_ref() {
                        if ('0'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\u{9b}' => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            '\u{9d}' => {
                for c in chars.by_ref() {
                    if matches!(c, '\x07' | '\u{9c}') {
                        break;
                    }
                }
            }
            '\r' | '\n' | '\t' => plain.push(c),
            c if !c.is_control() => plain.push(c),
            _ => {}
        }
    }
    // CR returns the cursor to column zero; a shorter replacement retains the suffix.
    let mut normalized = String::new();
    let mut line = Vec::new();
    let mut cursor = 0;
    for c in plain.chars() {
        match c {
            '\r' => cursor = 0,
            '\n' => {
                normalized.extend(line.drain(..));
                normalized.push('\n');
                cursor = 0;
            }
            _ => {
                if cursor < line.len() {
                    line[cursor] = c;
                } else {
                    line.push(c);
                }
                cursor += 1;
            }
        }
    }
    normalized.extend(line);
    normalized
}
#[cfg(test)]
mod tests {
    use super::*;
    fn output(s: &[u8]) -> ToolOutput {
        shape(s, [], Some(0), Duration::from_millis(42), false, false)
    }
    #[test]
    fn ansi_osc_cr_cleanup_and_invalid_utf8() {
        let o = output(b"\x1b[31mred\x1b[0m\x1b]0;title\x07\x1b]8;;url\x1b\\link\x1b]8;;\x1b\\\n100%\r20%\r9\nPASSWORD=hidden\n\xff");
        assert_eq!(o.stdout(), "redlink\n90%%\nPASSWORD=hidden\n�");
        assert!(o.wire_text().contains("hidden"));
        assert!(o.stored().contains("hidden"));
        assert_eq!(output(b"a\r\nb").stdout(), "a\nb");
    }
    #[test]
    fn head_tail_exact_bytes_and_utf8_boundaries() {
        let s = format!(
            "{}{}{}",
            "a".repeat(4096),
            "m".repeat(123),
            "z".repeat(4096)
        );
        let o = output(s.as_bytes());
        assert_eq!(
            o.stdout(),
            format!(
                "{}\n[省略 123 字节]\n{}",
                "a".repeat(4096),
                "z".repeat(4096)
            )
        );
        assert!(o.truncated);
        assert!(o.stored().len() <= STORED_LIMIT);
        let s = "中".repeat(4000);
        let o = output(s.as_bytes());
        assert_eq!(
            o.stdout(),
            format!(
                "{}\n[省略 3810 字节]\n{}",
                "中".repeat(1365),
                "中".repeat(1365)
            )
        );
        assert!(o.stored().len() <= STORED_LIMIT);
    }
}

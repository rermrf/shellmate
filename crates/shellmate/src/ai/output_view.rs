//! Only unwrap Shellmate's persisted tool-result envelope, never the underlying log file.
pub struct OutputView<'a> {
    pub preview: &'a str,
    pub output_id: Option<&'a str>,
    pub total_bytes: Option<u64>,
    pub truncated: bool,
}
pub fn display(output: &str) -> OutputView<'_> {
    let original = || OutputView {
        preview: output,
        output_id: None,
        total_bytes: None,
        truncated: false,
    };
    let Some((header, preview)) = output.split_once('\n') else {
        return original();
    };
    let Some((id, rest)) = header
        .strip_prefix("输出 id ")
        .and_then(|s| s.split_once(" · 总计 "))
    else {
        return original();
    };
    let Some((bytes, status)) = rest.split_once(" 字节 · ") else {
        return original();
    };
    let Ok(bytes) = bytes.parse::<u64>() else {
        return original();
    };
    let valid = if let Some(exit) = status.strip_prefix("退出码 ") {
        let exit = exit.split(" · ").next().unwrap_or_default();
        exit == "未知" || exit.parse::<i32>().is_ok()
    } else {
        status.strip_prefix("日志读取成功 · 原命令：").is_some_and(|s| !s.is_empty())
    };
    if id.is_empty() || !valid {
        return original();
    }
    let footer = "\n完整输出已保存，用 read_output 查看或搜索";
    OutputView {
        preview: preview.strip_suffix(footer).unwrap_or(preview),
        output_id: Some(id),
        total_bytes: Some(bytes),
        truncated: preview.ends_with(footer)
            || preview.contains("[省略")
            || preview.contains("[... omitted ...]"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hides_only_the_envelope_and_preserves_literal_identifiers_in_the_body() {
        let body = "输出 id literal-user-uuid\nA path: /tmp/a.log\n[stderr] failed";
        let output = format!("输出 id output-1 · 总计 100 字节 · 退出码 1\n{body}");
        let view = display(&output);
        assert_eq!(view.preview, body);
        assert_eq!(view.output_id, Some("output-1"));
        assert_eq!(view.total_bytes, Some(100));
        assert!(!view.truncated);
    }
    #[test]
    fn model_preview_can_be_short_while_the_saved_output_is_long() {
        let output = "输出 id output-1 · 总计 1048576 字节 · 退出码 0\npreview\n完整输出已保存，用 read_output 查看或搜索";
        let view = display(output);
        assert_eq!(view.preview, "preview");
        assert!(view.truncated);
        assert_eq!(view.total_bytes, Some(1048576));
    }
    #[test]
    fn ordinary_legacy_and_malformed_outputs_are_unchanged() {
        for output in [
            "",
            "标准输出：\nok\n标准错误：\n",
            "输出 id example\nunchanged",
            "输出 id x · 总计 oops 字节 · 退出码 0\nbody",
            "输出 id x · 总计 10 字节 · user text\nbody",
            "输出 id x · 总计 10 字节 · 日志读取成功 · 原命令：\nbody",
        ] {
            let view = display(output);
            assert_eq!(view.preview, output);
            assert!(view.output_id.is_none());
        }
    }
}
